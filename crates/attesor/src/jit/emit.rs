//! AArch64 -> x86-64 block translator.
//!
//! Conventions for generated code:
//!  * `r15` holds `&mut Cpu`; guest registers live in the Cpu struct.
//!  * `rsp` is 16-byte aligned, so helpers can be `call`ed directly.
//!  * Generated code never embeds absolute host addresses: helpers and the
//!    exit trampoline are reached through `[r15 + off]`. Code is therefore
//!    position independent, which lets the AOT cache store it on disk.
//!  * Instructions the translator does not handle natively are executed by
//!    calling the reference interpreter (`HELPER_INTERP`) for that single
//!    instruction; all guest state is in memory, so this is always safe.

use iced_x86::code_asm::*;
use attesor_core::cpu::off;
use attesor_core::interp::simd::vfp_expand_imm;
use std::collections::HashMap;

pub const HELPER_INTERP: usize = 0;
pub const HELPER_COMMPAGE: usize = 1;

pub const EXIT_CONTINUE: u64 = 0;
pub const EXIT_SVC: u64 = 1;
pub const EXIT_PENDING: u64 = 2;
/// The block needs the interpreter (FPCR not in the default mode the native FP code assumes).
pub const EXIT_INTERP: u64 = 3;
/// FPCR bits the native FP translations cannot honour: AHP, DN, FZ16. (RMode and FZ are applied to the
/// host MXCSR whenever FPCR is written, see interp::sys::sync_host_fp_mode.)
const FPCR_NONDEFAULT: u32 = 0x0608_0000;

pub const MAX_BLOCK_INSNS: usize = 96;

/// How a block leaves to a direct successor that isn't part of this unit.
#[derive(Clone, Copy, PartialEq)]
pub enum StubKind {
    /// Exit through a patchable slot (JIT): `jmp [slot]`, slot initially
    /// points to the exit continuation.
    Patchable,
    /// Plain exit to the dispatcher (AOT blobs).
    Plain,
}

pub struct Stub {
    pub target: u64,
    /// Label of the continuation code the slot initially points to.
    pub cont: CodeLabel,
    /// Label of the 8-byte chaining slot (Patchable only).
    pub slot: CodeLabel,
}

/// Emission context shared by all blocks assembled into one unit.
pub struct Unit {
    pub a: CodeAssembler,
    /// Guest pc -> label of a block inside this unit.
    pub labels: HashMap<u64, CodeLabel>,
    pub stubs: Vec<Stub>,
    pub stub_kind: StubKind,
    /// Verification mode: every block exits after one pass (no self loops).
    pub no_internal_jumps: bool,
    /// Padding inserted before the slot table so slots are 8-byte aligned.
    pub slot_padding: usize,
    /// Label of the first slot (to check alignment after assembly).
    pub first_slot: Option<CodeLabel>,
    /// Emit commpage redirection checks on memory accesses.
    pub commpage_checks: bool,
    pub exit_label: CodeLabel,
    /// Stubs emitted by `finish` (slot init needs their continuation labels).
    pub finished_stubs: Vec<Stub>,
    pub insns_translated: u64,
    pub insns_fallback: u64,
    /// Set by a flag-setting instruction whose x86 flags are still live (nothing emitted after it
    /// touches them); consumed by the very next instruction of the block to branch/select on the
    /// x86 flags directly instead of re-deriving the condition from the stored NZCV bytes.
    pub fuse: Option<FuseKind>,
    /// Write-through cache of guest general registers in callee-saved x86 registers that the JIT
    /// never uses otherwise (see `CREGS`). Memory stays authoritative, so faults, signals and
    /// helper calls always see current values; the cache only saves reloads. Compile-time state,
    /// reset at every public block entry (natural-loop private entries reconcile it).
    pub rc: RegCache,
    // Private loop entry, never published in Unit labels or AOT registration.
    loop_cache: Option<LoopCache>,
    regioncache: bool,
    /// Set once an instruction has created an internal label (conditional code follows): cached
    /// state may then be used but not extended, or paths could disagree about what is cached.
    pub rc_frozen: bool,
    /// The next few instruction words of the current block (up to and including a terminator):
    /// only used to decide whether caching a register is worthwhile.
    pub look: Vec<u32>,
    /// Bank-local next reads for the current window; MAX means dead or not seen.
    cache_uses: [[u8; 32]; 2],
    /// The flag-setting instruction being translated feeds a fused branch/select and its NZCV bytes
    /// are dead afterwards: skip the four flag stores.
    pub dead_flags: bool,
    /// The instruction just translated also covered the next one (a fused pair such as AESE+AESMC).
    pub skip_next: bool,
    /// The unit may take extra blocks (the JIT's own units; AOT units are laid out up front).
    pub inline_ok: bool,
    /// Blocks to translate into this unit after the current one: call continuations, so a translated
    /// RET lands on code that runs straight into the caller's next block.
    pub inline_queue: Vec<u64>,
    /// Guest V registers held in xmm8-xmm15 within the current block (see `VCache`).
    pub vc: VCache,
    /// The V register(s) the instruction being translated may write: never read from the cache, since
    /// its own code may have stored part of them already.
    pub vc_dest: [u8; 2],
    /// r11 holds the TBI mask on every path to this point. Per-block, never
    /// established under conditional emission, invalidated by scratch writes/calls.
    pub tbi_mask_live: bool,
    /// Rare FP fixups, emitted after the current block's unconditional transfer.
    cold_paths: Vec<Box<dyn FnOnce(&mut Unit)>>,
    /// Broadcast constants live in this unit's code buffer, including in AOT blobs.
    constants: HashMap<u64, CodeLabel>,
    constant_pool: Vec<(CodeLabel, u64)>,
    pool_avx: bool,
    #[cfg(any(debug_assertions, test))]
    cold_ranges: Vec<(std::ops::Range<usize>, bool)>,
}

pub const NCREGS: usize = 5;
pub const NVREGS: usize = 8;

/// Read cache of guest V registers in xmm8-xmm15, within one block. Memory (`Cpu::v`) stays up to date
/// (every write still stores it); a slot is a copy, so dependent FP operations read a register instead
/// of reloading what the previous instruction just stored (store-forwarding latency on the critical
/// path). Kept correct from the guest side: before each instruction, the V registers its encoding can
/// write are dropped (`v_writes`); any emitted `call` (helpers, the interpreter) drops everything, since
/// xmm registers are caller-saved; every block starts empty.
#[derive(Clone, Copy)]
pub struct VCache {
    pub reg: [u8; NVREGS],
    pub stamp: [u32; NVREGS],
    pub tick: u32,
}

impl VCache {
    fn new() -> VCache {
        VCache { reg: [0xff; NVREGS], stamp: [0; NVREGS], tick: 0 }
    }
}

const VREGS: [AsmRegisterXmm; NVREGS] = [xmm8, xmm9, xmm10, xmm11, xmm12, xmm13, xmm14, xmm15];

/// V registers `insn` may write: `None` = none, `Some([a, b])` (0xff = unused), or all of them when
/// the encoding is not understood (structure loads, anything new).
fn v_writes(insn: u32) -> Option<[u8; 2]> {
    const ALL: Option<[u8; 2]> = Some([0xfe, 0xfe]);
    let rd = (insn & 31) as u8;
    if (insn >> 25) & 7 == 7 {
        return Some([rd, 0xff]); // data processing, SIMD & FP: only Vd (or a GPR / NZCV)
    }
    if (insn >> 25) & 5 == 4 {
        // loads and stores
        if insn & (1 << 26) == 0 {
            return None; // general registers only
        }
        let load = insn & (1 << 22) != 0;
        return match (insn >> 27) & 7 {
            0b011 => Some([rd, 0xff]), // LDR (literal), SIMD & FP
            0b101 => if load { Some([rd, ((insn >> 10) & 31) as u8]) } else { None }, // LDP/STP and the no-allocate pair
            0b111 => if load { Some([rd, 0xff]) } else { None }, // LDR/STR single register (all addressing forms)
            _ => ALL, // structure loads (LD1-LD4, LD1R, ...) write several registers
        };
    }
    None // data processing (immediate / register), branches, system
}
pub static DEAD_FLAG_SKIPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const LOOKAHEAD: usize = 10;

/// Operand masks used only for cache profitability. Unknown encodings read
/// everything and kill nothing: decode coverage never determines correctness.
#[derive(Clone, Copy, Default)]
struct CacheAccess { xr: u32, xw: u32, vr: u32, vw: u32 }
fn reg_bit(r: u32, allow_sp: bool) -> u32 { if r == 31 && !allow_sp { 0 } else { 1u32 << r } }
fn cache_access(w: u32) -> CacheAccess {
    let (d, n, m, a) = (w & 31, (w >> 5) & 31, (w >> 16) & 31, (w >> 10) & 31);
    let mut x = CacheAccess::default();
    let rd = reg_bit(d, false); let rn = reg_bit(n, false); let rm = reg_bit(m, false);
    // Immediate integer families (same class split as interp::dp_imm).
    if (w >> 25) & 14 == 8 {
        match (w >> 23) & 7 {
            0 | 1 => x.xw = rd, // ADR(P)
            2 => { x.xr = reg_bit(n, true); x.xw = reg_bit(d, w & (1 << 29) == 0); }
            4 => { x.xr = rn; x.xw = reg_bit(d, (w >> 29) & 3 != 3); }
            5 => { x.xr = if (w >> 29) & 3 == 3 { rd } else { 0 }; x.xw = rd; }
            6 => { x.xr = rn | if (w >> 29) & 3 == 1 { rd } else { 0 }; x.xw = rd; }
            7 => { x.xr = rn | rm; x.xw = rd; }
            _ => return CacheAccess { xr: !0, vr: !0, ..x },
        }
        return x;
    }
    if w & 0x1f00_0000 == 0x0a00_0000 { x.xr = rn | rm; x.xw = rd; return x; }
    if w & 0x1f00_0000 == 0x0b00_0000 {
        let extended = w & (1 << 21) != 0;
        x.xr = reg_bit(n, extended) | rm;
        x.xw = reg_bit(d, extended && w & (1 << 29) == 0); return x;
    }
    if w & 0x5fe0_0000 == 0x5ac0_0000 { x.xr = rn; x.xw = rd; return x; }
    if w & 0x5fe0_0000 == 0x1ac0_0000 || w & 0x1fe0_0000 == 0x1a00_0000 || w & 0x1fe0_0000 == 0x1a80_0000 {
        x.xr = rn | rm; x.xw = rd; return x;
    }
    if w & 0x1f00_0000 == 0x1b00_0000 { x.xr = rn | rm | reg_bit(a, false); x.xw = rd; return x; }
    // Simple scalar floating-point arithmetic and fused multiply-add.
    if w & 0xff20_0c00 == 0x1e20_0800 && (w >> 12) & 15 <= 3 {
        x.vr = (1 << n) | (1 << m); x.vw = 1 << d; return x;
    }
    if w & 0xff3f_fc00 == 0x1e21_c000 { x.vr = 1 << n; x.vw = 1 << d; return x; }
    if w & 0xff00_0000 == 0x1f00_0000 {
        x.vr = (1 << n) | (1 << m) | (1 << a); x.vw = 1 << d; return x;
    }
    // Common single-register memory accesses: avoid interpreting offset fields
    // or the other register bank as a use of the cached value.
    if w & 0x3b00_0000 == 0x3900_0000 || w & 0x3b20_0000 == 0x3800_0000 {
        let v = w & (1 << 26) != 0;
        let opc = (w >> 22) & 3;
        x.xr = reg_bit(n, true);
        if w & 0x3b00_0000 != 0x3900_0000 && ((w >> 10) & 3 == 1 || (w >> 10) & 3 == 3) { x.xw |= reg_bit(n, true); }
        if v {
            if opc & 1 != 0 { x.vw = 1 << d; } else { x.vr = 1 << d; }
        } else if opc == 0 { x.xr |= rd; } else { x.xw |= rd; }
        return x;
    }
    if w & 0x3b20_0c00 == 0x3820_0800 {
        x.xr = reg_bit(n, true) | rm;
        if w & (1 << 26) != 0 {
            if (w >> 22) & 1 != 0 { x.vw = 1 << d; } else { x.vr = 1 << d; }
        } else if (w >> 22) & 3 == 0 { x.xr |= rd; } else { x.xw = rd; }
        return x;
    }
    if w & 0x7e00_0000 == 0x3400_0000 || w & 0x7e00_0000 == 0x3600_0000 {
        x.xr = rd; return x; // CBZ/TBZ use Rt, not Rd
    }
    if w & 0xfe00_0000 == 0xd600_0000 { x.xr = rn; return x; }
    if is_terminator(w) { return x; }
    CacheAccess { xr: !0, vr: !0, ..x }
}

fn cache_next_uses(look: &[u32]) -> [[u8; 32]; 2] {
    let mut next = [[u8::MAX; 32]; 2];
    let mut unresolved = [!0u32; 2];
    for (i, &w) in look.iter().enumerate() {
        let a = cache_access(w);
        for (bank, (reads, writes)) in [(a.xr, a.xw), (a.vr, a.vw)].into_iter().enumerate() {
            let mut used = reads & unresolved[bank];
            while used != 0 {
                let reg = used.trailing_zeros() as usize;
                next[bank][reg] = i as u8;
                used &= used - 1;
            }
            // An old value has no next use when overwritten before being read.
            unresolved[bank] &= !(reads | writes);
        }
        if unresolved == [0, 0] { break; }
    }
    next
}
#[cfg(test)]
fn next_cache_use(look: &[u32], n: u32, vector: bool) -> Option<usize> {
    let n = cache_next_uses(look)[vector as usize][n as usize];
    (n != u8::MAX).then_some(n as usize)
}


#[derive(Clone, Copy)]
struct LoopCache { pc: u64, fast: CodeLabel, reg: [u8; NCREGS] }

#[derive(Clone, Copy, Default)]
pub struct RegCache {
    /// Guest register held in each slot (0xff = empty).
    pub reg: [u8; NCREGS],
    pub stamp: [u32; NCREGS],
    pub tick: u32,
}

impl RegCache {
    fn new() -> RegCache {
        RegCache { reg: [0xff; NCREGS], stamp: [0; NCREGS], tick: 0 }
    }
}

/// Which x86 operation produced the live flags (decides how ARM conditions map onto x86 ones).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FuseKind {
    Add,
    Sub,
    Logic,
}

/// x86 condition codes used for fused compare-and-branch / select.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Cc {
    E, Ne, B, Ae, S, Ns, O, No, A, Be, L, Ge, G, Le,
}

/// The x86 condition equivalent to ARM condition `cond` (0..=13) for flags produced by `kind`,
/// if there is a single one.
pub fn x86_cc(kind: FuseKind, cond: u32) -> Option<Cc> {
    use Cc::*;
    const SUB: [Option<Cc>; 14] = [Some(E), Some(Ne), Some(Ae), Some(B), Some(S), Some(Ns), Some(O), Some(No), Some(A), Some(Be), Some(Ge), Some(L), Some(G), Some(Le)];
    const ADD: [Option<Cc>; 14] = [Some(E), Some(Ne), Some(B), Some(Ae), Some(S), Some(Ns), Some(O), Some(No), None, None, Some(Ge), Some(L), Some(G), Some(Le)];
    // and/test clear CF and OF, so the carry/overflow/unsigned conditions are constants: not fused.
    const LOGIC: [Option<Cc>; 14] = [Some(E), Some(Ne), None, None, Some(S), Some(Ns), None, None, None, None, Some(Ge), Some(L), Some(G), Some(Le)];
    if cond >= 14 {
        return None;
    }
    match kind {
        FuseKind::Sub => SUB[cond as usize],
        FuseKind::Add => ADD[cond as usize],
        FuseKind::Logic => LOGIC[cond as usize],
    }
}

impl Unit {
    pub fn new(stub_kind: StubKind, commpage_checks: bool) -> Unit {
        let mut a = CodeAssembler::new(64).unwrap();
        let exit_label = a.create_label();
        Unit {
            a,
            labels: HashMap::new(),
            stubs: Vec::new(),
            stub_kind,
            slot_padding: 0,
            no_internal_jumps: false,
            first_slot: None,
            commpage_checks,
            exit_label,
            finished_stubs: Vec::new(),
            insns_translated: 0,
            insns_fallback: 0,
            fuse: None,
            rc: RegCache::new(),
            loop_cache: None,
            regioncache: regioncache_enabled(),
            rc_frozen: false,
            look: Vec::new(),
            cache_uses: [[u8::MAX; 32]; 2],
            dead_flags: false,
            skip_next: false,
            inline_ok: false,
            inline_queue: Vec::new(),
            vc: VCache::new(),
            vc_dest: [0xff; 2],
            tbi_mask_live: false,
            cold_paths: Vec::new(),
            constants: HashMap::new(),
            constant_pool: Vec::new(),
            pool_avx: has_avx(),
            #[cfg(any(debug_assertions, test))]
            cold_ranges: Vec::new(),
        }
    }

    fn emit_cold_paths(&mut self) {
        for emit in std::mem::take(&mut self.cold_paths) {
            emit(self);
        }
        debug_assert!(self.cold_paths.is_empty());
    }

    /// Emit the shared "jump to dispatcher" tail. Call once after all blocks.
    pub fn finish(&mut self) {
        let a = &mut self.a;
        a.set_label(&mut self.exit_label).unwrap();
        a.jmp(qword_ptr(r15 + off::JIT_EXIT)).unwrap();
        let stubs = std::mem::take(&mut self.stubs);
        for mut s in stubs.into_iter() {
            let a = &mut self.a;
            a.set_label(&mut s.cont).unwrap();
            a.mov(rax, s.target).unwrap();
            a.mov(qword_ptr(r15 + off::PC), rax).unwrap();
            if self.stub_kind == StubKind::Patchable {
                a.lea(rax, qword_ptr(s.slot)).unwrap();
                a.mov(qword_ptr(r15 + off::EXIT_DATA), rax).unwrap();
            } else {
                a.mov(qword_ptr(r15 + off::EXIT_DATA), 0).unwrap();
            }
            a.mov(qword_ptr(r15 + off::EXIT_REASON), EXIT_CONTINUE as i32).unwrap();
            a.jmp(qword_ptr(r15 + off::JIT_EXIT)).unwrap();
            self.finished_stubs.push(s);
        }
        if self.stub_kind == StubKind::Patchable && !self.finished_stubs.is_empty() {
            let a = &mut self.a;
            if self.slot_padding > 0 {
                a.db(&vec![0xccu8; self.slot_padding]).unwrap();
            }
            for (i, s) in self.finished_stubs.iter_mut().enumerate() {
                a.set_label(&mut s.slot).unwrap();
                if i == 0 {
                    self.first_slot = Some(s.slot);
                }
                a.dq(&[0]).unwrap();
            }
        }
        self.finish_constants();
    }

    fn finish_constants(&mut self) {
        // Unaligned pool operands use VEX forms or explicit movdqu loads.
        // No preliminary assembly is needed; AOT stores this same buffer.
        for (label, value) in &mut self.constant_pool {
            self.a.set_label(label).unwrap();
            self.a.dq(&[*value, *value]).unwrap();
        }
    }
}

// ---------- tiny helpers over the assembler ----------

type R64 = AsmRegister64;
type R32 = AsmRegister32;

fn r32_of(r: R64) -> R32 {
    match r {
        x if x == rax => eax,
        x if x == rcx => ecx,
        x if x == rdx => edx,
        x if x == rsi => esi,
        x if x == rdi => edi,
        x if x == r8 => r8d,
        x if x == r9 => r9d,
        x if x == r10 => r10d,
        x if x == r11 => r11d,
        _ => panic!("no 32-bit alias"),
    }
}

fn r8_of(r: R64) -> AsmRegister8 {
    match r {
        x if x == rax => al,
        x if x == rcx => cl,
        x if x == rdx => dl,
        x if x == rsi => sil,
        x if x == rdi => dil,
        x if x == r8 => r8b,
        x if x == r9 => r9b,
        x if x == r10 => r10b,
        x if x == r11 => r11b,
        _ => panic!("no 8-bit alias"),
    }
}

fn r16_of(r: R64) -> AsmRegister16 {
    match r {
        x if x == rax => ax,
        x if x == rcx => cx,
        x if x == rdx => dx,
        x if x == rsi => si,
        x if x == rdi => di,
        x if x == r8 => r8w,
        x if x == r9 => r9w,
        x if x == r10 => r10w,
        x if x == r11 => r11w,
        _ => panic!("no 16-bit alias"),
    }
}

fn xoff(n: u32) -> i32 {
    (off::X + n as usize * 8) as i32
}
fn voff(n: u32) -> i32 {
    (off::V + n as usize * 16) as i32
}

#[inline]
fn bits(insn: u32, hi: u32, lo: u32) -> u32 {
    (insn >> lo) & ((1u32 << (hi - lo + 1)) - 1)
}
#[inline]
fn bit(insn: u32, b: u32) -> u32 {
    (insn >> b) & 1
}
fn sext(v: u64, n: u32) -> i64 {
    ((v << (64 - n)) as i64) >> (64 - n)
}

struct E<'a> {
    u: &'a mut Unit,
    pc: u64,
    /// Flag producer of the immediately preceding instruction (see `Unit::fuse`).
    fuse_in: Option<FuseKind>,
    // Snapshot before destination invalidation. Only scalar FP operand loads use this:
    // they all precede stores, cache fills and helper calls in their instruction.
    scalar_inputs: Option<VCache>,
}

/// Callee-saved x86 registers the JIT keeps guest registers in (see `RegCache`).
const CREGS: [R64; NCREGS] = [rbx, rbp, r12, r13, r14];
const CREGS32: [R32; NCREGS] = [ebx, ebp, r12d, r13d, r14d];

macro_rules! a {
    ($e:expr, $m:ident ( $($arg:expr),* )) => { $e.u.a.$m($($arg),*).unwrap() };
}

// Legacy SSE arithmetic memory operands require alignment. VEX forms allow
// unaligned constants; on SSE-only hosts each caller supplies a dead scratch.
macro_rules! pool_op {
    ($e:expr, $sse:ident, $avx:ident, $dst:expr, $mem:expr, $scratch:expr) => {{
        if $e.u.pool_avx {
            a!($e, $avx($dst, $dst, $mem));
        } else {
            debug_assert_ne!($dst, $scratch);
            a!($e, movdqu($scratch, $mem));
            a!($e, $sse($dst, $scratch));
        }
    }};
}

#[cfg(any(debug_assertions, test))]
fn check_cold_registers(instructions: &[iced_x86::Instruction], tbi_mask_live: bool) -> Result<(), String> {
    use iced_x86::{InstructionInfoFactory, OpAccess, Register};
    let mut info = InstructionInfoFactory::new();
    for (index, instruction) in instructions.iter().enumerate() {
        for used in info.info(instruction).used_registers() {
            if !matches!(used.access(), OpAccess::Write | OpAccess::CondWrite | OpAccess::ReadWrite | OpAccess::ReadCondWrite) {
                continue;
            }
            let reg = used.register();
            let allowed = if reg.is_vector_register() {
                reg.number() < 8
            } else {
                matches!(reg.full_register(), Register::RAX | Register::RCX | Register::RDX)
                    || (reg.full_register() == Register::R11 && !tbi_mask_live)
            };
            if !allowed {
                return Err(format!("cold instruction {index} ({:?}) writes {reg:?}, tbi_mask_live={tbi_mask_live}", instruction.code()));
            }
        }
    }
    Ok(())
}

/// A V operand location captured before emitting an out-of-line fixup. Cold
/// emission must never consult the cache state of a later guest instruction.
#[derive(Clone, Copy)]
struct FpSource {
    cached: Option<AsmRegisterXmm>,
    offset: i32,
}

impl<'a> E<'a> {
    fn rc_find(&self, n: u32) -> Option<usize> {
        self.u.rc.reg.iter().position(|&r| r as u32 == n)
    }
    fn rc_touch(&mut self, s: usize) {
        self.u.rc.tick += 1;
        self.u.rc.stamp[s] = self.u.rc.tick;
    }
    /// Region members use fixed slots; ordinary blocks use next-use/LRU.
    /// Memory stays authoritative in both cases.
    fn rc_alloc(&mut self, n: u32) -> usize {
        let fixed = self.u.loop_cache.and_then(|l| l.reg.iter().position(|&r| r as u32 == n));
        debug_assert!(self.u.loop_cache.is_none() || fixed.is_some());
        let rc = &self.u.rc;
        let s = fixed.unwrap_or_else(|| (0..NCREGS).find(|&i| rc.reg[i] == 0xff).unwrap_or_else(|| {
            if jit_off("nextuse") { (0..NCREGS).min_by_key(|&i| rc.stamp[i]).unwrap() }
            else { (0..NCREGS).max_by_key(|&i| (self.u.cache_uses[0][rc.reg[i] as usize], u32::MAX - rc.stamp[i])).unwrap() }
        }));
        self.u.rc.reg[s] = n as u8;
        self.rc_touch(s);
        s
    }
    fn rc_inval(&mut self, n: u32) {
        if let Some(s) = self.rc_find(n) {
            self.u.rc.reg[s] = 0xff;
        }
    }
    fn rc_clear(&mut self) {
        self.u.tbi_mask_live = false;
        self.u.rc = RegCache::new();
        self.u.vc = VCache::new();
    }
    fn vc_find(&self, n: u32) -> Option<usize> {
        self.u.vc.reg.iter().position(|&r| r as u32 == n)
    }
    fn vc_touch(&mut self, s: usize) {
        self.u.vc.tick += 1;
        self.u.vc.stamp[s] = self.u.vc.tick;
    }
    fn vc_alloc(&mut self, n: u32) -> usize {
        let vc = &self.u.vc;
        let s = (0..NVREGS).find(|&i| vc.reg[i] == 0xff).unwrap_or_else(|| {
            if jit_off("nextuse") { (0..NVREGS).min_by_key(|&i| vc.stamp[i]).unwrap() }
            else { (0..NVREGS).max_by_key(|&i| (self.u.cache_uses[1][vc.reg[i] as usize], u32::MAX - vc.stamp[i])).unwrap() }
        });
        self.u.vc.reg[s] = n as u8;
        self.vc_touch(s);
        s
    }
    /// xmm register holding all of Vn, if cached; `None`: read memory. Slots are filled only from
    /// results (`vfill`), never by loading: a 16-byte load right after the two 8-byte stores of an
    /// LDR Dt or FMOV Dd, Xn cannot be store-forwarded and stalls longer than the load it saves.
    fn vsrc(&mut self, n: u32) -> Option<AsmRegisterXmm> {
        if self.u.vc_dest.contains(&(n as u8)) {
            return None;
        }
        let s = self.vc_find(n)?;
        self.vc_touch(s);
        Some(VREGS[s])
    }
    /// All of Vd was just stored from `x` as the instruction's last V write: keep a copy if it is read
    /// soon. Only at the end of an instruction, where every path through its code has merged.
    fn vfill(&mut self, d: u32, x: AsmRegisterXmm) {
        if jit_off("vcache") {
            return;
        }
        let s = match self.vc_find(d) {
            Some(s) => s,
            None if self.used_soon_bank(d, true) => self.vc_alloc(d),
            None => return,
        };
        a!(self, movdqa(VREGS[s], x));
        self.vc_touch(s);
    }
    /// Scalar FP operand Vn into `x` (64-bit lane for double, 32-bit otherwise), rest of `x` zero.
    fn fld_scalar(&mut self, x: AsmRegisterXmm, n: u32, double: bool) {
        let src = self.scalar_source(n);
        self.fld_source(x, src, double);
    }
    fn scalar_source(&mut self, n: u32) -> FpSource {
        let cached = self.vsrc(n).or_else(|| {
            self.scalar_inputs.as_ref()
                .and_then(|vc| vc.reg.iter().position(|&r| r as u32 == n))
                .map(|s| VREGS[s])
        });
        FpSource { cached, offset: voff(n) }
    }
    fn fld_source(&mut self, x: AsmRegisterXmm, src: FpSource, double: bool) {
        match src.cached {
            Some(c) if double => a!(self, movq(x, c)),
            Some(c) => {
                a!(self, xorps(x, x));
                a!(self, movss(x, c));
            }
            None if double => a!(self, movsd_2(x, qword_ptr(r15 + src.offset))),
            None => a!(self, movss(x, dword_ptr(r15 + src.offset))),
        }
    }
    /// Register contracts are the same as the hot FP helper: only xmm0-xmm7
    /// and rax/rcx/rdx/r11 may be scratched, with no cache fills or helper calls.
    fn cold_path(&mut self, mut cold: CodeLabel, emit: impl FnOnce(&mut E<'_>) + 'static) {
        let mut resume = self.u.a.create_label();
        self.u.a.set_label(&mut resume).unwrap();
        let pc = self.pc;
        #[cfg(any(debug_assertions, test))]
        let tbi_mask_live = self.u.tbi_mask_live;
        self.u.cold_paths.push(Box::new(move |u| {
            u.a.set_label(&mut cold).unwrap();
            #[cfg(any(debug_assertions, test))]
            let start = u.a.instructions().len();
            let mut e = E { u, pc, fuse_in: None, scalar_inputs: None };
            emit(&mut e);
            a!(e, jmp(resume));
            #[cfg(any(debug_assertions, test))]
            {
                let range = start..e.u.a.instructions().len();
                #[cfg(debug_assertions)]
                check_cold_registers(&e.u.a.instructions()[range.clone()], tbi_mask_live).unwrap();
                e.u.cold_ranges.push((range, tbi_mask_live));
            }
        }));
    }

    /// Is the current bank-local value read before a known overwrite in the
    /// lookahead? Unknown encodings are conservative; this affects speed only.
    fn used_soon_bank(&self, n: u32, vector: bool) -> bool {
        if !vector {
            if let Some(l) = self.u.loop_cache { return l.reg.contains(&(n as u8)); }
        }
        if !jit_off("nextuse") { return self.u.cache_uses[vector as usize][n as usize] != u8::MAX; }
        self.u.look.iter().any(|&w| (w & 31) == n || ((w >> 5) & 31) == n || ((w >> 10) & 31) == n || ((w >> 16) & 31) == n)
    }
    fn used_soon(&self, n: u32) -> bool { self.used_soon_bank(n, false) }
    /// A label is about to be created: code after it may be conditional.
    fn new_label(&mut self) -> CodeLabel {
        self.u.rc_frozen = true;
        self.u.a.create_label()
    }
    /// Guest register `n` was just written from `r` (already stored to memory).
    fn rc_wrote(&mut self, n: u32, r: R64) {
        let soon = self.used_soon(n);
        if let Some(s) = self.rc_find(n) {
            if self.u.rc_frozen || !soon {
                self.u.rc.reg[s] = 0xff;
            } else {
                a!(self, mov(CREGS[s], r));
                self.rc_touch(s);
            }
        } else if !self.u.rc_frozen && soon {
            let s = self.rc_alloc(n);
            a!(self, mov(CREGS[s], r));
        }
    }

    /// Load Xn (31 = XZR or SP) into `r`.
    fn ldx(&mut self, r: R64, n: u32, use_sp: bool) {
        if n == 31 && !use_sp {
            a!(self, xor(r32_of(r), r32_of(r)));
        } else {
            self.ldx_nf(r, n);
        }
    }
    /// Load Xn without touching the x86 flags (n = 31 must be SP).
    fn ldx_nf(&mut self, r: R64, n: u32) {
        if let Some(s) = self.rc_find(n) {
            self.rc_touch(s);
            a!(self, mov(r, CREGS[s]));
        } else if !self.u.rc_frozen && self.used_soon(n) {
            let s = self.rc_alloc(n);
            a!(self, mov(CREGS[s], qword_ptr(r15 + xoff(n))));
            a!(self, mov(r, CREGS[s]));
        } else {
            a!(self, mov(r, qword_ptr(r15 + xoff(n))));
        }
    }
    /// Load Wn zero-extended.
    fn ldw(&mut self, r: R64, n: u32, use_sp: bool) {
        if n == 31 && !use_sp {
            a!(self, xor(r32_of(r), r32_of(r)));
        } else {
            self.ldw_nf(r, n);
        }
    }
    fn ldw_nf(&mut self, r: R64, n: u32) {
        if let Some(s) = self.rc_find(n) {
            self.rc_touch(s);
            a!(self, mov(r32_of(r), CREGS32[s]));
        } else if !self.u.rc_frozen && self.used_soon(n) {
            let s = self.rc_alloc(n);
            a!(self, mov(CREGS[s], qword_ptr(r15 + xoff(n))));
            a!(self, mov(r32_of(r), CREGS32[s]));
        } else {
            a!(self, mov(r32_of(r), dword_ptr(r15 + xoff(n))));
        }
    }
    fn ld(&mut self, r: R64, n: u32, use_sp: bool, sf: bool) {
        if sf {
            self.ldx(r, n, use_sp)
        } else {
            self.ldw(r, n, use_sp)
        }
    }
    /// Store `r` to Xn (31 = discard, unless `sp`).
    fn stx(&mut self, n: u32, r: R64, use_sp: bool) {
        if n == 31 && !use_sp {
            return;
        }
        a!(self, mov(qword_ptr(r15 + xoff(n)), r));
        self.rc_wrote(n, r);
    }
    fn mov_imm(&mut self, r: R64, v: u64) {
        if r == r11 { self.u.tbi_mask_live = false; }
        if v == 0 {
            a!(self, xor(r32_of(r), r32_of(r)));
        } else if v <= u32::MAX as u64 {
            a!(self, mov(r32_of(r), v as u32));
        } else {
            a!(self, mov(r, v));
        }
    }
    fn st_imm(&mut self, n: u32, v: u64) {
        if n == 31 {
            return;
        }
        self.rc_inval(n);
        if v as i64 >= i32::MIN as i64 && v as i64 <= i32::MAX as i64 {
            a!(self, mov(qword_ptr(r15 + xoff(n)), v as i64 as i32));
        } else {
            // r11 is reserved as the scratch for this (callers may hold rax).
            self.mov_imm(r11, v);
            a!(self, mov(qword_ptr(r15 + xoff(n)), r11));
        }
    }

    /// Store NZCV from the x86 flags of a preceding add/sub/logic op.
    fn flags_add(&mut self) {
        if self.u.dead_flags {
            return;
        }
        a!(self, sets(byte_ptr(r15 + off::NF)));
        a!(self, sete(byte_ptr(r15 + off::ZF)));
        a!(self, setb(byte_ptr(r15 + off::CF)));
        a!(self, seto(byte_ptr(r15 + off::VF)));
    }
    fn flags_sub(&mut self) {
        if self.u.dead_flags {
            return;
        }
        a!(self, sets(byte_ptr(r15 + off::NF)));
        a!(self, sete(byte_ptr(r15 + off::ZF)));
        a!(self, setae(byte_ptr(r15 + off::CF)));
        a!(self, seto(byte_ptr(r15 + off::VF)));
    }
    fn flags_logic(&mut self) {
        if self.u.dead_flags {
            return;
        }
        a!(self, sets(byte_ptr(r15 + off::NF)));
        a!(self, sete(byte_ptr(r15 + off::ZF)));
        a!(self, mov(byte_ptr(r15 + off::CF), 0));
        a!(self, mov(byte_ptr(r15 + off::VF), 0));
    }

    /// Like `ld` but never touches the x86 flags (no xor-zeroing).
    fn ld_nf(&mut self, r: R64, n: u32, sf: bool) {
        if n == 31 {
            a!(self, mov(r32_of(r), 0u32));
        } else if sf {
            self.ldx_nf(r, n);
        } else {
            self.ldw_nf(r, n);
        }
    }

    fn jcc(&mut self, cc: Cc, l: CodeLabel) {
        match cc {
            Cc::E => a!(self, je(l)),
            Cc::Ne => a!(self, jne(l)),
            Cc::B => a!(self, jb(l)),
            Cc::Ae => a!(self, jae(l)),
            Cc::S => a!(self, js(l)),
            Cc::Ns => a!(self, jns(l)),
            Cc::O => a!(self, jo(l)),
            Cc::No => a!(self, jno(l)),
            Cc::A => a!(self, ja(l)),
            Cc::Be => a!(self, jbe(l)),
            Cc::L => a!(self, jl(l)),
            Cc::Ge => a!(self, jge(l)),
            Cc::G => a!(self, jg(l)),
            Cc::Le => a!(self, jle(l)),
        }
    }

    /// dst = src if `cc` holds.
    fn cmov(&mut self, cc: Cc, d: R64, s: R64, sf: bool) {
        let (d32, s32) = (r32_of(d), r32_of(s));
        macro_rules! cm {
            ($m:ident) => { if sf { a!(self, $m(d, s)) } else { a!(self, $m(d32, s32)) } };
        }
        match cc {
            Cc::E => cm!(cmove),
            Cc::Ne => cm!(cmovne),
            Cc::B => cm!(cmovb),
            Cc::Ae => cm!(cmovae),
            Cc::S => cm!(cmovs),
            Cc::Ns => cm!(cmovns),
            Cc::O => cm!(cmovo),
            Cc::No => cm!(cmovno),
            Cc::A => cm!(cmova),
            Cc::Be => cm!(cmovbe),
            Cc::L => cm!(cmovl),
            Cc::Ge => cm!(cmovge),
            Cc::G => cm!(cmovg),
            Cc::Le => cm!(cmovle),
        }
    }

    /// Evaluate condition `cond` into `al` (0/1).
    fn cond_to_al(&mut self, cond: u32) {
        let base = cond >> 1;
        match base {
            0 => a!(self, movzx(eax, byte_ptr(r15 + off::ZF))),
            1 => a!(self, movzx(eax, byte_ptr(r15 + off::CF))),
            2 => a!(self, movzx(eax, byte_ptr(r15 + off::NF))),
            3 => a!(self, movzx(eax, byte_ptr(r15 + off::VF))),
            4 => {
                a!(self, movzx(eax, byte_ptr(r15 + off::ZF)));
                a!(self, xor(eax, 1));
                a!(self, and(al, byte_ptr(r15 + off::CF)));
            }
            5 => {
                a!(self, movzx(eax, byte_ptr(r15 + off::NF)));
                a!(self, xor(al, byte_ptr(r15 + off::VF)));
                a!(self, xor(eax, 1));
            }
            6 => {
                a!(self, movzx(eax, byte_ptr(r15 + off::NF)));
                a!(self, xor(al, byte_ptr(r15 + off::VF)));
                a!(self, or(al, byte_ptr(r15 + off::ZF)));
                a!(self, xor(eax, 1));
            }
            _ => a!(self, mov(eax, 1)),
        }
        if cond & 1 != 0 && cond != 0xf {
            a!(self, xor(eax, 1));
        }
    }

    /// Call the interpreter for one instruction; exit if it reports anything.
    fn fallback(&mut self, insn: u32) {
        self.u.insns_fallback += 1;
        a!(self, mov(rdi, r15));
        a!(self, mov(esi, insn));
        self.mov_imm(rdx, self.pc);
        a!(self, call(qword_ptr(r15 + off::helper(HELPER_INTERP))));
        self.rc_clear();   // the interpreter may have written any guest register
        a!(self, test(eax, eax));
        let exit = self.u.exit_label;
        a!(self, jnz(exit));
    }

    /// Fallback for a control-flow instruction: interpreter sets pc; return
    /// to the dispatcher.
    fn fallback_terminator(&mut self, insn: u32) {
        self.fallback(insn);
        let exit = self.u.exit_label;
        a!(self, mov(qword_ptr(r15 + off::EXIT_REASON), EXIT_CONTINUE as i32));
        a!(self, mov(qword_ptr(r15 + off::EXIT_DATA), 0));
        a!(self, jmp(exit));
    }

    /// Jump to a direct successor: inside the unit, or through a stub.
    fn goto(&mut self, target: u64) {
        if !self.u.no_internal_jumps {
            if let Some(l) = self.u.loop_cache.filter(|l| l.pc == target) {
                // Cpu remains authoritative. Reload only absent/mismatched slots; no
                // register shuffles, scratch clobbers or compile-state changes on this edge.
                for (s, &n) in l.reg.iter().enumerate() {
                    if n != 0xff && self.u.rc.reg[s] != n {
                        a!(self, mov(CREGS[s], qword_ptr(r15 + xoff(n as u32))));
                    }
                }
                a!(self, jmp(l.fast));
                return;
            }
            if let Some(l) = self.u.labels.get(&target).copied() {
                a!(self, jmp(l));
                return;
            }
        }
        let cont = self.u.a.create_label();
        let slot = self.u.a.create_label();
        if self.u.stub_kind == StubKind::Patchable {
            // jmp [slot]; the slot initially points at `cont`.
            a!(self, jmp(qword_ptr(slot)));
        } else {
            a!(self, jmp(cont));
        }
        self.u.stubs.push(Stub { target, cont, slot });
    }

    /// Guest effective address in rax; applies commpage redirection if needed.
    fn commpage_fix(&mut self) {
        // ARM top-byte-ignore: keep 55:0, with one dependent operation instead
        // of shl/shr. r11 is otherwise only st_imm's scratch; mov_imm invalidates
        // this copy. Do not establish it under conditional emission: a branch
        // could skip the load and join code that would otherwise trust it.
        // Redirected commpages call a host helper, so keep their existing path.
        if !self.u.commpage_checks && !jit_off("tbimask") {
            if !self.u.tbi_mask_live {
                self.mov_imm(r11, 0x00ff_ffff_ffff_ffff);
                self.u.tbi_mask_live = !self.u.rc_frozen;
            }
            a!(self, and(rax, r11));
        } else {
            a!(self, shl(rax, 8));
            a!(self, shr(rax, 8));
        }
        if !self.u.commpage_checks {
            return;
        }
        let mut skip = self.u.a.create_label();
        a!(self, mov(rcx, rax));
        a!(self, shr(rcx, 16));
        a!(self, cmp(ecx, 0xF_FFFF));
        a!(self, jne(skip));
        a!(self, push(r8));
        a!(self, push(r9));
        a!(self, mov(rdi, rax));
        a!(self, call(qword_ptr(r15 + off::helper(HELPER_COMMPAGE))));
        a!(self, pop(r9));
        a!(self, pop(r8));
        self.u.a.set_label(&mut skip).unwrap();
        a!(self, nop());
    }
}


/// The kind of flags produced by a fusable flag-setting instruction (the encodings that set
/// `Unit::fuse`), if `insn` is one.
fn fusable_setter(insn: u32) -> Option<FuseKind> {
    let sub = |i: u32| if i >> 30 & 1 != 0 { FuseKind::Sub } else { FuseKind::Add };
    if insn & 0x3F80_0000 == 0x3100_0000 {
        return Some(sub(insn)); // add/sub (immediate), S
    }
    if insn & 0x3F20_0000 == 0x2B00_0000 && (insn >> 22) & 3 != 3 {
        return Some(sub(insn)); // add/sub (shifted register), S
    }
    if insn & 0x3F20_0000 == 0x2B20_0000 {
        return Some(sub(insn)); // add/sub (extended register), S
    }
    if insn & 0x7F80_0000 == 0x7200_0000 || insn & 0x7F00_0000 == 0x6A00_0000 {
        return Some(FuseKind::Logic); // ANDS / BICS (immediate, shifted register)
    }
    None
}

/// Does `insn` read NZCV?
fn reads_flags(insn: u32) -> bool {
    insn & 0xFF00_0010 == 0x5400_0000          // B.cond
        || insn & 0xFF00_0010 == 0x5400_0010   // BC.cond
        || insn & 0x3FE0_0800 == 0x1A80_0000   // CSEL/CSINC/CSINV/CSNEG (CSET, CINC, ...)
        || insn & 0x1FE0_0400 == 0x1A40_0000   // CCMP/CCMN
        || insn & 0x1FE0_0000 == 0x1A00_0000   // ADC/SBC (+S)
        || insn & 0xFF20_0C00 == 0x1E20_0C00   // FCSEL
        || insn & 0xFF20_0400 == 0x1E20_0400   // FCCMP/FCCMPE
        || insn & 0xFFF0_0000 == 0xD530_0000   // MRS (any system register, conservative)
}

/// Does `insn` overwrite all of NZCV unconditionally (and not read it)?
fn writes_all_flags(insn: u32) -> bool {
    insn & 0x3F80_0000 == 0x3100_0000                                   // ADDS/SUBS imm
        || (insn & 0x3F20_0000 == 0x2B00_0000 && (insn >> 22) & 3 != 3) // ADDS/SUBS shifted
        || insn & 0x3F20_0000 == 0x2B20_0000                            // ADDS/SUBS extended
        || insn & 0x7F80_0000 == 0x7200_0000                            // ANDS imm
        || insn & 0x7F00_0000 == 0x6A00_0000                            // ANDS/BICS shifted
        || insn & 0xFF20_FC07 == 0x1E20_2000                            // FCMP/FCMPE
}

/// Guest memory faults end the program (signals.rs: no delivery to guest handlers), so the NZCV a
/// fault would leave behind is only seen by the crash dump. Loads and stores may then sit between a
/// setter and its overwriting writer. Turn this off when fault delivery is implemented.
const GUEST_FAULTS_FATAL: bool = true;

/// Instructions that cannot fault (into anything that observes guest state), call a helper,
/// change or read NZCV, or leave this block. Valid encodings only: a reserved one exits to the
/// interpreter, which would see the stale flags.
fn flags_safe_passthrough(w: u32) -> bool {
    let sf = w >> 31 != 0;
    if w == 0xd503_201f { return true; } // NOP
    if w & 0x1f00_0000 == 0x1000_0000 { return true; } // ADR/ADRP
    if w & 0x3f80_0000 == 0x1100_0000 { return true; } // ADD/SUB imm, !S
    if w & 0x1f80_0000 == 0x1280_0000 { // MOVN/Z/K, valid width/halfword
        return (w >> 29) & 3 != 1 && (sf || (w >> 21) & 3 < 2);
    }
    if w & 0x1f80_0000 == 0x1200_0000 && (w >> 29) & 3 != 3 { // AND/ORR/EOR immediate (not ANDS)
        return (sf || bit(w, 22) == 0) && decode_logic_imm(bit(w, 22), bits(w, 15, 10), bits(w, 21, 16), sf).is_some();
    }
    if w & 0x1f80_0000 == 0x1300_0000 && (w >> 29) & 3 != 3 { // SBFM/BFM/UBFM (LSL, LSR, UBFX, SXTW, ...)
        return bit(w, 22) == sf as u32 && (sf || (bits(w, 21, 16) < 32 && bits(w, 15, 10) < 32));
    }
    if w & 0x7fa0_0000 == 0x1380_0000 { // EXTR
        return bit(w, 22) == sf as u32 && (sf || bits(w, 15, 10) < 32);
    }
    if w & 0x1f00_0000 == 0x0a00_0000 && (w >> 29) & 3 != 3 { // logical shifted register (not ANDS/BICS)
        return sf || bit(w, 15) == 0;
    }
    if w & 0x3f20_0000 == 0x0b00_0000 { // ADD/SUB shifted register, !S
        return (w >> 22) & 3 != 3 && (sf || bit(w, 15) == 0);
    }
    if w & 0x3fe0_0000 == 0x0b20_0000 { // ADD/SUB extended register, !S
        return bits(w, 12, 10) <= 4;
    }
    if w & 0x7fe0_0000 == 0x1ac0_0000 { // UDIV/SDIV/LSLV/LSRV/ASRV/RORV (division by zero gives 0 on ARM)
        return matches!(bits(w, 15, 10), 2 | 3 | 8..=11);
    }
    if w & 0x7fff_0000 == 0x5ac0_0000 { // RBIT/REV16/REV/REV32/CLZ/CLS
        return bits(w, 15, 10) <= 5 && (sf || bits(w, 15, 10) != 3);
    }
    if w & 0x7f00_0000 == 0x1b00_0000 { // MADD/MSUB (MUL), SMADDL/UMADDL/SMULH/UMULH
        let op = bits(w, 23, 21);
        return op == 0 || (sf && (op == 1 || op == 5 || ((op == 2 || op == 6) && bit(w, 15) == 0)));
    }
    if GUEST_FAULTS_FATAL {
        // LDR/STR (immediate pre/post-index, unscaled, unsigned offset, register offset), LDP/STP;
        // general and SIMD&FP registers. Not exclusives, atomics or PAC loads.
        if w & 0x3b00_0000 == 0x3900_0000 { return true; } // unsigned offset
        if w & 0x3b20_0000 == 0x3800_0000 { return bits(w, 11, 10) != 2; } // unscaled, post-index, pre-index (not LDTR)
        if w & 0x3b20_0c00 == 0x3820_0800 { return bit(w, 14) == 1; } // register offset, UXTW/LSL/SXTW/SXTX
        if w & 0x3a00_0000 == 0x2800_0000 { return bits(w, 31, 30) != 3; } // pairs
    }
    false
}

/// Restrict liveness kills to encodings known to execute without an undefined
/// instruction exit. Broad opcode-family matches also include reserved encodings.
fn flags_safe_writer(w: u32) -> bool {
    if w & 0x3f80_0000 == 0x3100_0000 { return true; } // ADDS/SUBS imm
    if w & 0x3f20_0000 == 0x2b00_0000 {
        return (w >> 22) & 3 < 3 && (w >> 31 != 0 || (w >> 15) & 1 == 0);
    }
    if w & 0x3fe0_0000 == 0x2b20_0000 {
        return (w >> 10) & 7 <= 4;
    }
    if w & 0x7f80_0000 == 0x7200_0000 {
        let sf = w >> 31 != 0;
        return (sf || bit(w, 22) == 0)
            && decode_logic_imm(bit(w, 22), bits(w, 15, 10), bits(w, 21, 16), sf).is_some();
    }
    if w & 0x7f00_0000 == 0x6a00_0000 {
        return w >> 31 != 0 || (w >> 15) & 1 == 0;
    }
    false
}

/// Is `insn` a CSEL-family select that a `kind` setter's live x86 flags can feed (fused select)?
fn fused_select(kind: FuseKind, insn: u32) -> bool {
    insn & 0x3FE0_0800 == 0x1A80_0000 && x86_cc(kind, ((insn >> 12) & 15) ^ 1).is_some()
}

/// Flags of a fusable `kind` setter at `pc` that only fused selects read: the selects right after it
/// run on the live x86 flags (each keeps them), then the guest flags must be dead, either at a fused
/// B.cond (both successors) or by being overwritten in straight-line code before any other reader.
/// `room` is how many more instructions the block may take; no label may sit inside the select chain.
fn flags_dead_after_selects(kind: FuseKind, pc: u64, room: usize, labels: &HashMap<u64, CodeLabel>, read: &dyn Fn(u64) -> Option<u32>) -> bool {
    // A disabled select executes in the interpreter, which reads stored NZCV.
    if disabled_classes() & 8 != 0 { return false; }
    let mut p = pc + 4;
    let mut n = 0;
    while let Some(w) = read(p) {
        if !fused_select(kind, w) || labels.contains_key(&p) || n + 2 > room {
            break;
        }
        p += 4;
        n += 1;
    }
    if n == 0 {
        return false;
    }
    let Some(w) = read(p) else { return false };
    if w & 0xFF00_0010 == 0x5400_0000 && (w & 15) < 14 {
        if x86_cc(kind, w & 15).is_none() || labels.contains_key(&p) || n + 2 > room {
            return false;
        }
        let target = p.wrapping_add((sext(((w >> 5) & 0x7ffff) as u64, 19) << 2) as u64);
        return flags_dead_at(target, read) && flags_dead_at(p + 4, read);
    }
    flags_dead_at(p, read)
}

/// Are the flags dead at `pc`: overwritten before anything reads them, looking at most a few
/// straight-line instructions ahead (unconditional direct branches are followed).
fn flags_dead_at(mut pc: u64, read: &dyn Fn(u64) -> Option<u32>) -> bool {
    let mut hops = 0;
    for _ in 0..24 {
        let Some(w) = read(pc) else { return false };
        if writes_all_flags(w) && flags_safe_writer(w) {
            return true;
        }
        if reads_flags(w) {
            return false;
        }
        if w & 0xFC00_0000 == 0x1400_0000 {
            // B: follow
            hops += 1;
            if hops > 3 {
                return false;
            }
            pc = pc.wrapping_add((sext((w & 0x03ff_ffff) as u64, 26) << 2) as u64);
            continue;
        }
        if is_terminator(w) || !flags_safe_passthrough(w) {
            return false;
        }
        pc += 4;
    }
    false
}

/// Debugging aid: ATTESOR_JIT_DISABLE=dpimm,branch,ldst,dpreg routes those
/// instruction classes to the interpreter.
fn disabled_classes() -> u32 {
    use std::sync::OnceLock;
    static D: OnceLock<u32> = OnceLock::new();
    *D.get_or_init(|| {
        let v = std::env::var("ATTESOR_JIT_DISABLE").unwrap_or_default();
        let mut m = 0;
        for p in v.split(',') {
            m |= match p {
                "dpimm" => 1,
                "branch" => 2,
                "ldst" => 4,
                "dpreg" => 8,
                "all" => 15,
                "b" => 16,
                "bcond" => 32,
                "cbz" => 64,
                "tbz" => 128,
                "br" => 256,
                "simd" => 512,
                _ => 0,
            };
        }
        m
    })
}

/// Instruction synchronization barrier (all immediate option encodings).
/// Unlike SB, this is a context synchronization point for published guest code.
pub fn is_isb(insn: u32) -> bool {
    insn & 0xFFFF_F0FF == 0xD503_30DF
}

/// FPCR changes affect every following FP instruction, including block guards.
/// Its interpreter helper updates host rounding/flush modes before returning.
pub fn is_fpcr_write(insn: u32) -> bool {
    insn & 0xffff_ffe0 == 0xd51b_4400
}

/// Is this instruction a block terminator (control flow or execution context)?
pub fn is_terminator(insn: u32) -> bool {
    (insn & 0x7C00_0000 == 0x1400_0000)          // B, BL
        || (insn & 0xFF00_0000 == 0x5400_0000)   // B.cond / BC.cond
        || (insn & 0x7E00_0000 == 0x3400_0000)   // CBZ/CBNZ
        || (insn & 0x7E00_0000 == 0x3600_0000)   // TBZ/TBNZ
        || (insn & 0xFE00_0000 == 0xD600_0000)   // BR/BLR/RET(+PAC)
        || (insn & 0xFF00_0000 == 0xD400_0000)   // exceptions (SVC, BRK...)
        || (insn & 0xFFFF_FFE0 == 0xD50B_7520)   // IC IVAU only (runtime must see it); DC ops are not terminators
        || is_isb(insn)                         // context synchronization must pass through dispatch
        || is_fpcr_write(insn)                  // recheck the new FP mode at a public entry
}

/// Debugging aid: ATTESOR_JIT_OFF=ext,fma,… routes the named SIMD translation groups to the interpreter
/// (finer than ATTESOR_JIT_DISABLE=simd), to bisect a misbehaving translation.
fn jit_off(group: &str) -> bool {
    static OFF: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    OFF.get_or_init(|| std::env::var("ATTESOR_JIT_OFF").unwrap_or_default().split(',').filter(|g| !g.is_empty()).map(str::to_string).collect())
        .iter()
        .any(|g| g == group)
}

/// Translated BL/BLR/RET as host call/ret (see E::host_call). Opt-in with ATTESOR_JIT_ON=callret: measured
/// slower than the indirect-branch cache (callsites bench ~110 ms vs ~70 ms, qsort -5%), since the epoch and
/// target checks on every return cost more than the return-stack prediction saves.
fn callret_enabled() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g| g == "callret"))
}

/// Experimental write-through natural-loop region; public entries remain canonical.
fn regioncache_enabled() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g| g == "regioncache"))
}

// Discover only a single-entry basic block ending in a direct natural backedge.
// Stop at other Unit labels exactly as emit_block does; joins and other edges
// continue through public entries, never through this private cache contract.
fn loop_layout(u: &Unit, pc: u64, read: &dyn Fn(u64) -> Option<u32>) -> Option<[u8; NCREGS]> {
    let mut counts = [0u32; 32];
    for i in 0..MAX_BLOCK_INSNS {
        let p = pc + i as u64 * 4;
        if i != 0 && u.labels.contains_key(&p) { return None; }
        let w = read(p)?;
        let a = cache_access(w);
        // Unknown decodings advertise every read conservatively. They must not
        // dominate the layout; correctness never depends on this ranking.
        if a.xr != u32::MAX {
            for n in 0..32 { counts[n] += ((a.xr >> n) & 1) as u32; }
        }
        if is_terminator(w) {
            let target = if w & 0xfc00_0000 == 0x1400_0000 {
                p.wrapping_add((sext((w & 0x03ff_ffff) as u64, 26) << 2) as u64)
            } else if w & 0xff00_0010 == 0x5400_0000 || w & 0x7e00_0000 == 0x3400_0000 {
                p.wrapping_add((sext(((w >> 5) & 0x7ffff) as u64, 19) << 2) as u64)
            } else if w & 0x7e00_0000 == 0x3600_0000 {
                p.wrapping_add((sext(((w >> 5) & 0x3fff) as u64, 14) << 2) as u64)
            } else { return None; };
            if target != pc { return None; }
            let mut regs: Vec<usize> = (0..32).filter(|&n| counts[n] != 0).collect();
            regs.sort_by_key(|&n| (std::cmp::Reverse(counts[n]), n));
            let mut layout = [0xff; NCREGS];
            for (s, n) in regs.into_iter().take(NCREGS).enumerate() { layout[s] = n as u8; }
            return (layout[0] != 0xff).then_some(layout);
        }
    }
    None
}

/// AVX permits unaligned 128-bit arithmetic memory operands (without AVX2).
fn has_avx() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::is_x86_feature_detected!("avx"))
}

/// FMA3 (fused multiply-add) on the host: FMLA/FMADD are translated natively only with it, since an
/// unfused multiply + add rounds twice. Not part of the SSE4.1 baseline, so detected at run time.
pub fn has_fma() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::is_x86_feature_detected!("fma"))
}

/// AES-NI on the host, for AESE/AESD/AESMC/AESIMC.
pub fn has_aes() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::is_x86_feature_detected!("aes"))
}

/// PCLMULQDQ on the host, for PMULL/PMULL2 (64-bit polynomial multiply).
pub fn has_pclmul() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::is_x86_feature_detected!("pclmulqdq"))
}

/// SHA extensions (SHA-NI) on the host, for SHA256H/H2.
pub fn has_sha() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::is_x86_feature_detected!("sha"))
}

/// F16C (half-precision conversions) on the host, for FCVTL/FCVTN between half and single.
pub fn has_f16c() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::is_x86_feature_detected!("f16c"))
}

/// Translate one basic block starting at `pc` into `u`. The caller must have
/// created and registered the label for `pc` if it wants intra-unit jumps.
/// Returns the guest address just past the block.
pub fn emit_block(u: &mut Unit, pc: u64, read: &dyn Fn(u64) -> Option<u32>) -> u64 {
    u.fuse = None;
    u.loop_cache = None;
    u.tbi_mask_live = false;
    u.rc = RegCache::new();
    u.vc = VCache::new();
    u.rc_frozen = false;
    u.look.clear();
    {
        let mut c2 = pc + 4;
        while u.look.len() < LOOKAHEAD {
            let Some(w) = read(c2) else { break };
            u.look.push(w);
            if is_terminator(w) {
                break;
            }
            c2 += 4;
        }
    }
    let mut label = *u.labels.get(&pc).expect("block label");
    u.a.set_label(&mut label).unwrap();
    if u.regioncache && !u.no_internal_jumps {
        if let Some(reg) = loop_layout(u, pc, read) {
            for (s, &n) in reg.iter().enumerate() {
                if n != 0xff { u.a.mov(CREGS[s], qword_ptr(r15 + xoff(n as u32))).unwrap(); }
            }
            let mut fast = u.a.create_label();
            u.a.set_label(&mut fast).unwrap();
            u.loop_cache = Some(LoopCache { pc, fast, reg });
            u.rc.reg = reg;
        }
    }
    // Native FP code follows FPCR.RMode/FZ through MXCSR, but not DN/AHP/FZ16: a block with any SIMD&FP
    // instruction checks those on entry and hands itself to the interpreter when one is set.
    {
        let mut p = pc;
        let mut fp = false;
        for _ in 0..MAX_BLOCK_INSNS {
            let Some(w) = read(p) else { break };
            if (w >> 25) & 7 == 0b111 {
                fp = true;
                break;
            }
            if is_terminator(w) {
                break;
            }
            p += 4;
        }
        if fp {
            let mut body = u.a.create_label();
            u.a.test(dword_ptr(r15 + off::FPCR), FPCR_NONDEFAULT as i32).unwrap();
            u.a.jz(body).unwrap();
            let mut e = E { u, pc, fuse_in: None, scalar_inputs: None };
            e.mov_imm(rax, pc);
            a!(e, mov(qword_ptr(r15 + off::PC), rax));
            a!(e, mov(qword_ptr(r15 + off::EXIT_REASON), EXIT_INTERP as i32));
            let exit = e.u.exit_label;
            a!(e, jmp(exit));
            u.a.set_label(&mut body).unwrap();
        }
    }
    // set_label updates the label; keep the updated value for label_ip().
    u.labels.insert(pc, label);
    let mut cur = pc;
    let mut count = 0;
    loop {
        let Some(insn) = read(cur) else {
            // Unreadable: let the dispatcher fault properly.
            let mut e = E { u, pc: cur, fuse_in: None, scalar_inputs: None };
            e.st_pc_and_exit(cur);
            u.emit_cold_paths();
            return cur;
        };
        let fuse_in = u.fuse.take();
        if !jit_off("nextuse") { u.cache_uses = cache_next_uses(&u.look); }
        u.rc_frozen = false;
        u.dead_flags = false;
        if let Some(kind) = fusable_setter(insn) {
            if let Some(&nx) = u.look.first() {
                if nx & 0xFF00_0010 == 0x5400_0000
                    && (nx & 15) < 14
                    && x86_cc(kind, nx & 15).is_some()
                    && !u.labels.contains_key(&(cur + 4))
                    && count + 1 < MAX_BLOCK_INSNS
                    && disabled_classes() & 32 == 0
                {
                    // cmp/adds/ands feeding B.cond: the flags are dead if both successors overwrite them
                    let target = (cur + 4).wrapping_add((sext(((nx >> 5) & 0x7ffff) as u64, 19) << 2) as u64);
                    if flags_dead_at(target, read) && flags_dead_at(cur + 8, read) {
                        u.dead_flags = true;
                        DEAD_FLAG_SKIPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
            if !u.dead_flags
                && disabled_classes() & 32 == 0
                && !jit_off("selflags")
                && flags_dead_after_selects(kind, cur, MAX_BLOCK_INSNS - count, &u.labels, read)
            {
                u.dead_flags = true;
                DEAD_FLAG_SKIPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let scalar_inputs = if jit_off("scalaralias") { None } else { Some(u.vc) };
        let vw = v_writes(insn);
        match vw {
            Some([0xfe, _]) => u.vc = VCache::new(),
            Some(regs) => {
                for r in regs {
                    if let Some(s) = u.vc.reg.iter().position(|&x| x == r) {
                        u.vc.reg[s] = 0xff;
                    }
                }
            }
            None => {}
        }
        u.vc_dest = match vw { Some([0xfe, _]) => [0xff; 2], Some(r) => r, None => [0xff; 2] };
        let code_start = u.a.instructions().len();
        let mut e = E { u, pc: cur, fuse_in, scalar_inputs };
        let term = is_terminator(insn);
        e.u.insns_translated += 1;
        let translated = e.translate(insn);
        e.u.dead_flags = false;
        e.u.vc_dest = [0xff; 2];
        if e.u.skip_next || e.u.a.instructions()[code_start..].iter().any(|i| i.mnemonic() == iced_x86::Mnemonic::Call) {
            e.u.vc = VCache::new(); // fused pair (writes the next one's Vd too) or helper call (clobbers xmm8-15)
            e.u.tbi_mask_live = false; // host calls may overwrite caller-saved r11
        }
        if !translated {
            if term {
                e.fallback_terminator(insn);
            } else {
                e.fallback(insn);
            }
        }
        cur += 4;
        count += 1;
        if term {
            u.emit_cold_paths();
            return cur;
        }
        // a fused pair consumed the next instruction too: step over it (its pc is not a label)
        let skips = if std::mem::take(&mut u.skip_next) { 2 } else { 1 };
        if skips == 2 {
            cur += 4;
            count += 1;
            u.look.remove(0);
        }
        // slide the lookahead window to start at the next instruction
        if !u.look.is_empty() {
            let last_was_term = is_terminator(*u.look.last().unwrap());
            u.look.remove(0);
            if !last_was_term && u.look.len() < LOOKAHEAD {
                if let Some(w) = read(cur + 4 * u.look.len() as u64 + 4) {
                    u.look.push(w);
                }
            }
        }
        if count >= MAX_BLOCK_INSNS {
            let mut e = E { u, pc: cur, fuse_in: None, scalar_inputs: None };
            e.goto(cur);
            u.emit_cold_paths();
            return cur;
        }
        // Stop before running into another block of this unit: jump to it.
        if u.labels.contains_key(&cur) {
            let mut e = E { u, pc: cur, fuse_in: None, scalar_inputs: None };
            e.goto(cur);
            u.emit_cold_paths();
            return cur;
        }
    }
}

impl<'a> E<'a> {
    fn st_pc_and_exit(&mut self, pc: u64) {
        self.mov_imm(rax, pc);
        a!(self, mov(qword_ptr(r15 + off::PC), rax));
        a!(self, mov(qword_ptr(r15 + off::EXIT_REASON), EXIT_CONTINUE as i32));
        a!(self, mov(qword_ptr(r15 + off::EXIT_DATA), 0));
        let exit = self.u.exit_label;
        a!(self, jmp(exit));
    }

    /// Returns false if the instruction must go to the interpreter.
    fn translate(&mut self, insn: u32) -> bool {
        let op0 = (insn >> 25) & 0xf;
        let dis = disabled_classes();
        let class = match op0 {
            0b1000 | 0b1001 => 1,
            0b1010 | 0b1011 => 2,
            0b0100 | 0b0110 | 0b1100 | 0b1110 => 4,
            0b0101 | 0b1101 => 8,
            0b0111 | 0b1111 => 512,
            _ => 0,
        };
        if dis & class != 0 {
            return false;
        }
        match op0 {
            0b1000 | 0b1001 => self.dp_imm(insn),
            0b1010 | 0b1011 => self.branch_sys(insn),
            0b0100 | 0b0110 | 0b1100 | 0b1110 => self.ldst(insn),
            0b0101 | 0b1101 => self.dp_reg(insn),
            0b0111 | 0b1111 => self.simd_fp(insn),
            _ => false,
        }
    }

    // ---------------- data processing (immediate) ----------------

    fn dp_imm(&mut self, insn: u32) -> bool {
        let sf = bit(insn, 31) != 0;
        let rd = bits(insn, 4, 0);
        let rn = bits(insn, 9, 5);
        match bits(insn, 25, 23) {
            0b000 | 0b001 => {
                let immlo = bits(insn, 30, 29) as u64;
                let immhi = bits(insn, 23, 5) as u64;
                let imm = sext((immhi << 2) | immlo, 21) as u64;
                let v = if bit(insn, 31) != 0 {
                    (self.pc & !0xfff).wrapping_add(imm << 12)
                } else {
                    self.pc.wrapping_add(imm)
                };
                self.st_imm(rd, v);
                true
            }
            0b010 => {
                let op = bit(insn, 30);
                let s = bit(insn, 29) != 0;
                let mut imm = bits(insn, 21, 10);
                if bit(insn, 22) != 0 {
                    imm <<= 12;
                }
                self.ld(rax, rn, true, sf);
                if sf {
                    if op == 0 { a!(self, add(rax, imm as i32)) } else { a!(self, sub(rax, imm as i32)) }
                } else if op == 0 {
                    a!(self, add(eax, imm as i32))
                } else {
                    a!(self, sub(eax, imm as i32))
                }
                if s {
                    if op == 0 { self.flags_add(); self.u.fuse = Some(FuseKind::Add) } else { self.flags_sub(); self.u.fuse = Some(FuseKind::Sub) }
                }
                self.stx(rd, rax, !s);
                true
            }
            0b100 => {
                let opc = bits(insn, 30, 29);
                let n = bit(insn, 22);
                if !sf && n != 0 {
                    return false;
                }
                let Some(imm) = decode_logic_imm(n, bits(insn, 15, 10), bits(insn, 21, 16), sf) else { return false };
                self.ld(rax, rn, false, sf);
                // an immediate x86 can encode directly (sign-extended imm32; any value for 32-bit ops)
                let simm = if sf { imm as i64 } else { imm as u32 as i32 as i64 };
                if simm >= i32::MIN as i64 && simm <= i32::MAX as i64 {
                    let i = simm as i32;
                    match (sf, opc) {
                        (true, 0 | 3) => a!(self, and(rax, i)),
                        (true, 1) => a!(self, or(rax, i)),
                        (true, _) => a!(self, xor(rax, i)),
                        (false, 0 | 3) => a!(self, and(eax, i)),
                        (false, 1) => a!(self, or(eax, i)),
                        (false, _) => a!(self, xor(eax, i)),
                    }
                } else if sf {
                    self.mov_imm(rcx, imm);
                    match opc {
                        0 | 3 => a!(self, and(rax, rcx)),
                        1 => a!(self, or(rax, rcx)),
                        _ => a!(self, xor(rax, rcx)),
                    }
                }
                if opc == 3 {
                    self.flags_logic();
                    self.u.fuse = Some(FuseKind::Logic);
                    self.stx(rd, rax, false);
                } else {
                    self.stx(rd, rax, true);
                }
                true
            }
            0b101 => {
                let opc = bits(insn, 30, 29);
                let hw = bits(insn, 22, 21);
                if !sf && hw > 1 {
                    return false;
                }
                let imm = (bits(insn, 20, 5) as u64) << (hw * 16);
                match opc {
                    0 => {
                        let v = if sf { !imm } else { (!imm) & 0xffff_ffff };
                        self.st_imm(rd, v);
                    }
                    2 => self.st_imm(rd, imm),
                    3 => {
                        if rd == 31 {
                            return true;
                        }
                        self.ldx(rax, rd, false);
                        let mask = !(0xffffu64 << (hw * 16));
                        self.mov_imm(rcx, mask);
                        a!(self, and(rax, rcx));
                        if imm != 0 {
                            self.mov_imm(rcx, imm);
                            a!(self, or(rax, rcx));
                        }
                        if !sf {
                            a!(self, mov(eax, eax));
                        }
                        self.stx(rd, rax, false);
                    }
                    _ => return false,
                }
                true
            }
            0b110 => self.bitfield(insn),
            0b111 => {
                // EXTR
                if bit(insn, 21) != 0 || bits(insn, 30, 29) != 0 || (sf as u32) != bit(insn, 22) {
                    return false;
                }
                let rm = bits(insn, 20, 16);
                let lsb = bits(insn, 15, 10);
                if !sf && lsb >= 32 {
                    return false;
                }
                self.ld(rax, rn, false, sf);
                self.ld(rcx, rm, false, sf);
                if lsb != 0 {
                    if sf {
                        a!(self, shrd(rcx, rax, lsb));
                    } else {
                        a!(self, shrd(ecx, eax, lsb));
                    }
                }
                self.stx(rd, rcx, false);
                true
            }
            _ => false,
        }
    }

    fn bitfield(&mut self, insn: u32) -> bool {
        let sf = bit(insn, 31) != 0;
        let opc = bits(insn, 30, 29);
        let n = bit(insn, 22);
        if (sf as u32) != n || opc == 3 {
            return false;
        }
        let rd = bits(insn, 4, 0);
        let rn = bits(insn, 9, 5);
        let immr = bits(insn, 21, 16);
        let imms = bits(insn, 15, 10);
        let dsz = if sf { 64 } else { 32 };
        if immr >= dsz || imms >= dsz {
            return false;
        }
        if opc == 1 {
            // BFM (BFI / BFXIL / BFC): dst = (dst & ~tmask) | (((dst & ~wmask) | (ROR(src, immr) & wmask)) & tmask)
            let Some((wmask, tmask)) = attesor_core::interp::decode_bit_masks(n, imms, immr, false, dsz) else {
                return false;
            };
            self.ld(rax, rn, false, sf);
            if immr != 0 {
                if sf {
                    a!(self, ror(rax, immr as i32));
                } else {
                    a!(self, ror(eax, immr as i32));
                }
            }
            self.ld(rcx, rd, false, sf);
            self.mov_imm(rdx, wmask);
            a!(self, and(rax, rdx));
            self.mov_imm(rdx, !wmask);
            a!(self, and(rdx, rcx));
            a!(self, or(rax, rdx));
            self.mov_imm(rdx, tmask);
            a!(self, and(rax, rdx));
            self.mov_imm(rdx, !tmask);
            a!(self, and(rdx, rcx));
            a!(self, or(rax, rdx));
            if !sf {
                a!(self, mov(eax, eax));
            }
            self.stx(rd, rax, false);
            return true;
        }
        self.ld(rax, rn, false, sf);
        if sf {
            if opc == 2 {
                // UBFM
                if imms >= immr {
                    // UBFX / LSR
                    let width = imms - immr + 1;
                    if immr != 0 {
                        a!(self, shr(rax, immr));
                    }
                    if width < 64 {
                        if width == 32 {
                            a!(self, mov(eax, eax));
                        } else {
                            self.mov_imm(rcx, (1u64 << width) - 1);
                            a!(self, and(rax, rcx));
                        }
                    }
                } else {
                    // UBFIZ / LSL
                    let width = imms + 1;
                    let sh = 64 - immr;
                    a!(self, shl(rax, 64 - width));
                    a!(self, shr(rax, 64 - width - sh));
                }
            } else {
                // SBFM
                if imms >= immr {
                    let l = 63 - imms;
                    if l != 0 {
                        a!(self, shl(rax, l));
                    }
                    let r = l + immr;
                    if r != 0 {
                        a!(self, sar(rax, r));
                    }
                } else {
                    let l = 63 - imms;
                    a!(self, shl(rax, l));
                    a!(self, sar(rax, l));
                    a!(self, shl(rax, 64 - immr));
                }
            }
        } else if opc == 2 {
            if imms >= immr {
                let width = imms - immr + 1;
                if immr != 0 {
                    a!(self, shr(eax, immr));
                }
                if width < 32 {
                    a!(self, and(eax, ((1u64 << width) - 1) as i32));
                }
            } else {
                let width = imms + 1;
                let sh = 32 - immr;
                a!(self, shl(eax, 32 - width));
                a!(self, shr(eax, 32 - width - sh));
            }
        } else if imms >= immr {
            let l = 31 - imms;
            if l != 0 {
                a!(self, shl(eax, l));
            }
            let r = l + immr;
            if r != 0 {
                a!(self, sar(eax, r));
            }
        } else {
            let l = 31 - imms;
            a!(self, shl(eax, l));
            a!(self, sar(eax, l));
            a!(self, shl(eax, 32 - immr));
        }
        self.stx(rd, rax, false);
        true
    }

    // ---------------- data processing (register) ----------------

    /// Apply shift `ty` by `amt` to `r` (datasize from sf).
    fn shift(&mut self, r: R64, ty: u32, amt: u32, sf: bool) {
        if amt == 0 {
            return;
        }
        if sf {
            match ty {
                0 => a!(self, shl(r, amt)),
                1 => a!(self, shr(r, amt)),
                2 => a!(self, sar(r, amt)),
                _ => a!(self, ror(r, amt)),
            }
        } else {
            let r = r32_of(r);
            match ty {
                0 => a!(self, shl(r, amt)),
                1 => a!(self, shr(r, amt)),
                2 => a!(self, sar(r, amt)),
                _ => a!(self, ror(r, amt)),
            }
        }
    }

    /// Extend `r` in place according to `option`, then shift left by `sh`.
    fn extend(&mut self, r: R64, option: u32, sh: u32) {
        let r32 = r32_of(r);
        match option {
            0 => a!(self, movzx(r32, r8_of(r))),
            1 => a!(self, movzx(r32, r16_of(r))),
            2 => a!(self, mov(r32, r32)),
            3 | 7 => {}
            4 => a!(self, movsx(r, r8_of(r))),
            5 => a!(self, movsx(r, r16_of(r))),
            _ => a!(self, movsxd(r, r32)),
        }
        if sh != 0 {
            a!(self, shl(r, sh));
        }
    }

    fn dp_reg(&mut self, insn: u32) -> bool {
        let sf = bit(insn, 31) != 0;
        let rd = bits(insn, 4, 0);
        let rn = bits(insn, 9, 5);
        let rm = bits(insn, 20, 16);
        if insn & 0x1F00_0000 == 0x0A00_0000 {
            // logical shifted register
            let opc = bits(insn, 30, 29);
            let ty = bits(insn, 23, 22);
            let nflag = bit(insn, 21) != 0;
            let amt = bits(insn, 15, 10);
            if !sf && amt >= 32 {
                return false;
            }
            // MOV (register) fast path: ORR Rd, ZR, Rm
            if opc == 1 && rn == 31 && amt == 0 && !nflag {
                self.ld(rax, rm, false, sf);
                self.stx(rd, rax, false);
                return true;
            }
            self.ld(rcx, rm, false, sf);
            self.shift(rcx, ty, amt, sf);
            if nflag {
                if sf { a!(self, not(rcx)) } else { a!(self, not(ecx)) }
            }
            self.ld(rax, rn, false, sf);
            if sf {
                match opc {
                    0 | 3 => a!(self, and(rax, rcx)),
                    1 => a!(self, or(rax, rcx)),
                    _ => a!(self, xor(rax, rcx)),
                }
            } else {
                match opc {
                    0 | 3 => a!(self, and(eax, ecx)),
                    1 => a!(self, or(eax, ecx)),
                    _ => a!(self, xor(eax, ecx)),
                }
            }
            if opc == 3 {
                self.flags_logic();
                self.u.fuse = Some(FuseKind::Logic);
            }
            self.stx(rd, rax, false);
            return true;
        }
        if insn & 0x1F20_0000 == 0x0B00_0000 {
            // add/sub shifted register
            let op = bit(insn, 30);
            let s = bit(insn, 29) != 0;
            let ty = bits(insn, 23, 22);
            let amt = bits(insn, 15, 10);
            if ty == 3 || (!sf && amt >= 32) {
                return false;
            }
            self.ld(rcx, rm, false, sf);
            self.shift(rcx, ty, amt, sf);
            self.ld(rax, rn, false, sf);
            self.addsub(op, sf, s);
            self.stx(rd, rax, false);
            return true;
        }
        if insn & 0x1F20_0000 == 0x0B20_0000 {
            // add/sub extended register
            let op = bit(insn, 30);
            let s = bit(insn, 29) != 0;
            let option = bits(insn, 15, 13);
            let imm3 = bits(insn, 12, 10);
            if imm3 > 4 || bits(insn, 23, 22) != 0 {
                return false;
            }
            self.ldx(rcx, rm, false);
            self.extend(rcx, option, imm3);
            self.ld(rax, rn, true, sf);
            self.addsub(op, sf, s);
            self.stx(rd, rax, !s);
            return true;
        }
        if insn & 0x1FE0_FC00 == 0x1A00_0000 {
            // ADC / ADCS / SBC / SBCS
            let op = bit(insn, 30);
            let s = bit(insn, 29) != 0;
            self.ld(rax, rn, false, sf);
            self.ld(rcx, rm, false, sf);
            if op == 0 {
                // CF := C
                a!(self, bt(dword_ptr(r15 + (off::NF as i32 & !3)), ((off::CF & 3) * 8) as u32));
                if sf { a!(self, adc(rax, rcx)) } else { a!(self, adc(eax, ecx)) }
                if s {
                    self.flags_add();
                }
            } else {
                // CF := !C (borrow)
                a!(self, cmp(byte_ptr(r15 + off::CF), 1));
                if sf { a!(self, sbb(rax, rcx)) } else { a!(self, sbb(eax, ecx)) }
                if s {
                    self.flags_sub();
                }
            }
            self.stx(rd, rax, false);
            return true;
        }
        if insn & 0x1FE0_0410 == 0x1A40_0000 && bit(insn, 29) == 1 {
            // CCMN / CCMP (register or immediate)
            let op = bit(insn, 30);
            let cond = bits(insn, 15, 12);
            let nzcv = bits(insn, 3, 0);
            let imm = bit(insn, 11) != 0;
            let mut l_else = self.new_label();
            let mut l_end = self.new_label();
            self.cond_to_al(cond);
            a!(self, test(al, al));
            a!(self, jz(l_else));
            self.ld(rax, rn, false, sf);
            if imm {
                self.mov_imm(rcx, rm as u64);
            } else {
                self.ld(rcx, rm, false, sf);
            }
            if op == 0 {
                if sf { a!(self, add(rax, rcx)) } else { a!(self, add(eax, ecx)) }
                self.flags_add();
            } else {
                if sf { a!(self, cmp(rax, rcx)) } else { a!(self, cmp(eax, ecx)) }
                self.flags_sub();
            }
            a!(self, jmp(l_end));
            self.u.a.set_label(&mut l_else).unwrap();
            a!(self, mov(byte_ptr(r15 + off::NF), ((nzcv >> 3) & 1) as u32));
            a!(self, mov(byte_ptr(r15 + off::ZF), ((nzcv >> 2) & 1) as u32));
            a!(self, mov(byte_ptr(r15 + off::CF), ((nzcv >> 1) & 1) as u32));
            a!(self, mov(byte_ptr(r15 + off::VF), (nzcv & 1) as u32));
            self.u.a.set_label(&mut l_end).unwrap();
            a!(self, nop());
            return true;
        }
        if insn & 0x3FE0_0800 == 0x1A80_0000 {
            // CSEL / CSINC / CSINV / CSNEG
            let op = bit(insn, 30);
            let o2 = bit(insn, 10);
            let cond = bits(insn, 15, 12);
            if let Some(k) = self.fuse_in {
                if let Some(cc_false) = x86_cc(k, cond ^ 1) {
                    // Select on the live x86 flags: nothing below may modify them (no xor/inc/neg).
                    self.ld_nf(rcx, rn, sf);
                    self.ld_nf(rdx, rm, sf);
                    match (op, o2) {
                        (0, 0) => {}
                        (0, _) => if sf { a!(self, lea(rdx, qword_ptr(rdx + 1))) } else { a!(self, lea(edx, dword_ptr(rdx + 1))) },
                        (_, 0) => if sf { a!(self, not(rdx)) } else { a!(self, not(edx)) },
                        _ => {
                            if sf { a!(self, not(rdx)); a!(self, lea(rdx, qword_ptr(rdx + 1))) } else { a!(self, not(edx)); a!(self, lea(edx, dword_ptr(rdx + 1))) }
                        }
                    }
                    self.cmov(cc_false, rcx, rdx, sf);
                    self.stx(rd, rcx, false);
                    // the x86 flags are untouched: a following select can use them too
                    self.u.fuse = Some(k);
                    return true;
                }
            }
            self.cond_to_al(cond);
            self.ld(rcx, rn, false, sf);
            self.ld(rdx, rm, false, sf);
            match (op, o2) {
                (0, 0) => {}
                (0, _) => if sf { a!(self, inc(rdx)) } else { a!(self, inc(edx)) },
                (_, 0) => if sf { a!(self, not(rdx)) } else { a!(self, not(edx)) },
                _ => if sf { a!(self, neg(rdx)) } else { a!(self, neg(edx)) },
            }
            a!(self, test(al, al));
            if sf { a!(self, cmovz(rcx, rdx)) } else { a!(self, cmovz(ecx, edx)) }
            self.stx(rd, rcx, false);
            return true;
        }
        if insn & 0x5FE0_0000 == 0x1AC0_0000 {
            return self.dp2(insn, sf, rd, rn, rm);
        }
        if insn & 0x5FE0_0000 == 0x5AC0_0000 {
            return self.dp1(insn, sf, rd, rn);
        }
        if insn & 0x1F00_0000 == 0x1B00_0000 {
            // 3-source
            let op31 = bits(insn, 23, 21);
            let o0 = bit(insn, 15);
            let ra = bits(insn, 14, 10);
            if bits(insn, 30, 29) != 0 {
                return false;
            }
            match (op31, o0) {
                (0, _) => {
                    self.ld(rax, rn, false, sf);
                    self.ld(rcx, rm, false, sf);
                    if sf { a!(self, imul_2(rax, rcx)) } else { a!(self, imul_2(eax, ecx)) }
                    if ra != 31 {
                        self.ld(rcx, ra, false, sf);
                        if o0 == 0 {
                            if sf { a!(self, add(rax, rcx)) } else { a!(self, add(eax, ecx)) }
                        } else if sf {
                            a!(self, sub(rcx, rax));
                            a!(self, mov(rax, rcx));
                        } else {
                            a!(self, sub(ecx, eax));
                            a!(self, mov(eax, ecx));
                        }
                    } else if o0 == 1 {
                        if sf { a!(self, neg(rax)) } else { a!(self, neg(eax)) }
                    }
                    self.stx(rd, rax, false);
                    true
                }
                (1, _) | (5, _) if sf => {
                    // SMADDL/SMSUBL/UMADDL/UMSUBL
                    self.ldw(rax, rn, false);
                    self.ldw(rcx, rm, false);
                    if op31 == 1 {
                        a!(self, movsxd(rax, eax));
                        a!(self, movsxd(rcx, ecx));
                    }
                    a!(self, imul_2(rax, rcx));
                    if ra != 31 || o0 == 1 {
                        self.ldx(rcx, ra, false);
                        if o0 == 0 {
                            a!(self, add(rax, rcx));
                        } else {
                            a!(self, sub(rcx, rax));
                            a!(self, mov(rax, rcx));
                        }
                    }
                    self.stx(rd, rax, false);
                    true
                }
                (2, 0) | (6, 0) if sf => {
                    self.ldx(rax, rn, false);
                    self.ldx(rcx, rm, false);
                    if op31 == 2 { a!(self, imul(rcx)) } else { a!(self, mul(rcx)) }
                    self.stx(rd, rdx, false);
                    true
                }
                _ => false,
            }
        } else {
            false
        }
    }

    /// rax = rax op rcx with optional flags.
    fn addsub(&mut self, op: u32, sf: bool, s: bool) {
        if sf {
            if op == 0 { a!(self, add(rax, rcx)) } else { a!(self, sub(rax, rcx)) }
        } else if op == 0 {
            a!(self, add(eax, ecx))
        } else {
            a!(self, sub(eax, ecx))
        }
        if s {
            if op == 0 { self.flags_add(); self.u.fuse = Some(FuseKind::Add) } else { self.flags_sub(); self.u.fuse = Some(FuseKind::Sub) }
        }
    }

    fn dp2(&mut self, insn: u32, sf: bool, rd: u32, rn: u32, rm: u32) -> bool {
        let opcode = bits(insn, 15, 10);
        match opcode {
            0b000010 | 0b000011 => {
                // UDIV / SDIV (x86 faults on /0 and INT_MIN/-1; ARM does not)
                let signed = opcode == 3;
                let mut l_zero = self.new_label();
                let mut l_neg = self.new_label();
                let mut l_end = self.new_label();
                self.ld(rax, rn, false, sf);
                self.ld(rcx, rm, false, sf);
                if sf { a!(self, test(rcx, rcx)) } else { a!(self, test(ecx, ecx)) }
                a!(self, jz(l_zero));
                if signed {
                    if sf { a!(self, cmp(rcx, -1)) } else { a!(self, cmp(ecx, -1)) }
                    a!(self, je(l_neg));
                    if sf {
                        a!(self, cqo());
                        a!(self, idiv(rcx));
                    } else {
                        a!(self, cdq());
                        a!(self, idiv(ecx));
                    }
                } else {
                    a!(self, xor(edx, edx));
                    if sf { a!(self, div(rcx)) } else { a!(self, div(ecx)) }
                }
                a!(self, jmp(l_end));
                self.u.a.set_label(&mut l_neg).unwrap();
                if sf { a!(self, neg(rax)) } else { a!(self, neg(eax)) }
                a!(self, jmp(l_end));
                self.u.a.set_label(&mut l_zero).unwrap();
                a!(self, xor(eax, eax));
                self.u.a.set_label(&mut l_end).unwrap();
                if !sf {
                    a!(self, mov(eax, eax));
                } else {
                    a!(self, nop());
                }
                self.stx(rd, rax, false);
                true
            }
            0b001000..=0b001011 => {
                self.ld(rax, rn, false, sf);
                self.ld(rcx, rm, false, sf);
                if sf {
                    match opcode & 3 {
                        0 => a!(self, shl(rax, cl)),
                        1 => a!(self, shr(rax, cl)),
                        2 => a!(self, sar(rax, cl)),
                        _ => a!(self, ror(rax, cl)),
                    }
                } else {
                    match opcode & 3 {
                        0 => a!(self, shl(eax, cl)),
                        1 => a!(self, shr(eax, cl)),
                        2 => a!(self, sar(eax, cl)),
                        _ => a!(self, ror(eax, cl)),
                    }
                }
                self.stx(rd, rax, false);
                true
            }
            _ => false,
        }
    }

    fn dp1(&mut self, insn: u32, sf: bool, rd: u32, rn: u32) -> bool {
        let opcode2 = bits(insn, 20, 16);
        let opcode = bits(insn, 15, 10);
        if bit(insn, 29) != 0 {
            return false;
        }
        if opcode2 == 1 && sf {
            // Pointer authentication: sign/auth are the identity.
            return match opcode {
                0..=15 => true,
                16 => {
                    // XPACI: clear PAC bits 63:56,54:47 (bit 55 clear => user pointer)
                    self.ldx(rax, rd, false);
                    let mut l = self.new_label();
                    a!(self, bt(rax, 55));
                    a!(self, jc(l));
                    self.mov_imm(rcx, 0x0000_7FFF_FFFF_FFFF);
                    a!(self, and(rax, rcx));
                    self.u.a.set_label(&mut l).unwrap();
                    a!(self, nop());
                    self.stx(rd, rax, false);
                    true
                }
                17 => {
                    self.ldx(rax, rd, false);
                    let mut l = self.new_label();
                    a!(self, bt(rax, 55));
                    a!(self, jc(l));
                    self.mov_imm(rcx, !0x007F_8000_0000_0000u64);
                    a!(self, and(rax, rcx));
                    self.u.a.set_label(&mut l).unwrap();
                    a!(self, nop());
                    self.stx(rd, rax, false);
                    true
                }
                _ => false,
            };
        }
        if opcode2 != 0 {
            return false;
        }
        match opcode {
            0b000000 => {
                // RBIT: byte swap, then swap nibbles, bit pairs and bits.
                self.ld(rax, rn, false, sf);
                if sf {
                    a!(self, bswap(rax));
                } else {
                    a!(self, bswap(eax));
                }
                let width_mask: u64 = if sf { u64::MAX } else { 0xffff_ffff };
                for (sh, m) in [(4, 0x0f0f_0f0f_0f0f_0f0fu64), (2, 0x3333_3333_3333_3333), (1, 0x5555_5555_5555_5555)] {
                    self.mov_rr(rcx, rax);
                    a!(self, shr(rax, sh));
                    self.mov_imm(rdx, m & width_mask);
                    a!(self, and(rax, rdx));
                    a!(self, and(rcx, rdx));
                    a!(self, shl(rcx, sh));
                    a!(self, or(rax, rcx));
                }
                self.stx(rd, rax, false);
                true
            }
            0b000010 if !sf => {
                self.ldw(rax, rn, false);
                a!(self, bswap(eax));
                self.stx(rd, rax, false);
                true
            }
            0b000011 if sf => {
                self.ldx(rax, rn, false);
                a!(self, bswap(rax));
                self.stx(rd, rax, false);
                true
            }
            0b000100 => {
                // CLZ via BSR
                self.ld(rax, rn, false, sf);
                let mut l_zero = self.new_label();
                let mut l_end = self.new_label();
                if sf {
                    a!(self, bsr(rcx, rax));
                    a!(self, jz(l_zero));
                    a!(self, mov(eax, 63));
                    a!(self, sub(eax, ecx));
                    a!(self, jmp(l_end));
                    self.u.a.set_label(&mut l_zero).unwrap();
                    a!(self, mov(eax, 64));
                } else {
                    a!(self, bsr(ecx, eax));
                    a!(self, jz(l_zero));
                    a!(self, mov(eax, 31));
                    a!(self, sub(eax, ecx));
                    a!(self, jmp(l_end));
                    self.u.a.set_label(&mut l_zero).unwrap();
                    a!(self, mov(eax, 32));
                }
                self.u.a.set_label(&mut l_end).unwrap();
                a!(self, nop());
                self.stx(rd, rax, false);
                true
            }
            _ => false,
        }
    }

    // ---------------- branches & system ----------------

    fn branch_sys(&mut self, insn: u32) -> bool {
        let dis = disabled_classes();
        if (dis & 16 != 0 && insn & 0x7C00_0000 == 0x1400_0000)
            || (dis & 32 != 0 && insn & 0xFF00_0000 == 0x5400_0000)
            || (dis & 64 != 0 && insn & 0x7E00_0000 == 0x3400_0000)
            || (dis & 128 != 0 && insn & 0x7E00_0000 == 0x3600_0000)
            || (dis & 256 != 0 && insn & 0xFE00_0000 == 0xD600_0000)
        {
            return false;
        }
        let pc = self.pc;
        let next = pc.wrapping_add(4);
        if insn & 0x7C00_0000 == 0x1400_0000 {
            let target = pc.wrapping_add((sext((insn & 0x03ff_ffff) as u64, 26) << 2) as u64);
            if insn & 0x8000_0000 != 0 {
                self.st_imm(30, next);
                if callret_enabled() {
                    self.host_call(next, |e| e.goto(target));
                    return true;
                }
            }
            self.goto(target);
            return true;
        }
        if insn & 0xFF00_0000 == 0x5400_0000 {
            let target = pc.wrapping_add((sext(bits(insn, 23, 5) as u64, 19) << 2) as u64);
            let cond = bits(insn, 3, 0);
            if cond >= 14 {
                self.goto(target);
                return true;
            }
            if let Some(k) = self.fuse_in {
                if let Some(cc_nt) = x86_cc(k, cond ^ 1) {
                    // x86 flags of the preceding compare are still live: branch on them directly.
                    let mut l_nt = self.new_label();
                    self.jcc(cc_nt, l_nt);
                    self.goto(target);
                    self.u.a.set_label(&mut l_nt).unwrap();
                    self.goto(next);
                    return true;
                }
            }
            self.cond_to_al(cond);
            self.cond_branch(target, next);
            return true;
        }
        if insn & 0x7E00_0000 == 0x3400_0000 {
            let sf = bit(insn, 31) != 0;
            let target = pc.wrapping_add((sext(bits(insn, 23, 5) as u64, 19) << 2) as u64);
            let rt = bits(insn, 4, 0);
            let nz = bit(insn, 24) != 0;
            if rt == 31 && !jit_off("directbranch") {
                // XZR/WZR are architectural zero, independent of Cpu::x[31] (SP).
                self.goto(if nz { next } else { target });
                return true;
            }
            if rt == 31 {
                a!(self, xor(eax, eax));
            } else if sf {
                a!(self, cmp(qword_ptr(r15 + xoff(rt)), 0));
            } else {
                a!(self, cmp(dword_ptr(r15 + xoff(rt)), 0));
            }
            if !jit_off("directbranch") {
                // CMP already produced ZF; branch before goto/chaining can clobber it.
                self.flags_branch(target, next, if nz { Cc::E } else { Cc::Ne });
            } else {
                if nz { a!(self, setne(al)) } else { a!(self, sete(al)) }
                self.cond_branch(target, next);
            }
            return true;
        }
        if insn & 0x7E00_0000 == 0x3600_0000 {
            let b = (bit(insn, 31) << 5) | bits(insn, 23, 19);
            let target = pc.wrapping_add((sext(bits(insn, 18, 5) as u64, 14) << 2) as u64);
            let rt = bits(insn, 4, 0);
            let nz = bit(insn, 24) != 0;
            if rt == 31 && !jit_off("directbranch") {
                self.goto(if nz { next } else { target });
                return true;
            }
            self.ldx(rax, rt, false);
            a!(self, bt(rax, b));
            if !jit_off("directbranch") {
                // BT reports the selected bit in CF. No SETcc/TEST round trip.
                self.flags_branch(target, next, if nz { Cc::Ae } else { Cc::B });
            } else {
                if nz { a!(self, setc(al)) } else { a!(self, setnc(al)) }
                self.cond_branch(target, next);
            }
            return true;
        }
        if insn & 0xFE00_0000 == 0xD600_0000 {
            let opc = bits(insn, 24, 21);
            let op2 = bits(insn, 20, 16);
            let op3 = bits(insn, 15, 10);
            let rn = bits(insn, 9, 5);
            if op2 != 0x1f {
                return false;
            }
            let reg = match opc {
                0 | 1 if op3 == 0 || op3 == 2 || op3 == 3 => rn,
                2 => if op3 == 0 { rn } else { 30 },
                8 | 9 => rn,
                _ => return false,
            };
            self.ldx(rax, reg, false);
            if opc & 1 != 0 {
                self.st_imm(30, next);
                if callret_enabled() {
                    // BLR/BLRAA…: rax holds the target across the call setup (only rcx is used)
                    self.host_call(next, |e| e.indirect(false));
                    return true;
                }
            }
            if opc == 2 && callret_enabled() {
                // RET: return through the host stack when a translated call is open, so the host's
                // return predictor sees a call/ret pair; the continuation checks the target.
                // a call frame is open iff rsp is below the entry stack pointer (loads only: no
                // read-modify-write chain on a counter)
                let mut l_plain = self.new_label();
                a!(self, cmp(rsp, qword_ptr(r15 + off::HOST_SP)));
                a!(self, jae(l_plain));
                a!(self, mov(qword_ptr(r15 + off::RET_TARGET), rax));
                a!(self, ret());
                self.u.a.set_label(&mut l_plain).unwrap();
            }
            self.indirect(opc == 2);
            return true;
        }
        if insn & 0xFFE0_001F == 0xD400_0001 {
            // SVC
            let imm = bits(insn, 20, 5);
            self.mov_imm(rax, next);
            a!(self, mov(qword_ptr(r15 + off::PC), rax));
            a!(self, mov(qword_ptr(r15 + off::EXIT_REASON), EXIT_SVC as i32));
            a!(self, mov(qword_ptr(r15 + off::EXIT_DATA), imm as i32));
            let exit = self.u.exit_label;
            a!(self, jmp(exit));
            return true;
        }
        // hints (NOP, BTI, PAC*SP, AUT*SP, YIELD...)
        if insn & 0xFFFF_F01F == 0xD503_201F {
            let crm_op2 = bits(insn, 11, 5);
            if crm_op2 == 7 {
                return false; // XPACLRI -> interpreter
            }
            if crm_op2 >= 0x08 && crm_op2 <= 0x0f {
                // PACIA1716 etc: identity
                return true;
            }
            return true;
        }
        // barriers
        if insn & 0xFFFF_F01F == 0xD503_301F {
            let op2 = bits(insn, 7, 5);
            let crm = bits(insn, 11, 8);
            match op2 {
                4 | 5 => {
                    if crm & 3 == 3 || crm & 3 == 0 {
                        a!(self, mfence());
                    }
                    return true;
                }
                6 => {
                    // ISB is a context synchronization event. A different thread
                    // may have invalidated code while this one stayed in an IBTC
                    // or intra-unit loop. Returning without a patchable slot lets
                    // the dispatcher observe EPOCH and clear stale local targets.
                    // Never use goto here: an internal edge or a chained slot
                    // could bypass that synchronization on subsequent executions.
                    self.st_pc_and_exit(next);
                    return true;
                }
                7 => return true, // SB (speculation barrier), not an ISB
                _ => return false,
            }
        }
        // SYS #3, C7, Cm, #1, Xt: data-cache maintenance (IC IVAU ends the block and goes to the runtime)
        if insn & 0xFFFF_F0E0 == 0xD50B_7020 {
            match bits(insn, 11, 8) {
                4 => {
                    // DC ZVA: zero the 64-byte block (libsystem's bzero/memset use it for large clears)
                    self.ldx(rax, insn & 31, false);
                    self.commpage_fix();
                    a!(self, and(rax, -64));
                    for i in 0..8 {
                        a!(self, mov(qword_ptr(rax + i * 8), 0));
                    }
                    return true;
                }
                // DC CVAC / CVAU / CVAP / CVADP / CIVAC: x86 caches are coherent, nothing to do
                10 | 11 | 12 | 13 | 14 => return true,
                _ => {}
            }
        }
        match insn & 0xFFFF_FFE0 {
            0xD53B_D060 => {
                // MRS TPIDRRO_EL0
                a!(self, mov(rax, qword_ptr(r15 + off::TPIDRRO)));
                self.stx(insn & 31, rax, false);
                return true;
            }
            0xD53B_D040 => {
                a!(self, mov(rax, qword_ptr(r15 + off::TPIDR)));
                self.stx(insn & 31, rax, false);
                return true;
            }
            0xD51B_D040 => {
                self.ldx(rax, insn & 31, false);
                a!(self, mov(qword_ptr(r15 + off::TPIDR), rax));
                return true;
            }
            _ => {}
        }
        false
    }

    /// Translated BL/BLR as a host call (Rosetta/Box64 CALLRET): the callee's RET becomes a host `ret`,
    /// which the CPU's return-address predictor gets right, instead of an IBTC lookup and an indirect
    /// jump. `callee` emits the jump to the target. Each open call keeps the translation epoch next to
    /// the return address (also keeping rsp 16-byte aligned); the continuation goes on to `next` only
    /// if no translation was invalidated meanwhile and the RET's target is `next`, else it dispatches
    /// to the real target. Calls nest at most RAS_LIMIT deep; deeper ones (and longjmp-style returns)
    /// take the plain paths, and the exit trampoline discards open host frames.
    fn host_call(&mut self, next: u64, callee: impl Fn(&mut Self)) {
        const RAS_LIMIT: i32 = 256; // open calls; 16 bytes of host stack each
        let mut l_plain = self.new_label();
        let mut l_callee = self.new_label();
        let mut l_target = self.new_label();
        // depth from the stack pointer itself: rsp > entry rsp - limit*16 leaves room for one more
        a!(self, mov(rcx, qword_ptr(r15 + off::HOST_SP)));
        a!(self, sub(rcx, RAS_LIMIT * 16));
        a!(self, cmp(rsp, rcx));
        a!(self, jbe(l_plain));
        a!(self, mov(rcx, qword_ptr(r15 + off::EPOCH_PTR)));
        a!(self, push(qword_ptr(rcx)));
        a!(self, call(l_callee));
        // continuation: the callee clobbered every host register but r15/rsp
        self.rc_clear();
        a!(self, pop(rcx));
        a!(self, mov(rdx, qword_ptr(r15 + off::EPOCH_PTR)));
        a!(self, mov(rax, qword_ptr(r15 + off::RET_TARGET)));
        a!(self, cmp(rcx, qword_ptr(rdx)));
        a!(self, jne(l_target));
        self.mov_imm(rcx, next);
        a!(self, cmp(rax, rcx));
        a!(self, jne(l_target));
        // translate the caller's next block into this unit, so the continuation runs straight into it
        if self.u.inline_ok && !self.u.labels.contains_key(&next) && self.u.inline_queue.len() < 8 {
            let l = self.u.a.create_label();
            self.u.labels.insert(next, l);
            self.u.inline_queue.push(next);
        }
        self.goto(next);
        self.u.a.set_label(&mut l_target).unwrap();
        self.indirect(true); // rax = where the RET wanted to go
        self.u.a.set_label(&mut l_callee).unwrap();
        callee(self);
        self.u.a.set_label(&mut l_plain).unwrap();
        callee(self);
    }

    /// Branch while the producer's host flags are still live. The condition is
    /// for the not-taken edge; successors retain the existing intra-unit/slot paths.
    /// Guest NZCV is in Cpu and is not modified by CBZ/TBZ or these host flags.
    fn flags_branch(&mut self, taken: u64, not_taken: u64, cc_not_taken: Cc) {
        let mut l_nt = self.new_label();
        self.jcc(cc_not_taken, l_nt);
        self.goto(taken);
        self.u.a.set_label(&mut l_nt).unwrap();
        self.goto(not_taken);
    }

    /// al holds the condition; emit taken/not-taken successors.
    fn cond_branch(&mut self, taken: u64, not_taken: u64) {
        let mut l_nt = self.new_label();
        a!(self, test(al, al));
        a!(self, jz(l_nt));
        self.goto(taken);
        self.u.a.set_label(&mut l_nt).unwrap();
        self.goto(not_taken);
    }

    /// Indirect branch to the guest address in rax via the per-thread
    /// indirect-branch table; misses go to the dispatcher.
    fn indirect(&mut self, _is_ret: bool) {
        let mut l_miss = self.new_label();
        a!(self, mov(rcx, rax));
        a!(self, shr(ecx, 2));
        a!(self, and(ecx, (crate::jit::IBTC_SIZE - 1) as i32));
        a!(self, shl(ecx, 4));
        a!(self, add(rcx, qword_ptr(r15 + off::IBTC)));
        a!(self, cmp(rax, qword_ptr(rcx)));
        a!(self, jne(l_miss));
        a!(self, jmp(qword_ptr(rcx + 8)));
        self.u.a.set_label(&mut l_miss).unwrap();
        a!(self, mov(qword_ptr(r15 + off::PC), rax));
        a!(self, mov(qword_ptr(r15 + off::EXIT_REASON), EXIT_CONTINUE as i32));
        a!(self, mov(qword_ptr(r15 + off::EXIT_DATA), 0));
        let exit = self.u.exit_label;
        a!(self, jmp(exit));
    }

    // ---------------- loads and stores ----------------

    /// Acquire/release loads and stores and LSE atomics. x86's TSO ordering already gives
    /// acquire/release semantics for plain accesses, and LOCK-prefixed ops are full barriers.
    fn try_atomics(&mut self, insn: u32) -> Option<bool> {
        let rt = bits(insn, 4, 0);
        let rn = bits(insn, 9, 5);
        let rs = bits(insn, 20, 16);
        let size = bits(insn, 31, 30);
        // LDAR/LDLAR/STLR/STLLR
        if insn & 0x3FBF_7C00 == 0x089F_7C00 {
            let load = bit(insn, 22) != 0;
            self.ldx(rax, rn, true);
            return Some(self.access(rt, false, size, load as u32, false));
        }
        // LDAPR (RCpc)
        if insn & 0x3FFF_FC00 == 0x38BF_C000 {
            self.ldx(rax, rn, true);
            return Some(self.access(rt, false, size, 1, false));
        }
        // LDAPUR/STLUR family (RCpc unscaled)
        if insn & 0x3F20_0C00 == 0x1900_0000 {
            let off = sext(bits(insn, 20, 12) as u64, 9) as i64;
            self.ldx(rax, rn, true);
            if off != 0 {
                self.mov_imm(rcx, off as u64);
                a!(self, add(rax, rcx));
            }
            let opc = bits(insn, 23, 22);
            return Some(self.access_sized(rt, false, size, opc));
        }
        // CAS/CASA/CASL/CASAL
        if insn & 0x3FA0_7C00 == 0x08A0_7C00 {
            self.ldx(rax, rn, true);
            self.commpage_fix();
            a!(self, mov(rsi, rax));
            self.ldx(rax, rs, false);
            self.ldx(rdx, rt, false);
            match size {
                0 => { self.u.a.lock().cmpxchg(byte_ptr(rsi), dl).unwrap(); a!(self, movzx(eax, al)); }
                1 => { self.u.a.lock().cmpxchg(word_ptr(rsi), dx).unwrap(); a!(self, movzx(eax, ax)); }
                2 => { self.u.a.lock().cmpxchg(dword_ptr(rsi), edx).unwrap(); a!(self, mov(eax, eax)); }
                _ => { self.u.a.lock().cmpxchg(qword_ptr(rsi), rdx).unwrap(); }
            }
            self.stx(rs, rax, false);
            return Some(true);
        }
        // LD<op> / SWP (LSE): size 111000 A R 1 Rs o3 opc 00 Rn Rt
        if insn & 0x3F20_0C00 == 0x3820_0000 {
            let o3 = bit(insn, 15);
            let opc = bits(insn, 14, 12);
            let simple = (o3 == 0 && opc <= 3) || (o3 == 1 && opc == 0);
            if !simple {
                return None;
            }
            self.ldx(rax, rn, true);
            self.commpage_fix();
            a!(self, mov(rsi, rax));
            self.ldx(rdx, rs, false);
            if o3 == 1 {
                // SWP
                match size {
                    0 => { a!(self, xchg(byte_ptr(rsi), dl)); a!(self, movzx(edx, dl)); }
                    1 => { a!(self, xchg(word_ptr(rsi), dx)); a!(self, movzx(edx, dx)); }
                    2 => { a!(self, xchg(dword_ptr(rsi), edx)); a!(self, mov(edx, edx)); }
                    _ => { a!(self, xchg(qword_ptr(rsi), rdx)); }
                }
                self.stx(rt, rdx, false);
                return Some(true);
            }
            if opc == 0 {
                // LDADD
                match size {
                    0 => { self.u.a.lock().xadd(byte_ptr(rsi), dl).unwrap(); a!(self, movzx(edx, dl)); }
                    1 => { self.u.a.lock().xadd(word_ptr(rsi), dx).unwrap(); a!(self, movzx(edx, dx)); }
                    2 => { self.u.a.lock().xadd(dword_ptr(rsi), edx).unwrap(); a!(self, mov(edx, edx)); }
                    _ => { self.u.a.lock().xadd(qword_ptr(rsi), rdx).unwrap(); }
                }
                self.stx(rt, rdx, false);
                return Some(true);
            }
            // LDCLR / LDEOR / LDSET: compare-exchange loop
            self.mov_rr(rcx, rdx);
            if opc == 1 {
                a!(self, not(rcx));
            }
            match size {
                0 => a!(self, movzx(eax, byte_ptr(rsi))),
                1 => a!(self, movzx(eax, word_ptr(rsi))),
                2 => a!(self, mov(eax, dword_ptr(rsi))),
                _ => a!(self, mov(rax, qword_ptr(rsi))),
            }
            let mut retry = self.new_label();
            self.u.a.set_label(&mut retry).unwrap();
            self.mov_rr(rdx, rax);
            match opc {
                1 => a!(self, and(rdx, rcx)),
                2 => a!(self, xor(rdx, rcx)),
                _ => a!(self, or(rdx, rcx)),
            }
            match size {
                0 => self.u.a.lock().cmpxchg(byte_ptr(rsi), dl).unwrap(),
                1 => self.u.a.lock().cmpxchg(word_ptr(rsi), dx).unwrap(),
                2 => self.u.a.lock().cmpxchg(dword_ptr(rsi), edx).unwrap(),
                _ => self.u.a.lock().cmpxchg(qword_ptr(rsi), rdx).unwrap(),
            }
            a!(self, jne(retry));
            match size {
                0 => a!(self, movzx(eax, al)),
                1 => a!(self, movzx(eax, ax)),
                2 => a!(self, mov(eax, eax)),
                _ => {}
            }
            self.stx(rt, rax, false);
            return Some(true);
        }
        None
    }

    fn mov_rr(&mut self, d: R64, s: R64) {
        a!(self, mov(d, s));
    }

    fn ldst(&mut self, insn: u32) -> bool {
        let rt = bits(insn, 4, 0);
        let rn = bits(insn, 9, 5);
        let v = bit(insn, 26) != 0;
        if !v {
            if let Some(r) = self.try_atomics(insn) {
                return r;
            }
        } else if insn & 0xBF00_0000 == 0x0C00_0000 || insn & 0xBF00_0000 == 0x0D00_0000 {
            // AdvSIMD load/store structure forms
            return self.simd_fp(insn);
        }
        // load literal
        if insn & 0x3B00_0000 == 0x1800_0000 {
            let opc = bits(insn, 31, 30);
            let addr = self.pc.wrapping_add((sext(bits(insn, 23, 5) as u64, 19) << 2) as u64);
            self.mov_imm(rax, addr);
            return if v {
                if opc == 3 {
                    return false;
                }
                self.access(rt, true, [2, 3, 4][opc as usize], 1, true)
            } else {
                match opc {
                    0 => self.access(rt, false, 2, 1, true),
                    1 => self.access(rt, false, 3, 1, true),
                    2 => self.access(rt, false, 18, 1, true),
                    _ => true, // PRFM (literal)
                }
            };
        }
        // pairs
        if insn & 0x3A00_0000 == 0x2800_0000 {
            return self.pair(insn);
        }
        if insn & 0x3B00_0000 == 0x3900_0000 {
            // unsigned immediate
            let size = bits(insn, 31, 30);
            let opc = bits(insn, 23, 22);
            let scale = if v && opc & 2 != 0 { 4 } else { size };
            let imm = (bits(insn, 21, 10) as u64) << scale;
            self.ldx(rax, rn, true);
            if imm != 0 {
                a!(self, add(rax, imm as i32));
            }
            return self.access_sized(rt, v, size, opc);
        }
        if insn & 0x3B20_0000 == 0x3800_0000 {
            let size = bits(insn, 31, 30);
            let opc = bits(insn, 23, 22);
            let imm = sext(bits(insn, 20, 12) as u64, 9);
            let mode = bits(insn, 11, 10);
            self.ldx(rax, rn, true);
            match mode {
                0 | 2 => {
                    if imm != 0 {
                        a!(self, add(rax, imm as i32));
                    }
                    self.access_sized(rt, v, size, opc)
                }
                1 => {
                    // post-index: access at base, then base += imm
                    a!(self, mov(r8, rax));
                    if !self.access_sized(rt, v, size, opc) {
                        return false;
                    }
                    a!(self, add(r8, imm as i32));
                    a!(self, mov(qword_ptr(r15 + xoff(rn)), r8));
                    self.rc_wrote(rn, r8);
                    true
                }
                _ => {
                    a!(self, add(rax, imm as i32));
                    a!(self, mov(r8, rax));
                    if !self.access_sized(rt, v, size, opc) {
                        return false;
                    }
                    a!(self, mov(qword_ptr(r15 + xoff(rn)), r8));
                    self.rc_wrote(rn, r8);
                    true
                }
            }
        } else if insn & 0x3B20_0C00 == 0x3820_0800 {
            // register offset
            let size = bits(insn, 31, 30);
            let opc = bits(insn, 23, 22);
            let rm = bits(insn, 20, 16);
            let option = bits(insn, 15, 13);
            let s = bit(insn, 12);
            if option & 2 == 0 {
                return false;
            }
            let scale = if v && opc & 2 != 0 { 4 } else { size };
            self.ldx(rcx, rm, false);
            self.extend(rcx, option, if s != 0 { scale } else { 0 });
            self.ldx(rax, rn, true);
            a!(self, add(rax, rcx));
            self.access_sized(rt, v, size, opc)
        } else {
            false
        }
    }

    /// Access with LDR/STR (size, opc) semantics at address rax.
    /// Preserves r8 (used for writeback).
    fn access_sized(&mut self, rt: u32, v: bool, size: u32, opc: u32) -> bool {
        if v {
            let q = opc & 2 != 0;
            if q && size != 0 {
                return false;
            }
            let kind = if q { 4 } else { size };
            return self.access(rt, true, kind, opc & 1, false);
        }
        // kind: 0..3 = zero-extend size, 5..6 = sign-extend to 64 (b,h), 7 = sign-extend w, 8.. = sign-extend to 32
        match opc {
            0 => self.access(rt, false, size, 0, false),
            1 => self.access(rt, false, size, 1, false),
            2 => {
                if size == 3 {
                    return true; // PRFM
                }
                self.access(rt, false, 16 + size, 1, false)
            }
            _ => {
                if size >= 2 {
                    return false;
                }
                self.access(rt, false, 32 + size, 1, false)
            }
        }
    }

    /// Core memory access at host address rax.
    /// kind: 0-3 = 1/2/4/8 bytes; 4 = 16 bytes (SIMD only); 16+s = sign-extend
    /// to 64; 32+s = sign-extend to 32. `load`: 1 = load, 0 = store.
    fn access(&mut self, rt: u32, v: bool, kind: u32, load: u32, literal: bool) -> bool {
        let _ = literal;
        self.commpage_fix();
        let load = load != 0;
        if v {
            match (kind, load) {
                (4, true) => {
                    a!(self, movdqu(xmm0, xmmword_ptr(rax)));
                    a!(self, movdqu(xmmword_ptr(r15 + voff(rt)), xmm0));
                    self.vfill(rt, xmm0);
                }
                (2 | 3, true) => {
                    // through xmm0 (upper part zero) with one 16-byte store, so the value can stay cached
                    if kind == 2 { a!(self, movd(xmm0, dword_ptr(rax))); } else { a!(self, movq(xmm0, qword_ptr(rax))); }
                    a!(self, movdqu(xmmword_ptr(r15 + voff(rt)), xmm0));
                    self.vfill(rt, xmm0);
                }
                (4, false) => {
                    a!(self, movdqu(xmm0, xmmword_ptr(r15 + voff(rt))));
                    a!(self, movdqu(xmmword_ptr(rax), xmm0));
                }
                (k, true) if k < 4 => {
                    match k {
                        0 => a!(self, movzx(ecx, byte_ptr(rax))),
                        1 => a!(self, movzx(ecx, word_ptr(rax))),
                        2 => a!(self, mov(ecx, dword_ptr(rax))),
                        _ => a!(self, mov(rcx, qword_ptr(rax))),
                    }
                    a!(self, mov(qword_ptr(r15 + voff(rt)), rcx));
                    a!(self, mov(qword_ptr(r15 + voff(rt) + 8), 0));
                }
                (k, false) if k < 4 => match k {
                    0 => {
                        a!(self, mov(cl, byte_ptr(r15 + voff(rt))));
                        a!(self, mov(byte_ptr(rax), cl));
                    }
                    1 => {
                        a!(self, mov(cx, word_ptr(r15 + voff(rt))));
                        a!(self, mov(word_ptr(rax), cx));
                    }
                    2 => {
                        a!(self, mov(ecx, dword_ptr(r15 + voff(rt))));
                        a!(self, mov(dword_ptr(rax), ecx));
                    }
                    _ => {
                        a!(self, mov(rcx, qword_ptr(r15 + voff(rt))));
                        a!(self, mov(qword_ptr(rax), rcx));
                    }
                },
                _ => return false,
            }
            return true;
        }
        if load {
            match kind {
                0 => a!(self, movzx(ecx, byte_ptr(rax))),
                1 => a!(self, movzx(ecx, word_ptr(rax))),
                2 => a!(self, mov(ecx, dword_ptr(rax))),
                3 => a!(self, mov(rcx, qword_ptr(rax))),
                16 => a!(self, movsx(rcx, byte_ptr(rax))),
                17 => a!(self, movsx(rcx, word_ptr(rax))),
                18 => a!(self, movsxd(rcx, dword_ptr(rax))),
                32 => a!(self, movsx(ecx, byte_ptr(rax))),
                33 => a!(self, movsx(ecx, word_ptr(rax))),
                _ => return false,
            }
            self.stx(rt, rcx, false);
        } else {
            self.ldx(rcx, rt, false);
            match kind {
                0 => a!(self, mov(byte_ptr(rax), cl)),
                1 => a!(self, mov(word_ptr(rax), cx)),
                2 => a!(self, mov(dword_ptr(rax), ecx)),
                3 => a!(self, mov(qword_ptr(rax), rcx)),
                _ => return false,
            }
        }
        true
    }

    fn pair(&mut self, insn: u32) -> bool {
        let opc = bits(insn, 31, 30);
        let v = bit(insn, 26) != 0;
        let idx = bits(insn, 24, 23);
        let l = bit(insn, 22) != 0;
        let rt = bits(insn, 4, 0);
        let rt2 = bits(insn, 14, 10);
        let rn = bits(insn, 9, 5);
        let (scale, kind) = if v {
            if opc == 3 {
                return false;
            }
            (2 + opc, if opc == 2 { 4 } else { 2 + opc })
        } else {
            match opc {
                0 => (2, 2),
                1 if l => (2, 18),
                2 => (3, 3),
                _ => return false,
            }
        };
        let off = (sext(bits(insn, 21, 15) as u64, 7) << scale) as i32;
        let nb = 1i32 << scale;
        self.ldx(rax, rn, true);
        if idx == 3 || idx == 2 || idx == 0 {
            if off != 0 {
                a!(self, add(rax, off));
            }
        }
        // r8 keeps the writeback value
        if idx == 1 {
            a!(self, lea(r8, qword_ptr(rax + off)));
        } else {
            a!(self, mov(r8, rax));
        }
        a!(self, mov(r9, rax));
        if l && !v && rt == rt2 {
            return false;
        }
        if !self.access(rt, v, kind, l as u32, false) {
            return false;
        }
        a!(self, lea(rax, qword_ptr(r9 + nb)));
        if !self.access(rt2, v, kind, l as u32, false) {
            return false;
        }
        if idx == 1 || idx == 3 {
            a!(self, mov(qword_ptr(r15 + xoff(rn)), r8));
            self.rc_wrote(rn, r8);
        }
        true
    }
}

/// Logical immediate decoding (returns None for reserved encodings).
fn decode_logic_imm(n: u32, imms: u32, immr: u32, sf: bool) -> Option<u64> {
    let combined = (n << 6) | (!imms & 0x3f);
    if combined == 0 {
        return None;
    }
    let len = 31 - combined.leading_zeros();
    if len < 1 {
        return None;
    }
    let esize = 1u32 << len;
    let dsz = if sf { 64 } else { 32 };
    if esize > dsz {
        return None;
    }
    let levels = esize - 1;
    let s = imms & levels;
    let r = immr & levels;
    if s == levels {
        return None;
    }
    let ones = |n: u32| if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    let welem = ones(s + 1);
    let emask = ones(esize);
    let rot = if r == 0 { welem } else { ((welem >> r) | (welem << (esize - r))) & emask };
    let mut v = 0u64;
    let mut i = 0;
    while i < dsz {
        v |= rot << i;
        i += esize;
    }
    Some(if sf { v } else { v & 0xffff_ffff })
}

#[allow(dead_code)]
fn _keep(_: u32) -> u64 {
    vfp_expand_imm(0)
}


// ---------------- Advanced SIMD and scalar floating point ----------------
//
// Only the forms that dominate real workloads (string/blit loops, colour and geometry maths)
// are translated; everything else falls back to the interpreter. Guest vector registers live in
// memory at `voff(n)`; xmm0-xmm3 are scratch.

impl<'a> E<'a> {
    fn vld(&mut self, x: AsmRegisterXmm, n: u32, q: bool) {
        if q {
            a!(self, movdqu(x, xmmword_ptr(r15 + voff(n))));
        } else {
            a!(self, movq(x, qword_ptr(r15 + voff(n))));
        }
    }

    /// Store xmm0 to Vd (clearing the upper half for 64-bit operations).
    fn vfin(&mut self, d: u32, q: bool) {
        if !q {
            a!(self, movq(xmm0, xmm0));
        }
        a!(self, movdqu(xmmword_ptr(r15 + voff(d)), xmm0));
    }

    /// Store the 64-bit value in rax to Vd (upper half zero unless `dup_hi`).
    fn vst_rax(&mut self, d: u32, dup_hi: bool) {
        a!(self, mov(qword_ptr(r15 + voff(d)), rax));
        if dup_hi {
            a!(self, mov(qword_ptr(r15 + voff(d) + 8), rax));
        } else {
            a!(self, mov(qword_ptr(r15 + voff(d) + 8), 0));
        }
    }

    fn elem_size(imm5: u32) -> Option<(u32, u32)> {
        // (log2 size, index)
        if imm5 & 1 != 0 {
            Some((0, imm5 >> 1))
        } else if imm5 & 2 != 0 {
            Some((1, imm5 >> 2))
        } else if imm5 & 4 != 0 {
            Some((2, imm5 >> 3))
        } else if imm5 & 8 != 0 {
            Some((3, imm5 >> 4))
        } else {
            None
        }
    }

    /// Load element `idx` of Vn zero-extended into rax.
    fn velem_ld(&mut self, n: u32, size: u32, idx: u32) {
        let o = voff(n) + (idx << size) as i32;
        match size {
            0 => a!(self, movzx(eax, byte_ptr(r15 + o))),
            1 => a!(self, movzx(eax, word_ptr(r15 + o))),
            2 => a!(self, mov(eax, dword_ptr(r15 + o))),
            _ => a!(self, mov(rax, qword_ptr(r15 + o))),
        }
    }

    /// Replicate the low `size` element of rax across 64 bits.
    fn replicate_rax(&mut self, size: u32) {
        match size {
            0 => {
                a!(self, movzx(eax, al));
                self.mov_imm(rcx, 0x0101_0101_0101_0101);
                a!(self, imul_2(rax, rcx));
            }
            1 => {
                a!(self, movzx(eax, ax));
                self.mov_imm(rcx, 0x0001_0001_0001_0001);
                a!(self, imul_2(rax, rcx));
            }
            2 => {
                a!(self, mov(eax, eax));
                self.mov_imm(rcx, 0x0000_0001_0000_0001);
                a!(self, imul_2(rax, rcx));
            }
            _ => {}
        }
    }

    /// Vector form of `fix_default_nan` on xmm0 (uses xmm2-xmm5).
    fn vfix_default_nan(&mut self, double: bool) {
        // x86's "real indefinite" NaN has the sign bit set; ARM's default NaN does not.
        let indefinite = self.vconstant(if double { 0xFFF8_0000_0000_0000 } else { 0xFFC0_0000_FFC0_0000 });
        let default = self.vconstant(if double { 0x7FF8_0000_0000_0000 } else { 0x7FC0_0000_7FC0_0000 });
        a!(self, movdqa(xmm3, xmm0));
        if double { pool_op!(self, pcmpeqq, vpcmpeqq, xmm3, indefinite, xmm2); } else { pool_op!(self, pcmpeqd, vpcmpeqd, xmm3, indefinite, xmm2); }
        a!(self, movdqa(xmm5, xmm3));
        a!(self, pandn(xmm5, xmm0));
        pool_op!(self, pand, vpand, xmm3, default, xmm4);
        a!(self, por(xmm5, xmm3));
        a!(self, movdqa(xmm0, xmm5));
    }

    /// Overlay a quieted NaN source onto xmm0, using xmm3-xmm5 only. Sources
    /// are visited in reverse priority; a second pass selects signalling NaNs.
    fn select_nan(&mut self, double: bool, src: AsmRegisterXmm, signalling: bool) {
        a!(self, movdqa(xmm3, src));
        if double { a!(self, cmppd(xmm3, xmm3, 3)); } else { a!(self, cmpps(xmm3, xmm3, 3)); }
        let quiet = self.vconstant(if double { 0x0008_0000_0000_0000 } else { 0x0040_0000_0040_0000 });
        if signalling {
            a!(self, movdqa(xmm4, src));
            pool_op!(self, pand, vpand, xmm4, quiet, xmm5);
            a!(self, pxor(xmm5, xmm5));
            if double { a!(self, pcmpeqq(xmm4, xmm5)); } else { a!(self, pcmpeqd(xmm4, xmm5)); }
            a!(self, pand(xmm3, xmm4));
        }
        a!(self, movdqa(xmm4, src));
        pool_op!(self, por, vpor, xmm4, quiet, xmm5);
        a!(self, pand(xmm4, xmm3));
        a!(self, pandn(xmm3, xmm0));
        a!(self, por(xmm3, xmm4));
        a!(self, movdqa(xmm0, xmm3));
    }

    /// Sticky flags for a nonempty lane mask. Does not inspect host MXCSR:
    /// host library work and earlier guest instructions may have left flags set.
    fn half_flag(&mut self, mask: AsmRegisterXmm, flags: i32) {
        let mut done = self.new_label();
        a!(self, ptest(mask, mask));
        a!(self, jz(done));
        a!(self, or(dword_ptr(r15 + off::FPSR as i32), flags));
        self.u.a.set_label(&mut done).unwrap();
        a!(self, nop());
    }

    /// FCVTN single->IEEE half flags: source in xmm1, half result in xmm0.
    /// ARM detects tininess before rounding, including values rounded up to
    /// the minimum normal half. F16C's own sticky state is not authoritative.
    /// DN/AHP/FZ16 blocks retain the existing interpreter entry guard.
    fn half_convert_status(&mut self) {
        a!(self, vcvtph2ps(xmm2, xmm0));
        a!(self, movdqa(xmm3, xmm1));
        self.vconst(xmm7, 0x7fff_ffff_7fff_ffff);
        a!(self, pand(xmm3, xmm7)); // absolute source bits
        a!(self, movdqa(xmm4, xmm2));
        a!(self, cmpps(xmm4, xmm1, 4)); // unequal, including unordered
        a!(self, movdqa(xmm5, xmm1));
        a!(self, cmpps(xmm5, xmm5, 7)); // ordered inputs only
        a!(self, pand(xmm4, xmm5)); // inexact finite conversion
        self.half_flag(xmm4, 1 << 4);

        self.vconst(xmm7, 0x3880_0000_3880_0000); // minimum normal half
        a!(self, movdqa(xmm5, xmm7));
        a!(self, pcmpgtd(xmm5, xmm3));
        a!(self, pand(xmm5, xmm4));
        self.half_flag(xmm5, 1 << 3);

        self.vconst(xmm7, 0x477f_ffff_477f_ffff); // abs source >= 65536
        a!(self, movdqa(xmm5, xmm3));
        a!(self, pcmpgtd(xmm5, xmm7));
        a!(self, pslld(xmm2, 1));
        a!(self, psrld(xmm2, 1)); // absolute widened result
        self.vconst(xmm7, 0x7f80_0000_7f80_0000);
        a!(self, pcmpeqd(xmm2, xmm7)); // rounded to infinity
        a!(self, por(xmm5, xmm2));
        a!(self, movdqa(xmm6, xmm7));
        a!(self, pcmpgtd(xmm6, xmm3)); // source finite
        a!(self, pand(xmm5, xmm6));
        self.half_flag(xmm5, (1 << 2) | (1 << 4));

        a!(self, movdqa(xmm5, xmm3));
        a!(self, pcmpgtd(xmm5, xmm7)); // NaN: abs > infinity
        self.vconst(xmm7, 0x7fc0_0000_7fc0_0000);
        a!(self, pcmpgtd(xmm7, xmm3)); // signalling: abs < quiet-bit cutoff
        a!(self, pand(xmm5, xmm7));
        self.half_flag(xmm5, 1);

        let mut no_fz = self.new_label();
        a!(self, test(dword_ptr(r15 + off::FPCR), 1 << 24));
        a!(self, jz(no_fz));
        self.vconst(xmm7, 0x0080_0000_0080_0000);
        a!(self, pcmpgtd(xmm7, xmm3)); // source below minimum normal FP32
        a!(self, pxor(xmm6, xmm6));
        a!(self, pcmpgtd(xmm3, xmm6)); // source nonzero
        a!(self, pand(xmm7, xmm3));
        self.half_flag(xmm7, 1 << 7);
        self.u.a.set_label(&mut no_fz).unwrap();
        a!(self, nop());
    }

    /// Binary vector arithmetic, with the second source still in xmm1. Reload
    /// the first source only on the rare NaN path, before the destination store.
    fn vfix_binary_nan(&mut self, double: bool, rn: u32, q: bool) {
        let src = FpSource { cached: self.vsrc(rn), offset: voff(rn) };
        let cold = self.u.a.create_label();
        a!(self, movdqa(xmm3, xmm0));
        if double { a!(self, cmppd(xmm3, xmm3, 3)); } else { a!(self, cmpps(xmm3, xmm3, 3)); }
        a!(self, ptest(xmm3, xmm3));
        a!(self, jnz(cold));
        self.cold_path(cold, move |e| {
            e.vfix_default_nan(double);
            match src.cached {
                Some(c) if q => a!(e, movdqa(xmm6, c)),
                Some(c) => a!(e, movq(xmm6, c)),
                None if q => a!(e, movdqu(xmm6, xmmword_ptr(r15 + src.offset))),
                None => a!(e, movq(xmm6, qword_ptr(r15 + src.offset))),
            }
            for signalling in [false, true] {
                for src in [xmm1, xmm6] { e.select_nan(double, src, signalling); }
            }
        });
    }

    /// Fused multiply-add with ARM NaN selection: xmm0 = op1 (already negated for FMLS-like forms),
    /// xmm1 = op2, xmm2 = addend (negated as needed); result in xmm0, including the default-NaN fixup.
    /// x86 picks a NaN operand in the order op1, op2, addend; ARM (FPProcessNaNs3) takes the addend
    /// first, and a quiet-NaN addend with op1*op2 = inf*0 gives the default NaN. Uses xmm2-xmm7.
    fn fma_arm(&mut self, double: bool) {
        a!(self, movdqa(xmm6, xmm2)); // addend
        a!(self, movdqa(xmm7, xmm0)); // op1
        // 213 writes the final result register directly.
        if double { a!(self, vfmadd213pd(xmm0, xmm1, xmm2)); } else { a!(self, vfmadd213ps(xmm0, xmm1, xmm2)); }
        // no NaN lane in the result: no NaN went in and nothing was invalid, x86 and ARM agree exactly
        let cold = self.u.a.create_label();
        a!(self, movdqa(xmm3, xmm0));
        if double { a!(self, cmppd(xmm3, xmm3, 3)); } else { a!(self, cmpps(xmm3, xmm3, 3)); }
        a!(self, ptest(xmm3, xmm3));
        a!(self, jnz(cold));
        self.cold_path(cold, move |e| e.fma_nan(double));
    }

    fn fma_nan(&mut self, double: bool) {
        self.vfix_default_nan(double);
        // The product is NaN from inf*0 only when neither multiplicand was
        // already NaN. Comparing its bits with x86's indefinite pattern also
        // matched propagated negative NaNs and lost their payloads.
        a!(self, movdqa(xmm2, xmm7));
        if double { a!(self, mulpd(xmm2, xmm1)); } else { a!(self, mulps(xmm2, xmm1)); }
        if double { a!(self, cmppd(xmm2, xmm2, 3)); } else { a!(self, cmpps(xmm2, xmm2, 3)); }
        a!(self, movdqa(xmm3, xmm7));
        a!(self, movdqa(xmm4, xmm1));
        if double {
            a!(self, cmppd(xmm3, xmm3, 3));
            a!(self, cmppd(xmm4, xmm4, 3));
        } else {
            a!(self, cmpps(xmm3, xmm3, 3));
            a!(self, cmpps(xmm4, xmm4, 3));
        }
        a!(self, por(xmm3, xmm4));
        a!(self, pandn(xmm3, xmm2));
        a!(self, movdqa(xmm2, xmm3)); // inf*0 mask
        // ARM's special default-NaN case requires a quiet NaN addend.
        a!(self, movdqa(xmm3, xmm6));
        if double { a!(self, cmppd(xmm3, xmm3, 3)); } else { a!(self, cmpps(xmm3, xmm3, 3)); }
        let quiet = self.vconstant(if double { 0x0008_0000_0000_0000 } else { 0x0040_0000_0040_0000 });
        a!(self, movdqa(xmm4, xmm6));
        pool_op!(self, pand, vpand, xmm4, quiet, xmm5);
        if double { pool_op!(self, pcmpeqq, vpcmpeqq, xmm4, quiet, xmm5); } else { pool_op!(self, pcmpeqd, vpcmpeqd, xmm4, quiet, xmm5); }
        a!(self, pand(xmm2, xmm3));
        a!(self, pand(xmm2, xmm4));
        // FPProcessNaNs3: all signalling operands precede all quiet ones;
        // within each category the order is addend, op1, op2.
        for signalling in [false, true] {
            for src in [xmm1, xmm7, xmm6] {
                self.select_nan(double, src, signalling);
            }
        }
        let default = self.vconstant(if double { 0x7FF8_0000_0000_0000 } else { 0x7FC0_0000_7FC0_0000 });
        a!(self, movdqa(xmm3, xmm2));
        pool_op!(self, pand, vpand, xmm3, default, xmm4);
        a!(self, pandn(xmm2, xmm0));
        a!(self, por(xmm2, xmm3));
        a!(self, movdqa(xmm0, xmm2));
    }

    /// ARM FMAX/FMIN/FMAXNM/FMINNM on every lane: xmm0 = op(xmm0, xmm1), using xmm2-xmm5.
    /// x86 MAX/MIN return the second operand for NaNs and equal values; ARM wants max(+0,-0) = +0,
    /// min = -0 (AND/OR of equal values), NM forms pick the number over a quiet NaN, the others
    /// propagate the first NaN, quietened; a signalling NaN always wins (FPProcessNaNs: sNaN a, sNaN b,
    /// then the quiet ones), quietened. Exact for every input. Uses xmm2-xmm7.
    fn fp_maxmin(&mut self, double: bool, max: bool, nm: bool, scalar: bool, q: bool) {
        let cold = self.u.a.create_label();
        if scalar {
            if double { a!(self, ucomisd(xmm0, xmm1)); } else { a!(self, ucomiss(xmm0, xmm1)); }
            a!(self, je(cold)); // equal or unordered
        } else {
            a!(self, movdqa(xmm3, xmm0));
            a!(self, movdqa(xmm4, xmm0));
            if double {
                a!(self, cmppd(xmm3, xmm1, 0));
                a!(self, cmppd(xmm4, xmm1, 3));
            } else {
                a!(self, cmpps(xmm3, xmm1, 0));
                a!(self, cmpps(xmm4, xmm1, 3));
            }
            a!(self, por(xmm3, xmm4));
            if !q { a!(self, movq(xmm3, xmm3)); }
            a!(self, ptest(xmm3, xmm3));
            a!(self, jnz(cold));
        }
        match (double, max) {
            (false, true) => a!(self, maxps(xmm0, xmm1)),
            (false, false) => a!(self, minps(xmm0, xmm1)),
            (true, true) => a!(self, maxpd(xmm0, xmm1)),
            (true, false) => a!(self, minpd(xmm0, xmm1)),
        }
        self.cold_path(cold, move |e| e.fp_maxmin_exact(double, max, nm));
    }

    fn fp_maxmin_exact(&mut self, double: bool, max: bool, nm: bool) {
        let unord = |e: &mut Self, x: AsmRegisterXmm| if double { a!(e, cmppd(x, x, 3)) } else { a!(e, cmpps(x, x, 3)) };
        // FPCR.FZ flushes denormal inputs to signed zeros before comparing: x*1.0 does that under the host
        // DAZ the FPCR sync sets (and is exact without it); NaN lanes keep their bits for the sNaN rules.
        for x in [xmm0, xmm1] {
            self.vconst(xmm2, if double { 0x3FF0_0000_0000_0000 } else { 0x3F80_0000_3F80_0000 });
            if double { a!(self, mulpd(xmm2, x)); } else { a!(self, mulps(xmm2, x)); }
            a!(self, movdqa(xmm3, x));
            unord(self, xmm3);
            a!(self, pand(x, xmm3));
            a!(self, pandn(xmm3, xmm2));
            a!(self, por(x, xmm3));
        }
        a!(self, movdqa(xmm6, xmm0)); // a, kept for the signalling-NaN step
        // xmm2 = x86 max/min(a, b)
        a!(self, movdqa(xmm2, xmm0));
        match (double, max) {
            (false, true) => a!(self, maxps(xmm2, xmm1)),
            (false, false) => a!(self, minps(xmm2, xmm1)),
            (true, true) => a!(self, maxpd(xmm2, xmm1)),
            (true, false) => a!(self, minpd(xmm2, xmm1)),
        }
        // equal (incl. +0 == -0): a & b for max, a | b for min
        a!(self, movdqa(xmm3, xmm0));
        if double { a!(self, cmppd(xmm3, xmm1, 0)); } else { a!(self, cmpps(xmm3, xmm1, 0)); }
        a!(self, movdqa(xmm4, xmm0));
        if max { a!(self, pand(xmm4, xmm1)); } else { a!(self, por(xmm4, xmm1)); }
        a!(self, pand(xmm4, xmm3));
        a!(self, pandn(xmm3, xmm2));
        a!(self, por(xmm3, xmm4));               // xmm3 = result for ordered inputs (and NM's a-NaN case)
        let quiet = self.vconstant(if double { 0x0008_0000_0000_0000 } else { 0x0040_0000_0040_0000 });
        if nm {
            // b NaN: take a (a number, or a NaN when both are)
            a!(self, movdqa(xmm4, xmm1));
            unord(self, xmm4);
            a!(self, movdqa(xmm5, xmm0));
            a!(self, pand(xmm5, xmm4));
            a!(self, pandn(xmm4, xmm3));
            a!(self, por(xmm4, xmm5));
            a!(self, movdqa(xmm0, xmm4));
            // a NaN result (both NaN): quieten it
            a!(self, movdqa(xmm4, xmm0));
            unord(self, xmm4);
            a!(self, movdqa(xmm5, xmm4));
            pool_op!(self, pand, vpand, xmm5, quiet, xmm2);
            a!(self, por(xmm0, xmm5));
        } else {
            // NaN propagates: a if a is NaN, else b if b is NaN, quietened
            a!(self, movdqa(xmm4, xmm1));
            unord(self, xmm4);                   // b NaN
            a!(self, movdqa(xmm5, xmm1));
            a!(self, pand(xmm5, xmm4));
            a!(self, pandn(xmm4, xmm3));
            a!(self, por(xmm4, xmm5));           // b-NaN lanes take b
            a!(self, movdqa(xmm3, xmm0));
            unord(self, xmm3);                   // a NaN
            a!(self, movdqa(xmm5, xmm0));
            a!(self, pand(xmm5, xmm3));
            a!(self, pandn(xmm3, xmm4));
            a!(self, por(xmm3, xmm5));           // a-NaN lanes take a
            a!(self, movdqa(xmm0, xmm3));
            a!(self, movdqa(xmm4, xmm0));
            unord(self, xmm4);
            a!(self, movdqa(xmm5, xmm4));
            pool_op!(self, pand, vpand, xmm5, quiet, xmm2);
            a!(self, por(xmm0, xmm5));
        }
        // signalling NaNs: b's, then a's (higher priority), each quietened
        for src in [xmm1, xmm6] {
            a!(self, movdqa(xmm4, src));
            unord(self, xmm4);                   // NaN lanes
            a!(self, movdqa(xmm3, src));
            pool_op!(self, pand, vpand, xmm3, quiet, xmm2);          // quiet bit of each lane
            a!(self, pxor(xmm2, xmm2));
            if double { a!(self, pcmpeqq(xmm3, xmm2)); } else { a!(self, pcmpeqd(xmm3, xmm2)); }
            a!(self, pand(xmm4, xmm3));          // sNaN lanes
            a!(self, movdqa(xmm7, src));
            pool_op!(self, por, vpor, xmm7, quiet, xmm2);           // quietened
            a!(self, pand(xmm7, xmm4));
            a!(self, pandn(xmm4, xmm0));
            a!(self, por(xmm4, xmm7));
            a!(self, movdqa(xmm0, xmm4));
        }
    }

    /// Flip the sign of every lane of `x` (FPNeg, NaNs included).
    fn vneg(&mut self, x: AsmRegisterXmm, double: bool) {
        let sign = self.vconstant(if double { 0x8000_0000_0000_0000 } else { 0x8000_0000_8000_0000 });
        pool_op!(self, pxor, vpxor, x, sign, xmm3);
    }

    /// Replace the x86 "real indefinite" NaN produced by invalid operations with the ARM default NaN.
    fn fix_default_nan(&mut self, double: bool, rn: u32, rm: Option<u32>) {
        let nsrc = self.scalar_source(rn);
        let msrc = rm.map(|m| self.scalar_source(m));
        let cold = self.u.a.create_label();
        // not a NaN (the usual case): nothing to fix
        if double { a!(self, ucomisd(xmm0, xmm0)); } else { a!(self, ucomiss(xmm0, xmm0)); }
        a!(self, jp(cold));
        self.cold_path(cold, move |e| e.scalar_nan(double, nsrc, msrc));
    }

    fn scalar_nan(&mut self, double: bool, nsrc: FpSource, msrc: Option<FpSource>) {
        self.vfix_default_nan(double);
        self.fld_source(xmm6, nsrc, double);
        if let Some(src) = msrc { self.fld_source(xmm1, src, double); }
        for signalling in [false, true] {
            if msrc.is_some() { self.select_nan(double, xmm1, signalling); }
            self.select_nan(double, xmm6, signalling);
        }
    }

    fn simd_fp(&mut self, insn: u32) -> bool {
        if self.simd_more(insn) {
            return true;
        }
        let rd = bits(insn, 4, 0);
        let rn = bits(insn, 9, 5);
        let rm = bits(insn, 20, 16);
        let q = bit(insn, 30) != 0;

        // ---- Advanced SIMD three same: logic, add/sub, compares ----
        if insn & 0x9F20_0400 == 0x0E20_0400 {
            let u = bit(insn, 29) != 0;
            let size = bits(insn, 23, 22);
            let opcode = bits(insn, 15, 11);
            match opcode {
                0b00011 => {
                    self.vld(xmm0, rn, q);
                    self.vld(xmm1, rm, q);
                    match (u, size) {
                        (false, 0) => a!(self, pand(xmm0, xmm1)),
                        (false, 1) => {
                            a!(self, pandn(xmm1, xmm0));
                            a!(self, movdqa(xmm0, xmm1));
                        }
                        (false, 2) => a!(self, por(xmm0, xmm1)),
                        (false, _) => {
                            a!(self, pcmpeqd(xmm2, xmm2));
                            a!(self, pxor(xmm1, xmm2));
                            a!(self, por(xmm0, xmm1));
                        }
                        (true, 0) => a!(self, pxor(xmm0, xmm1)),
                        (true, 1) => {
                            // BSL: (n & d) | (m & ~d)
                            self.vld(xmm2, rd, q);
                            a!(self, pand(xmm0, xmm2));
                            a!(self, pandn(xmm2, xmm1));
                            a!(self, por(xmm0, xmm2));
                        }
                        (true, 2) => {
                            // BIT: (d & ~m) | (n & m)
                            self.vld(xmm2, rd, q);
                            a!(self, pand(xmm0, xmm1));
                            a!(self, pandn(xmm1, xmm2));
                            a!(self, por(xmm0, xmm1));
                        }
                        (true, _) => {
                            // BIF: (d & m) | (n & ~m)
                            self.vld(xmm2, rd, q);
                            a!(self, pand(xmm2, xmm1));
                            a!(self, pandn(xmm1, xmm0));
                            a!(self, por(xmm1, xmm2));
                            a!(self, movdqa(xmm0, xmm1));
                        }
                    }
                    self.vfin(rd, q);
                    return true;
                }
                0b10000 => {
                    self.vld(xmm0, rn, q);
                    self.vld(xmm1, rm, q);
                    match (u, size) {
                        (false, 0) => a!(self, paddb(xmm0, xmm1)),
                        (false, 1) => a!(self, paddw(xmm0, xmm1)),
                        (false, 2) => a!(self, paddd(xmm0, xmm1)),
                        (false, _) => a!(self, paddq(xmm0, xmm1)),
                        (true, 0) => a!(self, psubb(xmm0, xmm1)),
                        (true, 1) => a!(self, psubw(xmm0, xmm1)),
                        (true, 2) => a!(self, psubd(xmm0, xmm1)),
                        (true, _) => a!(self, psubq(xmm0, xmm1)),
                    }
                    self.vfin(rd, q);
                    return true;
                }
                0b10001 if u && size < 3 => {
                    self.vld(xmm0, rn, q);
                    self.vld(xmm1, rm, q);
                    match size {
                        0 => a!(self, pcmpeqb(xmm0, xmm1)),
                        1 => a!(self, pcmpeqw(xmm0, xmm1)),
                        _ => a!(self, pcmpeqd(xmm0, xmm1)),
                    }
                    self.vfin(rd, q);
                    return true;
                }
                0b00110 | 0b00111 if size < 3 => {
                    // CMGT/CMGE (signed), CMHI/CMHS (unsigned); the "or equal" forms are ~(m > n).
                    let or_equal = opcode == 0b00111;
                    self.vld(xmm0, rn, q);
                    self.vld(xmm1, rm, q);
                    if or_equal {
                        a!(self, movdqa(xmm2, xmm0));
                        a!(self, movdqa(xmm0, xmm1));
                        a!(self, movdqa(xmm1, xmm2));
                    }
                    // Now compute xmm0 > xmm1.
                    if !u {
                        match size {
                            0 => a!(self, pcmpgtb(xmm0, xmm1)),
                            1 => a!(self, pcmpgtw(xmm0, xmm1)),
                            _ => a!(self, pcmpgtd(xmm0, xmm1)),
                        }
                    } else if size == 0 {
                        a!(self, movdqa(xmm2, xmm0));
                        a!(self, pmaxub(xmm2, xmm1));
                        a!(self, pcmpeqb(xmm2, xmm1)); // xmm0 <= xmm1
                        a!(self, pcmpeqd(xmm3, xmm3));
                        a!(self, pxor(xmm2, xmm3));
                        a!(self, movdqa(xmm0, xmm2));
                    } else {
                        a!(self, pcmpeqd(xmm2, xmm2));
                        if size == 1 {
                            a!(self, psllw(xmm2, 15));
                        } else {
                            a!(self, pslld(xmm2, 31));
                        }
                        a!(self, pxor(xmm0, xmm2));
                        a!(self, pxor(xmm1, xmm2));
                        if size == 1 {
                            a!(self, pcmpgtw(xmm0, xmm1));
                        } else {
                            a!(self, pcmpgtd(xmm0, xmm1));
                        }
                    }
                    if or_equal {
                        a!(self, pcmpeqd(xmm2, xmm2));
                        a!(self, pxor(xmm0, xmm2));
                    }
                    self.vfin(rd, q);
                    return true;
                }
                _ => return false,
            }
        }

        // ---- Advanced SIMD permute: 64-bit ZIP/UZP via one PSHUFB of n.lo:m.lo; TRN1/TRN2 (both widths)
        // by shifting one operand by an element and blending odd/even elements ----
        if insn & 0xBF20_8C00 == 0x0E00_0800 && !jit_off("permute") {
            let size = bits(insn, 23, 22);
            let opc = bits(insn, 14, 12);
            if !q && size < 3 && matches!(opc, 0b001 | 0b101 | 0b011 | 0b111) {
                let eb = 1usize << size;            // element bytes
                let k = 8 / eb;                      // elements per 64-bit half
                let elems: Vec<usize> = match opc {
                    0b011 => (0..k / 2).flat_map(|i| [i, k + i]).collect(),               // ZIP1
                    0b111 => (k / 2..k).flat_map(|i| [i, k + i]).collect(),               // ZIP2
                    0b001 => (0..k).map(|i| 2 * i).collect(),                             // UZP1
                    _ => (0..k).map(|i| 2 * i + 1).collect(),                             // UZP2
                };
                let mut tbl = [0x80u8; 16];
                for (j, &e) in elems.iter().enumerate() {
                    for b in 0..eb {
                        tbl[j * eb + b] = (e * eb + b) as u8;
                    }
                }
                a!(self, movq(xmm0, qword_ptr(r15 + voff(rn))));
                a!(self, movq(xmm1, qword_ptr(r15 + voff(rm))));
                a!(self, punpcklqdq(xmm0, xmm1));
                self.vconst128(xmm2, u64::from_le_bytes(tbl[..8].try_into().unwrap()), u64::from_le_bytes(tbl[8..].try_into().unwrap()));
                a!(self, pshufb(xmm0, xmm2));
                self.vfin(rd, false);
                return true;
            }
            if matches!(opc, 0b010 | 0b110) && (q || size < 3) {
                // TRN1: even lanes n[2i], odd lanes m[2i]; TRN2: even n[2i+1], odd m[2i+1]
                let trn2 = opc == 0b110;
                self.vld(xmm0, rn, q);
                self.vld(xmm1, rm, q);
                let shift = |e: &mut Self, x: AsmRegisterXmm, left: bool| match (size, left) {
                    (0, true) => a!(e, psllw(x, 8)),
                    (0, false) => a!(e, psrlw(x, 8)),
                    (1, true) => a!(e, pslld(x, 16)),
                    (1, false) => a!(e, psrld(x, 16)),
                    (2, true) => a!(e, psllq(x, 32)),
                    (2, false) => a!(e, psrlq(x, 32)),
                    (_, true) => a!(e, pslldq(x, 8)),
                    (_, false) => a!(e, psrldq(x, 8)),
                };
                if trn2 { shift(self, xmm0, false); } else { shift(self, xmm1, true); }
                // mask of odd elements
                self.vconst(xmm2, match size { 0 => 0xFF00_FF00_FF00_FF00, 1 => 0xFFFF_0000_FFFF_0000, 2 => 0xFFFF_FFFF_0000_0000, _ => 0 });
                if size == 3 {
                    self.vconst128(xmm2, 0, u64::MAX);
                }
                a!(self, pand(xmm1, xmm2));
                a!(self, pandn(xmm2, xmm0));
                a!(self, por(xmm1, xmm2));
                a!(self, movdqa(xmm0, xmm1));
                self.vfin(rd, q);
                return true;
            }
        }

        // ---- Advanced SIMD permute: ZIP/UZP (128-bit) ----
        if insn & 0xBF20_8C00 == 0x0E00_0800 && q {
            let size = bits(insn, 23, 22);
            let opc = bits(insn, 14, 12);
            let ok = matches!(opc, 0b001 | 0b101 | 0b011 | 0b111);
            if !ok {
                return false;
            }
            self.vld(xmm0, rn, true);
            self.vld(xmm1, rm, true);
            match opc {
                0b011 => match size {
                    0 => a!(self, punpcklbw(xmm0, xmm1)),
                    1 => a!(self, punpcklwd(xmm0, xmm1)),
                    2 => a!(self, punpckldq(xmm0, xmm1)),
                    _ => a!(self, punpcklqdq(xmm0, xmm1)),
                },
                0b111 => match size {
                    0 => a!(self, punpckhbw(xmm0, xmm1)),
                    1 => a!(self, punpckhwd(xmm0, xmm1)),
                    2 => a!(self, punpckhdq(xmm0, xmm1)),
                    _ => a!(self, punpckhqdq(xmm0, xmm1)),
                },
                0b001 => match size {
                    0 => {
                        a!(self, pcmpeqd(xmm2, xmm2));
                        a!(self, psrlw(xmm2, 8));
                        a!(self, pand(xmm0, xmm2));
                        a!(self, pand(xmm1, xmm2));
                        a!(self, packuswb(xmm0, xmm1));
                    }
                    1 => {
                        a!(self, pslld(xmm0, 16));
                        a!(self, psrad(xmm0, 16));
                        a!(self, pslld(xmm1, 16));
                        a!(self, psrad(xmm1, 16));
                        a!(self, packssdw(xmm0, xmm1));
                    }
                    2 => a!(self, shufps(xmm0, xmm1, 0x88)),
                    _ => a!(self, punpcklqdq(xmm0, xmm1)),
                },
                _ => match size {
                    0 => {
                        a!(self, psrlw(xmm0, 8));
                        a!(self, psrlw(xmm1, 8));
                        a!(self, packuswb(xmm0, xmm1));
                    }
                    1 => {
                        a!(self, psrad(xmm0, 16));
                        a!(self, psrad(xmm1, 16));
                        a!(self, packssdw(xmm0, xmm1));
                    }
                    2 => a!(self, shufps(xmm0, xmm1, 0xDD)),
                    _ => a!(self, punpckhqdq(xmm0, xmm1)),
                },
            }
            self.vfin(rd, true);
            return true;
        }

        // ---- MOVI / MVNI (modified immediate, plain forms) ----
        if insn & 0x9FF8_0C00 == 0x0F00_0400 {
            let op = bit(insn, 29);
            let cmode = bits(insn, 15, 12);
            let plain = cmode & 0b1001 == 0 || cmode & 0b1101 == 0b1000 || cmode & 0b1110 == 0b1100 || cmode == 0b1110;
            if !plain {
                return false;
            }
            let imm8 = (bits(insn, 18, 16) << 5) | bits(insn, 9, 5);
            let mut imm = attesor_core::interp::simd::adv_simd_expand_imm(op, cmode, imm8);
            if op == 1 && cmode < 0b1110 {
                imm = !imm;
            }
            self.mov_imm(rax, imm);
            self.vst_rax(rd, q);
            return true;
        }

        // ---- DUP (element / general), UMOV, SMOV, INS ----
        if insn & 0xBFE0_FC00 == 0x0E00_0400 {
            // DUP (element)
            let Some((size, idx)) = Self::elem_size(bits(insn, 20, 16)) else { return false };
            if size == 3 && !q {
                return false;
            }
            self.velem_ld(rn, size, idx);
            self.replicate_rax(size);
            self.vst_rax(rd, q);
            return true;
        }
        if insn & 0xBFE0_FC00 == 0x0E00_0C00 {
            // DUP (general)
            let Some((size, _)) = Self::elem_size(bits(insn, 20, 16)) else { return false };
            if size == 3 && !q {
                return false;
            }
            self.ldx(rax, rn, false);
            self.replicate_rax(size);
            self.vst_rax(rd, q);
            return true;
        }
        if insn & 0xFFE0_FC00 == 0x5E00_0400 {
            // DUP (scalar, element)
            let Some((size, idx)) = Self::elem_size(bits(insn, 20, 16)) else { return false };
            self.velem_ld(rn, size, idx);
            self.vst_rax(rd, false);
            return true;
        }
        if insn & 0xBFE0_FC00 == 0x0E00_3C00 {
            // UMOV
            let Some((size, idx)) = Self::elem_size(bits(insn, 20, 16)) else { return false };
            if (size == 3) != q {
                return false;
            }
            self.velem_ld(rn, size, idx);
            self.stx(rd, rax, false);
            return true;
        }
        if insn & 0xBFE0_FC00 == 0x0E00_2C00 {
            // SMOV
            let Some((size, idx)) = Self::elem_size(bits(insn, 20, 16)) else { return false };
            if size == 3 || (size == 2 && !q) {
                return false;
            }
            let o = voff(rn) + (idx << size) as i32;
            match (size, q) {
                (0, false) => { a!(self, movsx(eax, byte_ptr(r15 + o))); }
                (0, true) => { a!(self, movsx(rax, byte_ptr(r15 + o))); }
                (1, false) => { a!(self, movsx(eax, word_ptr(r15 + o))); }
                (1, true) => { a!(self, movsx(rax, word_ptr(r15 + o))); }
                _ => { a!(self, movsxd(rax, dword_ptr(r15 + o))); }
            }
            self.stx(rd, rax, false);
            return true;
        }
        if insn & 0xFFE0_FC00 == 0x4E00_1C00 {
            // INS (general)
            let Some((size, idx)) = Self::elem_size(bits(insn, 20, 16)) else { return false };
            self.ldx(rax, rn, false);
            let o = voff(rd) + (idx << size) as i32;
            match size {
                0 => a!(self, mov(byte_ptr(r15 + o), al)),
                1 => a!(self, mov(word_ptr(r15 + o), ax)),
                2 => a!(self, mov(dword_ptr(r15 + o), eax)),
                _ => a!(self, mov(qword_ptr(r15 + o), rax)),
            }
            return true;
        }
        if insn & 0xFFE0_8400 == 0x6E00_0400 {
            // INS (element)
            let Some((size, didx)) = Self::elem_size(bits(insn, 20, 16)) else { return false };
            let sidx = bits(insn, 14, 11) >> size;
            self.velem_ld(rn, size, sidx);
            let o = voff(rd) + (didx << size) as i32;
            match size {
                0 => a!(self, mov(byte_ptr(r15 + o), al)),
                1 => a!(self, mov(word_ptr(r15 + o), ax)),
                2 => a!(self, mov(dword_ptr(r15 + o), eax)),
                _ => a!(self, mov(qword_ptr(r15 + o), rax)),
            }
            return true;
        }

        // ---- Across lanes: UMINV / UMAXV on bytes ----
        if insn & 0x9F3E_0C00 == 0x0E30_0800 && bits(insn, 23, 22) == 0 && bit(insn, 29) == 1 {
            let opc = bits(insn, 16, 12);
            if opc != 0b11010 && opc != 0b01010 {
                return false;
            }
            self.vld(xmm0, rn, q);
            let shifts: &[i32] = if q { &[8, 4, 2, 1] } else { &[4, 2, 1] };
            for &sh in shifts {
                a!(self, movdqa(xmm1, xmm0));
                a!(self, psrldq(xmm1, sh));
                if opc == 0b11010 {
                    a!(self, pminub(xmm0, xmm1));
                } else {
                    a!(self, pmaxub(xmm0, xmm1));
                }
            }
            a!(self, movd(eax, xmm0));
            a!(self, movzx(eax, al));
            self.vst_rax(rd, false);
            return true;
        }

        // ---- LD1 / ST1 (multiple structures, 1-4 registers, optional post-index) ----
        if insn & 0xBFFF_0000 == 0x0C00_0000 || insn & 0xBFA0_0000 == 0x0C80_0000 {
            let post = bit(insn, 23) != 0;
            let load = bit(insn, 22) != 0;
            let nregs = match bits(insn, 15, 12) {
                0b0111 => 1,
                0b1010 => 2,
                0b0110 => 3,
                0b0010 => 4,
                _ => return false,
            };
            if !post && bits(insn, 20, 16) != 0 {
                return false;
            }
            let chunk = if q { 16 } else { 8 };
            self.ldx(rax, rn, true);
            self.commpage_fix();
            for i in 0..nregs {
                let reg = (rd + i) & 31;
                let disp = (i * chunk) as i32;
                if load {
                    if q {
                        a!(self, movdqu(xmm0, xmmword_ptr(rax + disp)));
                    } else {
                        a!(self, movq(xmm0, qword_ptr(rax + disp)));
                    }
                    a!(self, movdqu(xmmword_ptr(r15 + voff(reg)), xmm0));
                } else {
                    a!(self, movdqu(xmm0, xmmword_ptr(r15 + voff(reg))));
                    if q {
                        a!(self, movdqu(xmmword_ptr(rax + disp), xmm0));
                    } else {
                        a!(self, movq(qword_ptr(rax + disp), xmm0));
                    }
                }
            }
            if post {
                self.ldx(rcx, rn, true);
                if rm == 31 {
                    a!(self, add(rcx, (nregs * chunk) as i32));
                } else {
                    self.ldx(rdx, rm, false);
                    a!(self, add(rcx, rdx));
                }
                self.stx(rn, rcx, true);
            }
            return true;
        }

        // ---- scalar floating point ----
        let ftype = bits(insn, 23, 22);
        // FMOV to/from general registers
        if insn & 0x7F3E_FC00 == 0x1E26_0000 {
            let sf = bit(insn, 31) != 0;
            let to_fp = bit(insn, 16) != 0;
            let ok = (sf && ftype == 1) || (!sf && ftype == 0);
            if !ok || bits(insn, 20, 19) != 0 {
                return false;
            }
            if to_fp {
                self.ld(rax, rn, false, sf);
                self.vst_rax(rd, false);
            } else {
                if sf {
                    a!(self, mov(rax, qword_ptr(r15 + voff(rn))));
                } else {
                    a!(self, mov(eax, dword_ptr(r15 + voff(rn))));
                }
                self.stx(rd, rax, false);
            }
            return true;
        }
        // Scalar FRINT*, single/double; half precision keeps the interpreter.
        if insn & 0xFF20_7C00 == 0x1E20_4000 && ftype <= 1 {
            let opcode = bits(insn, 20, 15);
            let mode = match opcode {
                8..=12 => Some(opcode - 8),
                14 | 15 => Some(5), // FPCR rounding
                16 | 18 => Some(3),
                17 | 19 => Some(5),
                _ => None,
            };
            if let Some(mode) = mode {
                let double = ftype == 1;
                if double { a!(self, movq(xmm0, qword_ptr(r15 + voff(rn)))); }
                else { a!(self, movss(xmm0, dword_ptr(r15 + voff(rn)))); }
                self.frint_round(double, mode);
                if opcode >= 16 {
                    self.frint_limit(double, if opcode < 18 { 32 } else { 64 });
                }
                self.vfin(rd, true);
                return true;
            }
        }
        // FMOV (register), FABS, FNEG. Opcode 3 is FSQRT: leave it to its translation below (returning
        // false here used to send every FSQRT to the interpreter).
        if insn & 0xFF3E_7C00 == 0x1E20_4000 && ftype <= 1 && bits(insn, 16, 15) <= 2 {
            let opc = bits(insn, 16, 15);
            let double = ftype == 1;
            if double {
                a!(self, mov(rax, qword_ptr(r15 + voff(rn))));
                match opc {
                    1 => a!(self, btr(rax, 63)),
                    2 => a!(self, btc(rax, 63)),
                    _ => {}
                }
            } else {
                a!(self, mov(eax, dword_ptr(r15 + voff(rn))));
                match opc {
                    1 => a!(self, btr(eax, 31)),
                    2 => a!(self, btc(eax, 31)),
                    _ => {}
                }
            }
            self.vst_rax(rd, false);
            return true;
        }
        // FMOV (immediate)
        if insn & 0xFF20_1FE0 == 0x1E20_1000 && ftype <= 1 {
            let imm8 = bits(insn, 20, 13);
            let d = vfp_expand_imm(imm8);
            let bitsv = if ftype == 1 { d } else { (f64::from_bits(d) as f32).to_bits() as u64 };
            self.mov_imm(rax, bitsv);
            self.vst_rax(rd, false);
            return true;
        }
        // FADD/FSUB/FMUL/FDIV
        if insn & 0xFF20_0C00 == 0x1E20_0800 && ftype <= 1 {
            let opcode = bits(insn, 15, 12);
            if opcode > 3 {
                return false;
            }
            let double = ftype == 1;
            let cm = self.vsrc(rm);
            self.fld_scalar(xmm0, rn, double);
            if double {
                if let Some(c) = cm {
                    match opcode {
                        0 => a!(self, mulsd(xmm0, c)),
                        1 => a!(self, divsd(xmm0, c)),
                        2 => a!(self, addsd(xmm0, c)),
                        _ => a!(self, subsd(xmm0, c)),
                    }
                } else {
                    match opcode {
                        0 => a!(self, mulsd(xmm0, qword_ptr(r15 + voff(rm)))),
                        1 => a!(self, divsd(xmm0, qword_ptr(r15 + voff(rm)))),
                        2 => a!(self, addsd(xmm0, qword_ptr(r15 + voff(rm)))),
                        _ => a!(self, subsd(xmm0, qword_ptr(r15 + voff(rm)))),
                    }
                }
            } else if let Some(c) = cm {
                match opcode {
                    0 => a!(self, mulss(xmm0, c)),
                    1 => a!(self, divss(xmm0, c)),
                    2 => a!(self, addss(xmm0, c)),
                    _ => a!(self, subss(xmm0, c)),
                }
            } else {
                match opcode {
                    0 => a!(self, mulss(xmm0, dword_ptr(r15 + voff(rm)))),
                    1 => a!(self, divss(xmm0, dword_ptr(r15 + voff(rm)))),
                    2 => a!(self, addss(xmm0, dword_ptr(r15 + voff(rm)))),
                    _ => a!(self, subss(xmm0, dword_ptr(r15 + voff(rm)))),
                }
            }
            self.fix_default_nan(double, rn, Some(rm));
            a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            self.vfill(rd, xmm0);
            return true;
        }
        // FSQRT
        if insn & 0xFF3F_FC00 == 0x1E21_C000 && ftype <= 1 {
            let double = ftype == 1;
            self.fld_scalar(xmm0, rn, double);
            if double {
                a!(self, sqrtsd(xmm0, xmm0));
            } else {
                a!(self, sqrtss(xmm0, xmm0));
            }
            self.fix_default_nan(double, rn, None);
            a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            self.vfill(rd, xmm0);
            return true;
        }
        // SCVTF / UCVTF (scalar, integer source)
        if insn & 0x7F3F_FC00 == 0x1E22_0000 || insn & 0x7F3F_FC00 == 0x1E23_0000 {
            if ftype > 1 {
                return false;
            }
            let sf = bit(insn, 31) != 0;
            let unsigned = bit(insn, 16) != 0;
            let double = ftype == 1;
            self.ld(rax, rn, false, sf);
            self.int_to_fp_reg(unsigned, sf, double);
            // The convert leaves the upper lanes of xmm0 undefined; keep only the scalar.
            if double {
                a!(self, movq(xmm0, xmm0));
            } else {
                a!(self, movd(eax, xmm0));
                a!(self, movd(xmm0, eax));
            }
            a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            return true;
        }
        // FCMP / FCMPE (register and zero forms)
        if insn & 0xFF20_FC07 == 0x1E20_2000 && ftype <= 1 {
            let with_zero = bit(insn, 3) != 0;
            let double = ftype == 1;
            self.fld_scalar(xmm0, rn, double);
            if with_zero {
                a!(self, xorps(xmm1, xmm1));
            } else {
                self.fld_scalar(xmm1, rm, double);
            }
            if double {
                a!(self, ucomisd(xmm0, xmm1));
            } else {
                a!(self, ucomiss(xmm0, xmm1));
            }
            let mut un = self.new_label();
            let mut lt = self.new_label();
            let mut eq = self.new_label();
            let mut end = self.new_label();
            a!(self, jp(un));
            a!(self, jb(lt));
            a!(self, je(eq));
            // greater: N=0 Z=0 C=1 V=0
            self.set_nzcv_const(0, 0, 1, 0);
            a!(self, jmp(end));
            self.u.a.set_label(&mut lt).unwrap();
            self.set_nzcv_const(1, 0, 0, 0);
            a!(self, jmp(end));
            self.u.a.set_label(&mut eq).unwrap();
            self.set_nzcv_const(0, 1, 1, 0);
            a!(self, jmp(end));
            self.u.a.set_label(&mut un).unwrap();
            self.set_nzcv_const(0, 0, 1, 1);
            self.u.a.set_label(&mut end).unwrap();
            a!(self, nop());
            return true;
        }
        // FCVTZS / FCVTZU (scalar, to integer) with ARM saturation
        if insn & 0x7F3F_FC00 == 0x1E38_0000 || insn & 0x7F3F_FC00 == 0x1E39_0000 {
            if ftype > 1 {
                return false;
            }
            let sf = bit(insn, 31) != 0;
            let unsigned = bit(insn, 16) != 0;
            let double = ftype == 1;
            if unsigned {
                if double { a!(self, movq(xmm0, qword_ptr(r15 + voff(rn)))); }
                else { a!(self, movss(xmm0, dword_ptr(r15 + voff(rn)))); }
                self.fp_to_uint_reg(double, sf);
                self.stx(rd, rax, false);
                return true;
            }
            if double {
                a!(self, movsd_2(xmm0, qword_ptr(r15 + voff(rn))));
                if sf { a!(self, cvttsd2si(rax, xmm0)); } else { a!(self, cvttsd2si(eax, xmm0)); }
            } else {
                a!(self, movss(xmm0, dword_ptr(r15 + voff(rn))));
                if sf { a!(self, cvttss2si(rax, xmm0)); } else { a!(self, cvttss2si(eax, xmm0)); }
            }
            // x86 returns the "integer indefinite" value on overflow/NaN; ARM saturates (NaN -> 0).
            let mut ok = self.new_label();
            let mut nan = self.new_label();
            let mut neg = self.new_label();
            if sf {
                self.mov_imm(rcx, 0x8000_0000_0000_0000);
                a!(self, cmp(rax, rcx));
            } else {
                a!(self, cmp(eax, 0x8000_0000u32 as i32));
            }
            a!(self, jne(ok));
            if double { a!(self, ucomisd(xmm0, xmm0)); } else { a!(self, ucomiss(xmm0, xmm0)); }
            a!(self, jp(nan));
            if double { a!(self, movq(rcx, xmm0)); a!(self, test(rcx, rcx)); } else { a!(self, movd(ecx, xmm0)); a!(self, test(ecx, ecx)); }
            a!(self, js(neg));
            // positive overflow -> max
            if sf {
                self.mov_imm(rax, 0x7FFF_FFFF_FFFF_FFFF);
            } else {
                a!(self, mov(eax, 0x7FFF_FFFF));
            }
            a!(self, jmp(ok));
            self.u.a.set_label(&mut nan).unwrap();
            a!(self, xor(eax, eax));
            self.u.a.set_label(&mut neg).unwrap();
            a!(self, nop());
            self.u.a.set_label(&mut ok).unwrap();
            a!(self, nop());
            self.stx(rd, rax, false);
            return true;
        }
        // FCCMP / FCCMPE
        if insn & 0xFF20_0C00 == 0x1E20_0400 && ftype <= 1 {
            let cond = bits(insn, 15, 12);
            let nzcv = bits(insn, 3, 0);
            let double = ftype == 1;
            let mut cmp_it = self.new_label();
            let mut end = self.new_label();
            self.cond_to_al(cond);
            a!(self, test(al, al));
            a!(self, jnz(cmp_it));
            self.set_nzcv_const((nzcv >> 3) & 1, (nzcv >> 2) & 1, (nzcv >> 1) & 1, nzcv & 1);
            a!(self, jmp(end));
            self.u.a.set_label(&mut cmp_it).unwrap();
            if double {
                a!(self, movsd_2(xmm0, qword_ptr(r15 + voff(rn))));
                a!(self, movsd_2(xmm1, qword_ptr(r15 + voff(rm))));
                a!(self, ucomisd(xmm0, xmm1));
            } else {
                a!(self, movss(xmm0, dword_ptr(r15 + voff(rn))));
                a!(self, movss(xmm1, dword_ptr(r15 + voff(rm))));
                a!(self, ucomiss(xmm0, xmm1));
            }
            let mut un = self.new_label();
            let mut lt = self.new_label();
            let mut eq = self.new_label();
            a!(self, jp(un));
            a!(self, jb(lt));
            a!(self, je(eq));
            self.set_nzcv_const(0, 0, 1, 0);
            a!(self, jmp(end));
            self.u.a.set_label(&mut lt).unwrap();
            self.set_nzcv_const(1, 0, 0, 0);
            a!(self, jmp(end));
            self.u.a.set_label(&mut eq).unwrap();
            self.set_nzcv_const(0, 1, 1, 0);
            a!(self, jmp(end));
            self.u.a.set_label(&mut un).unwrap();
            self.set_nzcv_const(0, 0, 1, 1);
            self.u.a.set_label(&mut end).unwrap();
            a!(self, nop());
            return true;
        }
        // FCSEL
        if insn & 0xFF20_0C00 == 0x1E20_0C00 && ftype <= 1 {
            let cond = bits(insn, 15, 12);
            let double = ftype == 1;
            self.cond_to_al(cond);
            let mut use_m = self.new_label();
            let mut end = self.new_label();
            a!(self, test(al, al));
            a!(self, jz(use_m));
            if double { a!(self, mov(rax, qword_ptr(r15 + voff(rn)))); } else { a!(self, mov(eax, dword_ptr(r15 + voff(rn)))); }
            a!(self, jmp(end));
            self.u.a.set_label(&mut use_m).unwrap();
            if double { a!(self, mov(rax, qword_ptr(r15 + voff(rm)))); } else { a!(self, mov(eax, dword_ptr(r15 + voff(rm)))); }
            self.u.a.set_label(&mut end).unwrap();
            a!(self, nop());
            self.vst_rax(rd, false);
            return true;
        }
        false
    }

    fn set_nzcv_const(&mut self, n: u32, z: u32, c: u32, v: u32) {
        a!(self, mov(byte_ptr(r15 + off::NF), n));
        a!(self, mov(byte_ptr(r15 + off::ZF), z));
        a!(self, mov(byte_ptr(r15 + off::CF), c));
        a!(self, mov(byte_ptr(r15 + off::VF), v));
    }
}


// ---- second batch: Skia/pixel-pipeline NEON ----

impl<'a> E<'a> {
    /// Shift each element of xmm0 right (logical) by `n` for element size `size` (0=b..3=d).
    fn psrl_elem(&mut self, x: AsmRegisterXmm, size: u32, n: u32) {
        match size {
            0 => {
                a!(self, psrlw(x, n as i32));
                self.mov_imm(rax, 0x0101_0101_0101_0101u64 * (0xffu64 >> n));
                a!(self, movq(xmm3, rax));
                a!(self, punpcklqdq(xmm3, xmm3));
                a!(self, pand(x, xmm3));
            }
            1 => a!(self, psrlw(x, n as i32)),
            2 => a!(self, psrld(x, n as i32)),
            _ => a!(self, psrlq(x, n as i32)),
        }
    }

    fn psll_elem(&mut self, x: AsmRegisterXmm, size: u32, n: u32) {
        match size {
            0 => {
                a!(self, psllw(x, n as i32));
                self.mov_imm(rax, 0x0101_0101_0101_0101u64 * ((0xffu64 << n) & 0xff));
                a!(self, movq(xmm3, rax));
                a!(self, punpcklqdq(xmm3, xmm3));
                a!(self, pand(x, xmm3));
            }
            1 => a!(self, psllw(x, n as i32)),
            2 => a!(self, pslld(x, n as i32)),
            _ => a!(self, psllq(x, n as i32)),
        }
    }

    fn padd_elem(&mut self, x: AsmRegisterXmm, y: AsmRegisterXmm, size: u32) {
        match size {
            0 => a!(self, paddb(x, y)),
            1 => a!(self, paddw(x, y)),
            2 => a!(self, paddd(x, y)),
            _ => a!(self, paddq(x, y)),
        }
    }

    /// Load a 64-bit constant into all lanes of `x`.
    /// xmm0 = xmm0 * xmm1 per byte (low 8 bits of each product): SSE has no byte multiply, so multiply
    /// even and odd bytes as 16-bit lanes. Uses xmm2-xmm3.
    fn pmul_bytes(&mut self) {
        a!(self, movdqa(xmm2, xmm0));
        a!(self, movdqa(xmm3, xmm1));
        a!(self, pmullw(xmm0, xmm1));          // even bytes in the low halves of the words
        a!(self, psrlw(xmm2, 8));
        a!(self, psrlw(xmm3, 8));
        a!(self, pmullw(xmm2, xmm3));          // odd bytes
        a!(self, psllw(xmm2, 8));
        a!(self, psllw(xmm0, 8));
        a!(self, psrlw(xmm0, 8));
        a!(self, por(xmm0, xmm2));
    }

    /// Deinterleave xmm0:xmm1 (elements of `size`): xmm0 = even elements, xmm1 = odd elements
    /// (UZP1 / UZP2 of the pair). Uses xmm2-xmm3.
    fn uzp128(&mut self, size: u32) {
        a!(self, movdqa(xmm2, xmm0));
        a!(self, movdqa(xmm3, xmm1));
        match size {
            0 => {
                a!(self, pcmpeqd(xmm1, xmm1));
                a!(self, psrlw(xmm1, 8));
                a!(self, pand(xmm0, xmm1));
                a!(self, pand(xmm1, xmm3));
                a!(self, packuswb(xmm0, xmm1));    // evens
                a!(self, psrlw(xmm2, 8));
                a!(self, psrlw(xmm3, 8));
                a!(self, packuswb(xmm2, xmm3));    // odds
                a!(self, movdqa(xmm1, xmm2));
            }
            1 => {
                a!(self, pslld(xmm0, 16));
                a!(self, psrad(xmm0, 16));
                a!(self, pslld(xmm1, 16));
                a!(self, psrad(xmm1, 16));
                a!(self, packssdw(xmm0, xmm1));    // evens (sign-extended, so the pack is lossless)
                a!(self, psrad(xmm2, 16));
                a!(self, psrad(xmm3, 16));
                a!(self, packssdw(xmm2, xmm3));    // odds
                a!(self, movdqa(xmm1, xmm2));
            }
            2 => {
                a!(self, shufps(xmm0, xmm1, 0x88));
                a!(self, shufps(xmm2, xmm3, 0xDD));
                a!(self, movdqa(xmm1, xmm2));
            }
            _ => {
                a!(self, punpcklqdq(xmm0, xmm1));
                a!(self, punpckhqdq(xmm2, xmm3));
                a!(self, movdqa(xmm1, xmm2));
            }
        }
    }

    /// 128-bit constant (low and high halves).
    fn vconst128(&mut self, x: AsmRegisterXmm, lo: u64, hi: u64) {
        self.mov_imm(rax, lo);
        a!(self, movq(x, rax));
        self.mov_imm(rax, hi);
        a!(self, pinsrq(x, rax, 1));
    }

    fn vconst(&mut self, x: AsmRegisterXmm, v: u64) {
        let mem = self.vconstant(v);
        a!(self, movdqu(x, mem));
    }

    fn vconstant(&mut self, v: u64) -> AsmMemoryOperand {
        let label = if let Some(&label) = self.u.constants.get(&v) { label } else {
            let label = self.u.a.create_label();
            self.u.constants.insert(v, label);
            self.u.constant_pool.push((label, v));
            label
        };
        xmmword_ptr(label)
    }

    /// FCVTZU: saturating truncation of XMM0 into unsigned RAX.
    fn fp_to_uint_reg(&mut self, double: bool, wide: bool) {
        let mut zero = self.new_label();
        let mut max = self.new_label();
        let mut done = self.new_label();
        a!(self, pxor(xmm1, xmm1));
        if double { a!(self, ucomisd(xmm0, xmm1)); }
        else { a!(self, ucomiss(xmm0, xmm1)); }
        a!(self, jbe(zero)); // negative, zero or unordered
        let lim = 2f64.powi(if wide { 64 } else { 32 });
        self.vconst(xmm1, if double { lim.to_bits() } else { (lim as f32).to_bits() as u64 });
        if double { a!(self, ucomisd(xmm0, xmm1)); }
        else { a!(self, ucomiss(xmm0, xmm1)); }
        a!(self, jae(max));
        if wide {
            let mut small = self.new_label();
            let half = 2f64.powi(63);
            self.vconst(xmm1, if double { half.to_bits() } else { (half as f32).to_bits() as u64 });
            if double { a!(self, ucomisd(xmm0, xmm1)); }
            else { a!(self, ucomiss(xmm0, xmm1)); }
            a!(self, jb(small));
            if double {
                a!(self, subsd(xmm0, xmm1));
                a!(self, cvttsd2si(rax, xmm0));
            } else {
                a!(self, subss(xmm0, xmm1));
                a!(self, cvttss2si(rax, xmm0));
            }
            a!(self, bts(rax, 63));
            a!(self, jmp(done));
            self.u.a.set_label(&mut small).unwrap();
        }
        // A 64-bit signed convert covers the entire uint32 range as well.
        if double { a!(self, cvttsd2si(rax, xmm0)); }
        else { a!(self, cvttss2si(rax, xmm0)); }
        a!(self, jmp(done));
        self.u.a.set_label(&mut zero).unwrap();
        a!(self, xor(eax, eax));
        a!(self, jmp(done));
        self.u.a.set_label(&mut max).unwrap();
        if wide { self.mov_imm(rax, u64::MAX); }
        else { a!(self, mov(eax, u32::MAX)); }
        self.u.a.set_label(&mut done).unwrap();
        a!(self, nop());
    }

    /// Convert an integer in RAX to the low FP lane of XMM0.
    fn int_to_fp_reg(&mut self, unsigned: bool, sf: bool, double: bool) {
        if unsigned && sf {
            let mut neg = self.new_label();
            let mut done = self.new_label();
            a!(self, test(rax, rax));
            a!(self, js(neg));
            if double { a!(self, cvtsi2sd(xmm0, rax)); } else { a!(self, cvtsi2ss(xmm0, rax)); }
            a!(self, jmp(done));
            self.u.a.set_label(&mut neg).unwrap();
            a!(self, mov(rcx, rax));
            a!(self, shr(rax, 1));
            a!(self, and(ecx, 1));
            a!(self, or(rax, rcx));
            if double {
                a!(self, cvtsi2sd(xmm0, rax));
                a!(self, addsd(xmm0, xmm0));
            } else {
                a!(self, cvtsi2ss(xmm0, rax));
                a!(self, addss(xmm0, xmm0));
            }
            self.u.a.set_label(&mut done).unwrap();
            a!(self, nop());
        } else {
            // 32-bit unsigned sources were zero-extended, so a 64-bit signed convert is exact.
            let wide = sf || unsigned;
            if double {
                if wide { a!(self, cvtsi2sd(xmm0, rax)); } else { a!(self, cvtsi2sd(xmm0, eax)); }
            } else if wide {
                a!(self, cvtsi2ss(xmm0, rax));
            } else {
                a!(self, cvtsi2ss(xmm0, eax));
            }
        }
    }

    /// ARM rounding modes 0=N, 1=P, 2=M, 3=Z, 4=A, 5=FPCR.
    fn frint_round(&mut self, double: bool, mode: u32) {
        if mode == 5 {
            let mut labels = [self.new_label(), self.new_label(), self.new_label()];
            let mut end = self.new_label();
            a!(self, mov(eax, dword_ptr(r15 + off::FPCR)));
            a!(self, shr(eax, 22));
            a!(self, and(eax, 3));
            for m in 0..3 {
                a!(self, cmp(eax, m as i32));
                a!(self, je(labels[m]));
            }
            self.frint_round(double, 3);
            a!(self, jmp(end));
            for m in 0..3 {
                self.u.a.set_label(&mut labels[m]).unwrap();
                self.frint_round(double, m as u32);
                a!(self, jmp(end));
            }
            self.u.a.set_label(&mut end).unwrap();
            a!(self, nop());
        } else if mode == 4 {
            // Work on magnitudes: trunc(x) + (fraction >= 0.5), then
            // restore the original sign, including negative zero and NaNs.
            self.vconst(xmm4, if double { 0x8000_0000_0000_0000 } else { 0x8000_0000_8000_0000 });
            a!(self, movdqa(xmm3, xmm4));
            a!(self, pand(xmm3, xmm0));
            a!(self, pandn(xmm4, xmm0));
            a!(self, movdqa(xmm0, xmm4));
            if double {
                a!(self, roundpd(xmm0, xmm0, 11));
                a!(self, subpd(xmm4, xmm0));
            } else {
                a!(self, roundps(xmm0, xmm0, 11));
                a!(self, subps(xmm4, xmm0));
            }
            self.vconst(xmm2, if double { 0x3FE0_0000_0000_0000 } else { 0x3F00_0000_3F00_0000 });
            if double { a!(self, cmppd(xmm2, xmm4, 2)); }
            else { a!(self, cmpps(xmm2, xmm4, 2)); }
            self.vconst(xmm1, if double { 0x3FF0_0000_0000_0000 } else { 0x3F80_0000_3F80_0000 });
            a!(self, pand(xmm1, xmm2));
            if double { a!(self, addpd(xmm0, xmm1)); }
            else { a!(self, addps(xmm0, xmm1)); }
            a!(self, por(xmm0, xmm3));
        } else {
            let imm = [0, 2, 1, 3][mode as usize] | 8;
            if double { a!(self, roundpd(xmm0, xmm0, imm)); }
            else { a!(self, roundps(xmm0, xmm0, imm)); }
        }
    }

    /// FRINT32/64 return -2^(n-1) for NaN, infinity or rounded overflow.
    fn frint_limit(&mut self, double: bool, n: i32) {
        let lim = 2f64.powi(n - 1);
        self.vconst(xmm1, if double { lim.to_bits() } else { (lim as f32).to_bits() as u64 * 0x1_0000_0001 });
        self.vconst(xmm2, if double { (-lim).to_bits() } else { (-lim as f32).to_bits() as u64 * 0x1_0000_0001 });
        a!(self, movdqa(xmm3, xmm0));
        a!(self, movdqa(xmm4, xmm2));
        if double {
            a!(self, cmppd(xmm3, xmm1, 1)); // r < lim (ordered)
            a!(self, cmppd(xmm4, xmm0, 2)); // -lim <= r
        } else {
            a!(self, cmpps(xmm3, xmm1, 1));
            a!(self, cmpps(xmm4, xmm0, 2));
        }
        a!(self, pand(xmm3, xmm4));
        a!(self, pand(xmm0, xmm3));
        a!(self, pandn(xmm3, xmm2));
        a!(self, por(xmm0, xmm3));
    }

    fn simd_more(&mut self, insn: u32) -> bool {
        let rd = bits(insn, 4, 0);
        let rn = bits(insn, 9, 5);
        let rm = bits(insn, 20, 16);
        let q = bit(insn, 30) != 0;
        let u = bit(insn, 29) != 0;
        let size = bits(insn, 23, 22);

        // ---- pairwise: ADDP (vector), [SU]MAXP, [SU]MINP; ADDP (scalar, 2d) ----
        if insn & 0xFFFF_FC00 == 0x5EF1_B800 && !jit_off("pairwise") {
            self.vld(xmm0, rn, true);
            a!(self, pshufd(xmm1, xmm0, 0x0E));
            a!(self, paddq(xmm0, xmm1));
            a!(self, movq(rax, xmm0));
            self.vst_rax(rd, false);
            return true;
        }
        if insn & 0x9F20_0400 == 0x0E20_0400 && !jit_off("pairwise") {
            let opcode = bits(insn, 15, 11);
            let op = match (u, opcode) {
                (false, 0b10111) => Some(0), // ADDP
                (_, 0b10100) => Some(1),     // [SU]MAXP
                (_, 0b10101) => Some(2),     // [SU]MINP
                _ => None,
            };
            if let Some(op) = op {
                // ADDP covers 64-bit lanes (128-bit form only); MAXP/MINP stop at 32-bit lanes
                if (op == 0 || size < 3) && !(size == 3 && !q) {
                    if q {
                        self.vld(xmm0, rn, true);
                        self.vld(xmm1, rm, true);
                    } else {
                        a!(self, movq(xmm0, qword_ptr(r15 + voff(rn))));
                        a!(self, movq(xmm1, qword_ptr(r15 + voff(rm))));
                        a!(self, punpcklqdq(xmm0, xmm1));
                        a!(self, movdqa(xmm1, xmm0));
                    }
                    self.uzp128(size);
                    match (op, u, size) {
                        (0, _, s) => self.padd_elem(xmm0, xmm1, s),
                        (1, true, 0) => a!(self, pmaxub(xmm0, xmm1)),
                        (1, true, 1) => a!(self, pmaxuw(xmm0, xmm1)),
                        (1, true, _) => a!(self, pmaxud(xmm0, xmm1)),
                        (1, false, 0) => a!(self, pmaxsb(xmm0, xmm1)),
                        (1, false, 1) => a!(self, pmaxsw(xmm0, xmm1)),
                        (1, false, _) => a!(self, pmaxsd(xmm0, xmm1)),
                        (_, true, 0) => a!(self, pminub(xmm0, xmm1)),
                        (_, true, 1) => a!(self, pminuw(xmm0, xmm1)),
                        (_, true, _) => a!(self, pminud(xmm0, xmm1)),
                        (_, false, 0) => a!(self, pminsb(xmm0, xmm1)),
                        (_, false, 1) => a!(self, pminsw(xmm0, xmm1)),
                        (_, false, _) => a!(self, pminsd(xmm0, xmm1)),
                    }
                    self.vfin(rd, q);
                    return true;
                }
            }
        }

        // ---- across lanes: ADDV, [SU]MAXV, [SU]MINV, [SU]ADDLV (b/h/s) ----
        // Fold the upper half onto the lower one until one element is left.
        if insn & 0x9F3E_0C00 == 0x0E30_0800 && size < 3 && !(size == 2 && !q) && !jit_off("acrosslanes") {
            let opcode = bits(insn, 16, 12);
            let (op, long) = match opcode {
                0b11011 if !u => (0, false), // ADDV
                0b00011 => (0, true),        // [SU]ADDLV
                0b01010 => (1, false),       // [SU]MAXV
                0b11010 => (2, false),       // [SU]MINV
                _ => (9, false),
            };
            if op != 9 {
                let bytes = if q { 16 } else { 8 };
                if q { self.vld(xmm0, rn, true); } else { a!(self, movq(xmm0, qword_ptr(r15 + voff(rn)))); }
                let mut wide = size; // element size of the accumulation
                if long || op == 0 {
                    if size == 0 && (op == 0 || u) {
                        // bytes: PSADBW sums eight unsigned bytes per qword (signed: bias by 0x80)
                        if long && !u { self.vconst(xmm1, 0x8080_8080_8080_8080); a!(self, pxor(xmm0, xmm1)); }
                        a!(self, pxor(xmm1, xmm1));
                        a!(self, psadbw(xmm0, xmm1));
                        if q { a!(self, pshufd(xmm1, xmm0, 0x0E)); a!(self, paddq(xmm0, xmm1)); }
                        a!(self, movq(rax, xmm0));
                        if long && !u { a!(self, sub(rax, 128 * bytes as i32)); }
                        let mask: u64 = if long { 0xFFFF } else { 0xFF };
                        self.mov_imm(rcx, mask);
                        a!(self, and(rax, rcx));
                        self.vst_rax(rd, false);
                        return true;
                    }
                    if size == 0 {
                        return false; // SADDLV/... on bytes handled above; nothing else left
                    }
                    if long {
                        // widen to 2*esize lanes, then add
                        a!(self, movdqa(xmm1, xmm0));
                        if q { a!(self, psrldq(xmm1, 8)); }
                        match (u, size) {
                            (true, 1) => { a!(self, pmovzxwd(xmm0, xmm0)); a!(self, pmovzxwd(xmm1, xmm1)); }
                            (false, 1) => { a!(self, pmovsxwd(xmm0, xmm0)); a!(self, pmovsxwd(xmm1, xmm1)); }
                            (true, _) => { a!(self, pmovzxdq(xmm0, xmm0)); a!(self, pmovzxdq(xmm1, xmm1)); }
                            (false, _) => { a!(self, pmovsxdq(xmm0, xmm0)); a!(self, pmovsxdq(xmm1, xmm1)); }
                        }
                        if q { self.padd_elem(xmm0, xmm1, size + 1); }
                        wide = size + 1;
                    }
                }
                // fold: lanes of `wide` bits, `n` of them live
                // widening keeps all elements of a 64-bit source; a 128-bit one was widened in halves and added
                let lanes = if long && q { (bytes >> size) / 2 } else { bytes >> size };
                let mut n = lanes;
                while n > 1 {
                    let half_bytes = (n / 2) << wide;
                    a!(self, movdqa(xmm1, xmm0));
                    a!(self, psrldq(xmm1, half_bytes as u32));
                    match (op, u, wide) {
                        (0, _, w) => self.padd_elem(xmm0, xmm1, w),
                        (1, true, 0) => a!(self, pmaxub(xmm0, xmm1)),
                        (1, true, 1) => a!(self, pmaxuw(xmm0, xmm1)),
                        (1, true, _) => a!(self, pmaxud(xmm0, xmm1)),
                        (1, false, 0) => a!(self, pmaxsb(xmm0, xmm1)),
                        (1, false, 1) => a!(self, pmaxsw(xmm0, xmm1)),
                        (1, false, _) => a!(self, pmaxsd(xmm0, xmm1)),
                        (_, true, 0) => a!(self, pminub(xmm0, xmm1)),
                        (_, true, 1) => a!(self, pminuw(xmm0, xmm1)),
                        (_, true, _) => a!(self, pminud(xmm0, xmm1)),
                        (_, false, 0) => a!(self, pminsb(xmm0, xmm1)),
                        (_, false, 1) => a!(self, pminsw(xmm0, xmm1)),
                        (_, false, _) => a!(self, pminsd(xmm0, xmm1)),
                    }
                    n /= 2;
                }
                a!(self, movq(rax, xmm0));
                let mask: u64 = match wide { 0 => 0xFF, 1 => 0xFFFF, 2 => 0xFFFF_FFFF, _ => u64::MAX };
                self.mov_imm(rcx, mask);
                a!(self, and(rax, rcx));
                self.vst_rax(rd, false);
                return true;
            }
        }

        // ---- compare with zero: CMGT/CMEQ/CMLT #0 (U=0), CMGE/CMLE #0 (U=1), b/h/s ----
        if insn & 0x9F3F_CC00 == 0x0E20_8800 && size < 3 && !jit_off("cmzero") {
            let opc = bits(insn, 13, 12); // 00 GT/GE, 01 EQ/LE, 10 LT (U=0 only)
            if !(u && opc == 2) && opc != 3 {
                self.vld(xmm0, rn, q);
                a!(self, pxor(xmm1, xmm1));
                let gt = |e: &mut Self, a: AsmRegisterXmm, b: AsmRegisterXmm| match size {
                    0 => a!(e, pcmpgtb(a, b)),
                    1 => a!(e, pcmpgtw(a, b)),
                    _ => a!(e, pcmpgtd(a, b)),
                };
                let eq = |e: &mut Self, a: AsmRegisterXmm, b: AsmRegisterXmm| match size {
                    0 => a!(e, pcmpeqb(a, b)),
                    1 => a!(e, pcmpeqw(a, b)),
                    _ => a!(e, pcmpeqd(a, b)),
                };
                match (u, opc) {
                    (false, 0) => gt(self, xmm0, xmm1),                     // x > 0
                    (false, 1) => eq(self, xmm0, xmm1),                     // x == 0
                    (false, _) => { gt(self, xmm1, xmm0); a!(self, movdqa(xmm0, xmm1)); } // 0 > x
                    (true, 0) => {                                          // x >= 0 = !(0 > x)
                        gt(self, xmm1, xmm0);
                        a!(self, pcmpeqd(xmm0, xmm0));
                        a!(self, pxor(xmm0, xmm1));
                    }
                    (true, _) => {                                          // x <= 0 = !(x > 0)
                        gt(self, xmm0, xmm1);
                        a!(self, pcmpeqd(xmm1, xmm1));
                        a!(self, pxor(xmm0, xmm1));
                    }
                }
                self.vfin(rd, q);
                return true;
            }
        }

        // ---- MUL / MLA / MLS by element (16- and 32-bit lanes) ----
        if insn & 0x9F00_0400 == 0x0F00_0000 && (size == 1 || size == 2) && !jit_off("mulelem") {
            let opcode = bits(insn, 15, 12);
            let kind = match (u, opcode) {
                (false, 0b1000) => Some(0), // MUL
                (true, 0b0000) => Some(1),  // MLA
                (true, 0b0100) => Some(2),  // MLS
                _ => None,
            };
            if let Some(kind) = kind {
                let h = bit(insn, 11);
                let l = bit(insn, 21);
                let m = bit(insn, 20);
                let (reg, idx) = if size == 1 { (bits(insn, 19, 16), (h << 2) | (l << 1) | m) } else { (rm, (h << 1) | l) };
                self.vld(xmm1, reg, true);
                if size == 1 {
                    let b0 = (idx * 2) as u64;
                    let pair = b0 | ((b0 + 1) << 8);
                    let w = pair | (pair << 16) | (pair << 32) | (pair << 48);
                    self.vconst(xmm2, w);
                    a!(self, pshufb(xmm1, xmm2));
                } else {
                    a!(self, pshufd(xmm1, xmm1, idx * 0x55));
                }
                self.vld(xmm0, rn, q);
                if size == 1 { a!(self, pmullw(xmm0, xmm1)); } else { a!(self, pmulld(xmm0, xmm1)); }
                if kind != 0 {
                    self.vld(xmm2, rd, q);
                    match (kind, size) {
                        (1, 1) => a!(self, paddw(xmm2, xmm0)),
                        (1, _) => a!(self, paddd(xmm2, xmm0)),
                        (_, 1) => a!(self, psubw(xmm2, xmm0)),
                        _ => a!(self, psubd(xmm2, xmm0)),
                    }
                    a!(self, movdqa(xmm0, xmm2));
                }
                self.vfin(rd, q);
                return true;
            }
        }

        // ---- FMAX/FMIN/FMAXNM/FMINNM: scalar (single/double) and vector three-same ----
        if !jit_off("fmaxmin") {
            let scalar = insn & 0xFF20_CC00 == 0x1E20_4800 && bits(insn, 23, 22) <= 1; // opcode 01xx10
            let vector = insn & 0x9F20_0400 == 0x0E20_0400 && !u && matches!(bits(insn, 15, 11), 0b11000 | 0b11110);
            if scalar || vector {
                let double = bit(insn, 22) == 1;
                let (max, nm) = if scalar {
                    let o = bits(insn, 13, 12); // 00 FMAX, 01 FMIN, 10 FMAXNM, 11 FMINNM
                    (o & 1 == 0, o & 2 != 0)
                } else {
                    (bit(insn, 23) == 0, bits(insn, 15, 11) == 0b11000)
                };
                if !(vector && double && !q) {
                    if scalar {
                        if double {
                            a!(self, movsd_2(xmm0, qword_ptr(r15 + voff(rn))));
                            a!(self, movsd_2(xmm1, qword_ptr(r15 + voff(rm))));
                        } else {
                            a!(self, movss(xmm0, dword_ptr(r15 + voff(rn))));
                            a!(self, movss(xmm1, dword_ptr(r15 + voff(rm))));
                        }
                    } else {
                        self.vld(xmm0, rn, q);
                        self.vld(xmm1, rm, q);
                    }
                    self.fp_maxmin(double, max, nm, scalar, q);
                    if scalar {
                        // upper lanes were zero in both operands; clear whatever the quiet-NaN step set
                        if double { a!(self, movq(xmm0, xmm0)); } else { a!(self, insertps(xmm0, xmm0, 0x0E)); }
                        a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
                    } else {
                        self.vfin(rd, q);
                    }
                    return true;
                }
            }
        }

        // ---- FCVTL/FCVTL2 (half->single needs F16C; single->double), FCVTN/FCVTN2 ----
        if (insn & 0xBFBF_FC00 == 0x0E21_7800 || insn & 0xBFBF_FC00 == 0x0E21_6800) && (bit(insn, 22) == 1 || has_f16c()) && !jit_off("fcvt") {
            let wide = bit(insn, 22) == 1; // single<->double instead of half<->single
            if insn & 0x1000 != 0 {
                // FCVTL: widen the lower (Q=0) or upper (FCVTL2) half
                self.vld(xmm0, rn, true);
                if q {
                    a!(self, psrldq(xmm0, 8));
                }
                if wide {
                    a!(self, cvtps2pd(xmm0, xmm0));
                } else {
                    a!(self, vcvtph2ps(xmm0, xmm0));
                }
                a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            } else {
                // FCVTN: narrow into the lower half (upper zeroed), or the upper half (FCVTN2, lower kept)
                self.vld(xmm0, rn, true);
                if wide {
                    a!(self, cvtpd2ps(xmm0, xmm0));
                } else {
                    a!(self, movdqa(xmm1, xmm0));
                    a!(self, vcvtps2ph(xmm0, xmm0, 4)); // use MXCSR.RC, synchronized with FPCR.RMode
                    self.half_convert_status();
                }
                a!(self, movq(xmm0, xmm0));
                if q {
                    a!(self, pslldq(xmm0, 8));
                    a!(self, movq(xmm1, qword_ptr(r15 + voff(rd))));
                    a!(self, por(xmm0, xmm1));
                }
                a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            }
            return true;
        }

        // ---- FMLA/FMLS/FMUL (vector, by element), single and double ----
        // FMLS negates the multiplicand first (FPNeg, so NaN signs match), then fuses like FMLA.
        if insn & 0x9F80_0400 == 0x0F80_0000 && !jit_off("fpelem") {
            let opcode = bits(insn, 15, 12);
            let double = bit(insn, 22) == 1;
            let h = bit(insn, 11);
            let l = bit(insn, 21);
            let fused = !u && (opcode == 0b0001 || opcode == 0b0101);
            let mul = !u && opcode == 0b1001;
            if (fused && has_fma() || mul) && !(double && (!q || l != 0)) {
                self.vld(xmm1, rm, true);
                if double {
                    a!(self, pshufd(xmm1, xmm1, if h != 0 { 0xEE } else { 0x44 }));
                } else {
                    a!(self, pshufd(xmm1, xmm1, ((h << 1) | l) * 0x55));
                }
                self.vld(xmm0, rn, q);
                if mul {
                    if double { a!(self, mulpd(xmm0, xmm1)); } else { a!(self, mulps(xmm0, xmm1)); }
                } else {
                    if opcode == 0b0101 {
                        self.vneg(xmm0, double);
                    }
                    self.vld(xmm2, rd, q);
                    self.fma_arm(double);
                }
                if mul { self.vfix_binary_nan(double, rn, q); }
                self.vfin(rd, q);
                return true;
            }
        }

        // ---- FMLA/FMLS (vector, three-same) ----
        if insn & 0xBF20_FC00 == 0x0E20_CC00 && has_fma() && !jit_off("fmla") {
            let double = bit(insn, 22) == 1;
            if double && !q {
                return false;
            }
            self.vld(xmm0, rn, q);
            self.vld(xmm1, rm, q);
            if bit(insn, 23) != 0 {
                self.vneg(xmm0, double); // FMLS
            }
            self.vld(xmm2, rd, q);
            self.fma_arm(double);
            self.vfin(rd, q);
            return true;
        }

        // ---- FMADD/FMSUB/FNMADD/FNMSUB (scalar, single/double) ----
        // ARM: FMSUB = a + (-n)m, FNMADD = (-a) + (-n)m, FNMSUB = (-a) + nm, all with one rounding.
        if insn & 0xFF00_0000 == 0x1F00_0000 && bits(insn, 23, 22) <= 1 && has_fma() && !jit_off("fmadd") {
            let double = bit(insn, 22) == 1;
            let o1 = bit(insn, 21) != 0;
            let o0 = bit(insn, 15) != 0;
            let ra = bits(insn, 14, 10);
            self.fld_scalar(xmm0, rn, double);
            self.fld_scalar(xmm1, rm, double);
            self.fld_scalar(xmm2, ra, double);
            if o1 != o0 {
                self.vneg(xmm0, double); // FMSUB, FNMADD: -n
            }
            if o1 {
                self.vneg(xmm2, double); // FNMADD, FNMSUB: -a
            }
            // packed on purpose: the upper lanes are zero (movss/movsd loads) and are cleared again below
            self.fma_arm(double);
            if double { a!(self, movq(xmm0, xmm0)); } else { a!(self, insertps(xmm0, xmm0, 0x0E)); }
            a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            self.vfill(rd, xmm0);
            return true;
        }

        // ---- AESE/AESD/AESMC/AESIMC via AES-NI ----
        // ARM splits a round differently from x86: AESE = ShiftRows+SubBytes of (d ^ n) = AESENCLAST with
        // a zero key; AESD likewise with AESDECLAST; MixColumns alone = AESENC(AESDECLAST(x, 0), 0)
        // (the inverse SubBytes/ShiftRows undoes AESENC's own); AESIMC is InvMixColumns directly.
        // Chromium's BoringSSL assumes the ARMv8 crypto extension on Apple arm64, so this runs on TLS paths.
        if insn & 0xFFFF_CC00 == 0x4E28_4800 && has_aes() && !jit_off("aes") {
            let op = bits(insn, 13, 12); // 0 AESE, 1 AESD, 2 AESMC, 3 AESIMC
            // The usual round pairs (fused on Apple CPUs) are one x86 instruction each:
            // AESE Vd,Vn; AESMC Vd,Vd = AESENC(d ^ n, 0) and AESD Vd,Vn; AESIMC Vd,Vd = AESDEC(d ^ n, 0).
            let next = self.u.look.first().copied();
            let pair = |mc: u32| next == Some(mc | (rd << 5) | rd) && !self.u.labels.contains_key(&(self.pc + 4));
            if (op == 0 && pair(0x4E28_6800)) || (op == 1 && pair(0x4E28_7800)) {
                self.vld(xmm0, rn, true);
                self.vld(xmm2, rd, true);
                a!(self, pxor(xmm0, xmm2));
                a!(self, pxor(xmm1, xmm1));
                if op == 0 { a!(self, aesenc(xmm0, xmm1)); } else { a!(self, aesdec(xmm0, xmm1)); }
                self.vfin(rd, true);
                self.u.skip_next = true;
                return true;
            }
            self.vld(xmm0, rn, true);
            a!(self, pxor(xmm1, xmm1));
            match op {
                0 | 1 => {
                    self.vld(xmm2, rd, true);
                    a!(self, pxor(xmm0, xmm2));
                    if op == 0 { a!(self, aesenclast(xmm0, xmm1)); } else { a!(self, aesdeclast(xmm0, xmm1)); }
                }
                2 => {
                    a!(self, aesdeclast(xmm0, xmm1));
                    a!(self, aesenc(xmm0, xmm1));
                }
                _ => a!(self, aesimc(xmm0, xmm0)),
            }
            self.vfin(rd, true);
            return true;
        }

        // ---- PMULL/PMULL2 (64x64 -> 128 carry-less multiply) via PCLMULQDQ ----
        if insn & 0xBFE0_FC00 == 0x0EE0_E000 && has_pclmul() && !jit_off("pmull") {
            self.vld(xmm0, rn, true);
            self.vld(xmm1, rm, true);
            a!(self, pclmulqdq(xmm0, xmm1, if q { 0x11 } else { 0x00 }));
            self.vfin(rd, true);
            return true;
        }

        // ---- EXT: bytes imm.. of the pair Vm:Vn ----
        if insn & 0xBFE0_8400 == 0x2E00_0000 && !jit_off("ext") {
            let imm = bits(insn, 14, 11);
            if q {
                self.vld(xmm0, rm, true);
                self.vld(xmm1, rn, true);
                a!(self, palignr(xmm0, xmm1, imm));
            } else {
                if imm > 7 {
                    return false;
                }
                a!(self, movq(xmm0, qword_ptr(r15 + voff(rn))));
                a!(self, movq(xmm1, qword_ptr(r15 + voff(rm))));
                a!(self, punpcklqdq(xmm0, xmm1));
                if imm != 0 {
                    a!(self, psrldq(xmm0, imm));
                }
            }
            self.vfin(rd, q);
            return true;
        }

        // ---- SQDMULH / SQRDMULH (vector, 16/32-bit lanes) ----
        // The only overflow is INT_MIN * INT_MIN. Its wrapped result is INT_MIN,
        // while every valid negative result is larger, so that value identifies
        // both the lanes to saturate and whether sticky FPSR.QC must be set.
        if insn & 0x9F20_FC00 == 0x0E20_B400 && (size == 1 || size == 2) && !jit_off("qdmulh") {
            self.vld(xmm0, rn, q);
            self.vld(xmm1, rm, q);
            if size == 1 {
                if u {
                    a!(self, pmulhrsw(xmm0, xmm1)); // (2ab + 0x8000) >> 16
                } else {
                    // (2ab) >> 16 = (high16(ab) << 1) | bit 15 of low16(ab)
                    a!(self, movdqa(xmm2, xmm0));
                    a!(self, pmulhw(xmm0, xmm1));
                    a!(self, pmullw(xmm2, xmm1));
                    a!(self, psllw(xmm0, 1));
                    a!(self, psrlw(xmm2, 15));
                    a!(self, por(xmm0, xmm2));
                }
                self.vconst(xmm1, 0x8000_8000_8000_8000);
                a!(self, pcmpeqw(xmm1, xmm0));
            } else {
                // PMULDQ multiplies dwords 0 and 2. Shift copies for dwords 1/3,
                // then interleave the low halves of (ab + rounding) >> 31.
                // Logical qword shift is sufficient: only the low 32 bits survive.
                a!(self, movdqa(xmm2, xmm0));
                a!(self, movdqa(xmm3, xmm1));
                a!(self, psrldq(xmm2, 4));
                a!(self, psrldq(xmm3, 4));
                a!(self, pmuldq(xmm0, xmm1));
                a!(self, pmuldq(xmm2, xmm3));
                if u {
                    self.vconst(xmm1, 0x4000_0000);
                    a!(self, paddq(xmm0, xmm1));
                    a!(self, paddq(xmm2, xmm1));
                }
                a!(self, psrlq(xmm0, 31));
                a!(self, psrlq(xmm2, 31));
                a!(self, pshufd(xmm0, xmm0, 0x88));
                a!(self, pshufd(xmm2, xmm2, 0x88));
                a!(self, punpckldq(xmm0, xmm2));
                self.vconst(xmm1, 0x8000_0000_8000_0000);
                a!(self, pcmpeqd(xmm1, xmm0));
            }
            a!(self, pxor(xmm0, xmm1)); // INT_MIN -> INT_MAX, all other lanes unchanged
            let mut no_sat = self.new_label();
            a!(self, ptest(xmm1, xmm1));
            a!(self, jz(no_sat));
            a!(self, or(dword_ptr(r15 + off::FPSR as i32), 1 << 27));
            self.u.a.set_label(&mut no_sat).unwrap();
            a!(self, nop());
            self.vfin(rd, q);
            return true;
        }

        // ---- UABD/SABD (01110), UABA/SABA (01111): |n - m| = max - min ----
        if insn & 0x9F20_F400 == 0x0E20_7400 && size < 3 && !jit_off("abd") {
            let acc = bit(insn, 11) != 0;
            self.vld(xmm0, rn, q);
            self.vld(xmm1, rm, q);
            a!(self, movdqa(xmm2, xmm0));
            match (u, size) {
                (true, 0) => { a!(self, pmaxub(xmm0, xmm1)); a!(self, pminub(xmm2, xmm1)); }
                (true, 1) => { a!(self, pmaxuw(xmm0, xmm1)); a!(self, pminuw(xmm2, xmm1)); }
                (true, _) => { a!(self, pmaxud(xmm0, xmm1)); a!(self, pminud(xmm2, xmm1)); }
                (false, 0) => { a!(self, pmaxsb(xmm0, xmm1)); a!(self, pminsb(xmm2, xmm1)); }
                (false, 1) => { a!(self, pmaxsw(xmm0, xmm1)); a!(self, pminsw(xmm2, xmm1)); }
                (false, _) => { a!(self, pmaxsd(xmm0, xmm1)); a!(self, pminsd(xmm2, xmm1)); }
            }
            match size {
                0 => a!(self, psubb(xmm0, xmm2)),
                1 => a!(self, psubw(xmm0, xmm2)),
                _ => a!(self, psubd(xmm0, xmm2)),
            }
            if acc {
                self.vld(xmm1, rd, q);
                match size {
                    0 => a!(self, paddb(xmm0, xmm1)),
                    1 => a!(self, paddw(xmm0, xmm1)),
                    _ => a!(self, paddd(xmm0, xmm1)),
                }
            }
            self.vfin(rd, q);
            return true;
        }

        // ---- UADDLP/SADDLP (00010), UADALP/SADALP (00110): pairwise add long (+ accumulate) ----
        if insn & 0x9F3F_BC00 == 0x0E20_2800 && size < 3 && !jit_off("addlp") {
            let acc = bit(insn, 14) != 0;
            self.vld(xmm0, rn, q);
            match (u, size) {
                (true, 0) => {
                    self.vconst(xmm1, 0x0101_0101_0101_0101);
                    a!(self, pmaddubsw(xmm0, xmm1)); // unsigned bytes * 1, pairs summed (max 510)
                }
                (false, 0) => {
                    self.vconst(xmm1, 0x0101_0101_0101_0101);
                    a!(self, pmaddubsw(xmm1, xmm0)); // 1 * signed bytes
                    a!(self, movdqa(xmm0, xmm1));
                }
                (false, 1) => {
                    self.vconst(xmm1, 0x0001_0001_0001_0001);
                    a!(self, pmaddwd(xmm0, xmm1));
                }
                (true, 1) => {
                    a!(self, movdqa(xmm1, xmm0));
                    a!(self, pslld(xmm0, 16));
                    a!(self, psrld(xmm0, 16));
                    a!(self, psrld(xmm1, 16));
                    a!(self, paddd(xmm0, xmm1));
                }
                (true, _) => {
                    a!(self, movdqa(xmm1, xmm0));
                    a!(self, psllq(xmm0, 32));
                    a!(self, psrlq(xmm0, 32));
                    a!(self, psrlq(xmm1, 32));
                    a!(self, paddq(xmm0, xmm1));
                }
                (false, _) => {
                    a!(self, pshufd(xmm0, xmm0, 0xD8)); // [x0, x2, x1, x3]
                    a!(self, movdqa(xmm1, xmm0));
                    a!(self, psrldq(xmm1, 8));
                    a!(self, pmovsxdq(xmm0, xmm0));
                    a!(self, pmovsxdq(xmm1, xmm1));
                    a!(self, paddq(xmm0, xmm1));
                }
            }
            if acc {
                self.vld(xmm1, rd, q);
                match size {
                    0 => a!(self, paddw(xmm0, xmm1)),
                    1 => a!(self, paddd(xmm0, xmm1)),
                    _ => a!(self, paddq(xmm0, xmm1)),
                }
            }
            self.vfin(rd, q);
            return true;
        }

        // SHA256H/H2 with SHA-NI: ARM keeps X = (a,b,c,d) and Y = (e,f,g,h) in lanes 0..3, x86 keeps
        // ABEF = (f,e,b,a) and CDGH = (h,g,d,c); shuffle in, two SHA256RNDS2 (two rounds each, W+K in
        // xmm0), shuffle out. SHA256H returns the new X, SHA256H2 the new Y.
        if insn & 0xFFE0_EC00 == 0x5E00_4000 && has_sha() && !jit_off("shani") {
            let h2 = bit(insn, 12) != 0;
            let (x, y) = if h2 { (rn, rd) } else { (rd, rn) };
            self.vld(xmm3, x, true);
            self.vld(xmm4, y, true);
            self.vld(xmm0, rm, true);                 // W+K for the four rounds
            a!(self, movaps(xmm1, xmm4));
            a!(self, shufps(xmm1, xmm3, 0x11));       // ABEF
            a!(self, movaps(xmm2, xmm4));
            a!(self, shufps(xmm2, xmm3, 0xBB));       // CDGH
            a!(self, sha256rnds2(xmm2, xmm1));        // xmm2 = ABEF after 2 rounds; CDGH is now old ABEF
            a!(self, pshufd(xmm0, xmm0, 0x0E));       // W+K of rounds 3 and 4
            a!(self, sha256rnds2(xmm1, xmm2));        // xmm1 = ABEF after 4 rounds; CDGH = xmm2
            a!(self, movaps(xmm0, xmm1));
            a!(self, shufps(xmm0, xmm2, if h2 { 0x11 } else { 0xBB }));
            self.vfin(rd, true);
            return true;
        }
        // SHA256H/H2 without SHA-NI: four rounds, with state kept in two XMM registers,
        // using ordinary rotates/Boolean operations.
        if insn & 0xFFE0_EC00 == 0x5E00_4000 {
            let h2 = bit(insn, 12) != 0;
            self.vld(xmm0, if h2 { rn } else { rd }, true);
            self.vld(xmm1, if h2 { rd } else { rn }, true);
            self.vld(xmm2, rm, true);
            for lane in 0..4 {
                a!(self, pextrd(esi, xmm1, 0));
                a!(self, mov(eax, esi)); a!(self, ror(eax, 6));
                a!(self, mov(ecx, esi)); a!(self, ror(ecx, 11)); a!(self, xor(eax, ecx));
                a!(self, mov(ecx, esi)); a!(self, ror(ecx, 25)); a!(self, xor(eax, ecx));
                a!(self, pextrd(edi, xmm1, 1));
                a!(self, pextrd(r8d, xmm1, 2));
                a!(self, xor(edi, r8d)); a!(self, and(edi, esi)); a!(self, xor(edi, r8d));
                a!(self, add(eax, edi));
                a!(self, pextrd(ecx, xmm1, 3)); a!(self, add(eax, ecx));
                a!(self, pextrd(ecx, xmm2, lane)); a!(self, add(eax, ecx));
                a!(self, pextrd(edx, xmm0, 3)); a!(self, add(edx, eax));
                a!(self, pextrd(esi, xmm0, 0));
                a!(self, mov(ecx, esi)); a!(self, ror(ecx, 2));
                a!(self, mov(edi, esi)); a!(self, ror(edi, 13)); a!(self, xor(ecx, edi));
                a!(self, mov(edi, esi)); a!(self, ror(edi, 22)); a!(self, xor(ecx, edi));
                a!(self, add(eax, ecx));
                a!(self, pextrd(edi, xmm0, 1));
                a!(self, pextrd(r8d, xmm0, 2));
                a!(self, mov(ecx, esi)); a!(self, and(ecx, edi));
                a!(self, xor(esi, edi)); a!(self, and(esi, r8d)); a!(self, xor(ecx, esi));
                a!(self, add(eax, ecx));
                a!(self, pslldq(xmm0, 4)); a!(self, pinsrd(xmm0, eax, 0));
                a!(self, pslldq(xmm1, 4)); a!(self, pinsrd(xmm1, edx, 0));
            }
            if h2 { a!(self, movdqa(xmm0, xmm1)); }
            self.vfin(rd, true);
            return true;
        }
        // SHA256SU0/SU1 schedule updates. Keep original inputs separate from
        // the result so Rd==Rn/Rm and SU1's r0/r1 dependency remain exact.
        let su0 = insn & 0xFFFF_FC00 == 0x5E28_2800;
        let su1 = insn & 0xFFE0_FC00 == 0x5E00_6000;
        if (su0 || su1) && has_sha() && !jit_off("shani") {
            // Intel MSG1 is exactly ARM SU0. MSG2 includes SU1's r0/r1
            // dependency but not its n1,n2,n3,m0 addend: add that first.
            // Load every input before storing so all Rd/Rn/Rm aliases work.
            self.vld(xmm0, rd, true);
            self.vld(xmm1, rn, true);
            if su0 {
                a!(self, sha256msg1(xmm0, xmm1));
            } else {
                self.vld(xmm2, rm, true);
                a!(self, movdqa(xmm3, xmm2));
                a!(self, palignr(xmm3, xmm1, 4)); // n1,n2,n3,m0
                a!(self, paddd(xmm0, xmm3));
                a!(self, sha256msg2(xmm0, xmm2));
            }
            self.vfin(rd, true);
            return true;
        }
        if su0 || su1 {
            self.vld(xmm1, rd, true);
            self.vld(xmm2, rn, true);
            if su1 { self.vld(xmm3, rm, true); }
            a!(self, pxor(xmm0, xmm0));
            for lane in 0..4 {
                if su0 {
                    if lane < 3 { a!(self, pextrd(eax, xmm1, lane + 1)); }
                    else { a!(self, pextrd(eax, xmm2, 0)); }
                } else if lane < 2 {
                    a!(self, pextrd(eax, xmm3, lane + 2));
                } else {
                    a!(self, pextrd(eax, xmm0, lane - 2));
                }
                a!(self, mov(ecx, eax));
                a!(self, mov(edx, eax));
                a!(self, ror(eax, if su0 { 7 } else { 17 }));
                a!(self, ror(ecx, if su0 { 18 } else { 19 }));
                a!(self, shr(edx, if su0 { 3 } else { 10 }));
                a!(self, xor(eax, ecx)); a!(self, xor(eax, edx));
                a!(self, pextrd(ecx, xmm1, lane)); a!(self, add(eax, ecx));
                if su1 {
                    if lane < 3 { a!(self, pextrd(ecx, xmm2, lane + 1)); }
                    else { a!(self, pextrd(ecx, xmm3, 0)); }
                    a!(self, add(eax, ecx));
                }
                a!(self, pinsrd(xmm0, eax, lane));
            }
            self.vfin(rd, true);
            return true;
        }

        // Vector FRINT* (single/double).
        if insn & 0x9F3E_0C00 == 0x0E20_0800 {
            let opcode = bits(insn, 16, 12);
            let fa = bit(insn, 23);
            let mode = match (u, fa, opcode) {
                (false, 0, 24) => Some(0), (false, 1, 24) => Some(1),
                (false, 0, 25) => Some(2), (false, 1, 25) => Some(3),
                (true, 0, 24) => Some(4), (true, _, 25) => Some(5),
                (_, 0, 30 | 31) => Some(if u { 5 } else { 3 }),
                _ => None,
            };
            if let Some(mode) = mode {
                let double = bit(insn, 22) != 0;
                if double && !q { return false; }
                self.vld(xmm0, rn, q);
                self.frint_round(double, mode);
                if opcode >= 30 {
                    self.frint_limit(double, if opcode == 30 { 32 } else { 64 });
                }
                self.vfin(rd, q);
                return true;
            }
        }

        // TBL/TBX: PSHUFB wraps indices modulo 16, so explicitly mask each
        // table's contribution by the index high nibble. Delay the store until
        // all sources have been read (including wrapped/overlapping tables).
        if insn & 0xBFE0_8C00 == 0x0E00_0000 {
            let len = bits(insn, 14, 13) + 1;
            let tbx = bit(insn, 12) != 0;
            self.vld(xmm1, rm, true);
            self.vconst(xmm2, 0xF0F0_F0F0_F0F0_F0F0);
            a!(self, pand(xmm2, xmm1));
            a!(self, pxor(xmm0, xmm0));
            a!(self, pxor(xmm5, xmm5));
            for k in 0..len {
                self.vconst(xmm3, 0x0101_0101_0101_0101 * (k * 16) as u64);
                a!(self, pcmpeqb(xmm3, xmm2));
                a!(self, por(xmm5, xmm3));
                self.vld(xmm4, (rn + k) % 32, true);
                a!(self, pshufb(xmm4, xmm1));
                a!(self, pand(xmm4, xmm3));
                a!(self, por(xmm0, xmm4));
            }
            if tbx {
                self.vld(xmm4, rd, true);
                a!(self, pandn(xmm5, xmm4));
                a!(self, por(xmm0, xmm5));
            }
            self.vfin(rd, q);
            return true;
        }

        // ---- three-same integer: saturating add/sub, min/max, mul/mla/mls ----
        if insn & 0x9F20_0400 == 0x0E20_0400 {
            let opcode = bits(insn, 15, 11);
            match opcode {
                0b00001 | 0b00101 if size < 2 => {
                    self.vld(xmm0, rn, q);
                    self.vld(xmm1, rm, q);
                    match (opcode, u, size) {
                        (0b00001, true, 0) => a!(self, paddusb(xmm0, xmm1)),
                        (0b00001, true, _) => a!(self, paddusw(xmm0, xmm1)),
                        (0b00001, false, 0) => a!(self, paddsb(xmm0, xmm1)),
                        (0b00001, false, _) => a!(self, paddsw(xmm0, xmm1)),
                        (_, true, 0) => a!(self, psubusb(xmm0, xmm1)),
                        (_, true, _) => a!(self, psubusw(xmm0, xmm1)),
                        (_, false, 0) => a!(self, psubsb(xmm0, xmm1)),
                        (_, false, _) => a!(self, psubsw(xmm0, xmm1)),
                    }
                    self.vfin(rd, q);
                    return true;
                }
                0b01100 | 0b01101 if size < 3 => {
                    let is_min = opcode == 0b01101;
                    self.vld(xmm0, rn, q);
                    self.vld(xmm1, rm, q);
                    match (is_min, u, size) {
                        (true, true, 0) => a!(self, pminub(xmm0, xmm1)),
                        (true, true, 1) => a!(self, pminuw(xmm0, xmm1)),
                        (true, true, _) => a!(self, pminud(xmm0, xmm1)),
                        (true, false, 0) => a!(self, pminsb(xmm0, xmm1)),
                        (true, false, 1) => a!(self, pminsw(xmm0, xmm1)),
                        (true, false, _) => a!(self, pminsd(xmm0, xmm1)),
                        (false, true, 0) => a!(self, pmaxub(xmm0, xmm1)),
                        (false, true, 1) => a!(self, pmaxuw(xmm0, xmm1)),
                        (false, true, _) => a!(self, pmaxud(xmm0, xmm1)),
                        (false, false, 0) => a!(self, pmaxsb(xmm0, xmm1)),
                        (false, false, 1) => a!(self, pmaxsw(xmm0, xmm1)),
                        (false, false, _) => a!(self, pmaxsd(xmm0, xmm1)),
                    }
                    self.vfin(rd, q);
                    return true;
                }
                0b10010 | 0b10011 if size <= 2 => {
                    // MLA/MLS (opcode 10010), MUL (10011, U=0; U=1 is PMUL)
                    if opcode == 0b10011 && u {
                        return false;
                    }
                    self.vld(xmm0, rn, q);
                    self.vld(xmm1, rm, q);
                    match size {
                        0 => self.pmul_bytes(),
                        1 => a!(self, pmullw(xmm0, xmm1)),
                        _ => a!(self, pmulld(xmm0, xmm1)),
                    }
                    if opcode == 0b10010 {
                        self.vld(xmm2, rd, q);
                        match (u, size) {
                            // MLS: d - n*m
                            (true, 0) => a!(self, psubb(xmm2, xmm0)),
                            (true, 1) => a!(self, psubw(xmm2, xmm0)),
                            (true, _) => a!(self, psubd(xmm2, xmm0)),
                            (false, 0) => a!(self, paddb(xmm2, xmm0)),
                            (false, 1) => a!(self, paddw(xmm2, xmm0)),
                            (false, _) => a!(self, paddd(xmm2, xmm0)),
                        }
                        a!(self, movdqa(xmm0, xmm2));
                    }
                    self.vfin(rd, q);
                    return true;
                }
                _ => {}
            }
        }

        // ---- vector floating-point three-same ----
        'fp: {
        if insn & 0x9F20_0400 != 0x0E20_0400 {
            break 'fp;
        }
        {
            let a_bit = bit(insn, 23);
            let sz = bit(insn, 22);
            let opcode = bits(insn, 15, 11);
            let double = sz == 1;
            let op = match (u, a_bit, opcode) {
                (false, 0, 0b11010) => 1, // FADD
                (false, 1, 0b11010) => 2, // FSUB
                (true, 0, 0b11011) => 3,  // FMUL
                (true, 0, 0b11111) => 4,  // FDIV
                (false, 0, 0b11100) => 5, // FCMEQ
                (true, 0, 0b11100) => 6,  // FCMGE
                (true, 1, 0b11100) => 7,  // FCMGT
                _ => 0,
            };
            if op == 0 || (double && !q) {
                break 'fp;
            }
            self.vld(xmm0, rn, q);
            self.vld(xmm1, rm, q);
            match (op, double) {
                (1, false) => a!(self, addps(xmm0, xmm1)),
                (1, true) => a!(self, addpd(xmm0, xmm1)),
                (2, false) => a!(self, subps(xmm0, xmm1)),
                (2, true) => a!(self, subpd(xmm0, xmm1)),
                (3, false) => a!(self, mulps(xmm0, xmm1)),
                (3, true) => a!(self, mulpd(xmm0, xmm1)),
                (4, false) => a!(self, divps(xmm0, xmm1)),
                (4, true) => a!(self, divpd(xmm0, xmm1)),
                (5, false) => a!(self, cmpps(xmm0, xmm1, 0)),
                (5, true) => a!(self, cmppd(xmm0, xmm1, 0)),
                // n >= m  <=>  m <= n : cmpleps(m, n)
                (6, false) => { a!(self, cmpps(xmm1, xmm0, 2)); a!(self, movdqa(xmm0, xmm1)); }
                (6, true) => { a!(self, cmppd(xmm1, xmm0, 2)); a!(self, movdqa(xmm0, xmm1)); }
                (7, false) => { a!(self, cmpps(xmm1, xmm0, 1)); a!(self, movdqa(xmm0, xmm1)); }
                (_, _) => { a!(self, cmppd(xmm1, xmm0, 1)); a!(self, movdqa(xmm0, xmm1)); }
            }
            if op <= 4 {
                self.vfix_binary_nan(double, rn, q);
            }
            self.vfin(rd, q);
            return true;
        }
        }

        // ---- MVN / NOT (two-reg misc) ----
        if insn & 0xBFFF_FC00 == 0x2E20_5800 {
            self.vld(xmm0, rn, q);
            a!(self, pcmpeqd(xmm1, xmm1));
            a!(self, pxor(xmm0, xmm1));
            self.vfin(rd, q);
            return true;
        }

        // SCVTF/UCVTF (vector integer source), using the same exact
        // unsigned-64 rounding sequence as the scalar conversion.
        if insn & 0x9FBF_FC00 == 0x0E21_D800 {
            let double = bit(insn, 22) != 0;
            if double && !q { return false; }
            if !double && !u {
                self.vld(xmm0, rn, q);
                a!(self, cvtdq2ps(xmm0, xmm0));
            } else {
                self.vld(xmm5, rn, true);
                a!(self, pxor(xmm4, xmm4));
                for lane in 0..(if double { 2 } else if q { 4 } else { 2 }) {
                    if double { a!(self, pextrq(rax, xmm5, lane)); }
                    else { a!(self, pextrd(eax, xmm5, lane)); }
                    self.int_to_fp_reg(u, double, double);
                    if double {
                        a!(self, movq(rax, xmm0));
                        a!(self, pinsrq(xmm4, rax, lane));
                    } else {
                        a!(self, movd(eax, xmm0));
                        a!(self, pinsrd(xmm4, eax, lane));
                    }
                }
                a!(self, movdqa(xmm0, xmm4));
            }
            self.vfin(rd, q);
            return true;
        }

        // ---- shifts by immediate: USHR/SSHR/URSHR, USRA/SSRA/URSRA (accumulate), SHL ----
        if insn & 0x9F80_0400 == 0x0F00_0400 && bits(insn, 22, 19) != 0 {
            let immh = bits(insn, 22, 19);
            let immb = bits(insn, 18, 16);
            let opcode = bits(insn, 15, 11);
            if immh == 0 {
                return false;
            }
            let sz = if immh & 8 != 0 { 3 } else if immh & 4 != 0 { 2 } else if immh & 2 != 0 { 1 } else { 0 };
            if sz == 3 && !q {
                return false;
            }
            let esize = 8u32 << sz;
            let immv = (immh << 3) | immb;
            match opcode {
                0b00000 | 0b00100 | 0b00010 | 0b00110 => {
                    // USHR/SSHR (00000), URSHR/SRSHR (00100); the 00x10 forms add the result to Vd
                    let n = 2 * esize - immv;
                    if n == 0 || n > esize {
                        return false;
                    }
                    let rounding = opcode & 0b00100 != 0;
                    let accumulate = opcode & 0b00010 != 0;
                    if !u && (sz == 0 || sz == 3) {
                        return false; // no arithmetic byte/qword shifts in SSE2
                    }
                    if rounding && !u {
                        return false;
                    }
                    self.vld(xmm0, rn, q);
                    if !rounding {
                        if u {
                            if n == esize {
                                a!(self, pxor(xmm0, xmm0));
                            } else {
                                self.psrl_elem(xmm0, sz, n);
                            }
                        } else if sz == 1 {
                            a!(self, psraw(xmm0, n.min(15) as i32));
                        } else {
                            a!(self, psrad(xmm0, n.min(31) as i32));
                        }
                    } else {
                        // URSHR: t = x >> (n-1); r = (t >> 1) + (t & 1)
                        if n > 1 {
                            self.psrl_elem(xmm0, sz, n - 1);
                        }
                        a!(self, movdqa(xmm1, xmm0));
                        self.psrl_elem(xmm0, sz, 1);
                        self.vconst(xmm2, match sz { 0 => 0x0101_0101_0101_0101, 1 => 0x0001_0001_0001_0001, 2 => 0x0000_0001_0000_0001, _ => 1 });
                        a!(self, pand(xmm1, xmm2));
                        self.padd_elem(xmm0, xmm1, sz);
                    }
                    if accumulate {
                        self.vld(xmm3, rd, q);
                        self.padd_elem(xmm0, xmm3, sz);
                    }
                    self.vfin(rd, q);
                    return true;
                }
                0b01010 if !u => {
                    // SHL
                    let n = immv - esize;
                    self.vld(xmm0, rn, q);
                    if n != 0 {
                        self.psll_elem(xmm0, sz, n);
                    }
                    self.vfin(rd, q);
                    return true;
                }
                _ => {}
            }
        }

        // ---- USHLL/SSHLL (and the 2 forms): widen + shift ----
        if insn & 0x9F80_FC00 == 0x0F00_A400 && bits(insn, 22, 19) != 0 {
            let immh = bits(insn, 22, 19);
            let immb = bits(insn, 18, 16);
            if immh == 0 || immh & 8 != 0 {
                return false;
            }
            let sz = if immh & 4 != 0 { 2 } else if immh & 2 != 0 { 1 } else { 0 };
            let shift = ((immh << 3) | immb) - (8u32 << sz);
            if q {
                // upper half of the source
                a!(self, movdqu(xmm0, xmmword_ptr(r15 + voff(rn))));
                a!(self, psrldq(xmm0, 8));
            } else {
                a!(self, movq(xmm0, qword_ptr(r15 + voff(rn))));
            }
            match (u, sz) {
                (true, 0) => a!(self, pmovzxbw(xmm0, xmm0)),
                (true, 1) => a!(self, pmovzxwd(xmm0, xmm0)),
                (true, _) => a!(self, pmovzxdq(xmm0, xmm0)),
                (false, 0) => a!(self, pmovsxbw(xmm0, xmm0)),
                (false, 1) => a!(self, pmovsxwd(xmm0, xmm0)),
                (false, _) => a!(self, pmovsxdq(xmm0, xmm0)),
            }
            if shift != 0 {
                self.psll_elem(xmm0, sz + 1, shift);
            }
            a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            return true;
        }

        // ---- UMULL/SMULL (+2): bytes->halves, halves->words ----
        if insn & 0x9F20_FC00 == 0x0E20_C000 && size < 2 {
            let upper = q;
            let src = |e: &mut Self, x: AsmRegisterXmm, n: u32| {
                if upper {
                    a!(e, movdqu(x, xmmword_ptr(r15 + voff(n))));
                    a!(e, psrldq(x, 8));
                } else {
                    a!(e, movq(x, qword_ptr(r15 + voff(n))));
                }
            };
            src(self, xmm0, rn);
            src(self, xmm1, rm);
            if size == 0 {
                if u {
                    a!(self, pmovzxbw(xmm0, xmm0));
                    a!(self, pmovzxbw(xmm1, xmm1));
                } else {
                    a!(self, pmovsxbw(xmm0, xmm0));
                    a!(self, pmovsxbw(xmm1, xmm1));
                }
                a!(self, pmullw(xmm0, xmm1));
            } else {
                a!(self, movdqa(xmm2, xmm0));
                a!(self, pmullw(xmm0, xmm1));
                if u {
                    a!(self, pmulhuw(xmm2, xmm1));
                } else {
                    a!(self, pmulhw(xmm2, xmm1));
                }
                a!(self, punpcklwd(xmm0, xmm2));
            }
            a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            return true;
        }

        // ---- three different: [US]ADDL/[US]ADDW/[US]SUBL/[US]SUBW, [US]MLAL/[US]MLSL (+2 forms) ----
        // The "2" forms (Q=1) take the narrow operands from the upper halves.
        if insn & 0x9F20_0C00 == 0x0E20_0000 && size < 3 && !jit_off("3diff") {
            let opcode = bits(insn, 15, 12);
            let upper = q;
            let narrow = |e: &mut Self, x: AsmRegisterXmm, n: u32| {
                if upper {
                    a!(e, movdqu(x, xmmword_ptr(r15 + voff(n))));
                    a!(e, psrldq(x, 8));
                } else {
                    a!(e, movq(x, qword_ptr(r15 + voff(n))));
                }
            };
            let widen = |e: &mut Self, x: AsmRegisterXmm| match (u, size) {
                (true, 0) => a!(e, pmovzxbw(x, x)),
                (true, 1) => a!(e, pmovzxwd(x, x)),
                (true, _) => a!(e, pmovzxdq(x, x)),
                (false, 0) => a!(e, pmovsxbw(x, x)),
                (false, 1) => a!(e, pmovsxwd(x, x)),
                (false, _) => a!(e, pmovsxdq(x, x)),
            };
            let add = |e: &mut Self, d: AsmRegisterXmm, s: AsmRegisterXmm, sub: bool| match (sub, size) {
                (false, 0) => a!(e, paddw(d, s)),
                (false, 1) => a!(e, paddd(d, s)),
                (false, _) => a!(e, paddq(d, s)),
                (true, 0) => a!(e, psubw(d, s)),
                (true, 1) => a!(e, psubd(d, s)),
                (true, _) => a!(e, psubq(d, s)),
            };
            match opcode {
                0b0000 | 0b0001 | 0b0010 | 0b0011 => {
                    // ADDL/SUBL widen both operands; ADDW/SUBW have a wide first operand
                    if opcode & 1 != 0 {
                        self.vld(xmm0, rn, true);
                    } else {
                        narrow(self, xmm0, rn);
                        widen(self, xmm0);
                    }
                    narrow(self, xmm1, rm);
                    widen(self, xmm1);
                    add(self, xmm0, xmm1, opcode & 2 != 0);
                    a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
                    return true;
                }
                0b1000 | 0b1010 if size < 2 => {
                    // MLAL/MLSL: the widened product is exact (8x8 -> 16, 16x16 -> 32 bits)
                    narrow(self, xmm0, rn);
                    narrow(self, xmm1, rm);
                    if size == 0 {
                        widen(self, xmm0);
                        widen(self, xmm1);
                        a!(self, pmullw(xmm0, xmm1));
                    } else {
                        a!(self, movdqa(xmm2, xmm0));
                        a!(self, pmullw(xmm0, xmm1));
                        if u {
                            a!(self, pmulhuw(xmm2, xmm1));
                        } else {
                            a!(self, pmulhw(xmm2, xmm1));
                        }
                        a!(self, punpcklwd(xmm0, xmm2));
                    }
                    self.vld(xmm1, rd, true);
                    add(self, xmm1, xmm0, opcode == 0b1010);
                    a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm1));
                    return true;
                }
                _ => {}
            }
        }

        // ---- ADDHN/RADDHN, SUBHN/RSUBHN (halves->bytes, words->halves, dwords->words), lower/upper "2" forms ----
        // The sum/difference wraps at the source element width; the result is its high half (+ rounding).
        if (insn & 0x9F20_FC00 == 0x0E20_4000 || insn & 0x9F20_FC00 == 0x0E20_6000) && size < 3 && !jit_off("hn") {
            let round = u;
            let sub = bit(insn, 13) != 0;
            self.vld(xmm0, rn, true);
            self.vld(xmm1, rm, true);
            match size {
                0 => {
                    if sub {
                        a!(self, psubw(xmm0, xmm1));
                    } else {
                        a!(self, paddw(xmm0, xmm1));
                    }
                    if round {
                        self.vconst(xmm2, 0x0080_0080_0080_0080);
                        a!(self, paddw(xmm0, xmm2));
                    }
                    a!(self, psrlw(xmm0, 8));
                    a!(self, packuswb(xmm0, xmm0));
                }
                1 => {
                    if sub {
                        a!(self, psubd(xmm0, xmm1));
                    } else {
                        a!(self, paddd(xmm0, xmm1));
                    }
                    if round {
                        self.vconst(xmm2, 0x0000_8000_0000_8000);
                        a!(self, paddd(xmm0, xmm2));
                    }
                    a!(self, psrld(xmm0, 16));
                    a!(self, packusdw(xmm0, xmm0));
                }
                _ => {
                    if sub {
                        a!(self, psubq(xmm0, xmm1));
                    } else {
                        a!(self, paddq(xmm0, xmm1));
                    }
                    if round {
                        self.vconst(xmm2, 0x8000_0000);
                        a!(self, paddq(xmm0, xmm2));
                    }
                    a!(self, psrlq(xmm0, 32));
                    a!(self, pshufd(xmm0, xmm0, 0x08));
                }
            }
            if q {
                a!(self, movq(xmm0, xmm0));
                a!(self, pslldq(xmm0, 8));
                a!(self, movq(xmm1, qword_ptr(r15 + voff(rd))));
                a!(self, por(xmm0, xmm1));
            } else {
                a!(self, movq(xmm0, xmm0));
            }
            a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            return true;
        }

        // ---- SQXTN / SQXTUN (+2), and SQSHRN / UQSHRN / SQSHRUN (+2, non-rounding) ----
        // Narrow h->b and s->h with saturation: PACKSS* saturate signed, PACKUS* saturate signed input to
        // unsigned (exactly SQXTUN); unsigned input is clamped with PMINU* first.
        {
            let sqxtn = insn & 0xBF3F_FC00 == 0x0E21_4800;
            let sqxtun = insn & 0xBF3F_FC00 == 0x2E21_2800;
            // bit 11 set = the rounding forms (SQRSHRN/UQRSHRN/SQRSHRUN): not handled here
            let shift_narrow = insn & 0x9F80_FC00 == 0x0F00_9400 || insn & 0xBF80_FC00 == 0x2F00_8400;
            if (sqxtn || sqxtun) && size < 2 || shift_narrow && !jit_off("qnarrow") {
                // dsize: 0 = results are bytes (source halves), 1 = halves (source words)
                let (dsize, shift, kind) = if shift_narrow {
                    let immh = bits(insn, 22, 19);
                    let immb = bits(insn, 18, 16);
                    if immh == 0 || immh >= 4 {
                        return false; // d -> s has no pack instruction
                    }
                    let dsize = if immh & 2 != 0 { 1 } else { 0 };
                    let esize = 8u32 << dsize;
                    let n = 2 * esize - ((immh << 3) | immb);
                    // 10010: SQSHRN (U=0) / UQSHRN (U=1); 10000 with U=1: SQSHRUN
                    let kind = if bits(insn, 15, 11) == 0b10000 { 2 } else if u { 1 } else { 0 };
                    (dsize, n, kind)
                } else {
                    (size, 0, if sqxtun { 2 } else { 0 })
                };
                if !jit_off("qnarrow") {
                    self.vld(xmm0, rn, true);
                    if shift != 0 {
                        match (kind, dsize) {
                            (1, 0) => a!(self, psrlw(xmm0, shift)),
                            (1, _) => a!(self, psrld(xmm0, shift)),
                            (_, 0) => a!(self, psraw(xmm0, shift)),
                            _ => a!(self, psrad(xmm0, shift)),
                        }
                    }
                    match (kind, dsize) {
                        (0, 0) => a!(self, packsswb(xmm0, xmm0)),
                        (0, _) => a!(self, packssdw(xmm0, xmm0)),
                        (1, 0) => { self.vconst(xmm1, 0x00ff_00ff_00ff_00ff); a!(self, pminuw(xmm0, xmm1)); a!(self, packuswb(xmm0, xmm0)); }
                        (1, _) => { self.vconst(xmm1, 0x0000_ffff_0000_ffff); a!(self, pminud(xmm0, xmm1)); a!(self, packusdw(xmm0, xmm0)); }
                        (_, 0) => a!(self, packuswb(xmm0, xmm0)),
                        _ => a!(self, packusdw(xmm0, xmm0)),
                    }
                    a!(self, movq(xmm0, xmm0));
                    if q {
                        a!(self, pslldq(xmm0, 8));
                        a!(self, movq(xmm1, qword_ptr(r15 + voff(rd))));
                        a!(self, por(xmm0, xmm1));
                    }
                    a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
                    return true;
                }
            }
        }

        // ---- XTN / UQXTN (+2) ----
        if insn & 0xBF3F_FC00 == 0x0E21_2800 || insn & 0xBF3F_FC00 == 0x2E21_4800 {
            if size > 2 {
                return false;
            }
            let saturating = bit(insn, 29) != 0;
            self.vld(xmm0, rn, true);
            match size {
                0 => {
                    if saturating {
                        self.vconst(xmm1, 0x00ff_00ff_00ff_00ff);
                        a!(self, pminuw(xmm0, xmm1));
                    } else {
                        self.vconst(xmm1, 0x00ff_00ff_00ff_00ff);
                        a!(self, pand(xmm0, xmm1));
                    }
                    a!(self, packuswb(xmm0, xmm0));
                }
                1 => {
                    if saturating {
                        self.vconst(xmm1, 0x0000_ffff_0000_ffff);
                        a!(self, pminud(xmm0, xmm1));
                    } else {
                        self.vconst(xmm1, 0x0000_ffff_0000_ffff);
                        a!(self, pand(xmm0, xmm1));
                    }
                    a!(self, packusdw(xmm0, xmm0));
                }
                _ => {
                    if saturating {
                        return false;
                    }
                    a!(self, pshufd(xmm0, xmm0, 0x08));
                }
            }
            if q {
                a!(self, movq(xmm0, xmm0));
                a!(self, pslldq(xmm0, 8));
                a!(self, movq(xmm1, qword_ptr(r15 + voff(rd))));
                a!(self, por(xmm0, xmm1));
            } else {
                a!(self, movq(xmm0, xmm0));
            }
            a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            return true;
        }

        // ---- FMOV (vector immediate) ----
        if insn & 0x9FF8_0C00 == 0x0F00_0400 && bits(insn, 15, 12) == 0b1111 && bit(insn, 11) == 0 {
            let op = bit(insn, 29);
            if op == 1 && !q {
                return false;
            }
            let imm8 = (bits(insn, 18, 16) << 5) | bits(insn, 9, 5);
            let imm = attesor_core::interp::simd::adv_simd_expand_imm(op, 0b1111, imm8);
            self.mov_imm(rax, imm);
            self.vst_rax(rd, q);
            return true;
        }

        // FCVTZU (vector single/double). Accumulate separately from the
        // input so all alias combinations are safe across per-lane branches.
        if insn & 0xBFBF_FC00 == 0x2EA1_B800 {
            let double = bit(insn, 22) != 0;
            if double && !q { return false; }
            self.vld(xmm5, rn, true);
            a!(self, pxor(xmm4, xmm4));
            for lane in 0..(if double { 2 } else if q { 4 } else { 2 }) {
                if double { a!(self, pextrq(rax, xmm5, lane)); }
                else { a!(self, pextrd(eax, xmm5, lane)); }
                a!(self, movq(xmm0, rax));
                self.fp_to_uint_reg(double, double);
                if double { a!(self, pinsrq(xmm4, rax, lane)); }
                else { a!(self, pinsrd(xmm4, eax, lane)); }
            }
            a!(self, movdqa(xmm0, xmm4));
            self.vfin(rd, q);
            return true;
        }

        // ---- FCVTNS / FCVTMS / FCVTPS / FCVTZS (vector, single precision) ----
        // bit 23 and bit 12 select the rounding: NS nearest-even, MS -inf, PS +inf, ZS zero; round with
        // ROUNDPS first, then the truncating conversion with ARM saturation.
        if insn & 0xBF7F_EC00 == 0x0E21_A800 && !jit_off("fcvtxs") {
            let mode = (bit(insn, 23) << 1) | bit(insn, 12); // 0 NS, 1 MS, 2 PS, 3 ZS
            self.vld(xmm0, rn, q);
            if mode != 3 {
                a!(self, roundps(xmm0, xmm0, mode));
            }
            a!(self, cvttps2dq(xmm1, xmm0));
            self.vconst(xmm2, 0x8000_0000_8000_0000);
            a!(self, pcmpeqd(xmm2, xmm1)); // lanes that overflowed / were NaN
            a!(self, movdqa(xmm3, xmm0));
            a!(self, psrad(xmm3, 31)); // sign of the input
            a!(self, pandn(xmm3, xmm2)); // positive overflow lanes
            a!(self, pxor(xmm1, xmm3)); // 0x80000000 -> 0x7fffffff
            a!(self, movdqa(xmm4, xmm0));
            a!(self, cmpps(xmm4, xmm0, 3)); // NaN lanes
            a!(self, pandn(xmm4, xmm1)); // NaN -> 0
            a!(self, movdqa(xmm0, xmm4));
            self.vfin(rd, q);
            return true;
        }

        // ---- USHL / SSHL (variable shift, scalar per-lane loop) ----
        if insn & 0x9F20_FC00 == 0x0E20_4400 {
            let esz_bytes = 1u32 << size;
            let lanes = (if q { 16 } else { 8 }) / esz_bytes;
            if lanes > 8 {
                return false;
            }
            let bitsz = 8 * esz_bytes;
            a!(self, pxor(xmm0, xmm0));
            for i in 0..lanes {
                let eo = (i * esz_bytes) as i32;
                // value
                let vo = voff(rn) + eo;
                match (size, u) {
                    (0, true) => a!(self, movzx(eax, byte_ptr(r15 + vo))),
                    (1, true) => a!(self, movzx(eax, word_ptr(r15 + vo))),
                    (2, true) => a!(self, mov(eax, dword_ptr(r15 + vo))),
                    (_, true) => a!(self, mov(rax, qword_ptr(r15 + vo))),
                    (0, false) => a!(self, movsx(rax, byte_ptr(r15 + vo))),
                    (1, false) => a!(self, movsx(rax, word_ptr(r15 + vo))),
                    (2, false) => a!(self, movsxd(rax, dword_ptr(r15 + vo))),
                    (_, false) => a!(self, mov(rax, qword_ptr(r15 + vo))),
                }
                a!(self, movsx(ecx, byte_ptr(r15 + voff(rm) + eo)));
                let mut right = self.new_label();
                let mut zero = self.new_label();
                let mut right_big = self.new_label();
                let mut done = self.new_label();
                a!(self, test(cl, cl));
                a!(self, js(right));
                a!(self, cmp(cl, bitsz as i32));
                a!(self, jae(zero));
                a!(self, shl(rax, cl));
                a!(self, jmp(done));
                self.u.a.set_label(&mut right).unwrap();
                a!(self, neg(ecx));
                a!(self, cmp(ecx, bitsz as i32));
                a!(self, jae(right_big));
                if u {
                    a!(self, shr(rax, cl));
                } else {
                    a!(self, sar(rax, cl));
                }
                a!(self, jmp(done));
                self.u.a.set_label(&mut right_big).unwrap();
                if u {
                    a!(self, xor(eax, eax));
                } else {
                    a!(self, sar(rax, 63));
                }
                a!(self, jmp(done));
                self.u.a.set_label(&mut zero).unwrap();
                a!(self, xor(eax, eax));
                self.u.a.set_label(&mut done).unwrap();
                a!(self, nop());
                // pack into xmm0 lane i
                match size {
                    0 => a!(self, pinsrb(xmm0, eax, i as i32)),
                    1 => a!(self, pinsrw(xmm0, eax, i as i32)),
                    2 => a!(self, pinsrd(xmm0, eax, i as i32)),
                    _ => a!(self, pinsrq(xmm0, rax, i as i32)),
                }
            }
            // lanes not written keep stale xmm0 data: clear first-time by zeroing at start would be
            // needed; instead zero xmm0 before the loop.
            self.vfin(rd, q);
            return true;
        }

        // ---- SHRN / SHRN2 ----
        if insn & 0xBF80_FC00 == 0x0F00_8400 && bits(insn, 22, 19) != 0 {
            let immh = bits(insn, 22, 19);
            let immb = bits(insn, 18, 16);
            if immh == 0 || immh & 8 != 0 {
                return false;
            }
            let sz = if immh & 4 != 0 { 2 } else if immh & 2 != 0 { 1 } else { 0 };
            let shift = (16u32 << sz) - ((immh << 3) | immb);
            self.vld(xmm0, rn, true);
            match sz {
                0 => {
                    a!(self, psrlw(xmm0, shift as i32));
                    self.vconst(xmm1, 0x00ff_00ff_00ff_00ff);
                    a!(self, pand(xmm0, xmm1));
                    a!(self, packuswb(xmm0, xmm0));
                }
                1 => {
                    a!(self, psrld(xmm0, shift as i32));
                    self.vconst(xmm1, 0x0000_ffff_0000_ffff);
                    a!(self, pand(xmm0, xmm1));
                    a!(self, packusdw(xmm0, xmm0));
                }
                _ => {
                    a!(self, psrlq(xmm0, shift as i32));
                    a!(self, pshufd(xmm0, xmm0, 0x08));
                }
            }
            if q {
                a!(self, movq(xmm0, xmm0));
                a!(self, pslldq(xmm0, 8));
                a!(self, movq(xmm1, qword_ptr(r15 + voff(rd))));
                a!(self, por(xmm0, xmm1));
            } else {
                a!(self, movq(xmm0, xmm0));
            }
            a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            return true;
        }

        // ---- REV64 / REV32 / REV16 ----
        {
            let group = if insn & 0xBF3F_FC00 == 0x0E20_0800 {
                Some(8u32)
            } else if insn & 0xBF3F_FC00 == 0x2E20_0800 {
                Some(4)
            } else if insn & 0xBF3F_FC00 == 0x0E20_1800 {
                Some(2)
            } else {
                None
            };
            if let Some(group) = group {
                let e = 1u32 << size;
                if e >= group {
                    return false;
                }
                let mut mask = [0u8; 16];
                for b in 0..16u32 {
                    let gb = (b / group) * group;
                    let inb = b % group;
                    let k = inb / e;
                    let within = inb % e;
                    mask[b as usize] = (gb + (group / e - 1 - k) * e + within) as u8;
                }
                let m = u128::from_le_bytes(mask);
                self.vld(xmm0, rn, true);
                self.mov_imm(rdx, m as u64);
                a!(self, movq(xmm1, rdx));
                self.mov_imm(rdx, (m >> 64) as u64);
                a!(self, movq(xmm2, rdx));
                a!(self, punpcklqdq(xmm1, xmm2));
                a!(self, pshufb(xmm0, xmm1));
                self.vfin(rd, q);
                return true;
            }
        }

        // ---- ORR / BIC (vector immediate) ----
        if insn & 0x9FF8_0400 == 0x0F00_0400 && bit(insn, 11) == 0 {
            let cmode = bits(insn, 15, 12);
            let op = bit(insn, 29);
            if !(cmode & 0b1001 == 0b0001 || cmode & 0b1101 == 0b1001) {
                return false;
            }
            let imm8 = (bits(insn, 18, 16) << 5) | bits(insn, 9, 5);
            let imm = attesor_core::interp::simd::adv_simd_expand_imm(op, cmode, imm8);
            self.vld(xmm0, rd, q);
            self.vconst(xmm1, imm);
            if op == 0 {
                a!(self, por(xmm0, xmm1));
            } else {
                a!(self, pandn(xmm1, xmm0));
                a!(self, movdqa(xmm0, xmm1));
            }
            self.vfin(rd, q);
            return true;
        }

        // ---- LD1/ST1 (single structure, one lane) and LD1R ----
        if insn & 0xBF9F_0000 == 0x0D00_0000 || insn & 0xBF80_0000 == 0x0D80_0000 {
            let post = bit(insn, 23) != 0;
            let load = bit(insn, 22) != 0;
            let r_bit = bit(insn, 21);
            let opcode = bits(insn, 15, 13);
            let s_bit = bit(insn, 12);
            let sz = bits(insn, 11, 10);
            if r_bit != 0 {
                return false;
            }
            if !post && bits(insn, 20, 16) != 0 {
                return false;
            }
            enum K { Lane(u32, u32), Rep(u32) }
            let kind = match opcode {
                0b000 => K::Lane(0, (q as u32) << 3 | s_bit << 2 | sz),
                0b010 if sz & 1 == 0 => K::Lane(1, (q as u32) << 2 | s_bit << 1 | (sz >> 1)),
                0b100 if sz == 0 => K::Lane(2, (q as u32) << 1 | s_bit),
                0b100 if sz == 1 && s_bit == 0 => K::Lane(3, q as u32),
                0b110 if load && s_bit == 0 => K::Rep(sz),
                _ => return false,
            };
            let esize_log = match kind { K::Lane(l, _) => l, K::Rep(l) => l };
            self.ldx(rax, rn, true);
            self.commpage_fix();
            match kind {
                K::Lane(l, idx) => {
                    let o = voff(rd) + (idx << l) as i32;
                    if load {
                        match l {
                            0 => { a!(self, movzx(ecx, byte_ptr(rax))); a!(self, mov(byte_ptr(r15 + o), cl)); }
                            1 => { a!(self, movzx(ecx, word_ptr(rax))); a!(self, mov(word_ptr(r15 + o), cx)); }
                            2 => { a!(self, mov(ecx, dword_ptr(rax))); a!(self, mov(dword_ptr(r15 + o), ecx)); }
                            _ => { a!(self, mov(rcx, qword_ptr(rax))); a!(self, mov(qword_ptr(r15 + o), rcx)); }
                        }
                    } else {
                        match l {
                            0 => { a!(self, movzx(ecx, byte_ptr(r15 + o))); a!(self, mov(byte_ptr(rax), cl)); }
                            1 => { a!(self, movzx(ecx, word_ptr(r15 + o))); a!(self, mov(word_ptr(rax), cx)); }
                            2 => { a!(self, mov(ecx, dword_ptr(r15 + o))); a!(self, mov(dword_ptr(rax), ecx)); }
                            _ => { a!(self, mov(rcx, qword_ptr(r15 + o))); a!(self, mov(qword_ptr(rax), rcx)); }
                        }
                    }
                }
                K::Rep(l) => {
                    match l {
                        0 => a!(self, movzx(eax, byte_ptr(rax))),
                        1 => a!(self, movzx(eax, word_ptr(rax))),
                        2 => a!(self, mov(eax, dword_ptr(rax))),
                        _ => a!(self, mov(rax, qword_ptr(rax))),
                    }
                    self.replicate_rax(l);
                    self.vst_rax(rd, q);
                }
            }
            if post {
                self.ldx(rcx, rn, true);
                if rm == 31 {
                    a!(self, add(rcx, (1i32 << esize_log)));
                } else {
                    self.ldx(rdx, rm, false);
                    a!(self, add(rcx, rdx));
                }
                self.stx(rn, rcx, true);
            }
            return true;
        }

        // ---- LD4 / ST4 (multiple structures), 128-bit, b/h/s elements ----
        if !q && (insn & 0xBFFF_F000 == 0x0C00_0000 || insn & 0xBFA0_F000 == 0x0C80_0000) {
            let post = bit(insn, 23) != 0;
            let load = bit(insn, 22) != 0;
            if !post && bits(insn, 20, 16) != 0 {
                return false;
            }
            let size = bits(insn, 11, 10);
            if size == 3 {
                return false;
            }
            self.ldx(rax, rn, true);
            self.commpage_fix();
            let regs = [rd & 31, (rd + 1) & 31, (rd + 2) & 31, (rd + 3) & 31];
            if load {
                a!(self, movdqu(xmm0, xmmword_ptr(rax)));
                a!(self, movdqu(xmm1, xmmword_ptr(rax + 16)));
                if size < 2 {
                    let mask: u128 = if size == 0 {
                        u128::from_le_bytes([0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15])
                    } else {
                        u128::from_le_bytes([0, 1, 8, 9, 2, 3, 10, 11, 4, 5, 12, 13, 6, 7, 14, 15])
                    };
                    self.mov_imm(rdx, mask as u64);
                    a!(self, movq(xmm4, rdx));
                    self.mov_imm(rdx, (mask >> 64) as u64);
                    a!(self, movq(xmm5, rdx));
                    a!(self, punpcklqdq(xmm4, xmm5));
                    a!(self, pshufb(xmm0, xmm4));
                    a!(self, pshufb(xmm1, xmm4));
                }
                a!(self, movdqa(xmm2, xmm0));
                a!(self, punpckldq(xmm2, xmm1)); // [a.d0 b.d0 a.d1 b.d1]: v0 (lo), v1 (hi)
                a!(self, punpckhdq(xmm0, xmm1)); // [a.d2 b.d2 a.d3 b.d3]: v2 (lo), v3 (hi)
                a!(self, movdqa(xmm3, xmm2));
                a!(self, psrldq(xmm3, 8));
                a!(self, movdqa(xmm1, xmm0));
                a!(self, psrldq(xmm1, 8));
                a!(self, movq(xmm2, xmm2));
                a!(self, movq(xmm3, xmm3));
                a!(self, movq(xmm0, xmm0));
                a!(self, movq(xmm1, xmm1));
                a!(self, movdqu(xmmword_ptr(r15 + voff(regs[0])), xmm2));
                a!(self, movdqu(xmmword_ptr(r15 + voff(regs[1])), xmm3));
                a!(self, movdqu(xmmword_ptr(r15 + voff(regs[2])), xmm0));
                a!(self, movdqu(xmmword_ptr(r15 + voff(regs[3])), xmm1));
            } else {
                for i in 0..4 {
                    a!(self, movq(XMM_TMP[i], qword_ptr(r15 + voff(regs[i]))));
                }
                match size {
                    0 => {
                        a!(self, punpcklbw(xmm0, xmm1)); // t0
                        a!(self, punpcklbw(xmm2, xmm3)); // t1
                        a!(self, movdqa(xmm1, xmm0));
                        a!(self, punpcklwd(xmm0, xmm2));
                        a!(self, punpckhwd(xmm1, xmm2));
                    }
                    1 => {
                        a!(self, punpcklwd(xmm0, xmm1));
                        a!(self, punpcklwd(xmm2, xmm3));
                        a!(self, movdqa(xmm1, xmm0));
                        a!(self, punpckldq(xmm0, xmm2));
                        a!(self, punpckhdq(xmm1, xmm2));
                    }
                    _ => {
                        a!(self, punpckldq(xmm0, xmm1));
                        a!(self, punpckldq(xmm2, xmm3));
                        a!(self, movdqa(xmm1, xmm0));
                        a!(self, punpcklqdq(xmm0, xmm2));
                        a!(self, punpckhqdq(xmm1, xmm2));
                    }
                }
                a!(self, movdqu(xmmword_ptr(rax), xmm0));
                a!(self, movdqu(xmmword_ptr(rax + 16), xmm1));
            }
            if post {
                self.ldx(rcx, rn, true);
                if rm == 31 {
                    a!(self, add(rcx, 32i32));
                } else {
                    self.ldx(rdx, rm, false);
                    a!(self, add(rcx, rdx));
                }
                self.stx(rn, rcx, true);
            }
            return true;
        }
        if q && (insn & 0xBFFF_F000 == 0x0C00_0000 || insn & 0xBFA0_F000 == 0x0C80_0000) {
            let post = bit(insn, 23) != 0;
            let load = bit(insn, 22) != 0;
            if !post && bits(insn, 20, 16) != 0 {
                return false;
            }
            let size = bits(insn, 11, 10);
            if size == 3 {
                return false;
            }
            self.ldx(rax, rn, true);
            self.commpage_fix();
            let regs = [rd & 31, (rd + 1) & 31, (rd + 2) & 31, (rd + 3) & 31];
            if load {
                for i in 0..4 {
                    a!(self, movdqu(XMM_TMP[i], xmmword_ptr(rax + (16 * i) as i32)));
                }
                // xmm0..xmm3 = memory quarters a,b,c,d
                match size {
                    0 | 1 => {
                        // group each register's bytes/halves by channel: 4 groups of 4 bytes
                        let mask: u128 = if size == 0 {
                            u128::from_le_bytes([0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15])
                        } else {
                            u128::from_le_bytes([0, 1, 8, 9, 2, 3, 10, 11, 4, 5, 12, 13, 6, 7, 14, 15])
                        };
                        self.mov_imm(rdx, mask as u64);
                        a!(self, movq(xmm4, rdx));
                        self.mov_imm(rdx, (mask >> 64) as u64);
                        a!(self, movq(xmm5, rdx));
                        a!(self, punpcklqdq(xmm4, xmm5));
                        for i in 0..4 {
                            a!(self, pshufb(XMM_TMP[i], xmm4));
                        }
                    }
                    _ => {}
                }
                // dword 4x4 transpose: xmm0..3 -> channel vectors
                a!(self, movdqa(xmm4, xmm0));
                a!(self, punpckldq(xmm4, xmm1)); // s0
                a!(self, movdqa(xmm5, xmm2));
                a!(self, punpckldq(xmm5, xmm3)); // s1
                a!(self, punpckhdq(xmm0, xmm1)); // s2
                a!(self, punpckhdq(xmm2, xmm3)); // s3
                a!(self, movdqa(xmm1, xmm4));
                a!(self, punpcklqdq(xmm4, xmm5)); // v0
                a!(self, punpckhqdq(xmm1, xmm5)); // v1
                a!(self, movdqa(xmm3, xmm0));
                a!(self, punpcklqdq(xmm0, xmm2)); // v2
                a!(self, punpckhqdq(xmm3, xmm2)); // v3
                a!(self, movdqu(xmmword_ptr(r15 + voff(regs[0])), xmm4));
                a!(self, movdqu(xmmword_ptr(r15 + voff(regs[1])), xmm1));
                a!(self, movdqu(xmmword_ptr(r15 + voff(regs[2])), xmm0));
                a!(self, movdqu(xmmword_ptr(r15 + voff(regs[3])), xmm3));
            } else {
                for i in 0..4 {
                    a!(self, movdqu(XMM_TMP[i], xmmword_ptr(r15 + voff(regs[i]))));
                }
                // xmm0..3 = channels 0..3
                match size {
                    0 => {
                        a!(self, movdqa(xmm4, xmm0));
                        a!(self, punpcklbw(xmm4, xmm1)); // t0
                        a!(self, movdqa(xmm5, xmm2));
                        a!(self, punpcklbw(xmm5, xmm3)); // t1
                        a!(self, punpckhbw(xmm0, xmm1)); // t2
                        a!(self, punpckhbw(xmm2, xmm3)); // t3
                        a!(self, movdqa(xmm1, xmm4));
                        a!(self, punpcklwd(xmm4, xmm5)); // a
                        a!(self, punpckhwd(xmm1, xmm5)); // b
                        a!(self, movdqa(xmm3, xmm0));
                        a!(self, punpcklwd(xmm0, xmm2)); // c
                        a!(self, punpckhwd(xmm3, xmm2)); // d
                    }
                    1 => {
                        a!(self, movdqa(xmm4, xmm0));
                        a!(self, punpcklwd(xmm4, xmm1));
                        a!(self, movdqa(xmm5, xmm2));
                        a!(self, punpcklwd(xmm5, xmm3));
                        a!(self, punpckhwd(xmm0, xmm1));
                        a!(self, punpckhwd(xmm2, xmm3));
                        a!(self, movdqa(xmm1, xmm4));
                        a!(self, punpckldq(xmm4, xmm5));
                        a!(self, punpckhdq(xmm1, xmm5));
                        a!(self, movdqa(xmm3, xmm0));
                        a!(self, punpckldq(xmm0, xmm2));
                        a!(self, punpckhdq(xmm3, xmm2));
                    }
                    _ => {
                        a!(self, movdqa(xmm4, xmm0));
                        a!(self, punpckldq(xmm4, xmm1));
                        a!(self, movdqa(xmm5, xmm2));
                        a!(self, punpckldq(xmm5, xmm3));
                        a!(self, punpckhdq(xmm0, xmm1));
                        a!(self, punpckhdq(xmm2, xmm3));
                        a!(self, movdqa(xmm1, xmm4));
                        a!(self, punpcklqdq(xmm4, xmm5));
                        a!(self, punpckhqdq(xmm1, xmm5));
                        a!(self, movdqa(xmm3, xmm0));
                        a!(self, punpcklqdq(xmm0, xmm2));
                        a!(self, punpckhqdq(xmm3, xmm2));
                    }
                }
                a!(self, movdqu(xmmword_ptr(rax), xmm4));
                a!(self, movdqu(xmmword_ptr(rax + 16), xmm1));
                a!(self, movdqu(xmmword_ptr(rax + 32), xmm0));
                a!(self, movdqu(xmmword_ptr(rax + 48), xmm3));
            }
            if post {
                self.ldx(rcx, rn, true);
                if rm == 31 {
                    a!(self, add(rcx, 64i32));
                } else {
                    self.ldx(rdx, rm, false);
                    a!(self, add(rcx, rdx));
                }
                self.stx(rn, rcx, true);
            }
            return true;
        }

        false
    }
}

const XMM_TMP: [AsmRegisterXmm; 4] = [xmm0, xmm1, xmm2, xmm3];

#[cfg(test)]
mod direct_branch_tests {
    use super::*;
    use attesor_core::cpu::Cpu;

    fn compile(word: u32, pc: u64, internal: bool) -> u64 {
        let mut u = Unit::new(StubKind::Plain, false);
        u.no_internal_jumps = !internal;
        let label = u.a.create_label();
        u.labels.insert(pc, label);
        let displacement = if word & 0x7e00_0000 == 0x3400_0000 {
            sext(bits(word, 23, 5) as u64, 19)
        } else {
            sext(bits(word, 18, 5) as u64, 14)
        };
        let target = pc.wrapping_add((displacement << 2) as u64);
        if internal {
            for address in [target, pc + 4] {
                let label = u.a.create_label();
                u.labels.insert(address, label);
            }
        }
        emit_block(&mut u, pc, &|address| if address == pc { Some(word) } else { None });
        if !jit_off("directbranch") {
            assert!(!u.a.instructions().iter().any(|i| matches!(i.mnemonic(),
                iced_x86::Mnemonic::Sete | iced_x86::Mnemonic::Setne |
                iced_x86::Mnemonic::Setb | iced_x86::Mnemonic::Setae |
                iced_x86::Mnemonic::Test)));
        }
        if internal {
            for address in [target, pc + 4] {
                let mut label = u.labels[&address];
                u.a.set_label(&mut label).unwrap();
                let mut e = E { u: &mut u, pc: address, fuse_in: None, scalar_inputs: None };
                e.st_imm(9, address);
                e.st_pc_and_exit(address);
            }
        }
        u.finish();
        let host = crate::jit::code_cache().alloc(4096);
        let code = u.a.assemble(host).unwrap();
        assert!(code.len() <= 4096);
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), host as *mut u8, code.len()); }
        host
    }

    #[test]
    fn compare_and_test_branches_preserve_nzcv_and_all_registers() {
        const PC: u64 = 0x1000_0000;
        let values = [0, 1, u64::MAX, 1 << 31, 1 << 32, 1 << 63,
                      0xffff_ffff_0000_0000, 0x5555_5555_aaaa_aaaa];
        let mut cpu = Cpu::new();
        crate::jit::setup_cpu(&mut cpu);
        // Both signed displacement extremes, negative, fallthrough, and self.
        for tb in [false, true] {
            let width_or_bits: Vec<u32> = if tb { (0..64).collect() } else { vec![0, 1] };
            for width_or_bit in width_or_bits {
                for nz in [false, true] {
                    for rt in [0, 7, 31] {
                        let offsets = if tb { [-8192, -1, 0, 1, 8191] }
                                      else { [-262144, -1, 0, 1, 262143] };
                        for displacement in offsets {
                            let word = if tb {
                                0x3600_0000 | ((width_or_bit >> 5) << 31) |
                                ((width_or_bit & 31) << 19) | ((nz as u32) << 24) |
                                (((displacement as u32) & 0x3fff) << 5) | rt
                            } else {
                                0x3400_0000 | (width_or_bit << 31) | ((nz as u32) << 24) |
                                (((displacement as u32) & 0x7ffff) << 5) | rt
                            };
                            let host = compile(word, PC, false);
                            for value in values {
                                for flags in 0..16 {
                                    cpu.x = std::array::from_fn(|i| 0xfedc_ba98_7654_3210 ^ i as u64);
                                    cpu.x[rt as usize] = value;
                                    let original = cpu.x;
                                    cpu.set_nzcv(flags);
                                    cpu.set_pc(PC);
                                    cpu.exit_reason = 0;
                                    let input = if rt == 31 { 0 } else { value };
                                    let nonzero = if tb { (input >> width_or_bit) & 1 != 0 }
                                        else if width_or_bit != 0 { input != 0 } else { input as u32 != 0 };
                                    let expected = if nonzero == nz {
                                        PC.wrapping_add((displacement as i64 * 4) as u64)
                                    } else { PC + 4 };
                                    unsafe { crate::jit::attesor_jit_enter(&mut *cpu, host); }
                                    assert_eq!(cpu.pc(), expected, "word={word:08x} value={value:x}");
                                    assert_eq!(cpu.nzcv(), flags, "word={word:08x}");
                                    assert_eq!(cpu.x, original, "word={word:08x}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn direct_conditions_reach_both_internal_successor_labels() {
        const PC: u64 = 0x1000_0000;
        let mut cpu = Cpu::new();
        crate::jit::setup_cpu(&mut cpu);
        for (word, tested_bit) in [(0xb4ff_ff00u32, None), (0xb5ff_ff00, None),
                            (0xb6ff_ff00, Some(63)), (0xb7ff_ff00, Some(63))] {
            // Every instruction branches -32 bytes, keeping target and next distinct.
            let word = (word & !0x0007_ffe0) | ((0x3ff8u32 & 0x3fff) << 5);
            let word = if tested_bit.is_none() { (word & !0x00ff_ffe0) | (0x7fff8 << 5) } else { word };
            let host = compile(word, PC, true);
            for value in [0, 1, 1u64 << 63] {
                cpu.x[0] = value;
                cpu.x[9] = 0;
                cpu.set_nzcv(0b1011);
                let nz = bit(word, 24) != 0;
                let nonzero = if let Some(b) = tested_bit { value & (1 << b) != 0 } else { value != 0 };
                let expected = if nonzero == nz { PC - 32 } else { PC + 4 };
                unsafe { crate::jit::attesor_jit_enter(&mut *cpu, host); }
                assert_eq!(cpu.pc(), expected);
                assert_eq!(cpu.x[9], expected);
                assert_eq!(cpu.nzcv(), 0b1011);
            }
        }
    }
}

#[cfg(test)]
mod tbi_mask_tests {
    use super::*;
    use attesor_core::cpu::Cpu;

    // Execute both sides of a join with r11 poisoned before entry. A mask first
    // loaded by a skipped path cannot be trusted by the following instruction.
    // Also cover a conditional scratch write that invalidates an earlier mask.
    #[test]
    fn conditional_mask_load_and_scratch_join_execute_correctly() {
        for initial_mask in [false, true] {
            let mut unit = Unit::new(StubKind::Plain, false);
            {
                let mut e = E { u: &mut unit, pc: 0, fuse_in: None, scalar_inputs: None };
                e.mov_imm(r11, 0xabab_abab_abab_abab);
                if initial_mask {
                    a!(e, mov(rax, qword_ptr(r15 + xoff(1))));
                    e.commpage_fix();
                }
                let mut skip = e.new_label();
                a!(e, test(qword_ptr(r15 + xoff(0)), 1));
                a!(e, jz(skip));
                if initial_mask {
                    e.st_imm(3, 0x1234_5678_9abc_def0);
                } else {
                    a!(e, mov(rax, qword_ptr(r15 + xoff(1))));
                    e.commpage_fix();
                }
                e.u.a.set_label(&mut skip).unwrap();
                a!(e, nop());
                // A new guest instruction begins after the internal join.
                e.u.rc_frozen = false;
                a!(e, mov(rax, qword_ptr(r15 + xoff(1))));
                e.commpage_fix();
                a!(e, mov(rcx, qword_ptr(rax)));
                e.stx(2, rcx, false);
                e.st_pc_and_exit(0);
            }
            unit.finish();
            let cache = crate::jit::code_cache();
            let host = cache.alloc(4096);
            let bytes = unit.a.assemble(host).unwrap();
            assert!(bytes.len() <= 4096);
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), host as *mut u8, bytes.len()); }
            let expected = 0xfeed_face_1122_3344u64;
            for branch in [0, 1] {
                let mut cpu = Cpu::new();
                cpu.x[0] = branch;
                cpu.x[1] = (&expected as *const u64 as u64) | 0xab00_0000_0000_0000;
                crate::jit::setup_cpu(&mut cpu);
                unsafe { crate::jit::attesor_jit_enter(&mut *cpu, host); }
                assert_eq!(cpu.x[2], expected, "initial_mask={initial_mask} branch={branch}");
            }
        }
    }
}

#[cfg(test)]
mod cache_nextuse_tests {
    use super::*;

    #[test]
    fn masks_separate_banks_and_discarded_old_values() {
        // MOVZ X3,#3 is a write, not an old-value use through Rd or imm16 bits.
        let movz = 0xd2800063;
        let a = cache_access(movz);
        assert_eq!((a.xr, a.xw, a.vr, a.vw), (0, 1 << 3, 0, 0));
        assert_eq!(next_cache_use(&[movz, 0x8b030020], 3, false), None);
        // ADD X3,X3,X4 consumes X3 before overwriting it.
        assert_eq!(next_cache_use(&[0x8b040063], 3, false), Some(0));
        let fadd = cache_access(0x1e622820); // FADD D0,D1,D2
        assert_eq!((fadd.xr, fadd.xw), (0, 0));
        assert_eq!((fadd.vr, fadd.vw), ((1 << 1) | (1 << 2), 1));
        assert_eq!(next_cache_use(&[0x1e622820, movz], 3, false), None);
        let store = cache_access(0xf90017e3); // STR X3,[SP,#40]
        assert_eq!(store.xr, (1 << 31) | (1 << 3));
        assert_eq!(store.xw, 0);
        let load = cache_access(0xf94017e3);
        assert_eq!((load.xr, load.xw), (1 << 31, 1 << 3));
        let unknown = cache_access(0xffff_ffff);
        assert_eq!((unknown.xr, unknown.vr), (!0, !0));
        assert_eq!((unknown.xw, unknown.vw), (0, 0));
    }

    #[test]
    fn eviction_prefers_dead_then_furthest_not_oldest() {
        if jit_off("nextuse") { return; } // caller can test the old policy separately
        let mut u = Unit::new(StubKind::Plain, false);
        u.rc.reg = [0, 1, 2, 3, 4];
        u.rc.stamp = [1, 2, 3, 4, 5];
        // X0 is oldest but consumed immediately; X1 and X2 are not consumed.
        u.look = vec![0x8b04000a, 0x8b03014b];
        u.cache_uses = cache_next_uses(&u.look);
        let mut e = E { u: &mut u, pc: 0, fuse_in: None, scalar_inputs: None };
        assert_eq!(e.rc_alloc(9), 1); // oldest dead slot (X1), not live X0
        e.u.vc.reg = [0, 1, 2, 3, 4, 5, 6, 7];
        e.u.vc.stamp = [1, 2, 3, 4, 5, 6, 7, 8];
        e.u.look = vec![0x1e622820]; // V1/V2 read now, V0 written
        e.u.cache_uses = cache_next_uses(&e.u.look);
        assert_eq!(e.vc_alloc(9), 0); // old V0 is killed; bank-local decision
        e.u.rc.reg = [0, 1, 2, 3, 4];
        e.u.rc.stamp = [1, 2, 3, 4, 5];
        e.u.look = vec![0x8b01000a, 0x8b03004b, 0x8b05008c];
        e.u.cache_uses = cache_next_uses(&e.u.look);
        assert_eq!(e.rc_alloc(9), 4); // newest slot, but used furthest away
        e.u.vc.reg = [0, 1, 2, 3, 4, 5, 6, 7];
        e.u.vc.stamp = [1, 2, 3, 4, 5, 6, 7, 8];
        e.u.look = (0..8).map(|r| 0x1e60281f | (r << 5) | (r << 16)).collect();
        e.u.cache_uses = cache_next_uses(&e.u.look);
        assert_eq!(e.vc_alloc(9), 7);
    }
}

#[cfg(test)]
mod context_sync_tests {
    use super::*;
    use attesor_core::cpu::Cpu;

    #[test]
    fn isb_exits_before_internal_successor_and_cannot_chain() {
        const PC: u64 = 0x1000_0000;
        for kind in [StubKind::Plain, StubKind::Patchable] {
            for option in 0..16 {
                let isb = 0xd503_30df | (option << 8);
                assert!(is_terminator(isb));
                assert!(!is_terminator(0xd503_30ff)); // SB remains distinct
                let words = [isb, 0x9100_0400, 0xd400_1001]; // ISB; ADD X0,X0,#1; SVC
                let mut u = Unit::new(kind, false);
                let entry = u.a.create_label();
                let successor = u.a.create_label();
                u.labels.insert(PC, entry);
                u.labels.insert(PC + 4, successor);
                let read = |pc: u64| words.get(((pc - PC) / 4) as usize).copied();
                assert_eq!(emit_block(&mut u, PC, &read), PC + 4);
                assert!(u.stubs.is_empty(), "ISB must not produce a chainable exit");
                emit_block(&mut u, PC + 4, &read);
                u.finish();
                let host = crate::jit::code_cache().alloc(4096);
                let bytes = u.a.assemble(host).unwrap();
                assert!(bytes.len() <= 4096);
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), host as *mut u8, bytes.len()); }
                let mut cpu = Cpu::new();
                crate::jit::setup_cpu(&mut cpu);
                cpu.x[0] = 42;
                cpu.set_nzcv(0b1011);
                cpu.exit_data = 0xfeed;
                unsafe { crate::jit::attesor_jit_enter(&mut *cpu, host); }
                assert_eq!(cpu.pc(), PC + 4);
                assert_eq!(cpu.x[0], 42, "must not execute the already-emitted internal successor");
                assert_eq!(cpu.nzcv(), 0b1011);
                assert_eq!(cpu.exit_reason, EXIT_CONTINUE);
                assert_eq!(cpu.exit_data, 0);
            }
        }
    }
}

#[cfg(test)]
mod region_cache_tests {
    use super::*;
    use attesor_core::{cpu::Cpu, interp};
    const PC: u64 = 0x1000_0000;

    fn compile(words: &[u32], entries: &[usize], kind: StubKind, enabled: bool) -> (Vec<u64>, bool) {
        let mut u = Unit::new(kind, false);
        u.regioncache = enabled;
        for &i in entries { let l = u.a.create_label(); u.labels.insert(PC + i as u64 * 4, l); }
        let read = |p: u64| p.checked_sub(PC).and_then(|o| words.get((o / 4) as usize)).copied();
        let mut loop_found = false;
        for &i in entries {
            emit_block(&mut u, PC + i as u64 * 4, &read);
            loop_found |= u.loop_cache.is_some();
        }
        u.finish();
        let host = crate::jit::code_cache().alloc(65536);
        let result = u.a.assemble_options(host, iced_x86::BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
        let hosts = entries.iter().map(|&i| result.label_ip(&u.labels[&(PC + i as u64 * 4)]).unwrap()).collect();
        assert!(result.inner.code_buffer.len() <= 65536);
        unsafe { std::ptr::copy_nonoverlapping(result.inner.code_buffer.as_ptr(), host as *mut u8, result.inner.code_buffer.len()); }
        (hosts, loop_found)
    }

    fn reference(cpu: &mut Cpu, words: &[u32]) {
        for _ in 0..10000 {
            let w = words[((cpu.pc() - PC) / 4) as usize];
            match interp::exec(cpu, w) {
                Ok(()) => {},
                Err(interp::Exit::Svc(_)) => return,
                Err(e) => panic!("reference {w:08x}: {e:?}"),
            }
        }
        panic!("nonterminating reference");
    }

    #[test]
    fn loop_randomized_public_entries_write_through_and_sp_alias() {
        // add x0,x0,x1; eor x2,x2,x0; add sp,sp,#16;
        // add x4,sp,#0; add x5,xzr,x0; subs x3,x3,#1; b.ne loop; svc
        let words = [0x8b010000, 0xca000042, 0x910043ff, 0x910003e4,
                     0x8b0003e5, 0xf1000463, 0x54ffff41, 0xd4001001];
        let mut seed = 0x123456789abcdefu64;
        for kind in [StubKind::Plain, StubKind::Patchable] {
            let (hosts, found) = compile(&words, &[0, 7], kind, true);
            assert!(found);
            // Every public entry gets fresh host registers through jit_enter; no
            // assumptions about CREG contents or a preceding region invocation.
            for entry in 0..2 {
                for _ in 0..128 {
                    let mut c = Cpu::new(); crate::jit::setup_cpu(&mut c);
                    for x in &mut c.x { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; *x = seed; }
                    c.x[3] = seed % 35 + 1; c.set_nzcv((seed % 16) as u32);
                    c.set_pc(PC + if entry == 0 { 0 } else { 28 });
                    let mut r = Cpu::new(); r.x = c.x; r.set_nzcv(c.nzcv()); r.set_pc(c.pc());
                    reference(&mut r, &words);
                    unsafe { crate::jit::attesor_jit_enter(&mut *c, hosts[entry]); }
                    assert_eq!(c.exit_reason, EXIT_SVC); assert_eq!(c.x, r.x); assert_eq!(c.nzcv(), r.nzcv());
                }
            }
        }
    }

    #[test]
    fn compare_and_test_bit_loop_edges_match_reference() {
        for test_bit in [false, true] {
            let words = [0x8b010000, if test_bit { 0x91000463 } else { 0x51000463 },
                         if test_bit { 0x3637ffc3 } else { 0x35ffffc3 }, 0xd4001001];
            let (hosts, found) = compile(&words, &[0, 3], StubKind::Plain, true); assert!(found);
            for count in 1..64 {
                let mut c = Cpu::new(); crate::jit::setup_cpu(&mut c);
                c.x[0] = u64::MAX - count; c.x[1] = 17; c.x[3] = count;
                c.x[31] = 0x123456789abcdef0; c.set_nzcv(11); c.set_pc(PC);
                let mut r = Cpu::new(); r.x = c.x; r.set_nzcv(c.nzcv()); r.set_pc(PC);
                reference(&mut r, &words);
                unsafe { crate::jit::attesor_jit_enter(&mut *c, hosts[0]); }
                assert_eq!(c.x, r.x); assert_eq!(c.nzcv(), r.nzcv()); assert_eq!(c.exit_reason, EXIT_SVC);
            }
        }
    }

    extern "C" fn mutate(cpu: *mut Cpu, _: u32, _: u64) -> u64 {
        let c = unsafe { &mut *cpu };
        // This sees the immediately preceding native write, then invalidates it.
        assert_eq!(c.x[0], c.x[6] + 1);
        c.x[0] += 100; c.x[6] = c.x[0];
        c.x[1] = c.x[1].wrapping_mul(3);
        0
    }

    #[test]
    fn helper_unknown_writes_reconcile_and_conditional_fallthrough() {
        let words = [0x91000400, 0, 0x8b010042, 0xf1000463, 0x54ffff81, 0x8b000044, 0xd4001001];
        let (hosts, found) = compile(&words, &[0, 5], StubKind::Plain, true); assert!(found);
        for count in 1..32 {
            let mut c = Cpu::new(); crate::jit::setup_cpu(&mut c);
            c.helpers[HELPER_INTERP] = mutate as *const () as usize as u64;
            c.x[1] = 1; c.x[3] = count;
            unsafe { crate::jit::attesor_jit_enter(&mut *c, hosts[0]); }
            let (mut a, mut b, mut sum) = (0u64, 1u64, 0u64);
            for _ in 0..count { a += 101; b = b.wrapping_mul(3); sum = sum.wrapping_add(b); }
            assert_eq!(c.x[0], a); assert_eq!(c.x[1], b); assert_eq!(c.x[2], sum);
            assert_eq!(c.x[4], sum.wrapping_add(a)); assert_eq!(c.exit_reason, EXIT_SVC);
        }
    }

    extern "C" fn change_fpcr(cpu: *mut Cpu, _: u32, _: u64) -> u64 {
        unsafe { (*cpu).fpcr = 1 << 25; } 0
    }

    #[test]
    fn backedge_does_not_skip_fpcr_guard_or_reuse_stale_vectors() {
        // fadd d0,d0,d1; helper changes FPCR; b loop. First iteration is native,
        // second must leave at the guard, before another FP write.
        let words = [0x91000400, 0x1e612800, 0, 0x17fffffd];
        let (hosts, found) = compile(&words, &[0], StubKind::Plain, true); assert!(found);
        let mut c = Cpu::new(); crate::jit::setup_cpu(&mut c);
        c.v[0][0] = 2f64.to_bits(); c.v[1][0] = 3f64.to_bits();
        c.helpers[HELPER_INTERP] = change_fpcr as *const () as usize as u64;
        unsafe { crate::jit::attesor_jit_enter(&mut *c, hosts[0]); }
        assert_eq!(c.exit_reason, EXIT_INTERP); assert_eq!(c.pc(), PC);
        assert_eq!(c.v[0][0], 5f64.to_bits());
    }

    #[test]
    fn diamond_and_external_join_use_canonical_entries() {
        // Non-self edges deliberately do not participate in this first region.
        let words = [0xb4000040, 0x91000421, 0x91000442, 0xd4001001];
        let (hosts, found) = compile(&words, &[0, 1, 2], StubKind::Plain, true); assert!(!found);
        for entry in 0..3 { for x in [0, 1] {
            let mut c = Cpu::new(); crate::jit::setup_cpu(&mut c); c.x[0] = x; c.set_pc(PC + entry as u64 * 4);
            let mut r = Cpu::new(); r.x = c.x; r.set_pc(c.pc());
            reference(&mut r, &words);
            unsafe { crate::jit::attesor_jit_enter(&mut *c, hosts[entry]); }
            assert_eq!(c.x, r.x); assert_eq!(c.exit_reason, EXIT_SVC);
        }}
    }
    #[test]
    fn loop_scalar_aliases_match_reference_across_iterations() {
        let words = [0x1e612800, 0x1e600821, 0xf1000463, 0x54ffffa1, 0xd4001001];
        let (hosts, found) = compile(&words, &[0, 4], StubKind::Plain, true); assert!(found);
        for count in 1..40 {
            let mut c = Cpu::new(); crate::jit::setup_cpu(&mut c);
            c.x[3] = count; c.set_pc(PC);
            c.v[0] = [0.125f64.to_bits(), u64::MAX]; c.v[1] = [0.75f64.to_bits(), u64::MAX];
            let mut r = Cpu::new(); r.x = c.x; r.v = c.v; r.set_pc(PC);
            reference(&mut r, &words);
            unsafe { crate::jit::attesor_jit_enter(&mut *c, hosts[0]); }
            assert_eq!(c.x, r.x); assert_eq!(c.v, r.v); assert_eq!(c.nzcv(), r.nzcv());
        }
    }

    #[test]
    fn verification_and_other_entries_never_publish_private_contract() {
        let words = [0x91000400, 0xf1000463, 0x54ffffc1, 0xd4001001];
        let mut u = Unit::new(StubKind::Plain, false); u.regioncache = true; u.no_internal_jumps = true;
        let l = u.a.create_label(); u.labels.insert(PC, l);
        emit_block(&mut u, PC, &|p| words.get(((p - PC) / 4) as usize).copied());
        assert!(u.loop_cache.is_none());
        assert_eq!(u.labels.len(), 1);
        let (_, found) = compile(&words, &[0, 3], StubKind::Plain, false); assert!(!found);
    }

    extern "C" fn fault_exit(_: libc::c_int) { unsafe { libc::_exit(73); } }

    #[test]
    fn fault_after_backedge_observes_every_completed_cpu_write() {
        // The second iteration loads a protected page. All completed writes,
        // including the increment directly before the fault, must be in Cpu.
        let words = [0x91000400, 0xf9400025, 0x91002021, 0xf1000463, 0x54ffff81, 0xd4001001];
        let (hosts, found) = compile(&words, &[0, 5], StubKind::Plain, true); assert!(found);
        unsafe {
            let page = libc::sysconf(libc::_SC_PAGESIZE) as usize;
            let data = libc::mmap(std::ptr::null_mut(), page * 2, libc::PROT_READ | libc::PROT_WRITE,
                                  libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0);
            assert_ne!(data, libc::MAP_FAILED);
            let c = libc::mmap(std::ptr::null_mut(), page, libc::PROT_READ | libc::PROT_WRITE,
                              libc::MAP_ANON | libc::MAP_SHARED, -1, 0) as *mut Cpu;
            assert_ne!(c as *mut libc::c_void, libc::MAP_FAILED);
            std::ptr::write(c, *Cpu::new()); crate::jit::setup_cpu(&mut *c);
            (*c).x[0] = 99; (*c).x[1] = data as u64 + page as u64 - 8; (*c).x[3] = 3;
            *((*c).x[1] as *mut u64) = 0x123456789abcdef0;
            assert_eq!(libc::mprotect((data as *mut u8).add(page).cast(), page, libc::PROT_NONE), 0);
            let pid = libc::fork(); assert!(pid >= 0);
            if pid == 0 {
                libc::signal(libc::SIGSEGV, fault_exit as *const () as libc::sighandler_t);
                libc::signal(libc::SIGBUS, fault_exit as *const () as libc::sighandler_t);
                crate::jit::attesor_jit_enter(c, hosts[0]); libc::_exit(99);
            }
            let mut status = 0; assert_eq!(libc::waitpid(pid, &mut status, 0), pid);
            assert!(libc::WIFEXITED(status)); assert_eq!(libc::WEXITSTATUS(status), 73);
            assert_eq!((*c).x[0], 101); assert_eq!((*c).x[1], data as u64 + page as u64);
            assert_eq!((*c).x[3], 2); assert_eq!((*c).x[5], 0x123456789abcdef0);
            libc::munmap(c.cast(), page); libc::munmap(data, page * 2);
        }
    }

}

#[cfg(test)]
mod flags_liveness_tests {
    use super::*;
    use attesor_core::cpu::Cpu;

    fn dead(words: &[u32]) -> bool {
        flags_dead_at(0x1000, &|pc| words.get(((pc - 0x1000) / 4) as usize).copied())
    }

    #[test]
    fn flags_scan_stops_before_faults_helpers_and_readers() {
        const WRITE: u32 = 0xf100_041f; // CMP X0, #1
        for boundary in [0,           // undefined -> interpreter helper
                         0xd400_1001, // SVC
                         0x9a02_0020, // ADC X0, X1, X2 consumes C
                         0xda02_0020, // SBC X0, X1, X2 consumes C
                         0x9a84_0062, // CSEL X2, X3, X4, EQ
                         0xfa40_0000, // CCMP consumes conditional NZCV
                         0xd53b_4200, // MRS X0, NZCV
                         0xd503_3fdf, // ISB dispatcher boundary
                         0x12c0_0000, // reserved 32-bit MOV halfword
                         0x32c0_0000, // reserved MOV opcode
                         0x7200_fc00, // reserved logical immediate
                         0x2bc0_0000, // reserved shifted SUBS
                         0x2b00_8000] // invalid 32-bit shift amount
        {
            assert!(!dead(&[boundary, WRITE]), "boundary={boundary:08x}");
        }
        assert!(dead(&[0xd503_201f, 0x9100_0401, 0xd280_0022, WRITE]));
        // non-faulting data processing: EOR, MUL, AND #0xff, LSL, UDIV, CLZ, and (faults being fatal) LDRB/STR
        assert!(dead(&[0xca01_0000, 0x9b02_7c20, 0x1200_1c00, 0xd37d_f000, 0x9ac2_0820, 0xdac0_1000, WRITE]));
        assert!(GUEST_FAULTS_FATAL && dead(&[0x3840_158d, 0xf900_0020, WRITE]));
        assert!(!dead(&[0xd503_201f])); // no proof past block/readable end
        assert!(!dead(&[0xd65f_03c0, WRITE])); // RET: flags live at exit
    }

    extern "C" fn observing_exit(cpu: *mut Cpu, insn: u32, pc: u64) -> u64 {
        let cpu = unsafe { &mut *cpu };
        if insn != 0 {
            // The debug dpreg switch routes CSEL here. It still must receive
            // the same architectural flags as its native implementation.
            cpu.set_pc(pc);
            return if attesor_core::interp::exec(cpu, insn).is_ok() { 0 } else { 1 };
        }
        cpu.x[10] = cpu.nzcv() as u64;
        cpu.set_pc(pc);
        cpu.exit_reason = EXIT_PENDING;
        1
    }

    #[test]
    fn helper_exit_observes_preceding_cmp_even_before_later_overwrite() {
        const PC: u64 = 0x1000_0000;
        let words = [0xf100_041f, // CMP X0,#1
                     0x9a84_0062, // CSEL X2,X3,X4,EQ (fused)
                     0,           // unknown helper may exit
                     0xf100_081f, // CMP X0,#2: never reached
                     0xd400_1001];
        let mut u = Unit::new(StubKind::Plain, false);
        let label = u.a.create_label();
        u.labels.insert(PC, label);
        emit_block(&mut u, PC, &|pc| words.get(((pc-PC)/4) as usize).copied());
        u.finish();
        let host = crate::jit::code_cache().alloc(4096);
        let code = u.a.assemble(host).unwrap();
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), host as *mut u8, code.len()); }
        let mut cpu = Cpu::new();
        crate::jit::setup_cpu(&mut cpu);
        cpu.helpers[HELPER_INTERP] = observing_exit as usize as u64;
        for value in [0u64, 1, 2, 1 << 63, u64::MAX] {
            // Independent integer subtraction reference, including borrow and signed overflow.
            let result = value.wrapping_sub(1);
            let expected = (((result >> 63) << 3) | (((result == 0) as u64) << 2)
                | (((value >= 1) as u64) << 1) | (((value ^ 1) & (value ^ result)) >> 63)) as u32;
            cpu.x[0] = value;
            cpu.x[3] = 77;
            cpu.x[4] = 88;
            cpu.set_nzcv(expected ^ 15);
            unsafe { crate::jit::attesor_jit_enter(&mut *cpu, host); }
            assert_eq!(cpu.x[10], expected as u64, "value={value:x}");
            assert_eq!(cpu.nzcv(), expected);
            assert_eq!(cpu.x[2], if value == 1 { 77 } else { 88 });
            assert_eq!(cpu.pc(), PC + 8);
            assert_eq!(cpu.exit_reason, EXIT_PENDING);
        }
    }
}

#[cfg(test)]
mod fsqrt_dn_tests {
    use super::*;
    use attesor_core::{cpu::Cpu, interp};

    #[test]
    fn dn_guard_fallback_agrees_with_arm_bits_for_all_sqrt_forms() {
        const PC: u64 = 0x2000_0000;
        for (word, width, lanes, dnan, quiet, inputs) in [
            (0x1ee1c020, 16, 1, 0x7e00u64, 0x0200, [0x7e34u64, 0xfe34, 0x7c34, 0xfc34]),
            (0x2ef9f820, 16, 4, 0x7e00, 0x0200, [0x7e34, 0xfe34, 0x7c34, 0xfc34]),
            (0x6ef9f820, 16, 8, 0x7e00, 0x0200, [0x7e34, 0xfe34, 0x7c34, 0xfc34]),
            (0x1e21c020, 32, 1, 0x7fc00000, 0x00400000, [0x7fc01234, 0xffc01234, 0x7f801234, 0xff801234]),
            (0x2ea1f820, 32, 2, 0x7fc00000, 0x00400000, [0x7fc01234, 0xffc01234, 0x7f801234, 0xff801234]),
            (0x6ea1f820, 32, 4, 0x7fc00000, 0x00400000, [0x7fc01234, 0xffc01234, 0x7f801234, 0xff801234]),
            (0x1e61c020, 64, 1, 0x7ff8000000000000, 0x0008000000000000,
             [0x7ff8000000001234, 0xfff8000000001234, 0x7ff0000000001234, 0xfff0000000001234]),
            (0x6ee1f820, 64, 2, 0x7ff8000000000000, 0x0008000000000000,
             [0x7ff8000000001234, 0xfff8000000001234, 0x7ff0000000001234, 0xfff0000000001234]),
        ] {
            let mut u = Unit::new(StubKind::Plain, false);
            for p in [PC, PC + 4] { let l = u.a.create_label(); u.labels.insert(p, l); }
            let read = |p| if p == PC { Some(word) } else if p == PC + 4 { Some(0xd4001001) } else { None };
            emit_block(&mut u, PC, &read); emit_block(&mut u, PC + 4, &read); u.finish();
            let host = crate::jit::code_cache().alloc(16384);
            let result = u.a.assemble_options(host, iced_x86::BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
            let entry = result.label_ip(&u.labels[&PC]).unwrap();
            let next = result.label_ip(&u.labels[&(PC + 4)]).unwrap();
            assert!(result.inner.code_buffer.len() <= 16384);
            unsafe { std::ptr::copy_nonoverlapping(result.inner.code_buffer.as_ptr(), host as *mut u8, result.inner.code_buffer.len()); }
            for input in inputs { for dn in [false, true] {
                let expected = if dn { dnan } else { input | quiet };
                let (mut source, mut want) = (0u128, 0u128);
                for lane in 0..lanes { source |= (input as u128) << (lane*width); want |= (expected as u128) << (lane*width); }
                let mut c = Cpu::new(); crate::jit::setup_cpu(&mut c); c.set_pc(PC);
                c.fpcr = if dn { 1 << 25 } else { 0 }; let mode = c.fpcr;
                c.set_vq(1, source); c.set_vq(0, u128::MAX);
                unsafe { crate::jit::attesor_jit_enter(&mut *c, entry); }
                if dn {
                    assert_eq!(c.exit_reason, EXIT_INTERP); assert_eq!(c.pc(), PC);
                    assert_eq!(c.vq(0), u128::MAX); assert_eq!(c.vq(1), source);
                    interp::exec(&mut c, word).unwrap();
                    unsafe { crate::jit::attesor_jit_enter(&mut *c, next); }
                }
                assert_eq!(c.exit_reason, EXIT_SVC);
                assert_eq!(c.vq(0), want, "word={word:08x} input={input:x} dn={dn}");
                assert_eq!(c.fpcr, mode);
            }}
        }
    }
}

#[cfg(test)]
mod fpcr_boundary_tests {
    use super::*;
    use attesor_core::cpu::Cpu;

    #[test]
    fn fpcr_write_exits_before_all_guarded_fp_successors() {
        const PC: u64 = 0x3000_0000;
        for r in 0..32 { assert!(is_terminator(0xd51b4400 | r)); }
        assert!(!is_terminator(0xd51b4420)); // MSR FPSR is distinct
        assert!(!is_terminator(0xd53b4400)); // MRS FPCR does not change mode
        for kind in [StubKind::Plain, StubKind::Patchable] {
            // Scalar sqrt, scalar add, vector add, fused scalar arithmetic.
            for fp in [0x1e61c020, 0x1e612800, 0x4e21d420, 0x1f410800] {
                for rt in [0, 7, 31] {
                    let words = [0x91000529, 0xd51b4400 | rt, fp, 0x91000529, 0xd4001001];
                    let mut u = Unit::new(kind, false);
                    let entry = u.a.create_label(); u.labels.insert(PC, entry);
                    let read = |p| if p >= PC { words.get(((p-PC)/4) as usize).copied() } else { None };
                    let end = emit_block(&mut u, PC, &read);
                    // Add the next public entry only after emitting the first
                    // block: its label cannot accidentally provide the boundary.
                    let next = u.a.create_label(); u.labels.insert(PC+8, next);
                    let unchainable = u.stubs.is_empty();
                    emit_block(&mut u, PC+8, &read); u.finish();
                    let host = crate::jit::code_cache().alloc(16384);
                    let result = u.a.assemble_options(host, iced_x86::BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
                    let first = result.label_ip(&u.labels[&PC]).unwrap();
                    let second = result.label_ip(&u.labels[&(PC+8)]).unwrap();
                    assert!(result.inner.code_buffer.len() <= 16384);
                    unsafe { std::ptr::copy_nonoverlapping(result.inner.code_buffer.as_ptr(),host as *mut u8,result.inner.code_buffer.len()); }
                    for mode in [0, 1<<25, 1<<26, 1<<19] { for initial in [0, 1<<25] {
                        let mut c = Cpu::new(); crate::jit::setup_cpu(&mut c);
                        c.set_pc(PC); c.fpcr = initial; c.x[9] = 42; c.set_nzcv(11);
                        if rt != 31 { c.x[rt as usize] = mode; }
                        c.v[0] = [0xfff8000000001234, 17]; c.v[1] = [0xfff8000000004321, 19];
                        let original = c.v; let expected_mode = if rt == 31 { 0 } else { mode };
                        unsafe { crate::jit::attesor_jit_enter(&mut *c, first); }
                        assert_eq!(c.exit_reason, EXIT_CONTINUE, "fp={fp:08x} mode={mode:x} initial={initial:x}");
                        assert_eq!(end, PC+8); assert!(unchainable);
                        assert_eq!(c.pc(), PC+8); assert_eq!(c.fpcr, expected_mode);
                        assert_eq!(c.x[9],43); assert_eq!(c.v, original); assert_eq!(c.nzcv(),11);
                        unsafe { crate::jit::attesor_jit_enter(&mut *c, second); }
                        if expected_mode & FPCR_NONDEFAULT as u64 != 0 {
                            assert_eq!(c.exit_reason, EXIT_INTERP); assert_eq!(c.pc(),PC+8);
                            assert_eq!(c.x[9],43); assert_eq!(c.v,original);
                        } else {
                            assert_eq!(c.exit_reason,EXIT_SVC); assert_eq!(c.x[9],44);
                        }
                    }}
                }
            }
        }
    }
}

#[cfg(test)]
mod fp_cold_tests {
    use super::*;
    use attesor_core::{cpu::Cpu, interp};

    #[test]
    fn register_checker_rejects_aliases_and_implicit_writes() {
        let mut a = CodeAssembler::new(64).unwrap();
        a.mov(eax, 1).unwrap();
        a.movdqa(xmm7, xmm8).unwrap(); // cache reads are allowed
        assert!(check_cold_registers(a.instructions(), true).is_ok());
        a.mov(r11d, 1).unwrap();
        assert!(check_cold_registers(a.instructions(), false).is_ok());
        assert!(check_cold_registers(a.instructions(), true).is_err());
        for reg in [ebx, ebp, r12d, r13d, r14d, r15d, esp, edi, esi, r8d, r9d, r10d] {
            let mut a = CodeAssembler::new(64).unwrap();
            a.mov(reg, 1).unwrap();
            assert!(check_cold_registers(a.instructions(), false).is_err(), "{reg:?}");
        }
        for reg in [xmm8, xmm9, xmm10, xmm11, xmm12, xmm13, xmm14, xmm15] {
            let mut a = CodeAssembler::new(64).unwrap();
            a.movdqa(reg, xmm0).unwrap();
            assert!(check_cold_registers(a.instructions(), false).is_err(), "{reg:?}");
        }
        let mut a = CodeAssembler::new(64).unwrap();
        a.push(rax).unwrap(); // implicit rsp write
        assert!(check_cold_registers(a.instructions(), false).is_err());
    }

    #[test]
    fn cold_range_captures_tbi_state_at_jump_out() {
        for live in [false, true] {
            let mut u = Unit::new(StubKind::Plain, false);
            u.tbi_mask_live = live;
            let cold = u.a.create_label();
            u.a.jmp(cold).unwrap();
            let mut e = E { u: &mut u, pc: 0, fuse_in: None, scalar_inputs: None };
            e.cold_path(cold, |e| { a!(e, nop()); });
            u.a.nop().unwrap(); // bind resume before the cold label
            u.tbi_mask_live = !live;
            u.emit_cold_paths();
            assert_eq!(u.cold_ranges.len(), 1);
            let (range, captured) = &u.cold_ranges[0];
            assert_eq!(*captured, live);
            assert_eq!(range.len(), 2); // nop and return jump
            check_cold_registers(&u.a.instructions()[range.clone()], *captured).unwrap();
        }
    }

    #[test]
    fn every_fp_fixup_obeys_register_contract_and_executes_with_unaligned_pool() {
        const PC: u64 = 0x4800_0000;
        for avx in [false, true].into_iter().filter(|&avx| !avx || has_avx()) {
        for double in [false, true] {
            let ty = if double { 1 << 22 } else { 0 };
            let mut words = Vec::new();
            // Scalar binary arithmetic, sqrt and every max/min variant.
            for op in [0x0800, 0x1800, 0x2800, 0x3800, 0x4800, 0x5800, 0x6800, 0x7800] {
                words.push(0x1e220020 | ty | op);
            }
            words.push(0x1e21c020 | ty);
            if has_fma() {
                for variant in 0..4 {
                    words.push(0x1f020c20 | ty | ((variant >> 1) << 21) | ((variant & 1) << 15));
                }
            }
            for q in [false, true].into_iter().filter(|&q| q || !double) {
                let qbit = if q { 1 << 30 } else { 0 };
                for base in [0x0e20d400, 0x0ea0d400, 0x2e20dc00, 0x2e20fc00,
                             0x0e20f400, 0x0ea0f400, 0x0e20c400, 0x0ea0c400] {
                    words.push(base | qbit | ty | (2 << 16) | (1 << 5));
                }
                for base in [0x0f809000, 0x0f801000, 0x0f805000, 0x0e20cc00, 0x0ea0cc00] {
                    if base != 0x0f809000 && !has_fma() { continue; }
                    words.push(base | qbit | ty | (2 << 16) | (1 << 5));
                }
            }
            for word in words {
                let mut u = Unit::new(StubKind::Plain, false);
                u.pool_avx = avx;
                let label = u.a.create_label(); u.labels.insert(PC, label);
                // Load first to establish a live TBI mask at each jump-out.
                let block = [0xf9400083, word, 0xd4001001]; // ldr x3,[x4]; FP; svc
                emit_block(&mut u, PC, &|p| block.get(((p - PC) / 4) as usize).copied());
                assert_eq!(u.insns_fallback, 0, "word={word:08x}");
                assert_eq!(u.cold_ranges.len(), 1, "word={word:08x}");
                for (range, live) in &u.cold_ranges {
                    assert!(*live, "word={word:08x}");
                    check_cold_registers(&u.a.instructions()[range.clone()], *live).unwrap();
                }
                u.finish();
                let host = crate::jit::code_cache().alloc(16384);
                let bytes = u.a.assemble(host).unwrap();
                assert!(bytes.len() <= 16384);
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), host as *mut u8, bytes.len()); }
                // Exercise ordinary, invalid and NaN cold paths on both pool implementations.
                let values: &[u64] = if double {
                    &[0, 0x3ff0000000000000, 0x7ff0000000000000, 0x7ff8000000001234, 0x7ff0000000001234]
                } else {
                    &[0, 0x3f800000, 0x7f800000, 0x7fc01234, 0x7f801234]
                };
                for &a in values { for &b in values {
                    let mut c = Cpu::new(); crate::jit::setup_cpu(&mut c);
                    let memory = 42u64;
                    c.x[4] = &memory as *const u64 as u64;
                    let broadcast = |v: u64| {
                        let v = if double { v } else { v | (v << 32) } as u128;
                        v | (v << 64)
                    };
                    c.set_vq(0, broadcast(a)); c.set_vq(1, broadcast(a));
                    c.set_vq(2, broadcast(b)); c.set_vq(3, broadcast(b));
                    let mut expected = Cpu::new();
                    for r in 0..4 { expected.set_vq(r, c.vq(r)); }
                    interp::sys_sync_host_fp_mode(0);
                    interp::exec(&mut expected, word).unwrap();
                    unsafe { crate::jit::attesor_jit_enter(&mut *c, host); }
                    assert_eq!(c.vq(0), expected.vq(0), "word={word:08x} avx={avx} a={a:x} b={b:x}");
                    assert_eq!(c.x[3], memory);
                }}
            }
        }
        }
    }

    #[test]
    fn fixups_resume_and_flush_at_every_block_ending() {
        const PC: u64 = 0x4000_0000;
        for ending in 0..4 {
            // FADD D1,D1,D2 aliases its cached input on subsequent iterations.
            let mut words = vec![0x1e622821; if ending == 2 { MAX_BLOCK_INSNS } else { 3 }];
            if ending == 0 { words.push(0xd4001001); }
            let mut u = Unit::new(StubKind::Plain, false);
            let entry = u.a.create_label(); u.labels.insert(PC, entry);
            if ending == 3 {
                let join = u.a.create_label(); u.labels.insert(PC + 8, join);
            }
            let read = |p| words.get(((p - PC) / 4) as usize).copied();
            let end = emit_block(&mut u, PC, &read);
            assert!(u.cold_paths.is_empty());
            assert_eq!(u.insns_fallback, 0);
            if ending == 3 {
                // A following public entry must come after all the first block's cold paths.
                emit_block(&mut u, PC + 8, &read);
                assert!(u.cold_paths.is_empty());
            }
            u.finish();
            let host = crate::jit::code_cache().alloc(65536);
            let bytes = u.a.assemble(host).unwrap();
            assert!(bytes.len() <= 65536);
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), host as *mut u8, bytes.len()); }
            let mut c = Cpu::new(); crate::jit::setup_cpu(&mut c);
            c.set_vq(1, 0xfff8_0000_0000_0000); // propagated x86-indefinite-looking NaN
            c.set_vq(2, 0x7ff0_0000_0000_1234); // signalling operand has priority
            c.x = [0x1234_5678_9abc_def0; 32];
            unsafe { crate::jit::attesor_jit_enter(&mut *c, host); }
            assert_eq!(c.vq(1), 0x7ff8_0000_0000_1234, "ending={ending}");
            assert_eq!(c.vq(2), 0x7ff0_0000_0000_1234);
            assert_eq!(c.x, [0x1234_5678_9abc_def0; 32]);
            assert_eq!(c.pc, if ending == 0 { end } else { PC + words.len() as u64 * 4 });
        }
    }

    #[test]
    fn maxmin_fast_and_cold_lanes_match_with_flush_to_zero() {
        const PC: u64 = 0x5000_0000;
        for base in [0x1e224820, 0x0e22f420, 0x4e22f420, 0x4e62f420] {
            let scalar = base >> 24 == 0x1e;
            let double = base & (1 << 22) != 0;
            for variant in 0..4 {
                let word = if scalar { base | (variant << 12) }
                    else { base ^ ((variant & 1) << 23) ^ ((variant >> 1) * 0x3000) };
                let mut u = Unit::new(StubKind::Plain, false);
                let label = u.a.create_label(); u.labels.insert(PC, label);
                emit_block(&mut u, PC, &|p| match p - PC { 0 => Some(word), 4 => Some(0xd4001001), _ => None });
                assert_eq!(u.insns_fallback, 0, "word={word:08x}");
                u.finish();
                let host = crate::jit::code_cache().alloc(8192);
                let bytes = u.a.assemble(host).unwrap();
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), host as *mut u8, bytes.len()); }
                let values: &[u64] = if double {
                    &[0, 1 << 63, 1, 0x8000_0000_0000_0001, 0x3ff0_0000_0000_0000,
                      0xbff0_0000_0000_0000, 0x7ff0_0000_0000_0000, 0x7ff8_0000_0000_1234, 0xfff0_0000_0000_1234]
                } else {
                    &[0, 1 << 31, 1, 0x8000_0001, 0x3f80_0000, 0xbf80_0000, 0x7f80_0000, 0x7fc0_1234, 0xff80_1234]
                };
                let width = if double { 64 } else { 32 };
                let lanes = if scalar { 1 } else if word & (1 << 30) != 0 { 128 / width } else { 64 / width };
                for fz in [false, true] { for &a in values { for &b in values {
                    let mut c = Cpu::new(); crate::jit::setup_cpu(&mut c);
                    c.fpcr = if fz { 1 << 24 } else { 0 };
                    let mut va = 0; let mut vb = 0;
                    for lane in 0..lanes {
                        // Mix ordinary and exceptional lanes.
                        va |= (if lane & 1 == 0 { a } else { 0 } as u128) << (lane * width);
                        vb |= (if lane & 1 == 0 { b } else { values[4] } as u128) << (lane * width);
                    }
                    c.set_vq(1, va); c.set_vq(2, vb);
                    let mut expected = Cpu::new(); expected.fpcr = c.fpcr;
                    expected.set_vq(1, va); expected.set_vq(2, vb);
                    interp::sys_sync_host_fp_mode(c.fpcr);
                    interp::exec(&mut expected, word).unwrap();
                    interp::sys_sync_host_fp_mode(c.fpcr);
                    unsafe { crate::jit::attesor_jit_enter(&mut *c, host); }
                    assert_eq!(c.vq(0), expected.vq(0), "word={word:08x} a={a:x} b={b:x} fz={fz}");
                }}}
            }
        }
        interp::sys_sync_host_fp_mode(0);
    }
}

#[cfg(test)]
mod fp_pool_tests {
    use super::*;
    use attesor_core::cpu::Cpu;

    #[test]
    fn unaligned_deduplicated_pool_executes_after_whole_buffer_relocation() {
        const PC: u64 = 0x6000_0000;
        const A: u64 = 0x7ff8_0000_0000_1234;
        const B: u64 = 0x8000_0000_0000_0000;
        for avx in [false, true].into_iter().filter(|&avx| !avx || has_avx()) {
        let mut saw_unaligned = false;
        for kind in [StubKind::Plain, StubKind::Patchable] {
            for padding in 0..16 {
                let mut u = Unit::new(kind, false);
                u.pool_avx = avx;
                {
                    let mut e = E { u: &mut u, pc: PC, fuse_in: None, scalar_inputs: None };
                    for _ in 0..padding { a!(e, nop()); }
                    e.vconst(xmm0, A);
                    e.vconst(xmm1, A); // one shared pool entry
                    let sign = e.vconstant(B);
                    pool_op!(e, pxor, vpxor, xmm0, sign, xmm2);
                    a!(e, movdqu(xmmword_ptr(r15 + voff(0)), xmm0));
                    e.goto(PC + 4);
                }
                u.finish();
                assert_eq!(u.constant_pool.len(), 2);
                // AOT assembles at an offset and stores the entire code_buffer.
                let res = u.a.assemble_options(0, iced_x86::BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
                let bytes = &res.inner.code_buffer;
                for &(label, value) in &u.constant_pool {
                    let at = res.label_ip(&label).unwrap() as usize;
                    saw_unaligned |= at % 16 != 0;
                    assert_eq!(&bytes[at..at + 8], &value.to_le_bytes());
                    assert_eq!(&bytes[at + 8..at + 16], &value.to_le_bytes());
                }
                for _ in 0..2 {
                    let host = crate::jit::code_cache().alloc(bytes.len() as u64);
                    // Encoding at a different base must produce identical bytes.
                    assert_eq!(u.a.assemble(host).unwrap(), *bytes);
                    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), host as *mut u8, bytes.len()); }
                    for s in &u.finished_stubs {
                        if kind == StubKind::Patchable {
                            let slot = host + res.label_ip(&s.slot).unwrap();
                            let cont = host + res.label_ip(&s.cont).unwrap();
                            unsafe { std::ptr::write_unaligned(slot as *mut u64, cont); }
                        }
                    }
                    let mut c = Cpu::new(); crate::jit::setup_cpu(&mut c);
                    unsafe { crate::jit::attesor_jit_enter(&mut *c, host); }
                    let value = (A ^ B) as u128;
                    assert_eq!(c.vq(0), value | (value << 64));
                    assert_eq!(c.pc, PC + 4);
                    assert_eq!(c.exit_reason, EXIT_CONTINUE);
                }
            }
        }
        assert!(saw_unaligned);
        }
    }
}
