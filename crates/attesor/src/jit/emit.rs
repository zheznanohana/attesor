//! AArch64 -> x86-64 block translator.
//!
//! Conventions for generated code:
//!  * `r15` holds `&mut Cpu`; guest registers live in the Cpu struct.
//!  * `rsp` is 16-byte aligned, so helpers can be `call`ed directly.
//!  * Generated code never embeds absolute host addresses: helpers and the
//!    exit trampoline are reached through `[r15 + off]`. Code is therefore
//!    position independent, which lets the AOT cache store it on disk. The
//!    runtime-only opt-in rel32chain prefix is added separately by jit/mod.rs
//!    and is never persisted as an AOT Unit.
//!  * Instructions the translator does not handle natively are executed by
//!    calling the reference interpreter (`HELPER_INTERP`) for that single
//!    instruction; write-back regions synchronize guest state around that call.

use iced_x86::code_asm::*;
use attesor_core::cpu::off;
use attesor_core::interp::simd::vfp_expand_imm;
use std::collections::HashMap;

#[path = "notbi.rs"]
mod notbi;
#[path = "functions.rs"]
mod functions;
#[path = "fp_trace.rs"]
mod fp_trace;
#[path = "fp_batch.rs"]
mod fp_batch;
#[path = "fold_addr.rs"]
mod fold_addr;
#[path = "byte_stream.rs"]
mod byte_stream;
#[path = "fun_stack.rs"]
mod fun_stack;
#[path = "fun_edges.rs"]
mod fun_edges;
#[path = "rec_leaf.rs"]
mod rec_leaf;
#[path = "fp_flags.rs"]
mod fp_flags;
#[path = "addr_hoist.rs"]
mod addr_hoist;
#[path = "leaf.rs"]
mod leaf;

pub const HELPER_INTERP: usize = 0;
pub const HELPER_COMMPAGE: usize = 1;

pub const EXIT_CONTINUE: u64 = 0;
pub const EXIT_SVC: u64 = 1;
pub const EXIT_PENDING: u64 = 2;
/// The block needs the interpreter (FPCR not in the default mode the native FP code assumes).
pub const EXIT_INTERP: u64 = 3;
/// A thread-private BLR site-cache miss; exit_data is the complete guest call PC.
pub const EXIT_BLICCACHE: u64 = 0x10;
/// Guarded inline BLR target mismatch, before call/LR effects.
pub const EXIT_LEAFINLINE_MISS:u64=0x11;
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
    /// Assembler instruction ordinal, resolved before JIT code publication.
    /// Unlike a fresh label this also works when a block-entry label is pending.
    pub link_site: usize,
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
    /// JIT-only cold tier: no speculative lookahead, regions, fusion or fast exits.
    pub(super) quick: bool,
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
    loopra: bool,
    loopra2: bool,
    vloop: bool,
    vloop2: bool,
    vchain: bool,
    vscalar: bool,
    // Straight-line scalar-write proofs, S and D respectively. Never seeded
    // from a previous block/backedge: the first public entry must be correct.
    vzero: [u32; 2],
    funra: bool,
    funflags: bool,
    regflags: bool,
    cfgflags: bool,
    xspill: bool,
    fmovra: bool,
    vpair: bool,
    vtrace: bool,
    vbatch:bool,
    recleaflazy:bool,
    recleaf_lazy_proof:std::collections::BTreeMap<u64,u32>,
    recleaf_lazy_emissions:u64,
    recleaf:bool,
    recleaf_plan:Option<rec_leaf::Plan>,
    recleaf_context:Option<rec_leaf::Context>,
    recleaf_emissions:u64,
    movflags:bool,
    movflags_emissions:u64,
    funedge:bool,
    funedge_analysis:fun_edges::Analysis,
    pending_funedge:Option<fun_edges::Edge>,
    funedge_emissions:u64,
    funstack:bool,
    funstack_sites:HashMap<u64,u32>,
    funstack_emissions:u64,
    bytestream:bool,
    bytestream_site:Option<(u64,u32)>,
    bytestream_emissions:u64,
    foldaddr:bool,
    foldaddr_emissions:u64,
    vbatch_emissions:u64,
    fpflags: bool,
    fpguard: bool,
    fpstable:bool,
    fp_mode_hoists:u64,
    addrhoist: bool,
    addr_shadow:Option<(addr_hoist::Plan,usize)>,
    addr_hoists:u64,
    fpflags_sinks:u64,
    leafra: bool,
    leafinline: bool,
    inlinewide:bool,
    inlinewide_emissions:u64,
    inlinegp:bool,
    inlinegp_emissions:u64,
    leafflags: bool,
    inline_flag_continuations: HashMap<u64,Option<u32>>,
    inline_flag_elisions: u64,
    inline_follow_branches: HashMap<u64,u32>,
    inline_branch_clones: u64,
    inline_leaf_calls: HashMap<u64,(u64,leaf::Plan)>,
    inline_call_words: HashMap<u64,u32>,
    csetfast: bool,
    csetpair: bool,
    // Exact live-ins for a currently emitted straight-line return block only.
    leaf_livein: Option<u32>,
    selectra: bool,
    function_entry: Option<u64>,
    // Private clones only: never published in the JIT registry or AOT index.
    region_labels: HashMap<u64, CodeLabel>,
    pub extra_guest_range: Option<(u64, u64)>,
    region: Option<LoopRa>,
    region_exits: Vec<LoopRa>,
    // Final fused loop condition: materialize NZCV only on the exit edge.
    loop_exit_flags: Option<FuseKind>,
    // Recompute a final CMP only on the public edge of an audited private loop.
    loop_exit_compare: Option<super::flagreg::ExitCompare>,
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
    skip_more: u8,
    /// The unit may take extra blocks (the JIT's own units; AOT units are laid out up front).
    pub inline_ok: bool,
    pub(super) callret: bool,
    /// Blocks to translate into this unit after the current one: call continuations, so a translated
    /// RET lands on code that runs straight into the caller's next block.
    pub inline_queue: Vec<u64>,
    /// Guest V registers held in xmm8-xmm15 (optionally through xmm31), per block.
    pub vc: VCache,
    /// Current result stores proven dead before an in-block full overwrite.
    dead_v: u32,
    /// The V register(s) the instruction being translated may write: never read from the cache, since
    /// its own code may have stored part of them already.
    pub vc_dest: [u8; 2],
    /// r11 holds the TBI mask on every path to this point. Per-block, never
    /// established under conditional emission, invalidated by scratch writes/calls.
    pub tbi_mask_live: bool,
    /// Exact guest values for the opt-in, block-local redundant-mask proof.
    notbi: notbi::Facts,
    notbi_enabled: bool,
    /// Rare FP fixups, emitted after the current block's unconditional transfer.
    cold_paths: Vec<Box<dyn FnOnce(&mut Unit)>>,
    /// Broadcast constants live in this unit's code buffer, including in AOT blobs.
    constants: HashMap<u64, CodeLabel>,
    constant_pool: Vec<(CodeLabel, u64)>,
    pool_avx: bool,
    /// Opt-in 128-bit EVEX cache bank; never enable without host/OS support.
    avx512: bool,
    /// Independently opt-in AVX-512 NaN selection / range instructions.
    avx512_nan: bool,
    #[cfg(any(debug_assertions, test))]
    cold_ranges: Vec<(std::ops::Range<usize>, bool)>,
    #[cfg(any(debug_assertions, test))]
    resident_cold_ranges: Vec<(std::ops::Range<usize>, bool, u32)>,
}

pub const NCREGS: usize = 5;
pub const NVREGS: usize = 24;
const BASE_NVREGS: usize = 8;

/// Cache of guest V registers in xmm8-xmm15, optionally xmm16-xmm31, within one block. Stores proven dead
/// before a full overwrite may be register-only; dirty eviction writes them back.
/// Dependent FP operations read a register instead
/// of reloading what the previous instruction just stored (store-forwarding latency on the critical
/// path). Kept correct from the guest side: before each instruction, the V registers its encoding can
/// write are dropped (`v_writes`); any emitted `call` (helpers, the interpreter) drops everything, since
/// xmm registers are caller-saved; every block starts empty.
#[derive(Clone, Copy)]
pub struct VCache {
    pub reg: [u8; NVREGS],
    pub stamp: [u32; NVREGS],
    pub tick: u32,
    dirty: [bool; NVREGS],
}

impl VCache {
    fn new() -> VCache {
        VCache { reg: [0xff; NVREGS], stamp: [0; NVREGS], tick: 0, dirty: [false; NVREGS] }
    }
}

const VREGS: [AsmRegisterXmm; NVREGS] = [
    xmm8, xmm9, xmm10, xmm11, xmm12, xmm13, xmm14, xmm15,
    xmm16, xmm17, xmm18, xmm19, xmm20, xmm21, xmm22, xmm23,
    xmm24, xmm25, xmm26, xmm27, xmm28, xmm29, xmm30, xmm31,
];

fn high_xmm(x: AsmRegisterXmm) -> bool { iced_x86::Register::from(x).number() >= 16 }

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
fn gpr_to_fp_access(w:u32)->Option<CacheAccess> {
    let ty=bits(w,23,22);let wide=bit(w,31)!=0;
    if w&0x7f3efc00==0x1e260000 && bit(w,16)!=0 && bits(w,20,19)==0
        && ((wide&&ty==1)||(!wide&&ty==0)) {
        Some(CacheAccess{xr:reg_bit(bits(w,9,5),false),vw:1<<(w&31),..CacheAccess::default()})
    }else {None}
}
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

const NATIVE_RAS_LIMIT:i32=256;
const NLOOPREGS: usize = 10;
const LOOPREGS: [R64; NLOOPREGS] = [rbx, rbp, r12, r13, r14, rsi, rdi, r8, r9, r10];
const LOOPREGS32: [R32; NLOOPREGS] = [ebx, ebp, r12d, r13d, r14d, esi, edi, r8d, r9d, r10d];

#[derive(Clone, Copy)]
struct LoopRa {
    pc: u64,
    fast: CodeLabel,
    exit: CodeLabel,
    reg: [u8; NLOOPREGS],
    // Union of guest writes emitted for this loop, including unknown helper
    // writes. finish emits cold tails only after this mask is complete.
    dirty: u32,
    // Experimental fixed V locations on a proven single-block FP loop.
    // Scratch xmm0..7 remain separate; all public exits materialize Cpu.
    vreg: [u8; NVREGS],
    // Pure-GPR CFG only: otherwise-unused XMM slots retain overflow GPRs.
    // Disjoint from reg and vreg; all slots synchronize at observations.
    xreg: [u8; NVREGS],
    // Optional unused GPR holding compact CMP flags: SF15/ZF14/borrow8/OF0.
    // CMP-only private CFGs. Public-edge writeback also changes host CF.
    flags: u8,
}

impl LoopRa {
    fn packed_flags(self) -> Option<R64> {
        (self.flags<NLOOPREGS as u8).then(||LOOPREGS[self.flags as usize])
    }
    fn vslot(self, n: u32) -> Option<usize> {
        self.vreg.iter().position(|&r| r as u32 == n)
    }
    fn slot(self, n: u32) -> Option<usize> {
        self.reg.iter().position(|&r| r as u32 == n)
    }
    fn xslot(self,n:u32)->Option<usize> {self.xreg.iter().position(|&r|r as u32==n)}
    fn writeback(self, a: &mut CodeAssembler) {
        for (s, n) in self.reg.into_iter().enumerate() {
            if n != 0xff && self.dirty & (1 << n) != 0 {
                a.mov(qword_ptr(r15 + xoff(n as u32)), LOOPREGS[s]).unwrap();
            }
        }
        for (s, n) in self.vreg.into_iter().enumerate() {
            if n != 0xff {
                if high_xmm(VREGS[s]) { a.vmovdqu64(xmmword_ptr(r15 + voff(n as u32)), VREGS[s]).unwrap(); }
                else { a.movdqu(xmmword_ptr(r15 + voff(n as u32)), VREGS[s]).unwrap(); }
            }
        }
        for(s,n)in self.xreg.into_iter().enumerate() {if n!=0xff {
            if high_xmm(VREGS[s]) {a.evex().vmovq(qword_ptr(r15+xoff(n as u32)),VREGS[s]).unwrap();}
            else {a.movq(qword_ptr(r15+xoff(n as u32)),VREGS[s]).unwrap();}
        }}
        if let Some(f)=self.packed_flags() {
            a.bt(f,15).unwrap();a.setc(byte_ptr(r15+off::NF)).unwrap();
            a.bt(f,14).unwrap();a.setc(byte_ptr(r15+off::ZF)).unwrap();
            a.bt(f,8).unwrap();a.setnc(byte_ptr(r15+off::CF)).unwrap();
            a.bt(f,0).unwrap();a.setc(byte_ptr(r15+off::VF)).unwrap();
        }
    }
    fn reload(self, a: &mut CodeAssembler) { self.reload_mask(a,u32::MAX); }
    fn reload_mask(self, a: &mut CodeAssembler, live:u32) {
        for (s, n) in self.reg.into_iter().enumerate() {
            if n != 0xff && live&(1u32<<n)!=0 { a.mov(LOOPREGS[s], qword_ptr(r15 + xoff(n as u32))).unwrap(); }
        }
        for (s, n) in self.vreg.into_iter().enumerate() {
            if n != 0xff {
                if high_xmm(VREGS[s]) { a.vmovdqu64(VREGS[s], xmmword_ptr(r15 + voff(n as u32))).unwrap(); }
                else { a.movdqu(VREGS[s], xmmword_ptr(r15 + voff(n as u32))).unwrap(); }
            }
        }
        for(s,n)in self.xreg.into_iter().enumerate() {if n!=0xff {
            if high_xmm(VREGS[s]) {a.evex().vmovq(VREGS[s],qword_ptr(r15+xoff(n as u32))).unwrap();}
            else {a.movq(VREGS[s],qword_ptr(r15+xoff(n as u32))).unwrap();}
        }}
        if let Some(f)=self.packed_flags() {
            let d=LOOPREGS32[self.flags as usize];
            // Do not clobber EAX: interpreter helpers return status there.
            a.movzx(d,byte_ptr(r15+off::CF)).unwrap();a.xor(d,1).unwrap();a.shl(d,8).unwrap();
            for (offset,shift) in [(off::NF,15),(off::ZF,14),(off::VF,0)] {
                a.movzx(edx,byte_ptr(r15+offset)).unwrap();
                if shift!=0 {a.shl(edx,shift).unwrap();}
                a.or(d,edx).unwrap();
            }
            let _=f;
        }
    }
}

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

impl Cc {
    fn inverse(self) -> Self {
        use Cc::*;
        match self {
            E => Ne, Ne => E, B => Ae, Ae => B, S => Ns, Ns => S,
            O => No, No => O, A => Be, Be => A, L => Ge, Ge => L, G => Le, Le => G,
        }
    }
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

#[cfg(test)]
thread_local! {
    // Only used by the native self-test matrix after explicit host authorization.
    static TEST_AVX512_FEATURES: std::cell::Cell<Option<(bool, bool)>> = const { std::cell::Cell::new(None) };
}

impl Unit {
    /// Finished-unit assembly; keep original instructions for relocation fallback.
    pub fn assemble_options(&mut self, ip: u64, options: u32) -> Result<CodeAssemblerResult, IcedError> {
        super::fastenc::assemble(&mut self.a, ip, options)
    }

    pub fn new(stub_kind: StubKind, commpage_checks: bool) -> Unit {
        let mut a = CodeAssembler::new(64).unwrap();
        let exit_label = a.create_label();
        let features = (avx512_enabled(), avx512_nan_enabled());
        #[cfg(test)]
        let features = TEST_AVX512_FEATURES.with(|f| f.get()).unwrap_or(features);
        Unit {
            a,
            labels: HashMap::new(),
            stubs: Vec::new(),
            stub_kind,
            slot_padding: 0,
            no_internal_jumps: false,
            quick: false,
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
            loopra: loopra_enabled(),
            loopra2: loopra2_enabled(),
            vloop: vloop_enabled(),
            vloop2: vloop2_enabled(),
            vchain: vchain_enabled(),
            vscalar: vscalar_enabled(),
            vzero: [0; 2],
            funra: funra_enabled(),
            funflags: funflags_enabled(),
            regflags: regflags_enabled(),
            cfgflags: cfgflags_enabled(),
            xspill: xspill_enabled(),
            fmovra: fmovra_enabled(),
            vpair: vpair_enabled(),
            vtrace: vtrace_enabled(),
            vbatch:vbatch_enabled(),
            recleaflazy:recleaflazy_enabled(),recleaf_lazy_proof:std::collections::BTreeMap::new(),recleaf_lazy_emissions:0,
            recleaf:recleaf_enabled(),recleaf_plan:None,recleaf_context:None,recleaf_emissions:0,
            movflags:movflags_enabled(),movflags_emissions:0,
            funedge:funedge_enabled(),funedge_analysis:fun_edges::Analysis::default(),pending_funedge:None,funedge_emissions:0,
            funstack:funstack_enabled(),funstack_sites:HashMap::new(),funstack_emissions:0,
            bytestream:bytestream_enabled(),bytestream_site:None,bytestream_emissions:0,
            foldaddr:foldaddr_enabled(),
            foldaddr_emissions:0,
            vbatch_emissions:0,
            fpflags: fpflags_enabled(),
            fpguard: fpguard_enabled(),
            fpstable:fpstable_enabled(),
            fp_mode_hoists:0,
            addrhoist:addrhoist_enabled(),
            addr_shadow:None,
            addr_hoists:0,
            fpflags_sinks:0,
            leafra: leafra_enabled(),
            leafinline: leafinline_enabled(),
            inlinewide:inlinewide_enabled(),inlinewide_emissions:0,
            inlinegp:inlinegp_enabled(),inlinegp_emissions:0,
            leafflags: leafflags_enabled(),
            inline_flag_continuations: HashMap::new(),
            inline_flag_elisions: 0,
            inline_follow_branches: HashMap::new(),
            inline_branch_clones: 0,
            inline_leaf_calls: HashMap::new(),
            inline_call_words: HashMap::new(),
            csetfast: csetfast_enabled(),
            csetpair: csetpair_enabled(),
            leaf_livein: None,
            selectra: selectra_enabled(),
            function_entry: None,
            region_labels: HashMap::new(),
            extra_guest_range: None,
            region: None,
            region_exits: Vec::new(),
            loop_exit_flags: None,
            loop_exit_compare: None,
            rc_frozen: false,
            look: Vec::new(),
            cache_uses: [[u8::MAX; 32]; 2],
            dead_flags: false,
            skip_next: false,
            skip_more: 0,
            inline_ok: false,
            callret: callret_enabled(),
            inline_queue: Vec::new(),
            vc: VCache::new(),
            dead_v: 0,
            vc_dest: [0xff; 2],
            tbi_mask_live: false,
            notbi: notbi::Facts::new(),
            notbi_enabled: !commpage_checks && notbi_enabled(),
            cold_paths: Vec::new(),
            constants: HashMap::new(),
            constant_pool: Vec::new(),
            pool_avx: has_avx(),
            avx512: features.0,
            avx512_nan: features.1,
            #[cfg(any(debug_assertions, test))]
            cold_ranges: Vec::new(),
            #[cfg(any(debug_assertions, test))]
            resident_cold_ranges: Vec::new(),
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
        // Each region owns its dispatcher tail: finish may contain several blocks
        // with different assignments. Stubs receive canonical state from goto.
        for mut region in std::mem::take(&mut self.region_exits) {
            a.set_label(&mut region.exit).unwrap();
            region.writeback(a);
            a.jmp(self.exit_label).unwrap();
        }
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
        x if x == rbx => ebx,
        x if x == rbp => ebp,
        x if x == r12 => r12d,
        x if x == r13 => r13d,
        x if x == r14 => r14d,
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
        x if x == rbx => bl,
        x if x == rbp => bpl,
        x if x == r12 => r12b,
        x if x == r13 => r13b,
        x if x == r14 => r14b,
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
        x if x == rbx => bx,
        x if x == rbp => bp,
        x if x == r12 => r12w,
        x if x == r13 => r13w,
        x if x == r14 => r14w,
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

// iced's default VEX preference does not promote high-XMM operands to EVEX.
// Explicitly select EVEX for instructions whose operands can name xmm16-xmm31.
macro_rules! evex {
    ($e:expr, $m:ident ( $($arg:expr),* )) => { $e.u.a.evex().$m($($arg),*).unwrap() };
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
    check_cold_registers_with_output(instructions,tbi_mask_live,None)
}

#[cfg(any(debug_assertions, test))]
fn check_cold_registers_with_output(instructions: &[iced_x86::Instruction], tbi_mask_live: bool, output:Option<u32>) -> Result<(), String> {
    use iced_x86::{InstructionInfoFactory, OpAccess, Register};
    let mut info = InstructionInfoFactory::new();
    for (index, instruction) in instructions.iter().enumerate() {
        for used in info.info(instruction).used_registers() {
            if !matches!(used.access(), OpAccess::Write | OpAccess::CondWrite | OpAccess::ReadWrite | OpAccess::ReadCondWrite) {
                continue;
            }
            let reg = used.register();
            let allowed = if reg.is_vector_register() {
                reg.number() < 8 || output==Some(reg.number() as u32)
            } else {
                // k1 is a dead scratch at every guest boundary and cold resume.
                matches!(reg.full_register(), Register::RAX | Register::RCX | Register::RDX | Register::K1)
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
    /// Region operands borrow assigned registers; unassigned values use a caller
    /// supplied scratch. Width belongs to the operation, not the borrowed value:
    /// reading Wn must never zero the high half of an assigned Xn.
    fn src(&mut self, n: u32, allow_sp: bool, sf: bool, scratch: R64) -> R64 {
        if n != 31 || allow_sp {
            if let Some(s) = self.region_slot(n) { return LOOPREGS[s]; }
        }
        if n == 31 && !allow_sp { a!(self, mov(r32_of(scratch), 0u32)); }
        else if sf { self.ldx_nf(scratch, n); }
        else { self.ldw_nf(scratch, n); }
        scratch
    }
    /// stx commits a scratch destination or marks an assigned one dirty.
    fn dst(&self, n: u32, allow_sp: bool, scratch: R64) -> R64 {
        if n != 31 || allow_sp {
            if let Some(s) = self.region_slot(n) { return LOOPREGS[s]; }
        }
        scratch
    }
    fn copy(&mut self, d: R64, s: R64, sf: bool) {
        if d != s {
            if sf { a!(self, mov(d, s)); } else { a!(self, mov(r32_of(d), r32_of(s))); }
        }
    }
    fn init_dst(&mut self, d: u32, n: u32, dsp: bool, nsp: bool, sf: bool, scratch: R64) -> R64 {
        let n = self.src(n, nsp, sf, scratch);
        let d = self.dst(d, dsp, scratch);
        self.copy(d, n, sf);
        d
    }

    /// Direct two-address operations. Capture an aliased right operand before
    /// initializing the destination, and use LEA for independent ADD results.
    fn region_binary(&mut self, rd: u32, mut n: R64, mut m: R64, sf: bool,
                     op: u32, flags: bool, allow_sp: bool) {
        // op: ADD, SUB, AND, OR, XOR, MUL.
        if flags && rd == 31 {
            if sf {
                match op { 1 => a!(self, cmp(n, m)), 2 => a!(self, test(n, m)),
                    _ => { self.copy(rax, n, sf); a!(self, add(rax, m)); } }
            } else {
                match op { 1 => a!(self, cmp(r32_of(n), r32_of(m))), 2 => a!(self, test(r32_of(n), r32_of(m))),
                    _ => { self.copy(rax, n, sf); a!(self, add(eax, r32_of(m))); } }
            }
        } else {
            let d = self.dst(rd, allow_sp, rax);
            if !flags && op == 0 && d != n && d != m {
                if sf { a!(self, lea(d, qword_ptr(n + m))); }
                else { a!(self, lea(r32_of(d), dword_ptr(n + m))); }
            } else {
                if d == m && d != n {
                    if op != 1 { std::mem::swap(&mut n, &mut m); }
                    else { self.copy(rcx, m, sf); m = rcx; }
                }
                self.copy(d, n, sf);
                if sf {
                    match op { 0 => a!(self, add(d, m)), 1 => a!(self, sub(d, m)),
                        2 => a!(self, and(d, m)), 3 => a!(self, or(d, m)),
                        4 => a!(self, xor(d, m)), _ => a!(self, imul_2(d, m)) }
                } else {
                    let (d, m) = (r32_of(d), r32_of(m));
                    match op { 0 => a!(self, add(d, m)), 1 => a!(self, sub(d, m)),
                        2 => a!(self, and(d, m)), 3 => a!(self, or(d, m)),
                        4 => a!(self, xor(d, m)), _ => a!(self, imul_2(d, m)) }
                }
            }
            self.stx(rd, d, allow_sp);
        }
        if flags {
            match op { 0 => { self.flags_add(); self.u.fuse = Some(FuseKind::Add); },
                1 => { self.flags_sub(); self.u.fuse = Some(FuseKind::Sub); },
                _ => { self.flags_logic(); self.u.fuse = Some(FuseKind::Logic); } }
        }
    }

    fn region_dp_reg(&mut self, w: u32) -> bool {
        let sf = bit(w, 31) != 0;
        let (d,n,m) = (bits(w,4,0),bits(w,9,5),bits(w,20,16));
        let logical = w & 0x1f00_0000 == 0x0a00_0000;
        let add = w & 0x1f00_0000 == 0x0b00_0000;
        if logical || add {
            let ext = add && bit(w,21) != 0;
            let ty = bits(w,23,22);
            let amt = bits(w,15,10);
            if (!ext && ((!sf && amt >= 32) || (add && ty == 3)))
                || (ext && (ty != 0 || bits(w,12,10) > 4)) { return false; }
            let opc = bits(w,30,29);
            if logical && opc == 1 && n == 31 && amt == 0 && bit(w,21) == 0 {
                let r = self.init_dst(d,m,false,false,sf,rax);
                // A 32-bit move to itself still clears Xd[63:32].
                if !sf { a!(self, mov(r32_of(r), r32_of(r))); }
                self.stx(d,r,false); return true;
            }
            let mut rhs = self.src(m,false,sf,rcx);
            if ext || amt != 0 || (logical && bit(w,21) != 0) {
                // Copy unless Rm is also the result and Rn needs no old Rm.
                if !(logical && d == m && d != n && d != 31) {
                    self.copy(rcx,rhs,if ext { true } else { sf }); rhs=rcx;
                }
                if ext { self.extend(rhs,bits(w,15,13),bits(w,12,10)); }
                else { self.shift(rhs,ty,amt,sf); }
                if logical && bit(w,21) != 0 {
                    if sf { a!(self, not(rhs)); } else { a!(self, not(r32_of(rhs))); }
                }
            }
            let lhs=self.src(n,ext,sf,rax);
            let flags=if logical { opc==3 } else { bit(w,29)!=0 };
            let op=if logical { match opc {0|3=>2,1=>3,_=>4} } else {bit(w,30)};
            self.region_binary(d,lhs,rhs,sf,op,flags,ext && !flags);
            return true;
        }
        if w & 0x3fe0_0800 == 0x1a80_0000 {
            let (op,o2,cond)=(bit(w,30),bit(w,10),bits(w,15,12));
            let fused=self.fuse_in.and_then(|k| x86_cc(k,cond^1).map(|cc|(k,cc)));
            if fused.is_none() { self.cond_to_al(cond); }
            // Keep rax/al for the non-fused condition until TEST below.
            let mut rhs=self.src(m,false,sf,rdx);
            if op != 0 || o2 != 0 {
                self.copy(rdx,rhs,sf); rhs=rdx;
                match (op,o2) {
                    (0,_) => if sf { a!(self, lea(rhs,qword_ptr(rhs+1))); } else { a!(self, lea(edx,dword_ptr(rhs+1))); },
                    (_,0) => if sf { a!(self, not(rhs)); } else { a!(self, not(edx)); },
                    _ => { if sf { a!(self, not(rhs)); a!(self, lea(rhs,qword_ptr(rhs+1))); }
                        else { a!(self, not(edx)); a!(self, lea(edx,dword_ptr(rhs+1))); } }
                }
            }
            let lhs=self.src(n,false,sf,rcx);
            let dest=self.dst(d,false,rcx);
            if dest == rhs && dest != lhs { self.copy(rdx,rhs,sf); rhs=rdx; }
            self.copy(dest,lhs,sf);
            if let Some((k,cc))=fused { self.cmov(cc,dest,rhs,sf); self.u.fuse=Some(k); }
            else { a!(self, test(al,al)); self.cmov(Cc::E,dest,rhs,sf); }
            // Guarantee Wd zero-extension on both select outcomes.
            if !sf { a!(self, mov(r32_of(dest),r32_of(dest))); }
            self.stx(d,dest,false); return true;
        }
        if w & 0x1fe0_0000 == 0x1b00_0000 && bits(w,30,29)==0 && bits(w,23,21)==0 {
            let ra=bits(w,14,10);
            let lhs=self.src(n,false,sf,rax);
            let rhs=self.src(m,false,sf,rcx);
            // MADD's addend may alias its destination; capture it first.
            let addend=if ra!=31 { let r=self.src(ra,false,sf,rdx); self.copy(rdx,r,sf); Some(rdx) } else {None};
            self.region_binary(d,lhs,rhs,sf,5,false,false);
            let dest=self.src(d,false,sf,rax);
            if bit(w,15)==0 {
                if let Some(a)=addend { if sf { a!(self, add(dest,a)); } else { a!(self, add(r32_of(dest),r32_of(a))); } }
            } else {
                if sf { a!(self, neg(dest)); } else { a!(self, neg(r32_of(dest))); }
                if let Some(a)=addend { if sf { a!(self, add(dest,a)); } else { a!(self, add(r32_of(dest),r32_of(a))); } }
            }
            self.stx(d,dest,false); return true;
        }
        false
    }
    fn region_dp_imm(&mut self, w: u32) -> bool {
        let sf=bit(w,31)!=0;
        let (d,n)=(bits(w,4,0),bits(w,9,5));
        match bits(w,25,23) {
            2 => {
                let op=bit(w,30); let flags=bit(w,29)!=0;
                let imm=(bits(w,21,10) << (if bit(w,22)!=0 {12} else {0})) as i32;
                if flags && d==31 && op==1 {
                    let r=self.src(n,true,sf,rax);
                    if sf { a!(self, cmp(r,imm)); } else { a!(self, cmp(r32_of(r),imm)); }
                } else if !flags {
                    let source = self.src(n,true,sf,rax);
                    let dest = self.dst(d,true,rax);
                    let offset = if op == 0 { imm } else { -imm };
                    if sf { a!(self, lea(dest,qword_ptr(source + offset))); }
                    else { a!(self, lea(r32_of(dest),dword_ptr(source + offset))); }
                    self.stx(d,dest,true);
                } else {
                    let r=self.init_dst(d,n,!flags,true,sf,rax);
                    if sf { if op==0 { a!(self, add(r,imm)); } else { a!(self, sub(r,imm)); } }
                    else if op==0 { a!(self, add(r32_of(r),imm)); } else { a!(self, sub(r32_of(r),imm)); }
                    self.stx(d,r,!flags);
                }
                if flags { if op==0 { self.flags_add(); self.u.fuse=Some(FuseKind::Add); }
                    else { self.flags_sub(); self.u.fuse=Some(FuseKind::Sub); } }
                true
            }
            4 => {
                let opc=bits(w,30,29); let nb=bit(w,22);
                if !sf && nb!=0 {return false;}
                let Some(imm)=decode_logic_imm(nb,bits(w,15,10),bits(w,21,16),sf) else {return false};
                let test=opc==3 && d==31;
                let r=if test {self.src(n,false,sf,rax)} else {self.init_dst(d,n,opc!=3,false,sf,rax)};
                let simm=if sf {imm as i64} else {imm as u32 as i32 as i64};
                if simm >= i32::MIN as i64 && simm <= i32::MAX as i64 {
                    let i=simm as i32;
                    match (sf,opc,test) {
                        (true,_,true)=>a!(self,test(r,i)), (false,_,true)=>a!(self,test(r32_of(r),i)),
                        (true,0|3,_)=>a!(self,and(r,i)), (true,1,_)=>a!(self,or(r,i)), (true,_,_)=>a!(self,xor(r,i)),
                        (false,0|3,_)=>a!(self,and(r32_of(r),i)), (false,1,_)=>a!(self,or(r32_of(r),i)), (false,_,_)=>a!(self,xor(r32_of(r),i)),
                    }
                } else {
                    self.mov_imm(rcx,imm);
                    match (opc,test) {(_,true)=>a!(self,test(r,rcx)),(0|3,_)=>a!(self,and(r,rcx)),(1,_)=>a!(self,or(r,rcx)),_=>a!(self,xor(r,rcx))}
                }
                if !test { self.stx(d,r,opc!=3); }
                if opc==3 {self.flags_logic();self.u.fuse=Some(FuseKind::Logic);}
                true
            }
            5 if bits(w,30,29)==3 => {
                let hw=bits(w,22,21); if !sf && hw>1 {return false;}
                if d==31 {return true;}
                let imm=(bits(w,20,5) as u64) << (hw*16);
                let r=self.init_dst(d,d,false,false,sf,rax);
                self.mov_imm(rcx,!(0xffffu64 << (hw*16)));
                a!(self,and(r,rcx));
                if imm!=0 {self.mov_imm(rcx,imm);a!(self,or(r,rcx));}
                if !sf {a!(self,mov(r32_of(r),r32_of(r)));}
                self.stx(d,r,false);true
            }
            _=>false
        }
    }
    fn region_slot(&self, n: u32) -> Option<usize> { self.u.region.and_then(|r| r.slot(n)) }
    fn writeback(&mut self) {
        if let Some(r) = self.u.region { r.writeback(&mut self.u.a); }
    }
    fn dirty_region(&mut self, mask: u32) {
        if let Some(r) = &mut self.u.region {
            r.dirty |= mask;
            self.u.region_exits.last_mut().unwrap().dirty = r.dirty;
        }
    }
    fn sync_region(&mut self) {
        if let Some(mut r) = self.u.region {
            // An early helper on a later iteration can see native writes that
            // follow it in the body. Synchronize all slots at in-body barriers;
            // cache_access is a profitability hint, never a correctness decoder.
            r.dirty = u32::MAX;
            r.writeback(&mut self.u.a);
        }
    }
    fn reload_region(&mut self) {
        if let Some(r) = self.u.region { r.reload(&mut self.u.a); }
    }
    fn dispatcher_exit(&self) -> CodeLabel {
        self.u.region.map_or(self.u.exit_label, |r| r.exit)
    }
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
        if self.u.notbi_enabled { self.u.notbi.clear(); }
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
        if let Some(s) = self.u.region.and_then(|r| r.vslot(n)) {
            self.u.vc.reg[s] = n as u8;
            self.u.vc.dirty[s] = false;
            self.vc_touch(s);
            return s;
        }
        let vc = &self.u.vc;
        let slots = if self.u.avx512 { NVREGS } else { BASE_NVREGS };
        let s = (0..slots).find(|&i| vc.reg[i] == 0xff).unwrap_or_else(|| {
            if jit_off("nextuse") { (0..slots).min_by_key(|&i| vc.stamp[i]).unwrap() }
            else { (0..slots).max_by_key(|&i| (self.u.cache_uses[1][vc.reg[i] as usize], u32::MAX - vc.stamp[i])).unwrap() }
        });
        // A dead memory store can still have live cache consumers. Eviction must
        // materialize it before any later operand can fall back to Cpu memory.
        if vc.dirty[s] {
            let dst = xmmword_ptr(r15 + voff(vc.reg[s] as u32));
            if high_xmm(VREGS[s]) { a!(self, vmovdqu64(dst, VREGS[s])); }
            else { a!(self, movdqu(dst, VREGS[s])); }
        }
        self.u.vc.dirty[s] = false;
        self.u.vc.reg[s] = n as u8;
        self.vc_touch(s);
        s
    }
    /// xmm register holding all of Vn, if cached; `None`: read memory. Slots are filled only from
    /// results (`vfill`), never by loading: a 16-byte load right after the two 8-byte stores of an
    /// LDR Dt or FMOV Dd, Xn cannot be store-forwarded and stalls longer than the load it saves.
    fn vsrc(&mut self, n: u32) -> Option<AsmRegisterXmm> {
        if let Some(s) = self.u.region.and_then(|r| r.vslot(n)) {
            return Some(VREGS[s]);
        }
        if self.u.vc_dest.contains(&(n as u8)) {
            return None;
        }
        let s = self.vc_find(n)?;
        self.vc_touch(s);
        Some(VREGS[s])
    }
    fn store_v(&mut self, d: u32, x: AsmRegisterXmm) {
        if let Some(s) = self.u.region.and_then(|r| r.vslot(d)) {
            self.cache_copy(VREGS[s], x);
            return;
        }
        if self.u.dead_v & (1 << d) == 0 {
            a!(self, movdqu(xmmword_ptr(r15 + voff(d)), x));
            self.vfill(d, x);
        } else {
            let s = self.vc_find(d).unwrap_or_else(|| self.vc_alloc(d));
            self.cache_copy(VREGS[s], x);
            self.u.vc.dirty[s] = true;
            self.vc_touch(s);
        }
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
        self.cache_copy(VREGS[s], x);
        self.u.vc.dirty[s] = false;
        self.vc_touch(s);
    }
    /// Legacy SSE cannot name xmm16-xmm31. Keep all cache transfers 128-bit;
    /// low-bank emission stays byte-for-byte unchanged when the flag is off.
    fn cache_copy(&mut self, dst: AsmRegisterXmm, src: AsmRegisterXmm) {
        if high_xmm(dst) || high_xmm(src) { a!(self, vmovdqa64(dst, src)); }
        else { a!(self, movdqa(dst, src)); }
    }
    fn cache_low64(&mut self, dst: AsmRegisterXmm, src: AsmRegisterXmm) {
        if high_xmm(src) { evex!(self, vmovq(dst, src)); }
        else { a!(self, movq(dst, src)); }
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
            Some(c) if double => self.cache_low64(x, c),
            Some(c) => {
                a!(self, xorps(x, x));
                if high_xmm(c) { evex!(self, vmovss_3(x, x, c)); }
                else { a!(self, movss(x, c)); }
            }
            None if double => a!(self, movsd_2(x, qword_ptr(r15 + src.offset))),
            None => a!(self, movss(x, dword_ptr(r15 + src.offset))),
        }
    }
    fn scalar_zero(&mut self, d: u32, double: bool) {
        if !self.u.vchain {return;}
        let bit = 1 << d;
        self.u.vzero[1] |= bit;
        if double { self.u.vzero[0] &= !bit; } else { self.u.vzero[0] |= bit; }
    }
    fn fp_register(&mut self, opcode: u32, double: bool, dst: AsmRegisterXmm,
                   n: AsmRegisterXmm, m: AsmRegisterXmm) {
        macro_rules! op { ($name:ident) => {
            if high_xmm(dst) || high_xmm(n) || high_xmm(m) { evex!(self, $name(dst,n,m)); }
            else { a!(self, $name(dst,n,m)); }
        }; }
        match (double,opcode) {
            (true,0) => op!(vmulsd), (false,0) => op!(vmulss),
            (true,1) => op!(vdivsd), (false,1) => op!(vdivss),
            (true,2) => op!(vaddsd), (false,2) => op!(vaddss),
            (true,3) => op!(vsubsd), (false,3) => op!(vsubss),
            (true,4) => op!(vsqrtsd), (false,4) => op!(vsqrtss),
            _ => unreachable!(),
        }
    }
    /// Scalar ALU directly between a proven region's V locations. Never destroy
    /// an original NaN operand before its fixup: aliases still use scratch XMM0.
    fn resident_scalar(&mut self,opcode:u32,double:bool,d:u32,n:u32,m:Option<u32>)->bool {
        if !self.u.vscalar||!self.u.vchain||!has_avx() {return false;}
        let Some(region)=self.u.region else {return false;};
        let Some(dest_slot)=region.vslot(d) else {return false;};
        let ns=self.scalar_source(n);let ms=m.map(|r|self.scalar_source(r));
        let Some(nr)=ns.cached else {return false;};
        if ms.is_some_and(|s|s.cached.is_none()) {return false;}
        let independent=d!=n&&m!=Some(d);
        let dst=if independent {VREGS[dest_slot]}else {xmm0};
        let mr=ms.and_then(|s|s.cached).unwrap_or(nr);
        let known=self.u.vzero[double as usize];
        if known&(1<<n)!=0 {
            self.fp_register(opcode,double,dst,nr,mr);
        } else if matches!(opcode,0|2)&&m.is_some_and(|r|known&(1<<r)!=0) {
            self.fp_register(opcode,double,dst,mr,nr);
        } else {
            self.fld_source(xmm0,ns,double);
            self.fp_register(opcode,double,dst,xmm0,if opcode==4 {xmm0}else {mr});
        }
        let cold=self.u.a.create_label();
        if high_xmm(dst) {
            if double {evex!(self,vucomisd(dst,dst));}else {evex!(self,vucomiss(dst,dst));}
        } else if double {a!(self,ucomisd(dst,dst));}else {a!(self,ucomiss(dst,dst));}
        a!(self,jp(cold));
        self.cold_path_output(cold,independent.then_some(dst),move |e| {
            if independent {e.cache_copy(xmm0,dst);}
            e.scalar_nan(double,ns,ms);
            if independent {e.cache_copy(dst,xmm0);}
        });
        if !independent {self.store_v(d,xmm0);}
        self.scalar_zero(d,double);
        true
    }
    /// Keep two separately rounded operations, not an FMA. A NaN anywhere in
    /// MUL -> ADD propagates to the final result, so one hot guard suffices.
    /// The cold path replays both ARM NaN selections from unmodified originals.
    fn vchain_pair(&mut self, insn: u32) -> bool {
        if !self.u.vchain || !has_avx() || disabled_classes()!=0 {return false;}
        let Some(next) = self.u.look.first().copied() else {return false;};
        let ty = bits(insn,23,22);
        if insn & 0xff20_fc00 != 0x1e20_0800 || ty>1
            || next & 0xff20_fc00 != 0x1e20_2800 || bits(next,23,22)!=ty
            || self.u.labels.contains_key(&(self.pc+4)) {return false;}
        let t=insn&31;let d=next&31;let an=bits(next,9,5);let am=bits(next,20,16);
        if t==d || (an==t)==(am==t) {return false;}
        let n=bits(insn,9,5);let m=bits(insn,20,16);let other=if an==t {am}else {an};
        let Some(region)=self.u.region else {return false;};
        if [t,d,n,m,other].iter().any(|&r|region.vslot(r).is_none()) {return false;}
        let double=ty==1;
        let ns=self.scalar_source(n);let ms=self.scalar_source(m);let os=self.scalar_source(other);
        let nr=ns.cached.unwrap();let mr=ms.cached.unwrap();let or=os.cached.unwrap();
        let known=self.u.vzero[double as usize];
        if known & (1<<n)!=0 {self.fp_register(0,double,xmm0,nr,mr);}
        else if known & (1<<m)!=0 {self.fp_register(0,double,xmm0,mr,nr);}
        else {self.fld_source(xmm0,ns,double);self.fp_register(0,double,xmm0,xmm0,mr);}
        // Source 1 is the zero-extended product; source order only differs for
        // NaNs, whose original architectural order is restored below.
        self.fp_register(2,double,xmm1,xmm0,or);
        let cold=self.u.a.create_label();
        if double {a!(self,ucomisd(xmm1,xmm1));} else {a!(self,ucomiss(xmm1,xmm1));}
        a!(self,jp(cold));
        self.cold_path(cold,move |e| {
            let mut finite=e.new_label();
            if double {a!(e,ucomisd(xmm0,xmm0));} else {a!(e,ucomiss(xmm0,xmm0));}
            a!(e,jnp(finite));
            e.scalar_nan(double,ns,Some(ms));
            e.u.a.set_label(&mut finite).unwrap();
            // Cold-only scratch materialization: mapped originals are still
            // untouched, including destructive MUL aliases and ADD's old d.
            a!(e,movdqu(xmmword_ptr(r15+voff(t)),xmm0));
            e.fp_register(2,double,xmm1,xmm0,or);
            a!(e,movdqa(xmm0,xmm1));
            let product=FpSource{cached:None,offset:voff(t)};
            let (a,b)=if an==t {(product,os)}else {(os,product)};
            e.scalar_nan(double,a,Some(b));
            a!(e,movdqa(xmm1,xmm0));
            e.fld_source(xmm0,product,double);
        });
        self.store_v(t,xmm0);self.store_v(d,xmm1);
        self.scalar_zero(t,double);self.scalar_zero(d,double);
        self.u.skip_next=true;self.u.insns_translated+=1;
        true
    }
    /// A two-node scalar dataflow slice. The second operation consumes the
    /// first result, so NaNs propagate to its guard. Keep mapped originals
    /// intact until both strict NaN selections have been repaired on cold exit.
    fn scalar_pair(&mut self,insn:u32)->bool {
        if !self.u.vpair||!self.u.vchain||!has_avx()||disabled_classes()!=0 {return false;}
        let Some(next)=self.u.look.first().copied() else {return false;};
        if self.u.labels.contains_key(&(self.pc+4)) {return false;}
        let decode=|w:u32| {
            let ty=bits(w,23,22);if ty>1 {return None;}
            let op=if w&0xff200c00==0x1e200800&&bits(w,15,12)<=3 {bits(w,15,12)}
                else if w&0xff3ffc00==0x1e21c000 {4}else {return None;};
            Some((ty,op,w&31,bits(w,9,5),(op!=4).then(||bits(w,20,16))))
        };
        let Some((ty,a,t,n,m))=decode(insn) else {return false;};
        let Some((ty2,b,d,p,q))=decode(next) else {return false;};
        if ty!=ty2||(p!=t&&q!=Some(t)) {return false;}
        // Keep the existing specially tuned MUL->ADD path where applicable.
        if a==0&&b==2&&t!=d&&((p==t)^(q==Some(t))) {return false;}
        let Some(region)=self.u.region else {return false;};
        if [Some(t),Some(d),Some(n),m,Some(p),q].into_iter().flatten().any(|r|region.vslot(r).is_none()) {return false;}
        let double=ty==1;let known=self.u.vzero[double as usize];
        let ns=self.scalar_source(n);let ms=m.map(|r|self.scalar_source(r));
        let ps=self.scalar_source(p);let qs=q.map(|r|self.scalar_source(r));
        let nr=ns.cached.unwrap();let mr=ms.and_then(|s|s.cached).unwrap_or(nr);
        if known&(1<<n)!=0 {self.fp_register(a,double,xmm0,nr,mr);}
        else if matches!(a,0|2)&&m.is_some_and(|r|known&(1<<r)!=0) {self.fp_register(a,double,xmm0,mr,nr);}
        else {self.fld_source(xmm2,ns,double);self.fp_register(a,double,xmm0,xmm2,if a==4 {xmm2}else {mr});}
        let pr=if p==t {xmm0}else {ps.cached.unwrap()};
        let qr=if q==Some(t) {xmm0}else {qs.and_then(|s|s.cached).unwrap_or(pr)};
        if p==t||known&(1<<p)!=0 {self.fp_register(b,double,xmm1,pr,qr);}
        else if matches!(b,0|2)&&q.is_some_and(|r|r==t||known&(1<<r)!=0) {self.fp_register(b,double,xmm1,qr,pr);}
        else {self.fld_source(xmm2,ps,double);self.fp_register(b,double,xmm1,xmm2,qr);}
        let cold=self.u.a.create_label();
        if double {a!(self,ucomisd(xmm1,xmm1));}else {a!(self,ucomiss(xmm1,xmm1));}
        a!(self,jp(cold));
        self.cold_path(cold,move|e| {
            e.scalar_nan(double,ns,ms);
            a!(e,movdqu(xmmword_ptr(r15+voff(t)),xmm0));
            let product=FpSource{cached:None,offset:voff(t)};
            let n2=if p==t {product}else {ps};let m2=q.map(|r|if r==t {product}else {qs.unwrap()});
            e.fld_source(xmm0,n2,double);
            if let Some(src)=m2 {e.fld_source(xmm2,src,double);e.fp_register(b,double,xmm0,xmm0,xmm2);}
            else {e.fp_register(4,double,xmm0,xmm0,xmm0);}
            e.scalar_nan(double,n2,m2);a!(e,movdqa(xmm1,xmm0));
            if t!=d {e.fld_source(xmm0,product,double);}
        });
        if t!=d {self.store_v(t,xmm0);self.scalar_zero(t,double);}
        self.store_v(d,xmm1);self.scalar_zero(d,double);
        self.u.skip_next=true;self.u.insns_translated+=1;true
    }
    /// One branch for two adjacent plain selects sharing live flags. Unlike a
    /// chain of CMOVs this permits prediction to remove condition dependencies.
    /// Each path retains guest instruction order, aliases and W zero-extension.
    fn selectra_pair(&mut self, insn:u32)->bool {
        if !self.u.selectra||self.u.region.is_none()||disabled_classes()!=0 {return false;}
        let Some(next)=self.u.look.first().copied() else {return false;};
        let plain=|w:u32|w&0x7fe00c00==0x1a800000;
        let cond=bits(insn,15,12);
        if !plain(insn)||!plain(next)||bits(next,15,12)!=cond||self.u.labels.contains_key(&(self.pc+4)) {return false;}
        let Some(kind)=self.fuse_in else {return false;};
        let Some(cc_false)=x86_cc(kind,cond^1) else {return false;};
        let mut otherwise=self.new_label();let mut done=self.new_label();
        self.jcc(cc_false,otherwise);
        for w in [insn,next] {self.selected_move(w,true);}
        a!(self,jmp(done));self.u.a.set_label(&mut otherwise).unwrap();
        let false_start=self.u.a.instructions().len();
        for w in [insn,next] {self.selected_move(w,false);}
        if self.u.a.instructions().len()==false_start {a!(self,nop());}
        self.u.a.set_label(&mut done).unwrap();
        self.u.fuse=Some(kind);self.u.skip_next=true;self.u.insns_translated+=1;
        true
    }
    fn selected_move(&mut self,w:u32,when_true:bool) {
        let d=w&31;if d==31 {return;}
        let n=if when_true {bits(w,9,5)}else {bits(w,20,16)};
        let sf=bit(w,31)!=0;let source=self.src(n,false,sf,rcx);let dest=self.dst(d,false,rcx);
        if sf {self.copy(dest,source,true);}else {a!(self,mov(r32_of(dest),r32_of(source)));}
        self.stx(d,dest,false);
    }
    /// Boolean CSINC -> CSINV dataflow, not a comparator/function recognizer.
    /// When c1 OR c2 is a tautology, b1=(NOT c1), b2=(NOT c2) cannot
    /// both be one. The original second result is exactly b1-b2.
    fn cset_pair(&mut self,w:u32)->bool {
        if !self.u.csetpair||disabled_classes()!=0||w&0x7fff0fe0!=0x1a9f07e0 {return false;}
        let Some(next)=self.u.look.first().copied() else {return false;};
        let t=w&31;let d=next&31;let(c1,c2)=(bits(w,15,12),bits(next,15,12));
        if t==31||d==31||next&0x7fff0c00!=0x5a9f0000||bits(next,9,5)!=t
            ||(w^next)&0x80000000!=0||self.u.labels.contains_key(&(self.pc+4))
            ||!(0..16).all(|f|condition_bits(c1,f)||condition_bits(c2,f)) {return false;}
        let Some((a,b))=self.fuse_in.and_then(|k|Some((x86_cc(k,c1^1)?,x86_cc(k,c2^1)?))) else {return false;};
        a!(self,mov(ecx,0u32));a!(self,mov(edx,0u32));self.setcc_to(a,cl);self.setcc_to(b,dl);
        // Capture the first architectural output before subtracting. If it is
        // overwritten by the second, only the final version is observable.
        if t!=d {self.stx(t,rcx,false);}
        if bit(w,31)!=0 {a!(self,sub(rcx,rdx));}else {a!(self,sub(ecx,edx));}
        self.stx(d,rcx,false);
        // SUB is host bookkeeping, never guest NZCV. Subsequent consumers use
        // the existing canonical/packed flags, not this result's host flags.
        self.u.fuse=None;self.u.skip_next=true;self.u.insns_translated+=1;true
    }

    /// Register contracts are the same as the hot FP helper: only xmm0-xmm7
    /// and rax/rcx/rdx/r11 plus dead opmask k1 may be scratched, with no cache
    /// fills or helper calls. r11 must be preserved while the TBI mask is live.
    fn cold_path(&mut self, cold: CodeLabel, emit: impl FnOnce(&mut E<'_>) + 'static) {
        self.cold_path_output(cold,None,emit);
    }
    /// The optional output is exactly one already-assigned, non-aliasing V
    /// destination. Other cache/GPR/TBI-mask writes remain forbidden.
    fn cold_path_output(&mut self, mut cold: CodeLabel, output:Option<AsmRegisterXmm>, emit: impl FnOnce(&mut E<'_>) + 'static) {
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
                check_cold_registers_with_output(&e.u.a.instructions()[range.clone()], tbi_mask_live,
                    output.map(|r|iced_x86::Register::from(r).number() as u32)).unwrap();
                if let Some(r)=output {e.u.resident_cold_ranges.push((range,tbi_mask_live,iced_x86::Register::from(r).number() as u32));}
                else {e.u.cold_ranges.push((range, tbi_mask_live));}
            }
        }));
    }

    /// Is the current bank-local value read before a known overwrite in the
    /// lookahead? Unknown encodings are conservative; this affects speed only.
    fn used_soon_bank(&self, n: u32, vector: bool) -> bool {
        if !vector && self.u.region.is_some() { return false; }
        if !vector {
            // A fixed slot is not a reason to copy a value whose next use is
            // only on the next iteration. The existing backedge repair reloads
            // invalidated slots from canonical Cpu memory, preserving semantics.
            if let Some(l) = self.u.loop_cache {
                if !l.reg.contains(&(n as u8)) { return false; }
            }
        }
        if !jit_off("nextuse") { return self.u.cache_uses[vector as usize][n as usize] != u8::MAX; }
        self.u.look.iter().any(|&w| (w & 31) == n || ((w >> 5) & 31) == n || ((w >> 10) & 31) == n || ((w >> 16) & 31) == n)
    }
    fn used_soon(&self, n: u32) -> bool { self.used_soon_bank(n, false) }
    /// A label is about to be created: code after it may be conditional.
    fn new_label(&mut self) -> CodeLabel {
        if self.u.notbi_enabled { self.u.notbi.clear(); }
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
        if let Some(s) = self.region_slot(n) {
            a!(self, mov(r, LOOPREGS[s]));
        } else if let Some(s)=self.u.region.and_then(|region|region.xslot(n)) {
            if high_xmm(VREGS[s]) {evex!(self,vmovq(r,VREGS[s]));}
            else {a!(self,movq(r,VREGS[s]));}
        } else if let Some(s) = self.rc_find(n) {
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
        if let Some(s) = self.region_slot(n) {
            a!(self, mov(r32_of(r), LOOPREGS32[s]));
        } else if let Some(s)=self.u.region.and_then(|region|region.xslot(n)) {
            if high_xmm(VREGS[s]) {evex!(self,vmovd(r32_of(r),VREGS[s]));}
            else {a!(self,movd(r32_of(r),VREGS[s]));}
        } else if let Some(s) = self.rc_find(n) {
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
        if let Some(s) = self.region_slot(n) {
            self.dirty_region(1 << n);
            if LOOPREGS[s] != r { a!(self, mov(LOOPREGS[s], r)); }
        } else if let Some(s)=self.u.region.and_then(|region|region.xslot(n)) {
            if high_xmm(VREGS[s]) {evex!(self,vmovq(VREGS[s],r));}
            else {a!(self,movq(VREGS[s],r));}
        } else {
            a!(self, mov(qword_ptr(r15 + xoff(n)), r));
            self.rc_wrote(n, r);
        }
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
        if let Some(s) = self.region_slot(n) {
            self.dirty_region(1 << n);
            // MOV preserves a preceding flag producer, including immediate zero.
            a!(self, mov(LOOPREGS[s], v));
            return;
        }
        if self.u.region.is_some_and(|region|region.xslot(n).is_some()) {
            // Same scratch contract as the old large-immediate store path.
            self.u.tbi_mask_live=false;a!(self,mov(r11,v));self.stx(n,r11,false);return;
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
        if let Some(r)=self.u.region.and_then(|r|r.packed_flags()) {
            // Only CMP writers: RAX has no architectural result to preserve.
            // Break the partial-register dependency on the preceding address/
            // value before SETO+LAHF. MOV, unlike XOR, keeps fused host flags.
            a!(self,mov(eax,0u32));a!(self,seto(al));a!(self,lahf());a!(self,movzx(r32_of(r),ax));
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

    fn setcc_to(&mut self,cc:Cc,b:AsmRegister8) {
        match cc {
            Cc::E=>a!(self,sete(b)),Cc::Ne=>a!(self,setne(b)),Cc::B=>a!(self,setb(b)),Cc::Ae=>a!(self,setae(b)),
            Cc::S=>a!(self,sets(b)),Cc::Ns=>a!(self,setns(b)),Cc::O=>a!(self,seto(b)),Cc::No=>a!(self,setno(b)),
            Cc::A=>a!(self,seta(b)),Cc::Be=>a!(self,setbe(b)),Cc::L=>a!(self,setl(b)),Cc::Ge=>a!(self,setge(b)),
            Cc::G=>a!(self,setg(b)),Cc::Le=>a!(self,setle(b)),
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
        if let Some(r)=self.u.region.and_then(|r|r.packed_flags()) {
            let inverse=cond&1!=0;
            match cond>>1 {
                0|1|2|3 => {
                    let bit=match cond>>1 {0=>14,1=>8,2=>15,_=>0};
                    a!(self,bt(r,bit));
                    if inverse ^ (cond>>1==1) {a!(self,setnc(al));}else {a!(self,setc(al));}
                }
                4 => {a!(self,test(r,0x4100));if inverse {a!(self,setnz(al));}else {a!(self,setz(al));}}
                5|6 => {
                    a!(self,mov(eax,r32_of(r)));a!(self,shr(eax,15));a!(self,xor(eax,r32_of(r)));a!(self,and(eax,1));
                    if cond>>1==6 {a!(self,bt(r,14));a!(self,adc(eax,0));}
                    a!(self,test(eax,eax));
                    if inverse {a!(self,setnz(al));}else {a!(self,setz(al));}
                }
                _ => a!(self,mov(eax,1u32)),
            }
            return;
        }
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
        self.sync_region();
        self.dirty_region(u32::MAX); // interpreter helpers may change any GPR
        a!(self, mov(rdi, r15));
        a!(self, mov(esi, insn));
        self.mov_imm(rdx, self.pc);
        a!(self, call(qword_ptr(r15 + off::helper(HELPER_INTERP))));
        self.rc_clear();   // the interpreter may have written any guest register
        self.reload_region(); // MOV only: preserve the helper status in eax
        a!(self, test(eax, eax));
        let exit = self.dispatcher_exit();
        a!(self, jnz(exit));
    }

    /// Fallback for a control-flow instruction: interpreter sets pc; return
    /// to the dispatcher.
    fn fallback_terminator(&mut self, insn: u32) {
        self.fallback(insn);
        let exit = self.dispatcher_exit();
        a!(self, mov(qword_ptr(r15 + off::EXIT_REASON), EXIT_CONTINUE as i32));
        a!(self, mov(qword_ptr(r15 + off::EXIT_DATA), 0));
        a!(self, jmp(exit));
    }

    /// Jump to a direct successor: inside the unit, or through a stub.
    fn goto(&mut self, target: u64) {
        if self.u.quick {
            self.st_pc_and_exit(target);
            return;
        }
        if let Some(r) = self.u.region {
            if let Some(label) = self.u.region_labels.get(&target).copied() {
                if !self.u.commpage_checks && !jit_off("tbimask") && !self.u.tbi_mask_live {
                    a!(self, mov(r11, 0x00ff_ffff_ffff_ffffu64));
                }
                a!(self, jmp(label));
                return;
            }
            if target == r.pc && !self.u.no_internal_jumps {
                // A helper or scratch immediate may have invalidated the mask.
                // MOV preserves fused condition flags on this edge.
                if !self.u.commpage_checks && !jit_off("tbimask") && !self.u.tbi_mask_live {
                    a!(self, mov(r11, 0x00ff_ffff_ffff_ffffu64));
                }
                a!(self, jmp(r.fast));
                return;
            }
            if let Some(cmp) = self.u.loop_exit_compare {
                // CBZ/TBZ destroyed host flags, not the CMP operand. Read that
                // operand before region writeback/cache changes and recompute
                // exactly the original W/X subtraction. No live EFLAGS cache.
                let r = self.src(cmp.rn, false, cmp.sf, rax);
                if cmp.sf { a!(self, cmp(r, cmp.imm)); }
                else { a!(self, cmp(r32_of(r), cmp.imm)); }
                self.flags_sub(); // ARM C = !borrow; N/Z/V use the existing path
            }
            if let Some(kind) = self.u.loop_exit_flags {
                match kind { FuseKind::Add => self.flags_add(), FuseKind::Sub => self.flags_sub(), FuseKind::Logic => self.flags_logic() }
            }
            // Before any public entry or patchable slot (including a chained
            // stub), Cpu must be canonical. MOV stores preserve fused flags.
            self.writeback();
        }
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
        let link_site = self.u.a.instructions().len();
        if self.u.stub_kind == StubKind::Patchable {
            // jmp [slot]; the slot initially points at `cont`.
            a!(self, jmp(qword_ptr(slot)));
        } else {
            a!(self, jmp(cont));
        }
        self.u.stubs.push(Stub { target, cont, slot, link_site });
    }

    /// Guest effective address in rax; applies commpage redirection if needed.
    fn commpage_fix(&mut self) {
        self.commpage_fix_known(false);
    }

    fn commpage_fix_known(&mut self, untagged: bool) {
        // ARM top-byte-ignore: keep 55:0, with one dependent operation instead
        // of shl/shr. r11 is otherwise only st_imm's scratch; mov_imm invalidates
        // this copy. Do not establish it under conditional emission: a branch
        // could skip the load and join code that would otherwise trust it.
        // Redirected commpages call a host helper, so keep their existing path.
        // Only the audited immediate-scalar path can supply a proof. Keep
        // redirected commpages on their original stripping/helper path.
        if untagged && self.u.notbi_enabled && !self.u.commpage_checks {
            return;
        }
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
        self.sync_region();
        // rdx holds the original guest writeback/pair address inside regions.
        // Preserve it across this cold helper, keeping the stack aligned.
        let saved = if self.u.region.is_some() { rdx } else { r8 };
        a!(self, push(saved));
        a!(self, push(r9));
        a!(self, mov(rdi, rax));
        a!(self, call(qword_ptr(r15 + off::helper(HELPER_COMMPAGE))));
        a!(self, pop(r9));
        a!(self, pop(saved));
        self.reload_region();
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

/// Every private path must overwrite NZCV before observing it. The reader is
/// the exact region snapshot: any public/missing successor is live, not a proof.
/// CB/TB branches do not read NZCV; arbitrary helpers/FP arithmetic do observe
/// state (including their FPCR guards), so remain barriers here.
fn private_flags_dead_at(pc:u64,read:&dyn Fn(u64)->Option<u32>)->bool {
    let mut pending=vec![pc];let mut seen=std::collections::BTreeSet::new();
    while let Some(p)=pending.pop() {
        if !seen.insert(p) {continue;}
        if seen.len()>128 {return false;}
        let Some(w)=read(p) else {return false;};
        if writes_all_flags(w)&&flags_safe_writer(w) {continue;}
        if reads_flags(w) {return false;}
        if w&0xfc000000==0x14000000||w&0x7e000000==0x34000000||w&0x7e000000==0x36000000 {
            pending.extend(super::loops::successors(p,w));
        } else {
            if is_terminator(w)||!flags_safe_passthrough(w) {return false;}
            pending.push(p+4);
        }
    }
    true
}

/// A callee's flags may die through a *private pure-GPR* continuation CFG.
/// Public/missing successors, flag readers and helpers are barriers. Strengthen
/// the old passthrough census for memory forms the native emitter rejects.
fn leaf_private_flags_killed(pc:u64,words:&std::collections::BTreeMap<u64,u32>)->bool {
    private_flags_dead_at(pc,&|p| {
        let w=*words.get(&p)?;
        if (w>>25)&5==4 {
            if w&(1<<26)!=0 {return None;}
            if w&0x3a000000==0x28000000 {
                let load=w&(1<<22)!=0;let size=w>>30;
                if size==3||(size==1&&!load)||(load&&(w&31)==((w>>10)&31)) {return None;}
            }else if w&0x3b000000==0x39000000||w&0x3b200000==0x38000000||w&0x3b200c00==0x38200800 {
                if (w>>22)&3==3&&w>>30>=2 {return None;}
            }else {return None;}
        }
        Some(w)
    })
}

/// Forward proof stays within this exact public block, including AOT boundaries
/// and the max-length cut. Unknown encodings, partial writes, helpers and exits
/// stop it. Only audited scalar templates can consume register-only values:
/// fld_scalar/scalar_source capture aliases and cold-path inputs before writes.
/// Guest stores read Cpu directly (including pair/lane stores), so they are
/// observers, NOT cache consumers. Loop-region exit writeback cannot be crossed.
fn v_store_dead_at(d: u32, mut pc: u64, room: usize, labels: &HashMap<u64, CodeLabel>,
                   read: &dyn Fn(u64) -> Option<u32>) -> bool {
    for _ in 0..room {
        if labels.contains_key(&pc) { return false; }
        let Some(w) = read(pc) else { return false; };
        if is_terminator(w) { return false; }
        let scalar = bits(w, 23, 22) <= 1 && (
            (w & 0xff20_0c00 == 0x1e20_0800 && bits(w, 15, 12) <= 3)
            || w & 0xff3f_fc00 == 0x1e21_c000
            || (w & 0xff00_0000 == 0x1f00_0000 && has_fma() && !jit_off("fmadd")));
        let same = w & 0x9f20_0400 == 0x0e20_0400;
        let op = bits(w, 15, 11);
        let size = bits(w, 23, 22);
        // Audited full-vector writers using vld/vfin. These inputs read Cpu
        // directly, so aliases are observers even when the destination is a
        // full write. BSL/BIT/BIF also read the old destination: not killers.
        let vector = same && ((op == 3 && (bit(w, 29) == 0 || size == 0))
            || (op == 16 && (bit(w, 30) != 0 || size < 3)));
        if scalar {
            if w & 31 == d { return true; } // scalar clears all upper bits
        } else if vector {
            if bits(w, 9, 5) == d || bits(w, 20, 16) == d { return false; }
            if w & 31 == d { return true; }
        } else if (w >> 25) & 5 == 4 {
            // Only valid ordinary memory templates; commpage helpers excluded
            // by the caller, guest faults fatal. All SIMD stores read memory.
            if !flags_safe_passthrough(w) { return false; }
            let pair = w & 0x3a00_0000 == 0x2800_0000;
            let opc = bits(w, 23, 22);
            let size = bits(w, 31, 30);
            if w & (1 << 26) == 0 {
                // Native pair/access_sized reject these forms before falling
                // back. A fallback observes *all* Cpu state, even another Vd.
                if pair {
                    if (size == 1 && w & (1 << 22) == 0)
                        || (w & (1 << 22) != 0 && w & 31 == bits(w, 14, 10)) { return false; }
                } else if opc == 3 && size >= 2 { return false; }
            } else {
                if pair {
                    if size > 2 { return false; }
                } else if opc & 2 != 0 && size != 0 { return false; }
                let touches = w & 31 == d || (pair && bits(w, 14, 10) == d);
                if touches { return w & (1 << 22) != 0; }
            }
        } else if !flags_safe_passthrough(w) && !flags_safe_writer(w) {
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

/// Exact-token, default-off two-way IBTC. Runtime allocation, fill and emission
/// all consult this single frozen decision; OFF takes precedence over ON.
pub(crate) fn ibtc2_enabled() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| !jit_off("ibtc2") && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g| g == "ibtc2"))
}

/// Exact token opt-in; OFF takes precedence.
fn notbi_enabled() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| !jit_off("notbi") && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g| g == "notbi"))
}

/// Host call/return remains opt-in. OFF takes precedence over ON.
pub(super) fn callret_enabled() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| !jit_off("callret") && (crate::policy::enabled("callret") || std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g| g == "callret")))
}

/// Ten-register write-back region for JIT units and position-independent AOT blobs.
/// Debugging aid: ATTESOR_LOOPRA_BISECT=lo-hi forms regions only for the lo..hi-th candidate loops
/// (counted in translation order, per process), and logs each candidate's pc with ATTESOR_LOOPRA_LOG=1.
fn loopra_bisect(pc: u64) -> bool {
    static RANGE: std::sync::OnceLock<Option<(u64, u64)>> = std::sync::OnceLock::new();
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let range = *RANGE.get_or_init(|| {
        let v = std::env::var("ATTESOR_LOOPRA_BISECT").ok()?;
        let (a, b) = v.split_once('-')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    });
    let i = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if std::env::var_os("ATTESOR_LOOPRA_LOG").is_some() {
        let mut body = String::new();
        for k in 0..MAX_BLOCK_INSNS as u64 {
            let Some(w) = crate::guestmem::read_u64((pc + 4 * k) & !7).map(|v| if (pc + 4 * k) & 4 != 0 { (v >> 32) as u32 } else { v as u32 }) else { break };
            body += &format!("\n    {:#x}: {w:08x}  {}", pc + 4 * k, attesor_core::disasm::disasm(w));
            if is_terminator(w) { break; }
        }
        eprintln!("[loopra] candidate {i} pc={pc:#x} {}{body}", crate::symbols::describe(pc));
    }
    static ALLOW: std::sync::OnceLock<Option<std::collections::HashSet<u64>>> = std::sync::OnceLock::new();
    let allow = ALLOW.get_or_init(|| {
        let f = std::env::var("ATTESOR_LOOPRA_ALLOW").ok()?;
        Some(std::fs::read_to_string(f).ok()?.split_whitespace().filter_map(|t| u64::from_str_radix(t.trim_start_matches("0x"), 16).ok()).collect())
    });
    if let Some(set) = allow { return set.contains(&pc); }
    range.is_none_or(|(a, b)| i >= a && i < b)
}

/// Opt-in pending whole-tree performance and no-regression acceptance. OFF wins.
fn loopra_enabled() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| !jit_off("loopra") && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g| g == "loopra"))
}

/// Phase 2 remains opt-in, independently of the accepted phase-1 default.
fn loopra2_enabled() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| !jit_off("loopra2") && !jit_off("loopra")
        && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g| g == "loopra2"))
}

fn vloop_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !jit_off("vloop") && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|t| t=="vloop"))
}

fn vloop2_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !jit_off("vloop2") && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|t|t=="vloop2"))
}

fn vchain_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !jit_off("vchain") && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|t|t=="vchain"))
}

fn vscalar_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !jit_off("vscalar") && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|t|t=="vscalar"))
}

fn funra_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !jit_off("funra") && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|t|t=="funra"))
}

fn funflags_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !jit_off("funflags") && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|t|t=="funflags")
        && unsafe {std::arch::x86_64::__cpuid(0x80000000).eax>=0x80000001 && std::arch::x86_64::__cpuid(0x80000001).ecx&1!=0})
}

fn selectra_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !jit_off("selectra") && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|t|t=="selectra"))
}

fn regflags_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !jit_off("regflags") && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|t|t=="regflags")
        && unsafe {std::arch::x86_64::__cpuid(0x80000000).eax>=0x80000001 && std::arch::x86_64::__cpuid(0x80000001).ecx&1!=0})
}

fn cfgflags_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !jit_off("cfgflags") && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|t|t=="cfgflags"))
}

fn xspill_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(||!jit_off("xspill")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|t|t=="xspill"))
}

fn fmovra_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(||!jit_off("fmovra")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|t|t=="fmovra"))
}

fn vpair_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(||!jit_off("vpair")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|t|t=="vpair"))
}

fn addrhoist_enabled()->bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(||!jit_off("addrhoist")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="addrhoist"))
}

fn recleaflazy_enabled()->bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(||!jit_off("recleaflazy")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="recleaflazy"))
}

fn recleaf_enabled()->bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(||!jit_off("recleaf")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="recleaf"))
}

fn movflags_enabled()->bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(||!jit_off("movflags")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="movflags"))
}

fn funedge_enabled()->bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(||!jit_off("funedge")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="funedge"))
}

fn funstack_enabled()->bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(||!jit_off("funstack")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="funstack"))
}

fn bytestream_enabled()->bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(||!jit_off("bytestream")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="bytestream"))
}

fn foldaddr_enabled()->bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(||!jit_off("foldaddr")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="foldaddr"))
}

fn vbatch_enabled()->bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(||!jit_off("vbatch")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="vbatch"))
}

fn fpstable_enabled()->bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(||!jit_off("fpstable")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="fpstable"))
}

fn fpguard_enabled()->bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(||!jit_off("fpguard")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="fpguard"))
}

fn fpflags_enabled()->bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(||!jit_off("fpflags")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="fpflags"))
}

fn vtrace_enabled() -> bool {
    static ON:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *ON.get_or_init(||!jit_off("vtrace")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|t|t=="vtrace"))
}

fn inline_epoch_check(u:&Unit)->bool {u.callret||ibtc2_enabled()}

fn inlinewide_enabled()->bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(||!jit_off("inlinewide")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="inlinewide"))
}

fn inlinegp_enabled()->bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(||!jit_off("inlinegp")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="inlinegp"))
}

pub(super) fn leafflags_enabled()->bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(||!jit_off("leafflags")&&std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="leafflags"))
}

pub(super) fn leafinline_enabled() -> bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(|| !jit_off("leafinline") && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="leafinline"))
}

pub(super) fn inline_leaf_candidate(target:u64,read:&dyn Fn(u64)->Option<u32>)->bool {
    target&3==0&&leaf::plan(target,read,&|_|false).is_some_and(|p|p.written&(1<<30)==0)
}

fn leaf_calls(u:&Unit,pc:u64,w:u32,read:&dyn Fn(u64)->Option<u32>)->Option<(u64,leaf::Plan)> {
    if super::bliccache::inline_blocked(pc) {return None;}
    let target=if w&0xfc000000==0x94000000 {
        pc.wrapping_add((sext((w&0x03ffffff)as u64,26)<<2)as u64)
    }else if super::loops::indirect_call_reg(w).is_some() {super::bliccache::target_hint(pc)?}else {return None;};
    if target&3!=0 {return None;}
    let p=leaf::plan(target,read,&|a|u.labels.contains_key(&a))?;
    (p.written&(1<<30)==0).then_some((target,p))
}

fn csetpair_enabled() -> bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(|| !jit_off("csetpair") && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="csetpair"))
}

// Compile-time truth table, not a sampled guest condition. AL/NV are always.
fn condition_bits(cond:u32,nzcv:u32)->bool {
    let (n,z,c,v)=(nzcv&8!=0,nzcv&4!=0,nzcv&2!=0,nzcv&1!=0);
    let base=match cond>>1 {0=>z,1=>c,2=>n,3=>v,4=>c&&!z,5=>n==v,6=>!z&&n==v,_=>true};
    base^(cond<14&&cond&1!=0)
}

fn csetfast_enabled() -> bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(|| !jit_off("csetfast") && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="csetfast"))
}

fn leafra_enabled() -> bool {
    static F:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *F.get_or_init(|| !jit_off("leafra") && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g|g=="leafra"))
}

fn xspill_layout(u:&Unit,l:&super::loops::Loop,reg:&[u8;NLOOPREGS])->[u8;NVREGS] {
    let mut layout=[0xff;NVREGS];
    if !u.xspill||u.function_entry.is_some()||u.quick||u.no_internal_jumps||u.commpage_checks||disabled_classes()!=0 {return layout;}
    if !l.words.values().all(|&w| {
        // No architectural V use, including SIMD/FP memory. No calls/system
        // state, atomics or special scratch templates. Unknowns are barriers.
        (w>>25)&7!=7 && !((w>>25)&5==4&&bit(w,26)!=0)
        &&(flags_safe_passthrough(w)||flags_safe_writer(w)||w&0x3fe00800==0x1a800000
            ||w&0xfc000000==0x14000000||w&0xff000010==0x54000000
            ||w&0x7e000000==0x34000000||w&0x7e000000==0x36000000)
    }) {return layout;}
    let mut counts=[0u32;32];
    for &w in l.words.values() {let a=cache_access(w);if a.xr!=u32::MAX {
        for n in 0..32 {counts[n]+=((a.xr>>n)&1)*2+((a.xw>>n)&1);}
    }}
    let mut remaining:Vec<usize>=(0..32).filter(|&n|counts[n]!=0&&!reg.contains(&(n as u8))).collect();
    remaining.sort_by_key(|&n|(std::cmp::Reverse(counts[n]),n));
    let cap=if u.avx512 {NVREGS}else {BASE_NVREGS};
    for(s,n)in remaining.into_iter().take(cap).enumerate() {layout[s]=n as u8;}
    layout
}

/// Experimental write-through natural-loop region; public entries remain canonical.
fn regioncache_enabled() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| !jit_off("regioncache") && (crate::policy::enabled("regioncache") || std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g| g == "regioncache")))
}

// Discover only a single-entry basic block ending in a direct natural backedge.
// Stop at other Unit labels exactly as emit_block does; joins and other edges
// continue through public entries, never through this private cache contract.
fn loop_layout<const N: usize>(u: &Unit, pc: u64, read: &dyn Fn(u64) -> Option<u32>) -> Option<[u8; N]> {
    let mut counts = [0u32; 32];
    for i in 0..MAX_BLOCK_INSNS {
        let p = pc + i as u64 * 4;
        if i != 0 && u.labels.contains_key(&p) { return None; }
        let w = read(p)?;
        // CAS/LSE use rsi; software SHA256H/H2 use rsi/rdi/r8. Keep
        // these templates outside write-back regions, independently of host ISA.
        if N == NLOOPREGS && (w & 0x3fa0_7c00 == 0x08a0_7c00
            || w & 0x3f20_0c00 == 0x3820_0000
            || w & 0xffe0_ec00 == 0x5e00_4000) { return None; }
        let a = if u.fmovra {gpr_to_fp_access(w).unwrap_or_else(||cache_access(w))}else {cache_access(w)};
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
            let mut layout = [0xff; N];
            for (s, n) in regs.into_iter().take(N).enumerate() { layout[s] = n as u8; }
            return (layout[0] != 0xff).then_some(layout);
        }
    }
    None
}

/// Bounded single-block V write-back contract. Only scalar binary/sqrt and
/// ordinary single S/D loads/stores can touch V. No vector lanes, conversions,
/// calls, FPCR writes or extra public entries. Unsupported shapes use baseline.
fn vloop_layout(u: &Unit, pc: u64, read: &dyn Fn(u64) -> Option<u32>) -> [u8; NVREGS] {
    let empty = [0xff; NVREGS];
    if !u.vloop
        || u.quick || u.no_internal_jumps || u.commpage_checks || disabled_classes() != 0 { return empty; }
    let mut mask = 0u32;
    let mut fp = false;
    for i in 0..MAX_BLOCK_INSNS {
        let p = pc + i as u64 * 4;
        if i != 0 && u.labels.contains_key(&p) { return empty; }
        let Some(w) = read(p) else { return empty; };
        if is_terminator(w) {
            // Caller already proved the natural self backedge using loop_layout.
            let slots = if u.avx512 { NVREGS } else { BASE_NVREGS };
            if !fp || mask.count_ones() as usize > slots { return empty; }
            let mut regs = empty;
            for (s, n) in (0..32).filter(|&n| mask & (1 << n) != 0).enumerate() { regs[s] = n as u8; }
            return regs;
        }
        let scalar_binary = w & 0xff20_0c00 == 0x1e20_0800 && bits(w,23,22) <= 1 && bits(w,15,12) <= 3;
        let sqrt = w & 0xff3f_fc00 == 0x1e21_c000 && bits(w,23,22) <= 1;
        if scalar_binary || sqrt {
            fp = true;
            mask |= (1 << (w & 31)) | (1 << bits(w,9,5));
            if scalar_binary { mask |= 1 << bits(w,20,16); }
        } else if (w >> 25) & 5 == 4 && bit(w,26) != 0 {
            let single = w & 0x3b00_0000 == 0x3900_0000
                || w & 0x3b20_0000 == 0x3800_0000
                || w & 0x3b20_0c00 == 0x3820_0800;
            if !single || bits(w,31,30) < 2 || bits(w,23,22) > 1 || !flags_safe_passthrough(w) { return empty; }
            mask |= 1 << (w & 31);
        } else if !flags_safe_passthrough(w) && !flags_safe_writer(w) { return empty; }
    }
    empty
}

/// A single fixed assignment for every path of an already-proven private CFG.
/// Whole V registers, including unchanged upper bits, are loaded at the only
/// public entry and all are synchronized on exits. No phi/LRU guess at joins.
fn vregion_layout(u:&Unit,l:&super::loops::Loop)->[u8;NVREGS] {
    let empty=[0xff;NVREGS];
    if !u.vloop||!u.vloop2||u.quick||u.no_internal_jumps||u.commpage_checks||disabled_classes()!=0 {return empty;}
    let mut mask=0u32;let mut fp=false;
    for &w in l.words.values() {
        if is_terminator(w) {
            if w&0xfc000000==0x14000000||w&0xff000010==0x54000000
                ||w&0x7e000000==0x34000000||w&0x7e000000==0x36000000 {continue;}
            return empty;
        }
        let ty=bits(w,23,22);
        let binary=w&0xff200c00==0x1e200800&&ty<=1&&bits(w,15,12)<=3;
        let sqrt=w&0xff3ffc00==0x1e21c000&&ty<=1;
        let from_gpr=w&0x7f3efc00==0x1e260000&&bit(w,16)!=0&&bits(w,20,19)==0
            && ((bit(w,31)!=0&&ty==1)||(bit(w,31)==0&&ty==0));
        let immediate=w&0xff201fe0==0x1e201000&&ty<=1;
        if binary||sqrt {
            fp=true;mask|=(1<<(w&31))|(1<<bits(w,9,5));
            if binary {mask|=1<<bits(w,20,16);}
        } else if from_gpr||immediate {fp=true;mask|=1<<(w&31);}
        else if (w>>25)&5==4&&bit(w,26)!=0 {
            let single=w&0x3b000000==0x39000000||w&0x3b200000==0x38000000||w&0x3b200c00==0x38200800;
            if !single||bits(w,31,30)<2||bits(w,23,22)>1||!flags_safe_passthrough(w) {return empty;}
            fp=true;mask|=1<<(w&31);
        } else if !flags_safe_passthrough(w)&&!flags_safe_writer(w) {return empty;}
    }
    if !fp||mask.count_ones()as usize>if u.avx512 {NVREGS}else {BASE_NVREGS} {return empty;}
    let mut regs=empty;
    for(s,n)in(0..32).filter(|&n|mask&(1<<n)!=0).enumerate(){regs[s]=n as u8;}
    regs
}

/// AVX permits unaligned 128-bit arithmetic memory operands (without AVX2).
fn has_avx() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::is_x86_feature_detected!("avx"))
}

/// Only 128-bit EVEX forms are used, but all four feature sets are required.
/// macOS lazily enables AVX-512 state on the first instruction: XCR0 can be
/// 0x7 on a fully supported thread, so Rust's XCR0-based detector is not valid
/// there. Require CPUID *and* the kernel's sysctl policy; errors fail closed.
pub fn has_avx512() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| {
        #[cfg(target_os = "macos")]
        {
            let leaf = std::arch::x86_64::__cpuid_count(7, 0);
            let os = [c"hw.optional.avx512f", c"hw.optional.avx512vl",
                      c"hw.optional.avx512bw", c"hw.optional.avx512dq"].map(|name| {
                let mut value: libc::c_int = 0;
                let mut len = std::mem::size_of_val(&value);
                let rc = unsafe { libc::sysctlbyname(name.as_ptr(),
                    &mut value as *mut _ as *mut libc::c_void, &mut len,
                    std::ptr::null_mut(), 0) };
                rc == 0 && len == std::mem::size_of_val(&value) && value == 1
            });
            avx512_macos_policy(leaf.ebx, os)
        }
        #[cfg(not(target_os = "macos"))]
        {
            std::is_x86_feature_detected!("avx512f")
                && std::is_x86_feature_detected!("avx512vl")
                && std::is_x86_feature_detected!("avx512bw")
                && std::is_x86_feature_detected!("avx512dq")
        }
    })
}

#[cfg(any(target_os = "macos", test))]
fn avx512_macos_policy(features: u32, os: [bool; 4]) -> bool {
    const REQUIRED: u32 = (1 << 16) | (1 << 31) | (1 << 30) | (1 << 17); // F/VL/BW/DQ
    features & REQUIRED == REQUIRED && os.into_iter().all(|v| v)
}

pub fn avx512_enabled() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| has_avx512() && !jit_off("avx512")
        && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g| g == "avx512"))
}

pub fn avx512_nan_enabled() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| has_avx512() && !jit_off("avx512nan")
        && std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|g| g == "avx512nan"))
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

/// Resolve the lazy switches reachable by restricted background emission on the
/// foreground thread. The worker disables internal jumps, hence never enters
/// loopra_bisect (which may read live guest memory and perform diagnostic I/O).
pub(super) fn prepare_bgtrans() {
    let _ = (disabled_classes(), jit_off("bgtrans"), callret_enabled(),
        loopra_enabled(), regioncache_enabled(), has_avx(), has_fma(),
        has_aes(), has_pclmul(), has_sha(), has_f16c(), loopra2_enabled(),
        has_avx512(), avx512_enabled(), avx512_nan_enabled(), super::fastenc_enabled(),
        super::loops::census_enabled(), super::cost::probes(), vloop_enabled(), vloop2_enabled(), vchain_enabled(), vscalar_enabled(), funra_enabled(), funflags_enabled(), selectra_enabled(), regflags_enabled(), cfgflags_enabled(), xspill_enabled(), fmovra_enabled(), vpair_enabled(), vtrace_enabled(), vbatch_enabled(), inlinewide_enabled(), inlinegp_enabled(), bytestream_enabled(), recleaflazy_enabled(), recleaf_enabled(), movflags_enabled(), funedge_enabled(), funstack_enabled(), foldaddr_enabled(), fpflags_enabled(), fpguard_enabled(), fpstable_enabled(), addrhoist_enabled(), leafra_enabled(), csetfast_enabled(), csetpair_enabled(), leafinline_enabled(), leafflags_enabled(), super::bliccache::enabled());
}

/// Translate one basic block starting at `pc` into `u`. The caller must have
/// created and registered the label for `pc` if it wants intra-unit jumps.
/// Returns the guest address just past the block.
pub fn emit_block(u: &mut Unit, pc: u64, read: &dyn Fn(u64) -> Option<u32>) -> u64 {
    if u.leafinline && u.loopra && u.inline_ok && u.stub_kind==StubKind::Patchable
        && !u.quick&&!u.no_internal_jumps&&!u.commpage_checks&&disabled_classes()==0
        && u.function_entry.is_none()&&u.region_labels.is_empty()&&super::precise_invalidation_enabled() {
        let plans=std::cell::RefCell::new(HashMap::new());
        let accepted=|at,w| {
            if plans.borrow().len()>=4&&!plans.borrow().contains_key(&at) {return false;}
            if let Some(p)=leaf_calls(u,at,w,read) {plans.borrow_mut().insert(at,p);true}else {let _=w;false}
        };
        let l=super::loops::allocation_with_calls(pc,read,&accepted);
        let mut plans=plans.into_inner();
        if let Some(mut l)=l {
            plans.retain(|p,_|l.words.contains_key(p));
            let safe=!plans.is_empty()&&l.words.values().all(|&w|
                // Pure GPR caller; keep the original special scratch exclusions.
                (w>>25)&7!=7&&!((w>>25)&5==4&&w&(1<<26)!=0)
                &&w&0x3fa07c00!=0x08a07c00&&w&0x3f200c00!=0x38200000
                &&(flags_safe_passthrough(w)||flags_safe_writer(w)||w&0x3fe00800==0x1a800000
                    ||w&0xfc000000==0x14000000||w&0xff000010==0x54000000
                    ||w&0x7e000000==0x34000000||w&0x7e000000==0x36000000
                    ||w&0xfc000000==0x94000000||super::loops::indirect_call_reg(w).is_some()));
            l.starts.extend(u.labels.keys().filter(|p|l.words.contains_key(p)).copied());
            let mut n=0;for &p in l.words.keys() {if l.starts.contains(&p)||n==MAX_BLOCK_INSNS {l.starts.insert(p);n=0;}n+=1;}
            // Keep all native/target/epoch/reducibility/source-word proofs;
            // only the bounded number of private starts changes under opt-in.
            if safe&&l.starts.len()<=(if u.inlinewide {64}else {32})&&l.words.len()<=256&&l.reducible() {
                if l.starts.len()>32 {u.inlinewide_emissions+=1;
                    if std::env::var_os("ATTESOR_INLINEWIDE_LOG").is_some() {eprintln!("inlinewide-region\t{pc:x}\t{}\t{}",l.starts.len(),l.words.len());}
                }
                if std::env::var_os("ATTESOR_LEAFINLINE_LOG").is_some() {eprintln!("leafinline-region\t{pc:x}\t{}\t{}",l.words.len(),plans.len());}
                u.inline_call_words=plans.keys().map(|&p|(p,l.words[&p])).collect();
                if u.leafflags&&!inline_epoch_check(u) {
                    u.inline_flag_continuations=plans.keys().filter(|&&p|l.starts.contains(&(p+4))&&leaf_private_flags_killed(p+4,&l.words)).map(|&p|(p,None)).collect();
                }
                u.inline_leaf_calls=plans;let end=emit_multi_region(u,&l,read);u.inline_leaf_calls.clear();u.inline_call_words.clear();u.inline_flag_continuations.clear();u.inline_follow_branches.clear();return end;
            }
        }
        // A call whose enclosing loop cannot be proven can still bypass the
        // two dispatch boundaries. All ordinary public Cpu rules remain.
        let mut at=pc;for _ in 0..MAX_BLOCK_INSNS {
            let Some(w)=read(at)else {break;};
            if let Some(p)=leaf_calls(u,at,w,read) {
                if u.leafflags&&!inline_epoch_check(u) {if let Some(w)=read(at+4).filter(|&w|flags_safe_writer(w)) {
                    u.inline_flag_continuations.insert(at,Some(w));
                    if let Some(b)=read(at+8).filter(|b|*b&0xff000010==0x54000000) {u.inline_follow_branches.insert(at,b);}
                }}
                u.inline_leaf_calls.insert(at,p);u.inline_call_words.insert(at,w);break;
            }
            if is_terminator(w) {break;}at+=4;
        }
        if !u.inline_leaf_calls.is_empty() {let end=emit_block_inner(u,pc,read,None);u.inline_leaf_calls.clear();u.inline_call_words.clear();u.inline_flag_continuations.clear();u.inline_follow_branches.clear();return end;}
    }
    if u.leafra && u.loopra && !u.quick && !u.no_internal_jumps && !u.commpage_checks
        && disabled_classes()==0 && u.function_entry.is_none() && u.region_labels.is_empty() {
        if let Some(p)=leaf::plan(pc,read,&|p|u.labels.contains_key(&p)) {
            let mut reg=[0xff;NLOOPREGS];
            for (s,n) in (0..32).filter(|&n|p.used&(1u32<<n)!=0).enumerate() {reg[s]=n as u8;}
            let r=LoopRa{pc,fast:u.a.create_label(),exit:u.a.create_label(),reg,dirty:0,
                vreg:[0xff;NVREGS],xreg:[0xff;NVREGS],flags:0xff};
            let hi=*p.words.last_key_value().unwrap().0+4;
            u.extra_guest_range=Some(u.extra_guest_range.map_or((pc,hi),|(a,b)|(a.min(pc),b.max(hi))));
            u.region_exits.push(r);u.region_labels.insert(pc,r.fast);u.leaf_livein=Some(p.livein);
            let end=emit_block_inner(u,pc,&|a|p.words.get(&a).copied(),Some(r));
            u.leaf_livein=None;u.region_labels.clear();u.region=None;
            return end;
        }
    }
    if u.funra && u.loopra && !u.quick && !u.no_internal_jumps && !u.commpage_checks
        && disabled_classes()==0 && u.function_entry.is_none() && u.region_labels.is_empty() {
        if let Some(mut f)=functions::recursive(pc,read) {
            f.starts.extend(u.labels.keys().filter(|p|f.words.contains_key(p)).copied());
            let mut n=0;
            for &p in f.words.keys() {
                if f.starts.contains(&p)||n==MAX_BLOCK_INSNS {f.starts.insert(p);n=0;}
                n+=1;
            }
            if f.starts.len()<=32 {
                u.function_entry=Some(pc);
                let end=emit_multi_region(u,&f,read);
                u.function_entry=None;
                return end;
            }
        }
    }
    if u.loopra2 && u.loopra && !u.no_internal_jumps
        && loop_layout::<NLOOPREGS>(u, pc, read).is_none() {
        if let Some(mut l) = super::loops::allocation(pc, read).filter(|l| l.starts.len() > 1) {
            // Calls/context changes and the phase-1 scratch exclusions remain
            // barriers. Unknown non-terminators still use synchronized helpers.
            let safe = l.words.values().all(|&w| {
                !(w & 0x3fa0_7c00 == 0x08a0_7c00 || w & 0x3f20_0c00 == 0x3820_0000
                    || w & 0xffe0_ec00 == 0x5e00_4000)
                && (!is_terminator(w) || w & 0xfc00_0000 == 0x1400_0000
                    || w & 0xff00_0010 == 0x5400_0000 || w & 0x7e00_0000 == 0x3400_0000
                    || w & 0x7e00_0000 == 0x3600_0000)
            });
            // Public AOT/profile cuts and maximum-length cuts are private block
            // boundaries too. No fusion/dead-store proof may straddle them.
            l.starts.extend(u.labels.keys().filter(|p| l.words.contains_key(p)).copied());
            let mut n = 0;
            for &p in l.words.keys() {
                if l.starts.contains(&p) || n == MAX_BLOCK_INSNS { l.starts.insert(p); n = 0; }
                n += 1;
            }
            // A GPR-only outer clone otherwise captures an inner V-resident
            // loop and silently replaces its stronger backedge contract.
            // Prefer the proven scalar inner loop; do not infer an assignment
            // across joins or select paths by executable/benchmark identity.
            let inner_vloop = u.vloop && vregion_layout(u,&l)[0]==0xff && l.starts.iter().any(|&p| {
                let snapshot = |at| l.words.get(&at).copied();
                p != pc && loop_layout::<NLOOPREGS>(u, p, &snapshot).is_some()
                    && vloop_layout(u, p, &snapshot)[0] != 0xff
            });
            if safe && !inner_vloop && l.starts.len() <= 32 && l.words.len() <= 256 && l.reducible() && loopra_bisect(pc) {
                return emit_multi_region(u, &l, read);
            }
        }
    }
    emit_block_inner(u, pc, read, None)
}

fn emit_multi_region(u: &mut Unit, l: &super::loops::Loop, read: &dyn Fn(u64) -> Option<u32>) -> u64 {
    assert!(GUEST_FAULTS_FATAL); // identical fatal-fault contract to phase 1
    // A block's FPCR entry guard precedes even its first GPR writer. Do not
    // infer that this writer kills incoming flags before that observer.
    let cfgflags=u.cfgflags;
    u.cfgflags &= u.inline_leaf_calls.is_empty() && u.function_entry.is_none() && l.words.values().all(|&w|(w>>25)&7!=7);
    if super::loops::census_enabled() {
        eprintln!("{}-region\t{:x}\t{}\t{}",if u.function_entry.is_some(){"funra"}else{"loopra2"},l.header,l.starts.len(),l.words.len());
    }
    let mut counts = [0u32; 32];
    for &w in l.words.values() {
        let a = if u.fmovra {gpr_to_fp_access(w).unwrap_or_else(||cache_access(w))}else {cache_access(w)};
        if a.xr != u32::MAX {
            for n in 0..32 { counts[n] += ((a.xr >> n) & 1) * 2 + ((a.xw >> n) & 1); }
        }
    }
    // Inline callees are explicit producers/consumers, not ABI-preserved guesses.
    for (_,p) in u.inline_leaf_calls.values() {for n in 0..32 {if p.used&(1<<n)!=0 {counts[n]+=4;}}}
    let mut regs: Vec<usize> = (0..32).filter(|&n| counts[n] != 0).collect();
    regs.sort_by_key(|&n| (std::cmp::Reverse(counts[n]), n));
    let mut reg = [0xff; NLOOPREGS];
    for (s, n) in regs.into_iter().take(NLOOPREGS).enumerate() { reg[s] = n as u8; }
    let vreg=if u.function_entry.is_none(){vregion_layout(u,l)}else {[0xff;NVREGS]};
    let xreg=xspill_layout(u,l,&reg);
    debug_assert!(xreg[0]==0xff||vreg[0]==0xff);
    // Do not treat "not a known GPR writer" as proof of flag transparency:
    // FCMP/FCCMP, MSR NZCV and ADC/SBC have separate native flag paths.
    let proven_flags=u.function_entry.is_some() || vreg[0]!=0xff || l.words.values().all(|&w|
        flags_safe_passthrough(w)||flags_safe_writer(w)||w&0x3fe00800==0x1a800000
        ||w&0xfc000000==0x14000000||w&0xff000010==0x54000000
        ||w&0x7e000000==0x34000000||w&0x7e000000==0x36000000);
    let flags=if proven_flags && ((u.funflags && u.function_entry.is_some()) || (u.regflags && u.function_entry.is_none()))
        && l.words.values().any(|&w|flags_safe_writer(w)) && l.words.values().all(|&w|
        !flags_safe_writer(w)||(w&31==31 && matches!(fusable_setter(w),Some(FuseKind::Sub)))) {
        reg.iter().position(|&n|n==0xff).map_or(0xff,|s|s as u8)
    }else {0xff};
    let region = LoopRa { pc: l.header, fast: u.a.create_label(), exit: u.a.create_label(), reg,
        // Every predecessor/iteration can have written any assigned slot.
        // A compile-order dirtiness mask is NOT a dataflow solution at joins.
        dirty: u32::MAX, vreg,xreg,flags };
    u.region_exits.push(region);
    let mut saved = Vec::new();
    for &p in &l.starts {
        let label = if p == l.header { region.fast } else { u.a.create_label() };
        u.region_labels.insert(p, label);
        if p != l.header { saved.push((p, u.labels.insert(p, label))); }
    }
    u.funedge_analysis=if u.funedge && u.function_entry==Some(l.header)
        && region.packed_flags().is_some() && !u.no_internal_jumps && !u.commpage_checks
        && disabled_classes()==0 && u.inline_leaf_calls.is_empty() && !super::cost::probes() {
        fun_edges::analyze(l)
    }else {fun_edges::Analysis::default()};
    u.recleaf_plan=if u.recleaf&&u.function_entry==Some(l.header)&&!u.commpage_checks
        &&!u.no_internal_jumps&&!u.quick&&!u.xspill&&disabled_classes()==0&&u.inline_leaf_calls.is_empty()
        &&!super::cost::probes() {rec_leaf::plan(l)}else {None};
    u.recleaf_lazy_proof=if u.recleaflazy&&region.packed_flags().is_some() {
        u.recleaf_plan.as_ref().map(rec_leaf::witnesses).unwrap_or_default()
    }else {std::collections::BTreeMap::new()};
    let lo = *l.words.first_key_value().unwrap().0;
    let hi = *l.words.last_key_value().unwrap().0 + 4;
    u.extra_guest_range = Some(u.extra_guest_range.map_or((lo, hi), |(a,b)| (a.min(lo),b.max(hi))));
    // Snapshot exactly the selected members. A cut/unreadable successor exits
    // canonically rather than accidentally emitting past the discovered region.
    let snapshot = |p| l.words.get(&p).copied();
    super::loops::census(l.header, read);
    let end = emit_block_inner(u, l.header, &snapshot, Some(region));
    for &p in l.starts.iter().filter(|&&p| p != l.header) {
        emit_block_inner(u, p, &snapshot, Some(region));
    }
    for (p, old) in saved {
        if let Some(label) = old { u.labels.insert(p, label); } else { u.labels.remove(&p); }
    }
    u.region_labels.clear();
    u.region = None;
    u.funedge_analysis=fun_edges::Analysis::default();u.pending_funedge=None;
    u.recleaf_plan=None;u.recleaf_context=None;u.recleaf_lazy_proof.clear();
    u.cfgflags=cfgflags;
    end
}

fn emit_block_inner(u: &mut Unit, pc: u64, read: &dyn Fn(u64) -> Option<u32>, multi: Option<LoopRa>) -> u64 {
    super::loops::census(pc, read);
    u.pending_funedge=None;
    u.fuse = None;
    u.loop_cache = None;
    u.region = multi;
    let mut fp_sink:Option<fp_flags::Plan>=None;
    let mut fp_body=None;
    let mut batch=None;
    let mut fp_guard_outlined=false;
    let mut fp_mode_hoisted=false;
    u.addr_shadow=None;
    if u.notbi_enabled { u.notbi.clear(); } // no facts cross public/private loop entries
    u.loop_exit_flags = None;
    u.loop_exit_compare = None;
    u.tbi_mask_live = multi.is_some() && !u.commpage_checks && !jit_off("tbimask");
    u.rc = RegCache::new();
    u.vc = VCache::new();
    u.vzero = [0; 2];
    u.rc_frozen = false;
    u.look.clear();
    if !u.quick {
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
    if super::cost::probes() {
        u.a.mov(qword_ptr(r15 + off::PROFILE_STATE), crate::profile::State::Jit as i32).unwrap();
    }
    if multi.is_some_and(|r| r.pc != pc) {
        // Private internal entry: all incoming edges share the same assignment.
    } else if u.loopra && !u.no_internal_jumps {
        let layout = multi.map(|r| r.reg).or_else(|| loop_layout::<NLOOPREGS>(u, pc, read).filter(|_| loopra_bisect(pc)));
        if let Some(reg) = layout {
            // signals.rs does not deliver guest fault handlers. Register-only
            // state is therefore visible only in fatal crash dumps; revisit this
            // feature before implementing resumable guest faults.
            assert!(GUEST_FAULTS_FATAL);
            let vreg = if multi.is_none() { vloop_layout(u, pc, read) } else { [0xff; NVREGS] };
            let region = multi.unwrap_or_else(|| LoopRa { pc, fast: u.a.create_label(), exit: u.a.create_label(), reg, dirty: 0, vreg,xreg:[0xff;NVREGS],flags:0xff });
            let fast = region.fast;
            // JIT-only instrumentation: absolute host addresses must never enter
            // an AOT blob. Count executed public entries, not translations.
            if u.function_entry.is_none() && u.leaf_livein.is_none() && u.stub_kind == StubKind::Patchable && super::COUNT_LOOP_REGIONS.load(std::sync::atomic::Ordering::Relaxed) {
                u.a.mov(rax, &super::STATS_LOOP_REGIONS as *const _ as u64).unwrap();
                u.a.lock().inc(qword_ptr(rax)).unwrap();
                if multi.is_some() {
                    u.a.mov(rax, &super::STATS_MULTI_LOOP_REGIONS as *const _ as u64).unwrap();
                    u.a.lock().inc(qword_ptr(rax)).unwrap();
                }
            }
            if u.function_entry.is_none() && u.leaf_livein.is_none() && u.stub_kind == StubKind::Plain && super::stats_enabled() {
                // Cpu belongs to this thread; no host pointer or locked shared
                // counter enters the disk blob. Count entries, not backedges.
                u.a.inc(qword_ptr(r15 + off::AOT_LOOP_REGIONS)).unwrap();
            }
            region.reload_mask(&mut u.a,u.leaf_livein.unwrap_or(u32::MAX));
            if u.function_entry==Some(pc) {
                // Private recursive calls bypass this canonical public entry.
                // A zero native-continuation marker separates this activation
                // from an unrelated caller's existing host-call frames.
                u.a.push(0i32).unwrap();u.a.push(0i32).unwrap();
            }
            if !u.commpage_checks && !jit_off("tbimask") {
                u.a.mov(r11, 0x00ff_ffff_ffff_ffffu64).unwrap();
                u.tbi_mask_live = true;
            }
            if let Some(p)=byte_stream::plan(u,pc,region,read) {
                u.region_exits.push(region);let end=byte_stream::emit(u,pc,p,region);u.labels.insert(pc,label);return end;
            }
            let plan=if u.fpflags&&multi.is_none()&&region.vreg[0]!=0xff&&u.vchain&&has_avx()
                &&!u.commpage_checks&&disabled_classes()==0 {
                fp_flags::plan(pc,read,&|p|u.labels.contains_key(&p))
            }else {None};
            if let Some(p)=plan.and_then(|p|fold_addr::plan(u,pc,p,region,read)) {
                u.region_exits.push(region);fold_addr::emit(u,pc,p,region);u.labels.insert(pc,label);return pc+32;
            }
            if let Some(p)=plan.and_then(|p|fp_batch::plan(u,pc,p,region,read)) {fp_batch::enter(u,p,region);batch=Some(p);}
            if let Some((p,base,index,shadow))=plan.filter(|_|u.addrhoist).and_then(|p|addr_hoist::plan(pc,p,read)).and_then(|p|
                Some((p,region.reg.iter().position(|&n|n==p.rn as u8)?,region.reg.iter().position(|&n|n==p.rm as u8)?,region.reg.iter().rposition(|&n|n==0xff)?))) {
                addr_hoist::enter(u,p,region,base,index,shadow);u.addr_shadow=Some((p,shadow));
            }
            if let Some(plan)=plan.filter(|p|u.fpstable&&fp_flags::read_only_mode(pc,*p,read)) {
                assert!(GUEST_FAULTS_FATAL);
                let mut bad=u.a.create_label();
                u.a.test(dword_ptr(r15+off::FPCR),FPCR_NONDEFAULT as i32).unwrap();u.a.jnz(bad).unwrap();
                // The public guard precedes every guest effect. Only this
                // audited, store-/helper-/system-free loop bypasses it privately.
                let mut fast_label=fast;u.a.set_label(&mut fast_label).unwrap();u.region=Some(region);
                fp_guard_outlined=true;fp_mode_hoisted=true;fp_sink=Some(plan);u.fp_mode_hoists+=1;
                u.cold_paths.push(Box::new(move|u| {u.a.set_label(&mut bad).unwrap();fp_flags::reject(u,pc,region,None);}));
                if std::env::var_os("ATTESOR_FPSTABLE_LOG").is_some() {eprintln!("fpstable-loop\t{pc:x}");}
            }else if let Some(plan)=plan.filter(|_|u.fpguard) {
                // A public entry cannot reconstruct a CMP that has not run.
                // Both paths keep their FPCR check, but only private rejection
                // repairs the previous iteration's known comparison.
                let mut public_bad=u.a.create_label();let mut private_bad=u.a.create_label();let mut body=u.a.create_label();
                u.a.test(dword_ptr(r15+off::FPCR),FPCR_NONDEFAULT as i32).unwrap();u.a.jnz(public_bad).unwrap();u.a.jmp(body).unwrap();
                let mut fast_label=fast;u.a.set_label(&mut fast_label).unwrap();u.region=Some(region);
                u.a.test(dword_ptr(r15+off::FPCR),FPCR_NONDEFAULT as i32).unwrap();u.a.jnz(private_bad).unwrap();
                u.a.set_label(&mut body).unwrap();fp_guard_outlined=true;fp_sink=Some(plan);
                u.cold_paths.push(Box::new(move |u| {
                    u.a.set_label(&mut public_bad).unwrap();
                    fp_flags::reject(u,pc,region,None);
                    u.a.set_label(&mut private_bad).unwrap();
                    fp_flags::reject(u,pc,region,Some(plan));
                }));
            }else {
                let public_guard=plan.map(|_|u.a.create_label());
                if let Some(g)=public_guard {u.a.jmp(g).unwrap();}
                let mut fast_label = fast;
                u.a.set_label(&mut fast_label).unwrap();
                u.region = Some(region);
                if let Some(plan)=plan {
                    let body=u.a.create_label();fp_body=Some(body);fp_sink=Some(plan);
                    // Every backedge still checks FPCR. On rejection, the previous
                    // tail CMP is reconstructed BEFORE canonical writeback/deopt.
                    u.a.test(dword_ptr(r15+off::FPCR),FPCR_NONDEFAULT as i32).unwrap();u.a.jz(body).unwrap();
                    {u.dead_flags=false;let mut e=E{u,pc,fuse_in:None,scalar_inputs:None};
                        e.fpflags_compare(plan);e.flags_sub();e.mov_imm(rax,pc);
                        a!(e,mov(qword_ptr(r15+off::PC),rax));a!(e,mov(qword_ptr(r15+off::EXIT_REASON),EXIT_INTERP as i32));
                        let exit=e.dispatcher_exit();a!(e,jmp(exit));}
                    let mut g=public_guard.unwrap();u.a.set_label(&mut g).unwrap();
                }
            }
            if multi.is_none() { u.region_exits.push(region); }
        }
    } else if u.regioncache && !u.no_internal_jumps {
        if let Some(reg) = loop_layout::<NCREGS>(u, pc, read) {
            for (s, &n) in reg.iter().enumerate() {
                if n != 0xff { u.a.mov(CREGS[s], qword_ptr(r15 + xoff(n as u32))).unwrap(); }
            }
            let mut fast = u.a.create_label();
            u.a.set_label(&mut fast).unwrap();
            u.loop_cache = Some(LoopCache { pc, fast, reg });
            u.rc.reg = reg;
        }
    }
    // Diagnostic-only exact block-PC publication, after the private backedge
    // label too. Existing sampling otherwise sees stale PCs in chained loops.
    if super::loops::census_enabled() {
        u.a.mov(rax, pc).unwrap();
        u.a.mov(qword_ptr(r15 + off::PC), rax).unwrap();
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
        if fp && !fp_guard_outlined {
            let mut body = fp_body.unwrap_or_else(||u.a.create_label());
            u.a.test(dword_ptr(r15 + off::FPCR), FPCR_NONDEFAULT as i32).unwrap();
            u.a.jz(body).unwrap();
            let mut e = E { u, pc, fuse_in: None, scalar_inputs: None };
            e.mov_imm(rax, pc);
            a!(e, mov(qword_ptr(r15 + off::PC), rax));
            a!(e, mov(qword_ptr(r15 + off::EXIT_REASON), EXIT_INTERP as i32));
            let exit = e.dispatcher_exit();
            a!(e, jmp(exit));
            u.a.set_label(&mut body).unwrap();
        }
    }
    // set_label updates the label; keep the updated value for label_ip().
    u.labels.insert(pc, label);
    let loop_body_start = u.a.instructions().len();
    if let Some(p)=batch {
        fp_batch::emit(u,pc,p);u.emit_cold_paths();
        fp_flags::check_mode_emission(u,loop_body_start).expect("vbatch backend contract");
        return pc+32;
    }
    if let Some(plan)=fun_stack::plan(u,pc,read) {
        return fun_stack::emit(u,pc,plan);
    }
    emit_block_body(u,pc,read,fp_sink,fp_mode_hoisted,loop_body_start)
}

fn emit_block_body(u:&mut Unit,pc:u64,read:&dyn Fn(u64)->Option<u32>,fp_sink:Option<fp_flags::Plan>,fp_mode_hoisted:bool,loop_body_start:usize)->u64 {
    let mut cur = pc;
    let mut count = 0;
    loop {
        let Some(insn) = read(cur) else {
            // Unreadable: let the dispatcher fault properly.
            let mut e = E { u, pc: cur, fuse_in: None, scalar_inputs: None };
            e.st_pc_and_exit(cur);
            u.emit_cold_paths();
            if fp_mode_hoisted {fp_flags::check_mode_emission(u,loop_body_start).expect("fpstable backend contract");}
            return cur;
        };
        let previous_fuse = u.fuse.take();
        let fuse_in = if u.quick { None } else { previous_fuse };
        if !u.quick && !jit_off("nextuse") { u.cache_uses = cache_next_uses(&u.look); }
        u.rc_frozen = false;
        u.dead_flags = false;
        if let Some(kind) = fusable_setter(insn).filter(|_| !u.quick) {
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
                    // On a self backedge, NZCV can stay in host flags until
                    // the exit when the next iteration overwrites it before a
                    // reader. No helper may observe it in between. Faults are
                    // fatal (GUEST_FAULTS_FATAL), as for flags_dead_at itself.
                    if u.region.is_some() && u.region_labels.is_empty() && target == pc && !u.commpage_checks
                        && disabled_classes() == 0 && flags_dead_at(pc, read)
                        && !u.a.instructions()[loop_body_start..].iter().any(|i| i.mnemonic() == iced_x86::Mnemonic::Call)
                    {
                        u.dead_flags = true;
                        u.loop_exit_flags = Some(kind);
                    }
                    if flags_dead_at(target, read) && flags_dead_at(cur + 8, read) {
                        u.dead_flags = true;
                        // Both successors overwrite NZCV. In particular, a
                        // region needs no deferred flag stores on its exit.
                        u.loop_exit_flags = None;
                        DEAD_FLAG_SKIPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if let Some(edge)=u.funedge_analysis.edges.get(&cur).copied() {
                        assert_eq!(edge.cmp,insn);assert_eq!(edge.branch,nx);
                        u.dead_flags=true;u.loop_exit_flags=None;u.loop_exit_compare=None;
                        u.pending_funedge=Some(edge);
                    }
                    if u.cfgflags && !u.region_labels.is_empty() && !u.commpage_checks
                        && !u.no_internal_jumps && disabled_classes()==0 && u.region.is_some_and(|r|r.packed_flags().is_none()) {
                        let successors=[target,cur+8];
                        // A private edge must kill the flags first. Public edges
                        // still serialize the live producer, after JCC decided
                        // the edge; goto() returns before serialization privately.
                        if successors.iter().all(|p|!u.region_labels.contains_key(p)||private_flags_dead_at(*p,read)) {
                            u.dead_flags=true;
                            u.loop_exit_flags=successors.iter().any(|p|!u.region_labels.contains_key(p)).then_some(kind);
                        }
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
        // A deliberately narrow lazy-NZCV slice. Every proof dependency is
        // inside this emitted block (including the private edge's next kill),
        // so normal unit source invalidation also covers the entire proof.
        // Unknown/native-disabled paths retain their original materialization.
        if super::flagreg::enabled() && u.region.is_some() && u.region_labels.is_empty()
            && !u.no_internal_jumps && disabled_classes() == 0
            && count + 1 < MAX_BLOCK_INSNS
        {
            if let Some(cmp) = super::flagreg::exit_compare(
                pc, cur, MAX_BLOCK_INSNS, &|p| u.labels.contains_key(&p), read,
            ) {
                u.dead_flags = true;
                u.loop_exit_compare = Some(cmp);
            }
        }
        if fp_sink.is_some_and(|p|p.cmp_pc==cur)
            && !u.a.instructions()[loop_body_start..].iter().any(|i|i.mnemonic()==iced_x86::Mnemonic::Call) {
            u.dead_flags=true;u.loop_exit_flags=Some(FuseKind::Sub);u.loop_exit_compare=None;u.fpflags_sinks+=1;
            if std::env::var_os("ATTESOR_FPFLAGS_LOG").is_some() {eprintln!("fpflags-sink\t{pc:x}\t{cur:x}");}
        }
        u.dead_v = 0;
        if !u.quick && !jit_off("vdead") && !jit_off("vcache") && !jit_off("scalaralias")
            && disabled_classes() == 0 && !u.commpage_checks {
            if let Some(regs) = v_writes(insn) {
                for d in regs.into_iter().filter(|&d| d < 32) {
                    if v_store_dead_at(d as u32, cur + 4, MAX_BLOCK_INSNS - count - 1, &u.labels, read) {
                        u.dead_v |= 1 << d;
                    }
                }
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
                        u.vc.dirty[s] = false;
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
        let vtrace=e.scalar_trace(insn,MAX_BLOCK_INSNS-count);
        let vpair=!vtrace&&count+1<MAX_BLOCK_INSNS&&e.scalar_pair(insn);
        let vchain = !vtrace && !vpair && count+1<MAX_BLOCK_INSNS && e.vchain_pair(insn);
        let selectpair = !vtrace && !vpair && !vchain && count+1<MAX_BLOCK_INSNS && e.selectra_pair(insn);
        let csetpair=!vtrace&&!vpair&&!vchain&&!selectpair&&count+1<MAX_BLOCK_INSNS&&e.cset_pair(insn);
        let translated = vtrace || vpair || vchain || selectpair || csetpair || e.translate(insn);
        e.u.dead_flags = false;
        e.u.dead_v = 0;
        e.u.vc_dest = [0xff; 2];
        let called = e.u.a.instructions()[code_start..].iter().any(|i| i.mnemonic() == iced_x86::Mnemonic::Call);
        if e.u.movflags && e.u.region.is_some() && !e.u.no_internal_jumps
            && fuse_in.is_some() && insn&0x7fe0ffe0==0x2a0003e0
            && translated && !called && !e.u.skip_next && e.u.fuse.is_none()
            && e.u.a.instructions()[code_start..].iter().all(|i|i.rflags_modified()==0) {
            // Exact GP MOV alias, with actual lowering audited to preserve ALL
            // host flags. Guest NZCV materialization is NOT removed/deferred.
            // Entry/helper/fallback barriers retain their existing resets.
            e.u.fuse=fuse_in;e.u.movflags_emissions+=1;
            if std::env::var_os("ATTESOR_MOVFLAGS_LOG").is_some() {eprintln!("movflags-carrier\t{cur:x}");}
        }
        if let Some(&word)=e.u.funedge_analysis.proof.get(&cur) {
            // Check every captured proof member against actual lowering. A
            // future template introducing a helper must not silently keep proof.
            assert_eq!(word,insn);assert!(translated);
            assert!(!called||insn&0xfc000000==0x94000000,"funedge native proof acquired a helper");
        }
        if let Some(&word)=e.u.recleaf_lazy_proof.get(&cur) {
            assert_eq!(word,insn);assert!(translated);
            assert!(!called||insn&0xfc000000==0x94000000,"recipe continuation proof acquired helper");
        }
        if e.u.pending_funedge.is_some_and(|p|p.cmp_pc==cur) {
            assert!(!called&&translated&&matches!(e.u.fuse,Some(FuseKind::Sub)));
        }

        if e.u.skip_next || called {
            e.u.vc = VCache::new(); // fused pair (writes the next one's Vd too) or helper call (clobbers caller-saved V cache)
            // The audited scalar pair never writes r11, even on its cold path.
            // Other fusions retain their old conservative contract.
            if called || !(vtrace||vpair||vchain||selectpair||csetpair) {e.u.tbi_mask_live = false;}
        }
        if !translated {
            if term {
                e.fallback_terminator(insn);
            } else {
                e.fallback(insn);
            }
        }
        if e.u.vchain && e.u.a.instructions()[code_start..].iter().any(|i| i.mnemonic() == iced_x86::Mnemonic::Call) {
            e.u.vzero=[0;2];
        }
        // The proof domain is not the profitability/cache decoder. Only its
        // small audited native subset can extend facts; everything else kills.
        // Labels and helpers (including conditional calls) are hard barriers.
        if e.u.notbi_enabled {
            if translated && !e.u.rc_frozen && !e.u.skip_next
                && !e.u.a.instructions()[code_start..].iter().any(|i| i.mnemonic() == iced_x86::Mnemonic::Call)
            {
                e.u.notbi.advance(insn, cur);
            } else {
                e.u.notbi.clear();
            }
        }
        cur += 4;
        count += 1;
        if term {
            u.emit_cold_paths();
            if fp_mode_hoisted {fp_flags::check_mode_emission(u,loop_body_start).expect("fpstable backend contract");}
            return cur;
        }
        // All skipped words are proven native members, never labelled entries.
        let more=std::mem::take(&mut u.skip_more)as usize;
        let skips = if std::mem::take(&mut u.skip_next) { 2+more } else { 1 };
        if skips > 1 {
            cur += (skips-1)as u64*4;
            count += skips-1;
            u.look.drain(..skips-1);
        }
        // slide the lookahead window to start at the next instruction
        if !u.look.is_empty() {
            let last_was_term = is_terminator(*u.look.last().unwrap());
            u.look.remove(0);
            if !last_was_term && u.look.len() < LOOKAHEAD {
                loop {
                    let Some(w)=read(cur+4*u.look.len()as u64+4) else {break;};u.look.push(w);
                    // Existing one/two-instruction emission keeps its original
                    // read horizon; a longer trace replenishes the lost words.
                    if more==0||u.look.len()==LOOKAHEAD||is_terminator(w) {break;}
                }
            }
        }
        if count >= MAX_BLOCK_INSNS {
            let mut e = E { u, pc: cur, fuse_in: None, scalar_inputs: None };
            e.goto(cur);
            u.emit_cold_paths();
            if fp_mode_hoisted {fp_flags::check_mode_emission(u,loop_body_start).expect("fpstable backend contract");}
            return cur;
        }
        // Stop before running into another block of this unit: jump to it.
        if u.labels.contains_key(&cur) {
            let mut e = E { u, pc: cur, fuse_in: None, scalar_inputs: None };
            e.goto(cur);
            u.emit_cold_paths();
            if fp_mode_hoisted {fp_flags::check_mode_emission(u,loop_body_start).expect("fpstable backend contract");}
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
        let exit = self.dispatcher_exit();
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
        if let Some(ctx)=self.u.recleaf_context {
            if let Some(recipe)=ctx.recipe {
                if self.pc==ctx.branch_pc-4 {assert_eq!(insn,recipe.cmp);rec_leaf::snapshot(self,recipe);return true;}
            }
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
        if self.u.region.is_some() && self.region_dp_imm(insn) { return true; }
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
                if self.u.region.is_some() {
                    let mut high = self.src(rn,false,sf,rax);
                    let low = self.src(rm,false,sf,rcx);
                    let dest = self.dst(rd,false,rcx);
                    if dest == high && dest != low && lsb != 0 {
                        self.copy(rax,high,sf); high = rax;
                    }
                    self.copy(dest,low,sf);
                    if lsb != 0 {
                        if sf { a!(self, shrd(dest,high,lsb)); }
                        else { a!(self, shrd(r32_of(dest),r32_of(high),lsb)); }
                    } else if !sf { a!(self, mov(r32_of(dest),r32_of(dest))); }
                    self.stx(rd,dest,false);
                    return true;
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
            let source = self.src(rn,false,sf,rax);
            self.copy(rax,source,sf);
            if immr != 0 { self.shift(rax,3,immr,sf); }
            let mask = wmask & tmask;
            self.mov_imm(rcx,mask);
            a!(self, and(rax,rcx));
            let dest = self.init_dst(rd,rd,false,false,sf,rdx);
            self.mov_imm(rcx,!mask);
            if sf { a!(self, and(dest,rcx)); a!(self, or(dest,rax)); }
            else { a!(self, and(r32_of(dest),ecx)); a!(self, or(r32_of(dest),eax)); }
            self.stx(rd,dest,false);
            return true;
        }
        if sf && opc == 0 && immr == 0 && imms == 31 {
            let src = self.src(rn,false,false,rax);
            let dest = self.dst(rd,false,rax);
            a!(self, movsxd(dest,r32_of(src)));
            self.stx(rd,dest,false); return true;
        }
        let r = if self.u.region.is_some() { self.init_dst(rd,rn,false,false,sf,rax) }
            else { self.ld(rax,rn,false,sf); rax };
        if sf {
            if opc == 2 {
                // UBFM
                if imms >= immr {
                    // UBFX / LSR
                    let width = imms - immr + 1;
                    if immr != 0 {
                        a!(self, shr(r, immr));
                    }
                    if width < 64 {
                        if width == 32 {
                            a!(self, mov(r32_of(r), r32_of(r)));
                        } else {
                            self.mov_imm(rcx, (1u64 << width) - 1);
                            a!(self, and(r, rcx));
                        }
                    }
                } else {
                    // UBFIZ / LSL
                    let width = imms + 1;
                    let sh = 64 - immr;
                    a!(self, shl(r, 64 - width));
                    a!(self, shr(r, 64 - width - sh));
                }
            } else {
                // SBFM
                if imms >= immr {
                    let l = 63 - imms;
                    if l != 0 {
                        a!(self, shl(r, l));
                    }
                    let amount = l + immr;
                    if amount != 0 {
                        a!(self, sar(r, amount));
                    }
                } else {
                    let l = 63 - imms;
                    a!(self, shl(r, l));
                    a!(self, sar(r, l));
                    a!(self, shl(r, 64 - immr));
                }
            }
        } else if opc == 2 {
            if imms >= immr {
                let width = imms - immr + 1;
                if immr != 0 {
                    a!(self, shr(r32_of(r), immr));
                }
                if width < 32 {
                    a!(self, and(r32_of(r), ((1u64 << width) - 1) as i32));
                }
            } else {
                let width = imms + 1;
                let sh = 32 - immr;
                a!(self, shl(r32_of(r), 32 - width));
                a!(self, shr(r32_of(r), 32 - width - sh));
            }
        } else if imms >= immr {
            let l = 31 - imms;
            if l != 0 {
                a!(self, shl(r32_of(r), l));
            }
            let amount = l + immr;
            if amount != 0 {
                a!(self, sar(r32_of(r), amount));
            }
        } else {
            let l = 31 - imms;
            a!(self, shl(r32_of(r), l));
            a!(self, sar(r32_of(r), l));
            a!(self, shl(r32_of(r), 32 - immr));
        }
        if !sf && immr == 0 && imms == 31 { a!(self, mov(r32_of(r),r32_of(r))); }
        self.stx(rd, r, false);
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
        // CSINC Wd/Xd,ZR,ZR,cond: the inverse condition is exactly CSET.
        // Clear the entire result with MOV (not XOR), then SETcc its low byte.
        // Both preserve the producer's flags for a subsequent fused select.
        if self.u.csetfast && insn&0x7fff0fe0==0x1a9f07e0 {
            if let Some((kind,cc))=self.fuse_in.and_then(|k|x86_cc(k,bits(insn,15,12)^1).map(|cc|(k,cc))) {
                let d=insn&31;
                if d!=31 {
                    let dest=self.dst(d,false,rcx);a!(self,mov(r32_of(dest),0u32));let b=r8_of(dest);
                    self.setcc_to(cc,b);
                    self.stx(d,dest,false);
                }
                self.u.fuse=Some(kind);return true;
            }
        }

        if self.u.region.is_some() && self.region_dp_reg(insn) { return true; }
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
                if opcode != 0b001011 && super::isel2::selected().bmi2 {
                    // Capture both operands before writing rd. BMI2 accepts
                    // independent sources/counts, including an assigned region
                    // register, without forcing the count through CL. The CPU
                    // masks count to 5/6 bits for 32/64-bit operands, as ARM does.
                    let n = self.src(rn, false, sf, rax);
                    let m = self.src(rm, false, sf, rcx);
                    let d = self.dst(rd, false, rax);
                    if sf {
                        match opcode & 3 {
                            0 => a!(self, shlx(d, n, m)),
                            1 => a!(self, shrx(d, n, m)),
                            2 => a!(self, sarx(d, n, m)),
                            _ => unreachable!(),
                        }
                    } else {
                        match opcode & 3 {
                            0 => a!(self, shlx(r32_of(d), r32_of(n), r32_of(m))),
                            1 => a!(self, shrx(r32_of(d), r32_of(n), r32_of(m))),
                            2 => a!(self, sarx(r32_of(d), r32_of(n), r32_of(m))),
                            _ => unreachable!(),
                        }
                    }
                    self.stx(rd, d, false);
                    return true;
                }
                self.ld(rcx, rm, false, sf);
                let r = if self.u.region.is_some() {self.init_dst(rd,rn,false,false,sf,rax)}
                    else {self.ld(rax,rn,false,sf);rax};
                if sf {
                    match opcode & 3 {
                        0 => a!(self, shl(r, cl)),
                        1 => a!(self, shr(r, cl)),
                        2 => a!(self, sar(r, cl)),
                        _ => a!(self, ror(r, cl)),
                    }
                } else {
                    match opcode & 3 {
                        0 => a!(self, shl(r32_of(r), cl)),
                        1 => a!(self, shr(r32_of(r), cl)),
                        2 => a!(self, sar(r32_of(r), cl)),
                        _ => a!(self, ror(r32_of(r), cl)),
                    }
                }
                self.stx(rd, r, false);
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
            0b000100 if super::isel2::selected().lzcnt => {
                let n = self.src(rn, false, sf, rax);
                let d = self.dst(rd, false, rax);
                // Unlike BSR, LZCNT defines the zero input as the operand width.
                if sf { a!(self, lzcnt(d, n)); }
                else { a!(self, lzcnt(r32_of(d), r32_of(n))); }
                self.stx(rd, d, false);
                true
            }
            0b000101 if super::isel2::selected().lzcnt => {
                // CLS(x) = CLZ(x XOR sign_mask(x)) - 1. Use scratch
                // registers for normalization so an assigned non-destination
                // source is never modified. The normalized sign bit is zero,
                // so CLZ >= 1 and subtraction cannot underflow.
                self.ld(rax, rn, false, sf);
                if sf {
                    a!(self, mov(rcx, rax));
                    a!(self, sar(rcx, 63));
                    a!(self, xor(rax, rcx));
                    a!(self, lzcnt(rax, rax));
                } else {
                    a!(self, mov(ecx, eax));
                    a!(self, sar(ecx, 31));
                    a!(self, xor(eax, ecx));
                    a!(self, lzcnt(eax, eax));
                }
                a!(self, sub(eax, 1));
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
        if let Some(ctx)=self.u.recleaf_context {
            if ctx.branch_pc==pc {self.goto(ctx.leaf);return true;}
        }
        if self.inline_leaf_call() {return true;}
        if insn & 0x7C00_0000 == 0x1400_0000 {
            let target = pc.wrapping_add((sext((insn & 0x03ff_ffff) as u64, 26) << 2) as u64);
            if insn & 0x8000_0000 != 0 {
                if super::cost::probes() {
                    a!(self, inc(qword_ptr(r15 + off::JIT_COST + 16)));
                    a!(self, mov(qword_ptr(r15 + off::PROFILE_STATE), crate::profile::State::CallReturn as i32));
                }
                self.st_imm(30, next);
                if self.u.function_entry==Some(target) {
                    if let Some(p)=self.u.recleaf_plan.clone() {rec_leaf::emit(self,p,next);return true;}
                    self.host_call(next,|e|e.goto(target));
                    return true;
                }
                if self.u.callret {
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
                    self.flags_branch(target, next, cc_nt);
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
            } else if let Some(s) = self.region_slot(rt) {
                if sf { a!(self, cmp(LOOPREGS[s], 0)); }
                else { a!(self, cmp(LOOPREGS32[s], 0)); }
            } else if sf {
                self.ldx_nf(rax, rt);
                a!(self, cmp(rax, 0));
            } else {
                self.ldw_nf(rax, rt);
                a!(self, cmp(eax, 0));
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
            let r = self.src(rt,false,true,rax);
            a!(self, bt(r, b));
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
            if super::cost::probes() && (opc == 2 || opc & 1 != 0) {
                let offset = if opc == 2 { 24 } else { 16 };
                a!(self, inc(qword_ptr(r15 + off::JIT_COST + offset)));
                a!(self, mov(qword_ptr(r15 + off::PROFILE_STATE), crate::profile::State::CallReturn as i32));
            }
            self.ldx(rax, reg, false);
            if opc==2 {if let Some(ctx)=self.u.recleaf_context {rec_leaf::ret(self,ctx);return true;}}
            if opc & 1 != 0 {
                self.st_imm(30, next);
                if self.u.callret {
                    // BLR/BLRAA…: rax holds the target across the call setup (only rcx is used)
                    self.host_call(next, |e| e.indirect(false));
                    return true;
                }
                if self.u.stub_kind == StubKind::Patchable && !self.u.quick
                    && ((self.u.inline_ok&&!self.u.no_internal_jumps)||self.u.leafinline)
                    && super::bliccache::enabled() {
                    self.blr_site_cached(pc);
                    return true;
                }
            }
            if opc==2 && self.u.function_entry.is_some() {
                let mut scan=self.new_label();let mut mismatch=self.new_label();
                let mut boundary=self.new_label();let mut plain=self.new_label();
                self.u.a.set_label(&mut scan).unwrap();
                a!(self,cmp(rsp,qword_ptr(r15+off::HOST_SP)));a!(self,jae(plain));
                a!(self,cmp(qword_ptr(rsp),0));a!(self,je(boundary));
                a!(self,cmp(rax,qword_ptr(rsp+8)));a!(self,jne(mismatch));
                a!(self,ret());
                self.u.a.set_label(&mut mismatch).unwrap();
                a!(self,add(rsp,16));a!(self,jmp(scan));
                self.u.a.set_label(&mut boundary).unwrap();a!(self,add(rsp,16));
                self.u.a.set_label(&mut plain).unwrap();
                // A caller outside this private assignment observes Cpu.
                self.writeback();
            }
            // A leaf has no recursive marker/private continuation ABI. Its
            // native RET fast hit must observe the same Cpu as an indirect exit.
            if opc==2 && self.u.leaf_livein.is_some() && self.u.callret {self.writeback();}
            if opc == 2 && self.u.callret {
                // Each frame is [host continuation, expected guest PC]. A changed
                // LR / longjmp searches older frames, never returns to a nonmatching
                // continuation. Depth is bounded by host_call. Plain/depth-overflow
                // calls need no special marker: matching by guest PC is sufficient.
                let mut l_scan = self.new_label();
                let mut l_plain = self.new_label();
                self.u.a.set_label(&mut l_scan).unwrap();
                a!(self, cmp(rsp, qword_ptr(r15 + off::HOST_SP)));
                a!(self, jae(l_plain));
                a!(self, cmp(rax, qword_ptr(rsp + 8)));
                let mut l_mismatch = self.new_label();
                a!(self, jne(l_mismatch));
                a!(self, ret());
                self.u.a.set_label(&mut l_mismatch).unwrap();
                a!(self, add(rsp, 16));
                a!(self, jmp(l_scan));
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
            let exit = self.dispatcher_exit();
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

    fn inline_leaf_call(&mut self)->bool {
        let Some((target,p))=self.u.inline_leaf_calls.get(&self.pc).cloned()else {return false;};
        let call_pc=self.pc;let next=call_pc+4;
        let mut bail=self.new_label();let mut miss=self.new_label();let mut body=self.new_label();
        // The planner stored only direct BL or exactly decoded indirect links. Re-read is forbidden
        // here: use the call word saved in Unit by the owning source snapshot.
        let call=self.u.inline_call_words[&call_pc];
        if let Some(rn)=super::loops::indirect_call_reg(call) {
            self.ldx(rax,rn,false);self.mov_imm(rcx,target);a!(self,cmp(rax,rcx));a!(self,jne(miss));
        }
        a!(self,mov(rcx,qword_ptr(r15+off::EPOCH_PTR)));a!(self,mov(rcx,qword_ptr(rcx)));
        a!(self,cmp(rcx,qword_ptr(r15+off::ENTRY_EPOCH)));a!(self,jne(bail));a!(self,jmp(body));
        self.u.a.set_label(&mut miss).unwrap();self.mov_imm(rax,call_pc);
        a!(self,mov(qword_ptr(r15+off::PC),rax));
        a!(self,mov(qword_ptr(r15+off::EXIT_REASON),EXIT_LEAFINLINE_MISS as i32));
        a!(self,mov(qword_ptr(r15+off::EXIT_DATA),0));let exit=self.dispatcher_exit();a!(self,jmp(exit));
        self.u.a.set_label(&mut bail).unwrap();self.st_pc_and_exit(call_pc);
        self.u.a.set_label(&mut body).unwrap();
        self.st_imm(30,next);self.u.fuse=None;self.rc_clear();self.u.notbi.clear();
        let look=std::mem::take(&mut self.u.look);
        let words:Vec<_>=p.words.iter().map(|(&pc,&w)|(pc,w)).collect();
        // Callret/IBTC2 post-return epoch bailouts observe callee state. Keep
        // canonical flags there; the OFF path has no observer before the
        // proven first writer (private next block, or its captured clone).
        let deferred=if self.u.leafflags&&!inline_epoch_check(self.u)&&self.u.inline_flag_continuations.contains_key(&call_pc) {
            p.final_fused_cmp()
        }else {None};
        // A bounded local assignment, not another standalone leaf/RET tier.
        // Guard misses still see PRE-call state. All leaf effects are flushed
        // before the existing post-return epoch guard or continuation observer.
        let local_gp=self.u.inlinegp&&self.u.region.is_none()&&self.u.loop_cache.is_none()
            &&!self.u.no_internal_jumps&&!self.u.commpage_checks&&disabled_classes()==0;
        let native_start=self.u.a.instructions().len();let fallback_before=self.u.insns_fallback;
        if local_gp {
            // RET itself reads LR, but is not emitted by this exact inline leaf.
            // Keep LR mapped/reloaded if ANY actual body member reads it.
            let body_lr=words[..words.len()-1].iter().any(|&(_,w)|w!=0xd503201f&&cache_access(w).xr&(1<<30)!=0);
            let used=p.used&if body_lr {u32::MAX}else {!(1<<30)};
            assert!(used.count_ones()<=NLOOPREGS as u32);
            let mut reg=[0xff;NLOOPREGS];for(s,n)in (0..32).filter(|&n|used&(1<<n)!=0).enumerate() {reg[s]=n as u8;}
            let r=LoopRa{pc:call_pc,fast:self.new_label(),exit:self.new_label(),reg,dirty:0,vreg:[0xff;NVREGS],xreg:[0xff;NVREGS],flags:0xff};
            r.reload_mask(&mut self.u.a,p.livein&used);self.u.region_exits.push(r);self.u.region=Some(r);
        }
        for (i,&(at,w)) in words.iter().enumerate().take(words.len()-1) {
            self.u.look=words[i+1..].iter().map(|&(_,w)|w).collect();
            self.u.cache_uses=cache_next_uses(&self.u.look);self.u.dead_flags=deferred==Some(at);self.u.dead_v=0;
            let fuse_in=self.u.fuse.take();let mut e=E{u:self.u,pc:at,fuse_in,scalar_inputs:None};
            assert!(e.translate(w),"exact inline leaf whitelist must be native");
        }
        if local_gp {
            assert!(GUEST_FAULTS_FATAL);
            assert_eq!(self.u.insns_fallback,fallback_before);
            assert!(!self.u.a.instructions()[native_start..].iter().any(|i|i.mnemonic()==iced_x86::Mnemonic::Call),"local inline GP scope acquired a helper");
            let r=self.u.region.take().unwrap();r.writeback(&mut self.u.a);
            let saved=self.u.region_exits.pop().unwrap();assert_eq!(saved.dirty,r.dirty);
            assert_eq!(r.dirty&!p.written,0);self.u.inlinegp_emissions+=1;
            if std::env::var_os("ATTESOR_INLINEGP_LOG").is_some() {eprintln!("inlinegp-leaf\t{call_pc:x}\t{target:x}\t{}",p.words.len());}
        }
        self.u.insns_translated+=p.words.len()as u64;self.u.look=look;self.u.fuse=None;self.u.dead_flags=false;
        self.rc_clear();self.u.notbi.clear();
        if inline_epoch_check(self.u) {
            // Inlining replaces a host CALL/RET or IBTC2 return, not its continuation
            // revocation check. Match host_call: reject the caller continuation
            // after callee effects, before executing the next guest instruction.
            let mut live=self.new_label();
            a!(self,mov(rcx,qword_ptr(r15+off::EPOCH_PTR)));a!(self,mov(rcx,qword_ptr(rcx)));
            a!(self,cmp(rcx,qword_ptr(r15+off::ENTRY_EPOCH)));a!(self,je(live));
            self.st_pc_and_exit(next);self.u.a.set_label(&mut live).unwrap();
        }
        let mut continuation=next;
        if let Some(cmp)=deferred {
            self.u.inline_flag_elisions+=1;
            if !self.u.region_labels.contains_key(&next) {
                // A private clone of exactly one total native NZCV writer.
                // Source is captured by the foreground dependency reader.
                // Execute it before any public boundary, then publish PC+4.
                let w=self.u.inline_flag_continuations[&call_pc].expect("only captured total writer may cross a public boundary");
                self.u.cache_uses=[[u8::MAX;32];2];self.u.dead_flags=false;
                let mut e=E{u:self.u,pc:next,fuse_in:None,scalar_inputs:None};
                assert!(e.translate(w),"proven continuation writer must be native");
                self.u.insns_translated+=1;
                if let Some(branch)=self.u.inline_follow_branches.get(&call_pc).copied() {
                    // Keep the writer's live x86 flags through its immediately
                    // following private branch clone. A public jump between
                    // them would destroy the existing compare/branch fusion.
                    let fuse_in=self.u.fuse.take();
                    let mut e=E{u:self.u,pc:next+4,fuse_in,scalar_inputs:None};
                    assert!(e.translate(branch),"captured conditional branch must be native");
                    self.u.insns_translated+=1;self.u.inline_branch_clones+=1;
                    self.u.fuse=None;self.rc_clear();
                    if std::env::var_os("ATTESOR_LEAFINLINE_LOG").is_some() {eprintln!("leafinline-flagsbranch\t{call_pc:x}\t{cmp:x}");}
                    return true; // both original guest successors were emitted
                }
                self.u.fuse=None;self.rc_clear();continuation=next+4;
            }
            if std::env::var_os("ATTESOR_LEAFINLINE_LOG").is_some() {eprintln!("leafinline-flags\t{call_pc:x}\t{cmp:x}\t{}",if continuation==next {"private"}else {"cloned"});}
        }
        if std::env::var_os("ATTESOR_LEAFINLINE_LOG").is_some() {eprintln!("leafinline-call\t{call_pc:x}\t{target:x}\t{}",p.words.len());}
        self.goto(continuation);true
    }

    fn fpflags_compare(&mut self,p:fp_flags::Plan) {
        let r=self.src(p.rn,false,p.sf,rax);
        if p.sf {a!(self,cmp(r,p.imm));}else {a!(self,cmp(r32_of(r),p.imm));}
    }

    /// Two-word, 16-byte-aligned frames: push guest PC, then CALL pushes host
    /// PC. No per-call epoch or RET_TARGET traffic. The epoch is sampled once at
    /// jit_enter; a continuation checks it BEFORE executing another guest insn.
    /// See docs/tasks/STAGE3-RESULTS.md for lifetime/concurrency requirements.
    fn host_call(&mut self, next: u64, callee: impl Fn(&mut Self)) {
        const RAS_LIMIT: i32 = NATIVE_RAS_LIMIT;
        debug_assert!(self.u.region.is_none()||self.u.function_entry.is_some(), "calls must not enter ordinary loopra regions");
        let mut l_plain = self.new_label();
        let mut l_callee = self.new_label();
        let mut l_epoch = self.new_label();
        a!(self, mov(rcx, qword_ptr(r15 + off::HOST_SP)));
        a!(self, sub(rcx, RAS_LIMIT * 16));
        a!(self, cmp(rsp, rcx));
        a!(self, jbe(l_plain));
        self.mov_imm(rcx, next);
        a!(self, push(rcx));
        a!(self, call(l_callee));
        // All caches and live flag-fusion state die across guest calls.
        self.rc_clear();
        self.u.fuse = None;
        a!(self, pop(rcx));
        // Invalidated translations are retained, not freed/reused. Returning
        // to this guard is safe even if another thread invalidated the caller.
        // Reject its guest continuation and discard ALL frames at jit_exit.
        a!(self, mov(rcx, qword_ptr(r15 + off::EPOCH_PTR)));
        a!(self, mov(rcx, qword_ptr(rcx)));
        a!(self, cmp(rcx, qword_ptr(r15 + off::ENTRY_EPOCH)));
        a!(self, jne(l_epoch));
        if self.u.inline_ok && !self.u.labels.contains_key(&next) && self.u.inline_queue.len() < 8 {
            let l = self.u.a.create_label();
            self.u.labels.insert(next, l);
            self.u.inline_queue.push(next);
        }
        self.goto(next);
        self.u.a.set_label(&mut l_epoch).unwrap();
        self.st_pc_and_exit(next); // do NOT consult a stale IBTC or chain slot
        self.u.a.set_label(&mut l_callee).unwrap();
        callee(self);
        self.u.a.set_label(&mut l_plain).unwrap();
        callee(self);
    }

    /// Branch while the producer's host flags are still live. The condition is
    /// for the not-taken edge; successors retain the existing intra-unit/slot paths.
    /// Guest NZCV is in Cpu and is not modified by CBZ/TBZ or these host flags.
    fn flags_branch(&mut self, taken: u64, not_taken: u64, cc_not_taken: Cc) {
        if let Some(p)=self.u.pending_funedge.take() {
            assert_eq!((self.pc,taken,not_taken),(p.cmp_pc+4,p.taken,p.not_taken));
            assert!(matches!(self.fuse_in,Some(FuseKind::Sub)));
            // Decide on producer EFLAGS BEFORE materialization; target SP
            // guards may clobber EFLAGS, so pack every LIVE edge before goto.
            if !self.u.commpage_checks && !jit_off("tbimask") && !self.u.tbi_mask_live {
                a!(self,mov(r11,0x00ff_ffff_ffff_ffffu64));
            }
            if !p.live_yes {
                self.jcc(cc_not_taken.inverse(),self.u.region_labels[&taken]);
                if p.live_no {self.flags_sub();}self.goto(not_taken);
            }else {
                assert!(!p.live_no);
                self.jcc(cc_not_taken,self.u.region_labels[&not_taken]);
                self.flags_sub();self.goto(taken);
            }
            self.u.funedge_emissions+=1;
            if std::env::var_os("ATTESOR_FUNEDGE_LOG").is_some() {eprintln!("funedge-branch\t{:x}\tlive={},{}",p.cmp_pc,p.live_yes,p.live_no);}
            return;
        }

        if let Some(label) = self.u.region_labels.get(&taken).copied() {
            if !self.u.commpage_checks && !jit_off("tbimask") && !self.u.tbi_mask_live {
                a!(self, mov(r11, 0x00ff_ffff_ffff_ffffu64));
            }
            self.jcc(cc_not_taken.inverse(), label);
            self.goto(not_taken);
            return;
        }
        if let Some(r) = self.u.region.filter(|r| r.pc == taken && !self.u.no_internal_jumps) {
            // Branch straight to the private entry on another iteration.
            // The exit falls through; no jcc-exit + unconditional backedge pair.
            // A helper may have invalidated r11; MOV keeps the producer flags.
            if !self.u.commpage_checks && !jit_off("tbimask") && !self.u.tbi_mask_live {
                a!(self, mov(r11, 0x00ff_ffff_ffff_ffffu64));
            }
            self.jcc(cc_not_taken.inverse(), r.fast);
            self.goto(not_taken);
            return;
        }
        let mut l_nt = self.new_label();
        self.jcc(cc_not_taken, l_nt);
        self.goto(taken);
        self.u.a.set_label(&mut l_nt).unwrap();
        self.goto(not_taken);
    }

    /// al holds the condition; emit taken/not-taken successors.
    fn cond_branch(&mut self, taken: u64, not_taken: u64) {
        a!(self, test(al, al));
        self.flags_branch(taken, not_taken, Cc::E);
    }

    /// Ordinary JIT BLR only. The target was loaded before writing x30, so
    /// BLR x30 keeps the original target. No helper/host call or code patching.
    fn blr_site_cached(&mut self, site: u64) {
        use super::bliccache as cache;
        self.writeback();
        let mut l_miss = self.new_label();
        let mut l_plain = self.new_label();
        a!(self, mov(rcx, qword_ptr(r15 + off::BLICCACHE)));
        // A private emitter consumer without setup must retain the old path.
        a!(self, test(rcx, rcx));
        a!(self, jz(l_plain));
        a!(self, mov(rdx, qword_ptr(r15 + off::EPOCH_PTR)));
        a!(self, mov(rdx, qword_ptr(rdx)));
        a!(self, cmp(rdx, qword_ptr(r15 + off::ENTRY_EPOCH)));
        a!(self, jne(l_miss));
        a!(self, add(rcx, cache::offset(site) as i32));
        a!(self, cmp(rdx, qword_ptr(rcx + cache::EPOCH_OFFSET)));
        a!(self, jne(l_miss));
        self.mov_imm(rdx, site);
        a!(self, cmp(rdx, qword_ptr(rcx + cache::SITE)));
        a!(self, jne(l_miss));
        a!(self, cmp(rax, qword_ptr(rcx + cache::TARGET)));
        a!(self, jne(l_miss));
        // host=0 is a negative entry, not a cached unregistered AOT address.
        a!(self, cmp(qword_ptr(rcx + cache::HOST), 0));
        a!(self, je(l_plain));
        a!(self, jmp(qword_ptr(rcx + cache::HOST)));
        self.u.a.set_label(&mut l_miss).unwrap();
        a!(self, mov(qword_ptr(r15 + off::PC), rax));
        a!(self, mov(qword_ptr(r15 + off::EXIT_REASON), EXIT_BLICCACHE as i32));
        self.mov_imm(rdx, site);
        a!(self, mov(qword_ptr(r15 + off::EXIT_DATA), rdx));
        let exit = self.dispatcher_exit();
        a!(self, jmp(exit));
        self.u.a.set_label(&mut l_plain).unwrap();
        self.indirect(false);
    }

    /// Indirect branch to the guest address in rax via the per-thread
    /// indirect-branch table; misses go to the dispatcher.
    fn indirect(&mut self, is_ret: bool) {
        self.writeback();
        if self.u.quick {
            a!(self, mov(qword_ptr(r15 + off::PC), rax));
            a!(self, mov(qword_ptr(r15 + off::EXIT_REASON), EXIT_CONTINUE as i32));
            a!(self, mov(qword_ptr(r15 + off::EXIT_DATA), 0));
            let exit = self.dispatcher_exit();
            a!(self, jmp(exit));
            return;
        }
        if super::cost::probes() {
            a!(self, inc(qword_ptr(r15 + off::JIT_COST)));
        }
        if super::cost::probes() && !is_ret {
            a!(self, mov(qword_ptr(r15 + off::PROFILE_STATE), crate::profile::State::Indirect as i32));
        }
        let mut l_miss = self.new_label();
        if ibtc2_enabled() {
            // Compare the epoch of the table, not the value resampled by
            // jit_enter. Cross-thread invalidation must reject both old ways.
            a!(self, mov(rcx, qword_ptr(r15 + off::EPOCH_PTR)));
            a!(self, mov(rcx, qword_ptr(rcx)));
            a!(self, cmp(rcx, qword_ptr(r15 + off::IBTC_EPOCH)));
            a!(self, jne(l_miss));
            a!(self, test(rax, 3));
            a!(self, jne(l_miss)); // never match the invalid/empty tag
            a!(self, mov(rcx, rax));
            a!(self, shr(ecx, crate::jit::IBTC_GUEST_SHIFT));
            a!(self, and(ecx, (crate::jit::IBTC2_SETS - 1) as i32));
            a!(self, shl(ecx, crate::jit::IBTC2_SET_BYTES.trailing_zeros()));
            a!(self, add(rcx, qword_ptr(r15 + off::IBTC)));
            let mut l_way1 = self.new_label();
            a!(self, cmp(rax, qword_ptr(rcx))); // complete 64-bit guest tag
            a!(self, jne(l_way1));
            a!(self, jmp(qword_ptr(rcx + 8)));
            self.u.a.set_label(&mut l_way1).unwrap();
            a!(self, cmp(rax, qword_ptr(rcx + crate::jit::IBTC_ENTRY_BYTES)));
            a!(self, jne(l_miss));
            a!(self, jmp(qword_ptr(rcx + crate::jit::IBTC_ENTRY_BYTES + 8)));
        } else {
            // Original direct-map fallback, including its original entry budget.
            a!(self, mov(rcx, rax));
            a!(self, shr(ecx, crate::jit::IBTC_GUEST_SHIFT));
            a!(self, and(ecx, (crate::jit::IBTC_SIZE - 1) as i32));
            a!(self, shl(ecx, crate::jit::IBTC_ENTRY_BYTES.trailing_zeros()));
            a!(self, add(rcx, qword_ptr(r15 + off::IBTC)));
            a!(self, cmp(rax, qword_ptr(rcx)));
            a!(self, jne(l_miss));
            a!(self, jmp(qword_ptr(rcx + 8)));
        }
        self.u.a.set_label(&mut l_miss).unwrap();
        if super::cost::probes() { a!(self, inc(qword_ptr(r15 + off::JIT_COST + 8))); }
        a!(self, mov(qword_ptr(r15 + off::PC), rax));
        a!(self, mov(qword_ptr(r15 + off::EXIT_REASON), EXIT_CONTINUE as i32));
        a!(self, mov(qword_ptr(r15 + off::EXIT_DATA), 0));
        let exit = self.dispatcher_exit();
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
        if self.u.bytestream_site==Some((self.pc,insn)) {return byte_stream::load(self,insn);}

        if let Some((p,shadow))=self.u.addr_shadow.filter(|(p,_)|p.at==self.pc&&p.word==insn) {
            if let Some((index,dst))=self.region_slot(p.rm).zip(self.u.region.and_then(|r|r.vslot(insn&31))) {
                let mem=qword_ptr(LOOPREGS[shadow]+LOOPREGS[index]*(1i32<<p.scale));let d=VREGS[dst];let double=insn>>30==3;
                if high_xmm(d) {if double {evex!(self,vmovq(d,mem));}else {evex!(self,vmovd(d,dword_ptr(LOOPREGS[shadow]+LOOPREGS[index]*(1i32<<p.scale))));}}
                else if double {a!(self,movq(d,mem));}else {a!(self,movd(d,dword_ptr(LOOPREGS[shadow]+LOOPREGS[index]*(1i32<<p.scale))));}
                self.scalar_zero(insn&31,double);self.u.addr_hoists+=1;
                if std::env::var_os("ATTESOR_ADDRHOIST_LOG").is_some() {eprintln!("addrhoist-load\t{:x}",self.pc);}
                return true;
            }
        }

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
            if self.u.region.is_some() {
                let base = self.src(rn,true,true,rax);
                a!(self, lea(rax,qword_ptr(base + imm as i32)));
            } else {
                self.ldx(rax, rn, true);
                if imm != 0 { a!(self, add(rax, imm as i32)); }
            }
            return self.access_immediate(insn, rt, v, size, opc);
        }
        if insn & 0x3B20_0000 == 0x3800_0000 {
            let size = bits(insn, 31, 30);
            let opc = bits(insn, 23, 22);
            let imm = sext(bits(insn, 20, 12) as u64, 9);
            let mode = bits(insn, 11, 10);
            if self.u.region.is_some() && (mode == 0 || mode == 2) {
                let base = self.src(rn,true,true,rax);
                a!(self, lea(rax,qword_ptr(base + imm as i32)));
                return self.access_immediate(insn, rt, v, size, opc);
            }
            self.ldx(rax, rn, true);
            if self.u.region.is_some() && (mode == 1 || mode == 3) {
                if mode == 3 { a!(self, add(rax, imm as i32)); }
                if let Some(slot) = self.region_slot(rn).filter(|_| v || rn != rt) {
                    let translated = self.access_sized(rt,v,size,opc);
                    if translated {
                        let base = LOOPREGS[slot];
                        a!(self, add(base,imm as i32));
                        self.stx(rn,base,true);
                    }
                    return translated;
                }
                a!(self, mov(rdx, rax));
                let translated = self.access_sized(rt, v, size, opc);
                a!(self, mov(rax, rdx));
                if mode == 1 { a!(self, add(rax, imm as i32)); }
                if translated { self.stx(rn, rax, true); }
                return translated;
            }
            match mode {
                0 | 2 => {
                    if imm != 0 {
                        a!(self, add(rax, imm as i32));
                    }
                    self.access_immediate(insn, rt, v, size, opc)
                }
                1 => {
                    // post-index: access at base, then base += imm
                    a!(self, mov(r8, rax));
                    if !self.access_sized(rt, v, size, opc) {
                        return false;
                    }
                    a!(self, add(r8, imm as i32));
                    self.stx(rn, r8, true);
                    true
                }
                _ => {
                    a!(self, add(rax, imm as i32));
                    a!(self, mov(r8, rax));
                    if !self.access_sized(rt, v, size, opc) {
                        return false;
                    }
                    self.stx(rn, r8, true);
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
            if self.u.region.is_some() && (option == 3 || option == 2) {
                let index = self.src(rm,false,true,rcx);
                let index = if option == 2 { // UXTW must ignore Xrm[63:32].
                    a!(self, mov(ecx,r32_of(index))); rcx
                } else { index };
                let shift = if s != 0 { scale } else { 0 };
                // x86 addressing scales only by 1, 2, 4 or 8: a Q access (LSL #4, x16) shifts a copy.
                let index = if shift > 3 {
                    if index != rcx { a!(self, mov(rcx, index)); }
                    a!(self, shl(rcx, shift)); rcx
                } else { index };
                let base = self.src(rn,true,true,rax);
                let mult = if shift > 3 { 1 } else { 1i32 << shift };
                a!(self, lea(rax,qword_ptr(base + index * mult)));
                return self.access_sized(rt,v,size,opc);
            }
            self.ldx(rcx, rm, false);
            self.extend(rcx, option, if s != 0 { scale } else { 0 });
            self.ldx(rax, rn, true);
            a!(self, add(rax, rcx));
            self.access_sized(rt, v, size, opc)
        } else {
            false
        }
    }

    /// Only these scalar, non-writeback immediate encodings can omit TBI.
    /// The decoder checks the complete EA, not merely the base register.
    fn access_immediate(&mut self, insn: u32, rt: u32, v: bool, size: u32, opc: u32) -> bool {
        let untagged = self.u.notbi_enabled && !self.u.rc_frozen && self.u.notbi.untagged_address(insn);
        self.access_sized_known(rt, v, size, opc, untagged)
    }

    /// Access with LDR/STR (size, opc) semantics at address rax.
    /// Preserves r8 (used for writeback).
    fn access_sized(&mut self, rt: u32, v: bool, size: u32, opc: u32) -> bool {
        self.access_sized_known(rt, v, size, opc, false)
    }

    fn access_sized_known(&mut self, rt: u32, v: bool, size: u32, opc: u32, untagged: bool) -> bool {
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
            0 => self.access_known(rt, false, size, 0, false, untagged),
            1 => self.access_known(rt, false, size, 1, false, untagged),
            2 => {
                if size == 3 {
                    return true; // PRFM
                }
                self.access_known(rt, false, 16 + size, 1, false, untagged)
            }
            _ => {
                if size >= 2 {
                    return false;
                }
                self.access_known(rt, false, 32 + size, 1, false, untagged)
            }
        }
    }

    /// Core memory access at host address rax.
    /// kind: 0-3 = 1/2/4/8 bytes; 4 = 16 bytes (SIMD only); 16+s = sign-extend
    /// to 64; 32+s = sign-extend to 32. `load`: 1 = load, 0 = store.
    fn access(&mut self, rt: u32, v: bool, kind: u32, load: u32, literal: bool) -> bool {
        self.access_known(rt, v, kind, load, literal, false)
    }

    fn access_known(&mut self, rt: u32, v: bool, kind: u32, load: u32, literal: bool, untagged: bool) -> bool {
        let _ = literal;
        self.commpage_fix_known(untagged);
        let load = load != 0;
        if v {
            match (kind, load) {
                (4, true) => {
                    a!(self, movdqu(xmm0, xmmword_ptr(rax)));
                    self.store_v(rt, xmm0);
                }
                (2 | 3, true) => {
                    if let Some(s) = self.u.region.and_then(|r| r.vslot(rt)) {
                        // The architectural scalar load clears the upper lane.
                        // Load directly into its mapped location, not xmm0 plus
                        // a second cache copy. Never widen the memory access.
                        let dst = VREGS[s];
                        if high_xmm(dst) {
                            if kind == 2 { evex!(self, vmovd(dst, dword_ptr(rax))); }
                            else { evex!(self, vmovq(dst, qword_ptr(rax))); }
                        } else if kind == 2 { a!(self, movd(dst, dword_ptr(rax))); }
                        else { a!(self, movq(dst, qword_ptr(rax))); }
                        self.scalar_zero(rt,kind==3);
                        return true;
                    }
                    // through xmm0 (upper part zero) with one 16-byte store, so the value can stay cached
                    if kind == 2 { a!(self, movd(xmm0, dword_ptr(rax))); } else { a!(self, movq(xmm0, qword_ptr(rax))); }
                    self.store_v(rt, xmm0);
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
                (k @ (2 | 3), false) if self.u.region.and_then(|r| r.vslot(rt)).is_some() => {
                    let s = self.u.region.unwrap().vslot(rt).unwrap();
                    self.cache_low64(xmm0, VREGS[s]);
                    if k == 2 { a!(self, movd(dword_ptr(rax), xmm0)); }
                    else { a!(self, movq(qword_ptr(rax), xmm0)); }
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
        self.access_gpr_at(rt,kind,load,rax,0)
    }

    // Shared GPR memory pipeline: keep exact access widths/order and register
    // semantics here, including any future memory observation/invalidation hook.
    fn access_gpr_at(&mut self,rt:u32,kind:u32,load:bool,base:AsmRegister64,disp:i32)->bool {
        if load {
            let dest = self.dst(rt,false,rcx);
            match kind {
                0 => a!(self, movzx(r32_of(dest), byte_ptr(base + disp))),
                1 => a!(self, movzx(r32_of(dest), word_ptr(base + disp))),
                2 => a!(self, mov(r32_of(dest), dword_ptr(base + disp))),
                3 => a!(self, mov(dest, qword_ptr(base + disp))),
                16 => a!(self, movsx(dest, byte_ptr(base + disp))),
                17 => a!(self, movsx(dest, word_ptr(base + disp))),
                18 => a!(self, movsxd(dest, dword_ptr(base + disp))),
                32 => a!(self, movsx(r32_of(dest), byte_ptr(base + disp))),
                33 => a!(self, movsx(r32_of(dest), word_ptr(base + disp))),
                _ => return false,
            }
            self.stx(rt, dest, false);
        } else {
            let value = self.src(rt,false,true,rcx);
            match kind {
                0 => a!(self, mov(byte_ptr(base + disp), r8_of(value))),
                1 => a!(self, mov(word_ptr(base + disp), r16_of(value))),
                2 => a!(self, mov(dword_ptr(base + disp), r32_of(value))),
                3 => a!(self, mov(qword_ptr(base + disp), value)),
                _ => return false,
            }
        }
        true
    }

    fn pair(&mut self, insn: u32) -> bool {
        if self.u.funstack_sites.get(&self.pc)==Some(&insn) {
            return fun_stack::pair(self,insn);
        }
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
        if self.u.region.is_some() {
            if l && !v && rt == rt2 { return false; }
            a!(self, mov(rdx, rax));
            let first = self.access(rt, v, kind, l as u32, false);
            a!(self, mov(rax, rdx));
            a!(self, add(rax, nb));
            let second = self.access(rt2, v, kind, l as u32, false);
            a!(self, mov(rax, rdx));
            if first && second && (idx == 1 || idx == 3) {
                if idx == 1 { a!(self, add(rax, off)); }
                self.stx(rn, rax, true);
            }
            return first && second;
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
            self.stx(rn, r8, true);
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
        self.store_v(d, xmm0);
    }

    /// Store the 64-bit value in rax to Vd (upper half zero unless `dup_hi`).
    fn vst_rax(&mut self, d: u32, dup_hi: bool) {
        if let Some(s)=self.u.region.and_then(|r|r.vslot(d)) {
            let dst=VREGS[s];
            if high_xmm(dst) {
                evex!(self,vmovq(dst,rax));
                if dup_hi {evex!(self,vpunpcklqdq(dst,dst,dst));}
            } else {
                a!(self,movq(dst,rax));
                if dup_hi {a!(self,punpcklqdq(dst,dst));}
            }
            return;
        }
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
        if self.u.avx512_nan {
            if double {
                a!(self, vpcmpeqq(k1, xmm0, indefinite));
                a!(self, vmovdqu64(xmm0.k1(), default));
            } else {
                a!(self, vpcmpeqd(k1, xmm0, indefinite));
                a!(self, vmovdqu32(xmm0.k1(), default));
            }
            return;
        }
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
        if self.u.avx512_nan {
            // VFPCLASS: bit 0 = qNaN, bit 7 = sNaN. VFIXUPIMM response
            // 2 quiets the source (including sign/payload); other categories
            // preserve DEST. imm8=0 requests no extra host exception flags.
            // Keep the ARM reverse-priority qNaN pass, then the sNaN pass.
            let table = self.vconstant(if double { 0x22 } else { 0x0000_0022_0000_0022 });
            let classes = if signalling { 0x80 } else { 0x81 };
            if double {
                a!(self, vfpclasspd(k1, src, classes));
                a!(self, vfixupimmpd(xmm0.k1(), src, table, 0));
            } else {
                a!(self, vfpclassps(k1, src, classes));
                a!(self, vfixupimmps(xmm0.k1(), src, table, 0));
            }
            return;
        }
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
                Some(c) if q => e.cache_copy(xmm6, c),
                Some(c) => e.cache_low64(xmm6, c),
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
        if self.u.avx512_nan {
            // VRANGE min/max (imm[1:0]=0/1), sign from the selected result
            // (imm[3:2]=1). Unlike MIN/MAX it handles opposite-signed zero
            // ties correctly, flushes inputs under DAZ, and implements ARM's
            // NM NaN priorities. Propagating variants still need ARM qNaNs.
            if !nm { a!(self, movdqa(xmm6, xmm0)); }
            if double { a!(self, vrangepd(xmm0, xmm0, xmm1, 4 | max as i32)); }
            else { a!(self, vrangeps(xmm0, xmm0, xmm1, 4 | max as i32)); }
            if !nm {
                let cold = self.u.a.create_label();
                if double { a!(self, vcmppd(k1, xmm6, xmm1, 3)); }
                else { a!(self, vcmpps(k1, xmm6, xmm1, 3)); }
                a!(self, kortestw(k1, k1));
                a!(self, jnz(cold));
                self.cold_path(cold, move |e| {
                    for signalling in [false, true] {
                        for src in [xmm1, xmm6] { e.select_nan(double, src, signalling); }
                    }
                });
            }
            return;
        }
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
                if self.u.fmovra&&rn!=31 {
                    if let (Some(g),Some(v))=(self.region_slot(rn),self.u.region.and_then(|r|r.vslot(rd))) {
                        let dst=VREGS[v];let source=LOOPREGS[g];
                        if high_xmm(dst) {
                            if sf {evex!(self,vmovq(dst,source));}else {evex!(self,vmovd(dst,r32_of(source)));}
                        }else if sf {a!(self,movq(dst,source));}else {a!(self,movd(dst,r32_of(source)));}
                        self.scalar_zero(rd,sf);return true;
                    }
                }
                self.ld(rax, rn, false, sf);
                self.vst_rax(rd, false);
                self.scalar_zero(rd,ftype==1);
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
            self.scalar_zero(rd,ftype==1);
            return true;
        }
        // FADD/FSUB/FMUL/FDIV
        if insn & 0xFF20_0C00 == 0x1E20_0800 && ftype <= 1 {
            let opcode = bits(insn, 15, 12);
            if opcode > 3 {
                return false;
            }
            let double = ftype == 1;
            if self.resident_scalar(opcode,double,rd,rn,Some(rm)) {return true;}
            let cm = self.scalar_source(rm).cached;
            self.fld_scalar(xmm0, rn, double);
            if let Some(c) = cm.filter(|&c| high_xmm(c)) {
                match (double, opcode) {
                    (true, 0) => evex!(self, vmulsd(xmm0, xmm0, c)),
                    (true, 1) => evex!(self, vdivsd(xmm0, xmm0, c)),
                    (true, 2) => evex!(self, vaddsd(xmm0, xmm0, c)),
                    (true, _) => evex!(self, vsubsd(xmm0, xmm0, c)),
                    (false, 0) => evex!(self, vmulss(xmm0, xmm0, c)),
                    (false, 1) => evex!(self, vdivss(xmm0, xmm0, c)),
                    (false, 2) => evex!(self, vaddss(xmm0, xmm0, c)),
                    (false, _) => evex!(self, vsubss(xmm0, xmm0, c)),
                }
            } else if double {
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
            self.store_v(rd, xmm0);
            self.scalar_zero(rd,double);
            return true;
        }
        // FSQRT
        if insn & 0xFF3F_FC00 == 0x1E21_C000 && ftype <= 1 {
            let double = ftype == 1;
            if self.resident_scalar(4,double,rd,rn,None) {return true;}
            self.fld_scalar(xmm0, rn, double);
            if double {
                a!(self, sqrtsd(xmm0, xmm0));
            } else {
                a!(self, sqrtss(xmm0, xmm0));
            }
            self.fix_default_nan(double, rn, None);
            self.store_v(rd, xmm0);
            self.scalar_zero(rd,double);
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
            self.store_v(rd, xmm0);
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
        e.u.avx512 = false;
        e.u.vc.reg = [0xff; NVREGS]; e.u.vc.reg[..8].copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7]);
        e.u.vc.stamp[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        e.u.look = vec![0x1e622820]; // V1/V2 read now, V0 written
        e.u.cache_uses = cache_next_uses(&e.u.look);
        assert_eq!(e.vc_alloc(9), 0); // old V0 is killed; bank-local decision
        e.u.rc.reg = [0, 1, 2, 3, 4];
        e.u.rc.stamp = [1, 2, 3, 4, 5];
        e.u.look = vec![0x8b01000a, 0x8b03004b, 0x8b05008c];
        e.u.cache_uses = cache_next_uses(&e.u.look);
        assert_eq!(e.rc_alloc(9), 4); // newest slot, but used furthest away
        e.u.avx512 = false;
        e.u.vc.reg = [0xff; NVREGS]; e.u.vc.reg[..8].copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7]);
        e.u.vc.stamp[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
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
        u.loopra = false; // these tests exercise the independent write-through feature
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
                assert!(u.cold_ranges.len() == 1 || (u.avx512_nan && u.a.instructions().iter().any(|i|
                    matches!(i.mnemonic(), iced_x86::Mnemonic::Vrangeps | iced_x86::Mnemonic::Vrangepd))), "word={word:08x}");
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
            assert_eq!(c.pc(), if ending == 0 { end } else { PC + words.len() as u64 * 4 });
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
                    assert_eq!(c.pc(), PC + 4);
                    assert_eq!(c.exit_reason, EXIT_CONTINUE);
                }
            }
        }
        assert!(saw_unaligned);
        }
    }
}

#[cfg(test)]
mod loopra_tests {
    use super::*;
    use attesor_core::cpu::Cpu;
    use attesor_core::interp;
    const PC: u64 = 0x2000_0000;

    fn unit(words: &[u32], entries: &[usize], kind: StubKind, verify: bool, commpage: bool) -> Unit {
        let mut u = Unit::new(kind, commpage);
        u.loopra = true;
        u.loopra2 = false;
        u.regioncache = false;
        u.no_internal_jumps = verify;
        for &i in entries { let l = u.a.create_label(); u.labels.insert(PC + i as u64 * 4, l); }
        for &i in entries {
            emit_block(&mut u, PC + i as u64 * 4, &|p| p.checked_sub(PC).and_then(|o| words.get((o / 4) as usize)).copied());
        }
        u
    }

    fn assemble(mut u: Unit, entry: usize) -> (u64, Vec<u64>) {
        let label = u.labels[&(PC + entry as u64 * 4)];
        u.finish();
        let host = crate::jit::code_cache().alloc(65536);
        let res = u.a.assemble_options(host, iced_x86::BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
        assert!(res.inner.code_buffer.len() <= 65536);
        unsafe { std::ptr::copy_nonoverlapping(res.inner.code_buffer.as_ptr(), host as *mut u8, res.inner.code_buffer.len()); }
        let mut slots = Vec::new();
        if u.stub_kind == StubKind::Patchable {
            for s in &u.finished_stubs {
                let slot = res.label_ip(&s.slot).unwrap();
                unsafe { (slot as *mut u64).write_unaligned(res.label_ip(&s.cont).unwrap()); }
                slots.push(slot);
            }
        }
        (res.label_ip(&label).unwrap(), slots)
    }

    fn cpu() -> Box<Cpu> {
        let mut c = Cpu::new(); crate::jit::setup_cpu(&mut c); c.set_pc(PC); c
    }


    // Exact four loop bodies from our unchanged kernels-arm64 (-O2); not an
    // emitter pattern match or replacement of the guest classification.
    const BYTE_STREAM_WORDS:[[u32;17];4]=[[0x3840156c,0x93407d2d,0xcb0d010e,0x8b0d010d,0x7200019f,0x9a8d01cd,0x5200052e,0x7219019f,0x9a8811ad,0x1a8e112e,0x11000529,0x12000929,0x7100fd9f,0x1a8981c9,0x9a8881a8,0xf100054a,0x54fffe01],
[0x38401589,0x1100056d,0x120009ad,0x5200056e,0x93407d6f,0xcb0f0110,0x8b0f010f,0x7200013f,0x9a8f020f,0x7219013f,0x9a8f010f,0x1a8b01cb,0x7101013f,0x1a8b31ab,0x9a8f3108,0xf100054a,0x54fffe01],
[0x3840154c,0x1100056d,0x120009ad,0x5200056e,0x93407d6f,0xcb0f0110,0x8b0f010f,0x7200019f,0x9a8f020f,0x7219019f,0x9a8f010f,0x1a8b01cb,0x7101019f,0x1a8b31ab,0x9a8f3108,0xf1000529,0x54fffe01],
[0x3840166a,0x1100056c,0x1200098c,0x5200056d,0x93407d6e,0xcb0e010f,0x8b0e010e,0x7200015f,0x9a8e01ee,0x7219015f,0x9a8e010e,0x1a8b01ab,0x7101015f,0x1a8b318b,0x9a8e3108,0xf1000529,0x54fffe01]];
    #[test]
    fn bytestream_scope_rejects_alias_write_memory_cut_reserved() {
        let mut u=Unit::new(StubKind::Plain,false);u.bytestream=true;
        let mut r=LoopRa{pc:PC,fast:u.a.create_label(),exit:u.a.create_label(),reg:[0xff;NLOOPREGS],dirty:0,vreg:[0xff;NVREGS],xreg:[0xff;NVREGS],flags:0xff};r.reg[..2].copy_from_slice(&[11,10]);
        let good=BYTE_STREAM_WORDS[0];let check=|u:&Unit,w:&[u32]|byte_stream::plan(u,PC,r,&|p|p.checked_sub(PC).and_then(|o|w.get((o/4)as usize)).copied());
        assert!(check(&u,&good).is_some());
        for(i,bad)in [(0,0x3840156b),(0,0x3840156a),(1,0x9100056b),(1,0x9100054a),(1,0xf9400020),(1,0x3840156c),(1,0x1ee02000),(1,0xffffffff),(1,0x0bc00000),(1,0x1a800800),(15,0xf100094a),(15,0x7100054a),(16,0x54fffe00)] {
            let mut w=good;w[i]=bad;assert!(check(&u,&w).is_none(),"{i}: {bad:x}");
        }
        let label=u.a.create_label();u.labels.insert(PC+4,label);assert!(check(&u,&good).is_none());u.labels.clear();
        u.bytestream=false;assert!(check(&u,&good).is_none());
    }
    #[test]
    fn bytestream_actual_range_guard_no_synthetic_dereference() {
        let mut u=Unit::new(StubKind::Plain,false);
        let mut r=LoopRa{pc:PC,fast:u.a.create_label(),exit:u.a.create_label(),reg:[0xff;NLOOPREGS],dirty:0,vreg:[0xff;NVREGS],xreg:[0xff;NVREGS],flags:0xff};r.reg[..2].copy_from_slice(&[11,10]);
        let mut entry=u.a.create_label();u.a.set_label(&mut entry).unwrap();u.labels.insert(PC,entry);r.reload_mask(&mut u.a,u32::MAX);
        let mut slow=u.a.create_label();byte_stream::guards(&mut u,r,11,10,slow);
        u.a.mov(qword_ptr(r15+off::AOT_LOOP_REGIONS),1i32).unwrap();u.a.jmp(u.exit_label).unwrap();
        u.a.set_label(&mut slow).unwrap();u.a.mov(qword_ptr(r15+off::AOT_LOOP_REGIONS),2i32).unwrap();u.a.jmp(u.exit_label).unwrap();
        let(host,_)=assemble(u,0);let mask=0x00ff_ffff_ffff_ffffu64;
        for n in [0,1,2,8,mask,mask+1,mask+2,u64::MAX] {for ptr in [0,1,mask-7,mask-1,mask,mask+1,0xab00000000001000,u64::MAX] {
            let mut c=cpu();c.x[10]=n;c.x[11]=ptr;let x=c.x;
            unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
            let expected=n>0&&(ptr as u128+n as u128-1)<=mask as u128;
            assert_eq!(c.aot_loop_regions,if expected {1}else {2},"ptr={ptr:x} count={n}");assert_eq!(c.x,x);
        }}
    }
    #[test]
    fn bytestream_original_four_loops_exact_fast_slow_raw_state() {
        for (which,body) in BYTE_STREAM_WORDS.into_iter().enumerate() {for kind in [StubKind::Plain,StubKind::Patchable] {for internal_exit in [false,true] {
            let mut words=body.to_vec();words.push(0xd4001001);let exit=PC+17*4;
            let read=|p:u64|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.bytestream=true;u.loopra=true;u.loopra2=true;u.regioncache=false;u.selectra=true;u.leafinline=false;
            for p in [PC,exit].into_iter().filter(|&p|p==PC||internal_exit) {let l=u.a.create_label();u.labels.insert(p,l);}
            emit_block(&mut u,PC,&read);if internal_exit {emit_block(&mut u,exit,&read);}assert_eq!(u.bytestream_emissions,1,"loop {which}");assert_eq!(u.insns_fallback,0);
            let(host,_)=assemble(u,0);let ptr=bits(body[0],9,5)as usize;let count=(body[15]&31)as usize;
            for tag in [0,0xab00000000000000] {for nzcv in 0..16 {
                let bytes=[0x55,0,1,63,64,65,127,128,129,254,255,42,7,0x80,0x81,0x7f];let before=bytes;
                let n=[1,2,8][nzcv as usize%3];let mut c=cpu();
                for i in 0..32 {c.x[i]=(0xfeed000080000000u64+i as u64*0x10001).wrapping_add(nzcv as u64);c.v[i]=[0x7ff0000000000042+i as u64,0xdeadbeef00000000+i as u64];}
                c.x[ptr]=bytes.as_ptr()as u64+1|tag;c.x[count]=n;c.fpcr=((nzcv%4)<<22)as u64|if nzcv&4!=0 {1<<24}else {0}|if nzcv&8!=0 {1<<25}else {0};c.fpsr=0x0800009f;c.set_nzcv(nzcv);
                let mut r=cpu();r.x=c.x;r.v=c.v;r.fpcr=c.fpcr;r.fpsr=c.fpsr;r.set_nzcv(nzcv);
                for _ in 0..200 {if !internal_exit&&r.pc()==exit {break;}match {let word=read(r.pc()).unwrap();interp::exec(&mut r,word)} {Ok(())=>{},Err(interp::Exit::Svc(0x80))=>break,Err(e)=>panic!("{e:?}")}}
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,r.x,"loop={which} tag={tag:x} nzcv={nzcv}");assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());assert_eq!(c.fpcr,r.fpcr);assert_eq!(c.fpsr,r.fpsr);assert_eq!(c.pc(),r.pc());assert_eq!(c.exit_reason,if internal_exit {EXIT_SVC}else {EXIT_CONTINUE});if internal_exit {assert_eq!(c.exit_data,0x80);}else if kind==StubKind::Plain {assert_eq!(c.exit_data,0);}else {assert_ne!(c.exit_data,0);}assert_eq!(bytes,before);
                assert_eq!(c.aot_loop_regions,if tag==0 {1}else {2});
            }}
        }}}
    }

    #[test]
    fn fpflags_foldaddr_actual_range_guards_near_bit56() {
        if !has_avx()||disabled_classes()!=0 {return;}
        let words=[0xfc6d6a61,0xfd4001c2,0x1e620821,0x1e612800,0x910021ad,0x912581ce,0xf12581bf,0x54ffff21];
        let read=|p:u64|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied();
        let mut u=Unit::new(StubKind::Plain,false);u.foldaddr=true;
        let mut r=LoopRa{pc:PC,fast:u.a.create_label(),exit:u.a.create_label(),reg:[0xff;NLOOPREGS],dirty:0,vreg:[0xff;NVREGS],xreg:[0xff;NVREGS],flags:0xff};r.reg[..3].copy_from_slice(&[13,14,19]);r.vreg[..3].copy_from_slice(&[0,1,2]);
        let fp=fp_flags::plan(PC,&read,&|_|false).unwrap();let p=fold_addr::plan(&u,PC,fp,r,&read).unwrap();
        let mut entry=u.a.create_label();u.a.set_label(&mut entry).unwrap();u.labels.insert(PC,entry);
        r.reload_mask(&mut u.a,u32::MAX);let mut slow=u.a.create_label();fold_addr::guards(&mut u,p,r,slow);
        u.a.mov(qword_ptr(r15+off::AOT_LOOP_REGIONS),1i32).unwrap();u.a.mov(qword_ptr(r15+off::EXIT_REASON),EXIT_SVC as i32).unwrap();u.a.jmp(u.exit_label).unwrap();
        u.a.set_label(&mut slow).unwrap();u.a.mov(qword_ptr(r15+off::AOT_LOOP_REGIONS),2i32).unwrap();u.a.mov(qword_ptr(r15+off::EXIT_REASON),EXIT_SVC as i32).unwrap();u.a.jmp(u.exit_label).unwrap();let(host,_)=assemble(u,0);
        let mask=0x00ff_ffff_ffff_ffffu64;let mut cases=0;
        for index in [0u64,8,16,2392,2400,2408,u64::MAX,1] {for base in [0,mask-2399,mask-2398,mask,0xab00000000001000] {
            let valid=index<=2392&&index%8==0;let threshold=if valid {mask-((2400-index)/8*2400+7)}else {mask-7};
            for ptr in [0,threshold,threshold+1,mask,0xff00000000001000] {
                let expected=valid&&(base as u128+2392+7)<=mask as u128&&(ptr as u128+((2400-index.min(2400))/8)as u128*2400+7)<=mask as u128;
                let mut c=cpu();c.x[13]=index;c.x[14]=ptr;c.x[19]=base;c.set_nzcv(15);let x=c.x;
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}assert_eq!(c.aot_loop_regions,if expected {1}else {2},"idx={index:x} base={base:x} ptr={ptr:x}");assert_eq!(c.x,x);assert_eq!(c.nzcv(),15);assert_eq!(c.exit_reason,EXIT_SVC);cases+=1;
            }
        }}
        eprintln!("foldaddr actual range-guard cases={cases}; no guest data dereferenced");
    }

    #[test]
    fn fpflags_foldaddr_dual_native_paths_exact_all_state() {
        if !has_avx()||disabled_classes()!=0 {return;}
        let(mut checked,mut rejected)=(0,0);
        for double in [false,true] {for shifted in [false,true] {for high_names in [false,true] {for kind in [StubKind::Plain,StubKind::Patchable] {for product_first in [false,true] {for nan512 in [false,true].into_iter().filter(|&n|!n||has_avx512()) {
            let d=if high_names {24}else {0};let(t,m)=(d+1,d+2);let size=if double {3}else {2};let step=if shifted {1}else {1<<size};let stride=5*(1<<size);let bound=8*step;let ty=(double as u32)<<22;let offset=if high_names {2}else {0};
            let words=[(if double {0xfc606800}else {0xbc606800})|(10<<16)|if shifted {1<<12}else {0}|t,
                (if double {0xfd400020}else {0xbd400020})|m|(offset<<10),0x1e200800|ty|(m<<16)|(t<<5)|t,
                0x1e202800|ty|((if product_first {d}else {t})<<16)|((if product_first {t}else {d})<<5)|d,
                0x9100014a|(step<<10),0x91000021|(stride<<10),0xf100015f|(bound<<10),0x54ffff21,0xd4001001];
            let read=|p:u64|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied();let exit=PC+32;
            let mut u=Unit::new(kind,false);u.vbatch=false;u.foldaddr=true;u.fpflags=true;u.fpstable=false;u.fpguard=false;u.addrhoist=false;u.vloop=true;u.vchain=true;u.vscalar=true;u.loopra=true;u.loopra2=false;u.leafinline=false;u.avx512=has_avx512();u.avx512_nan=nan512;
            for p in [PC,exit] {let l=u.a.create_label();u.labels.insert(p,l);}
            emit_block(&mut u,PC,&read);emit_block(&mut u,exit,&read);assert_eq!(u.foldaddr_emissions,1);assert_eq!(u.insns_fallback,0);let(host,_)=assemble(u,0);
            let vals:Vec<u64>=if double {vec![0,0x8000000000000000,1,0x7ff0000000000000,0xfff0000000000000,0x7ff8000000000042,0x7ff0000000000073,1.25f64.to_bits()]}
                else {vec![0,0x80000000,1,0x7f800000,0xff800000,0x7fc00042,0x7f800073,1.25f32.to_bits()as u64]};
            for a in 0..10 {for b in 0..10 {for iter_start in 0..8 {let start=iter_start*step as u64;for mode in 0..4 {for fz in [false,true] {for (tag_a,tag_b) in [(0u64,0u64),(0xab00000000000000,0),(0,0xff00000000000000),(0xab00000000000000,0xff00000000000000)] {
                let mut left=[0u64;8];let mut right=[0u64;40];
                for i in 0..8 {let x=if a<8 {vals[(a+i)%8]}else {let v=if a==8 {0.0}else {(i+1)as f64*0.1};if double {v.to_bits()}else {(v as f32).to_bits()as u64}};
                    let y=if b<8 {vals[(b+3*i)%8]}else {let v=if b==8 {0.0}else {1.0+(i+1)as f64*0.125};if double {v.to_bits()}else {(v as f32).to_bits()as u64}};if double {left[i]=x;right[i*5+offset as usize]=y;}else {let ptr=left.as_mut_ptr()as *mut u32;unsafe {*ptr.add(i)=x as u32;*(right.as_mut_ptr()as *mut u32).add(i*5+offset as usize)=y as u32;}}}
                let mut c=cpu();for r in 0..32 {c.x[r]=0xfeed000000000000+r as u64;c.v[r]=[0xface000000000000+r as u64,0xdeadbeef00000000+r as u64];}
                c.x[0]=left.as_ptr()as u64|tag_a;c.x[1]=right.as_ptr()as u64|tag_b;c.x[10]=start;
                // A nonzero valid start means the source pointer is already advanced.
                c.x[1]=c.x[1].wrapping_add((start/step as u64)*stride as u64);
                let acc=if a>=8&&b>=8 {if double {0.125f64.to_bits()}else {0.125f32.to_bits()as u64}}else {vals[(a+b)%8]};
                c.v[d as usize][0]=if double {acc}else {0xface000000000000|acc};c.fpcr=(mode<<22)|if fz {1<<24}else {0};c.set_nzcv(15);
                let mut r=cpu();r.x=c.x;r.v=c.v;r.fpcr=c.fpcr;r.set_nzcv(15);interp::sys_sync_host_fp_mode(r.fpcr);let mut steps=0;
                loop {let p=r.pc();if p==exit {break;}interp::exec(&mut r,read(p).unwrap()).unwrap();if r.pc()==p {r.set_pc(p+4);}steps+=1;assert!(steps<=64);}
                interp::sys_sync_host_fp_mode(c.fpcr);unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,r.x,"D={double} scaled={shifted} names={high_names} first={product_first} a={a} b={b} start={start} mode={mode} FZ={fz}");assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());assert_eq!(c.fpcr,r.fpcr);assert_eq!(c.exit_reason,EXIT_SVC);
                assert_eq!(c.aot_loop_regions,if tag_a==0&&tag_b==0 {1}else {2},"native path selection");
                checked+=1;
            }}}}}}
            for mode in [1<<25,1<<26,1<<19] {let start=0;
                let mut c=cpu();for r in 0..32 {c.x[r]=0xab00000000001000+r as u64;c.v[r]=[r as u64,0xdeadface];}c.x[10]=start;c.fpcr=mode;c.set_nzcv(15);let(x,v)=(c.x,c.v);
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}assert_eq!(c.exit_reason,EXIT_INTERP);assert_eq!(c.pc(),PC);assert_eq!(c.x,x);assert_eq!(c.v,v);assert_eq!(c.nzcv(),15);assert_eq!(c.fpcr,mode);rejected+=1;
            }
        }}}}}}
        interp::sys_sync_host_fp_mode(0);eprintln!("foldaddr full-state cases={checked} unsupported-mode public rejections={rejected}");
    }

    #[test]
    fn fpflags_vbatch_four_ordered_iterations_exact_all_state() {
        if !has_avx()||!has_avx512()||disabled_classes()!=0 {return;}
        let(mut checked,mut rejected)=(0,0);
        for double in [false,true] {for shifted in [false,true] {for high_names in [false,true] {for kind in [StubKind::Plain,StubKind::Patchable] {for product_first in [false,true] {for nan512 in [false,true] {
            let d=if high_names {24}else {0};let(t,m)=(d+1,d+2);let size=if double {3}else {2};let step=if shifted {1}else {1<<size};let stride=5*(1<<size);let bound=8*step;let ty=(double as u32)<<22;
            let words=[(if double {0xfc606800}else {0xbc606800})|(10<<16)|if shifted {1<<12}else {0}|t,
                (if double {0xfd400020}else {0xbd400020})|m,0x1e200800|ty|(m<<16)|(t<<5)|t,
                0x1e202800|ty|((if product_first {d}else {t})<<16)|((if product_first {t}else {d})<<5)|d,
                0x9100014a|(step<<10),0x91000021|(stride<<10),0xf100015f|(bound<<10),0x54ffff21,0xd4001001];
            let read=|p:u64|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied();let exit=PC+32;
            let mut u=Unit::new(kind,false);u.vbatch=true;u.foldaddr=false;u.fpflags=true;u.fpstable=false;u.fpguard=false;u.addrhoist=false;u.vloop=true;u.vchain=true;u.vscalar=true;u.loopra=true;u.loopra2=false;u.leafinline=false;u.avx512=true;u.avx512_nan=nan512;
            for p in [PC,exit] {let l=u.a.create_label();u.labels.insert(p,l);}
            emit_block(&mut u,PC,&read);emit_block(&mut u,exit,&read);assert_eq!(u.vbatch_emissions,1);assert_eq!(u.insns_fallback,0);let(host,_)=assemble(u,0);
            let vals:Vec<u64>=if double {vec![0,0x8000000000000000,1,0x7ff0000000000000,0xfff0000000000000,0x7ff8000000000042,0x7ff0000000000073,1.25f64.to_bits()]}
                else {vec![0,0x80000000,1,0x7f800000,0xff800000,0x7fc00042,0x7f800073,1.25f32.to_bits()as u64]};
            for a in 0..10 {for b in 0..10 {for iter_start in 0..8 {let start=iter_start*step as u64;for mode in 0..4 {for fz in [false,true] {
                let mut left=[0u64;8];let mut right=[0u64;40];
                for i in 0..8 {let x=if a<8 {vals[(a+i)%8]}else {let v=if a==8 {0.0}else {(i+1)as f64*0.1};if double {v.to_bits()}else {(v as f32).to_bits()as u64}};
                    let y=if b<8 {vals[(b+3*i)%8]}else {let v=if b==8 {0.0}else {1.0+(i+1)as f64*0.125};if double {v.to_bits()}else {(v as f32).to_bits()as u64}};if double {left[i]=x;right[i*5]=y;}else {let ptr=left.as_mut_ptr()as *mut u32;unsafe {*ptr.add(i)=x as u32;*(right.as_mut_ptr()as *mut u32).add(i*5)=y as u32;}}}
                let mut c=cpu();for r in 0..32 {c.x[r]=0xfeed000000000000+r as u64;c.v[r]=[0xface000000000000+r as u64,0xdeadbeef00000000+r as u64];}
                c.x[0]=left.as_ptr()as u64|0xab00000000000000;c.x[1]=right.as_ptr()as u64|0xff00000000000000;c.x[10]=start;
                // A nonzero valid start means the source pointer is already advanced.
                c.x[1]=c.x[1].wrapping_add((start/step as u64)*stride as u64);
                let acc=if a>=8&&b>=8 {if double {0.125f64.to_bits()}else {0.125f32.to_bits()as u64}}else {vals[(a+b)%8]};
                c.v[d as usize][0]=if double {acc}else {0xface000000000000|acc};c.fpcr=(mode<<22)|if fz {1<<24}else {0};c.set_nzcv(15);
                let mut r=cpu();r.x=c.x;r.v=c.v;r.fpcr=c.fpcr;r.set_nzcv(15);interp::sys_sync_host_fp_mode(r.fpcr);let mut steps=0;
                loop {let p=r.pc();if p==exit {break;}interp::exec(&mut r,read(p).unwrap()).unwrap();if r.pc()==p {r.set_pc(p+4);}steps+=1;assert!(steps<=64);}
                interp::sys_sync_host_fp_mode(c.fpcr);unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,r.x,"D={double} scaled={shifted} names={high_names} first={product_first} a={a} b={b} start={start} mode={mode} FZ={fz}");assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());assert_eq!(c.fpcr,r.fpcr);assert_eq!(c.exit_reason,EXIT_SVC);
                if a>=8&&b>=8 {assert_eq!(c.aot_loop_regions,if kind==StubKind::Plain&&super::super::stats_enabled(){1}else{0},"finite hot path unexpectedly replayed");}
                checked+=1;
            }}}}}
            for (start,mode) in [(bound as u64+1,0),(bound as u64,0),(if step>1 {1}else {bound as u64+2},0),(u64::MAX,0),(0,1<<25),(0,1<<26),(0,1<<19)] {
                let mut c=cpu();for r in 0..32 {c.x[r]=0xab00000000001000+r as u64;c.v[r]=[r as u64,0xdeadface];}c.x[10]=start;c.fpcr=mode;c.set_nzcv(15);let(x,v)=(c.x,c.v);
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}assert_eq!(c.exit_reason,EXIT_INTERP);assert_eq!(c.pc(),PC);assert_eq!(c.x,x);assert_eq!(c.v,v);assert_eq!(c.nzcv(),15);assert_eq!(c.fpcr,mode);rejected+=1;
            }
        }}}}}}
        interp::sys_sync_host_fp_mode(0);eprintln!("vbatch full-state cases={checked} pre-effect rejections={rejected}");
    }

    #[test]
    fn fpflags_addrhoist_guarded_scalar_loads_and_exits() {
        if !has_avx()||disabled_classes()!=0 {return;}
        let(mut checked,mut rejected,mut private)=(0,0,0);
        for double in [false,true] {for shifted in [false,true] {for high in [false,true].into_iter().filter(|&v|!v||has_avx512()) {for kind in [StubKind::Plain,StubKind::Patchable] {for outlined in [false,true] {for mid in [false,true] {
            let d=if high {24}else {0};let(n,m)=(d+1,d+2);let ty=(double as u32)<<22;let size=if double {3}else {2};let step=if shifted {1}else {1<<size};let bound=3*step;
            let mut words=Vec::new();if high {for r in 0..8 {words.push(0xfd400080|r);}}
            let load_at=words.len();words.extend([(if double {0xfc606800}else {0xbc606800})|(10<<16)|if shifted {1<<12}else {0}|n,
                (if double {0xfd400020}else {0xbd400020})|m,0x1e200800|ty|(m<<16)|(n<<5)|n,0x1e202800|ty|(n<<16)|(d<<5)|d]);
            if mid {words.push(0xbd0000bb);}// test-only STR S27,[X5] changes this own Cpu's FPCR after FP arithmetic
            words.extend([0x9100014a|(step<<10),0xf100015f|(bound<<10)]);
            let at=words.len();words.push(0x54000001|(((-(at as i32)as u32)&0x7ffff)<<5));let exit=PC+words.len()as u64*4;words.push(0xd4001001);
            let read=|p:u64|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.fpflags=true;u.fpguard=outlined;u.addrhoist=true;u.vloop=true;u.vchain=true;u.vscalar=true;u.loopra=true;u.loopra2=false;u.leafinline=false;u.avx512=high;
            for p in [PC,exit] {let l=u.a.create_label();u.labels.insert(p,l);}
            emit_block(&mut u,PC,&read);emit_block(&mut u,exit,&read);assert_eq!(u.addr_hoists,1,"D={double} shifted={shifted} high={high} mid={mid}");assert_eq!(u.fp_mode_hoists,(u.fpstable&&!mid)as u64);assert_eq!(u.fpflags_sinks,1);assert_eq!(u.insns_fallback,0);let(host,_)=assemble(u,0);
            let vals:Vec<u64>=if double {vec![0x8000000000000000,1,0x7ff0000000000073]}else {vec![0x80000000,1,0x7f800073]};let pad=1f64.to_bits();
            for a in vals {for b in [0u64,if double {2f64.to_bits()}else {2f32.to_bits()as u64}] {for tag in [0u64,0xab00000000000000] {for start in 0..if mid {1}else {3} {for mode in [0,3<<22] {
                let a=if double {a}else {a|(a<<32)};let data=[a;3];let mut c=cpu();for r in 0..32 {c.x[r]=0xface000000000000+r as u64;c.v[r]=[0xabcdef+r as u64,0xfedcba0000000000+r as u64];}
                c.x[0]=data.as_ptr()as u64|tag;c.x[1]=&b as *const _ as u64;c.x[4]=&pad as *const _ as u64;c.x[5]=&mut c.fpcr as *mut _ as u64;c.x[10]=start*step as u64;c.fpcr=mode;c.set_nzcv(15);c.v[27]=[1<<25,0xfaceface];
                c.v[d as usize][0]=if double {0.125f64.to_bits()}else {0xface000000000000|0.125f32.to_bits()as u64};
                let mut r=cpu();r.x=c.x;r.v=c.v;r.fpcr=mode;r.set_nzcv(15);if mid {r.x[5]=&mut r.fpcr as *mut _ as u64;}
                interp::sys_sync_host_fp_mode(mode);let mut count=0;
                loop {let p=r.pc();if p==exit||(mid&&count!=0&&p==PC) {break;}interp::exec(&mut r,read(p).unwrap()).unwrap();if r.pc()==p {r.set_pc(p+4);}count+=1;assert!(count<128);}
                if mid {r.x[5]=c.x[5];assert_eq!(r.fpcr,1<<25);}
                interp::sys_sync_host_fp_mode(mode);unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,r.x,"D={double} scale={shifted} high={high} outlined={outlined} mid={mid} start={start} tag={tag:x}");assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());assert_eq!(c.fpcr,r.fpcr);assert_eq!(c.exit_reason,if mid {EXIT_INTERP}else {EXIT_SVC});if mid {assert_eq!(c.pc(),PC);private+=1;}else {checked+=1;}
            }}}}}
            let fp=fp_flags::plan(PC,&read,&|_|false).unwrap();let p=addr_hoist::plan(PC,fp,&read).unwrap();assert_eq!(p.at,PC+load_at as u64*4);
            for (base,index) in [(0x1000,bound as u64),(0x1000,u64::MAX),(0x00ffffffffffffff-p.span+1,0)] .into_iter().chain((step!=1).then_some((0x1000,1))) {
                let mut c=cpu();for r in 0..32 {c.x[r]=r as u64+0xfeed000000000000;c.v[r]=[r as u64,0xbeef];}c.x[0]=base|0xab00000000000000;c.x[10]=index;c.set_nzcv(15);let(x,v)=(c.x,c.v);
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}assert_eq!(c.exit_reason,EXIT_INTERP);assert_eq!(c.pc(),PC);assert_eq!(c.x,x);assert_eq!(c.v,v);assert_eq!(c.nzcv(),15);rejected+=1;
            }
        }}}}}}
        interp::sys_sync_host_fp_mode(0);eprintln!("addrhoist full-state cases={checked} private FPCR repairs={private} pre-effect guard rejections={rejected}");
    }

    #[test]
    fn fpflags_scalar_loop_rounding_nan_full_state_and_guard_entries() {
        if !has_avx()||disabled_classes()!=0 {return;}
        let mut checked=0;let mut rejected=0;
        for double in [false,true] {for counter64 in [false,true] {for high in [false,true].into_iter().filter(|&v|!v||has_avx512()) {for nan512 in [false,true].into_iter().filter(|&v|!v||has_avx512()) {for kind in [StubKind::Plain,StubKind::Patchable] {
            let shift=if high {24}else {0};let(d,n,m)=(shift,shift+1,shift+2);let ty=(double as u32)<<22;
            let mut words=Vec::new();if high {for r in 0..8 {words.push(0xfd400080|r);}}
            words.extend([if double {0xfd400000|n}else {0xbd400000|n},if double {0xfd400020|m}else {0xbd400020|m},
                0x1e200800|ty|(m<<16)|(n<<5)|n,0x1e202800|ty|(n<<16)|(d<<5)|d,
                (if counter64 {0x9100054a}else {0x1100054a}), (if counter64 {0xf1000d5f}else {0x71000d5f})]);
            let branch_pc=words.len();words.push(0x54000001|(((-(branch_pc as i32)as u32)&0x7ffff)<<5));
            let exit=PC+words.len()as u64*4;words.push(0xd4001001);
            let read=|p:u64|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.fpflags=true;u.fpguard=true;u.vloop=true;u.vchain=true;u.vscalar=true;u.vpair=true;u.loopra=true;u.loopra2=false;u.leafinline=false;u.avx512=high;u.avx512_nan=nan512;
            for p in [PC,exit] {let l=u.a.create_label();u.labels.insert(p,l);}
            emit_block(&mut u,PC,&read);emit_block(&mut u,exit,&read);assert_eq!(u.fpflags_sinks,1);assert_eq!(u.fp_mode_hoists,u.fpstable as u64);assert_eq!(u.insns_fallback,0);
            let guards:Vec<_>=u.a.instructions().windows(2).filter(|p|p[0].mnemonic()==iced_x86::Mnemonic::Test
                &&p[0].memory_base()==iced_x86::Register::R15&&p[0].memory_displacement64()==off::FPCR as u64).collect();
            assert_eq!(guards.len(),if u.fp_mode_hoists==1 {1}else {2});assert!(guards.iter().all(|p|p[1].mnemonic()==iced_x86::Mnemonic::Jne));
            let(host,_)=assemble(u,0);
            let vals:Vec<u64>=if double {vec![0,0x8000000000000000,1,1f64.to_bits(),0x7ff0000000000000,0xfff0000000000000,0x7ff8000000000042,0x7ff0000000000073]}
                else {vec![0,0x80000000,1,1f32.to_bits()as u64,0x7f800000,0xff800000,0x7fc00042,0x7f800073]};
            let padding=1f64.to_bits();
            for &a in &vals {for &b in &vals {for mode in 0..4 {
                let data=[a,b];let mut c=cpu();for r in 0..32 {c.x[r]=0x1234567800000000+r as u64;c.v[r]=[0xfeedbeef00000000+r as u64,0xabcdef0100000000+r as u64];}
                c.x[0]=data.as_ptr()as u64|0xab00000000000000;c.x[1]=unsafe {data.as_ptr().add(1)}as u64;c.x[4]=&padding as *const _ as u64;
                c.x[10]=if counter64 {0}else {0x9876543200000000};c.v[d as usize][0]=if double {1.25f64.to_bits()}else {0xfeed000000000000|1.25f32.to_bits()as u64};c.fpcr=mode<<22;c.set_nzcv(15);
                let mut r=cpu();r.x=c.x;r.v=c.v;r.fpcr=c.fpcr;r.set_nzcv(c.nzcv());
                interp::sys_sync_host_fp_mode(r.fpcr);let mut steps=0;
                loop {let p=r.pc();if p==exit {break;}interp::exec(&mut r,read(p).unwrap()).unwrap();if r.pc()==p {r.set_pc(p+4);}steps+=1;assert!(steps<256);}
                interp::sys_sync_host_fp_mode(c.fpcr);unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,r.x,"D={double} Xcounter={counter64} high={high} a={a:x} b={b:x} mode={mode}");assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());assert_eq!(c.fpcr,r.fpcr);assert_eq!(c.exit_reason,EXIT_SVC);checked+=1;
            }}}
            for bad in [1<<25,1<<26,1<<19] {let mut c=cpu();for r in 0..32 {c.x[r]=0x123450000+r as u64;c.v[r]=[0xdead0000+r as u64,0xbeef0000+r as u64];}c.fpcr=bad;c.set_nzcv(15);let (x,v)=(c.x,c.v);
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.exit_reason,EXIT_INTERP);assert_eq!(c.pc(),PC);assert_eq!(c.x,x);assert_eq!(c.v,v);assert_eq!(c.nzcv(),15);rejected+=1;
            }
        }}}}}
        interp::sys_sync_host_fp_mode(0);eprintln!("fpflags differential cases={checked} public guard rejections={rejected}");
    }

    #[test]
    fn fpflags_private_fpcr_rejection_repairs_actual_last_cmp() {
        if !has_avx()||disabled_classes()!=0 {return;}
        let mut checked=0;
        for counter64 in [false,true] {for kind in [StubKind::Plain,StubKind::Patchable] {for high in [false,true].into_iter().filter(|&v|!v||has_avx512()) {
            let shift=if high {24}else {0};let(d,n,m)=(shift,shift+1,shift+2);
            let mut words=Vec::new();if high {for r in 0..8 {words.push(0xfd400080|r);}}
            words.extend([0xfd400000|n,0xfd400020|m,0x1e600800|(m<<16)|(n<<5)|n,0x1e602800|(n<<16)|(d<<5)|d,
                0xb9000043,if counter64 {0x9100054a}else {0x1100054a},if counter64 {0xf1000d5f}else {0x71000d5f}]);
            let at=words.len();words.push(0x54000001|(((-(at as i32)as u32)&0x7ffff)<<5));
            let read=|p:u64|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.fpflags=true;u.fpguard=true;u.vloop=true;u.vchain=true;u.vscalar=true;u.loopra=true;u.loopra2=false;u.leafinline=false;u.avx512=high;
            let l=u.a.create_label();u.labels.insert(PC,l);emit_block(&mut u,PC,&read);assert_eq!(u.fpflags_sinks,1);assert_eq!(u.fp_mode_hoists,0);let(host,_)=assemble(u,0);
            let padding=1f64.to_bits();
            for counter in [0,1,if counter64 {0x7fffffffffffffff}else {0x7fffffff},if counter64 {0x8000000000000000}else {0x80000000}] {for a in [0u64,1f64.to_bits(),0x7ff0000000000073] {
                let data=[a,2f64.to_bits()];let mut c=cpu();for r in 0..32 {c.x[r]=0xfeed000000000000+r as u64;c.v[r]=[r as u64,0x12345678];}
                c.x[0]=data.as_ptr()as u64;c.x[1]=unsafe {data.as_ptr().add(1)}as u64;c.x[3]=1<<25;c.x[4]=&padding as *const _ as u64;c.x[10]=counter;c.set_nzcv(15);
                c.x[2]=&mut c.fpcr as *mut _ as u64;
                let mut r=cpu();r.x=c.x;r.v=c.v;r.set_nzcv(c.nzcv());r.x[2]=&mut r.fpcr as *mut _ as u64;
                interp::sys_sync_host_fp_mode(0);
                for &w in &words {let p=r.pc();interp::exec(&mut r,w).unwrap();if r.pc()==p {r.set_pc(p+4);}}
                assert_eq!(r.pc(),PC);assert_eq!(r.fpcr,1<<25);r.x[2]=c.x[2];
                interp::sys_sync_host_fp_mode(0);unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.exit_reason,EXIT_INTERP);assert_eq!(c.pc(),PC);assert_eq!(c.x,r.x,"Xcounter={counter64} high={high} initial={counter:x}");assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());assert_eq!(c.fpcr,r.fpcr);checked+=1;
            }}
        }}}
        interp::sys_sync_host_fp_mode(0);eprintln!("fpflags private guard repair cases={checked}");
    }

    #[test]
    fn leafflags_cloned_compare_branch_preserves_both_successors() {
        if disabled_classes()!=0||!super::super::precise_invalidation_enabled() {return;}
        let mut checked=0;
        for enabled in [false,true] {for cond in 0..16 {for wide in [false,true] {
            let target=PC+0x4000;let site=PC+8;let sf=(wide as u32)<<31;
            let leaf=[if wide {0xf9400008}else {0xb9400008},if wide {0xf9400029}else {0xb9400029},sf|0x6b09011f,sf|0x1a9fd7e8,sf|0x5a9fa100,0xd65f03c0];
            let words=[0xaa1403e0,0xaa1503e1,0xd63f0adf,0x7100001f,0x54000060|cond,0x52800170,0x14000003,0x528002d0,0x14000001,0xd4001001];
            let read=|p:u64|if p>=target {leaf.get(((p-target)/4)as usize).copied()}else {p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied()};
            super::super::bliccache::set_hint_for_test(site,target);
            let mut u=Unit::new(StubKind::Patchable,false);u.leafinline=true;u.leafflags=enabled;u.callret=false;u.loopra=true;u.loopra2=false;u.inline_ok=true;
            for i in [0,3,5,7,9] {let l=u.a.create_label();u.labels.insert(PC+i*4,l);}
            for i in [0,3,5,7,9] {emit_block(&mut u,PC+i*4,&read);}
            assert_eq!(u.inline_branch_clones,if enabled&&!ibtc2_enabled() {1}else {0});assert_eq!(u.insns_fallback,0);let(host,_)=assemble(u,0);
            for (a,b) in [(0u64,0u64),(1,2),(2,1),(0x80000000,1)] {for initial in [0,15] {
                let data=[a,b];let mut c=cpu();for n in 0..32 {c.x[n]=0xfedc123400000000+n as u64;c.v[n]=[n as u64,0x4567];}c.x[20]=data.as_ptr()as u64;c.x[21]=unsafe {data.as_ptr().add(1)}as u64;c.x[22]=target;c.set_nzcv(initial);
                let mut r=cpu();r.x=c.x;r.v=c.v;r.set_nzcv(initial);let mut steps=0;
                loop {let p=r.pc();if p==PC+36 {break;}interp::exec(&mut r,read(p).unwrap()).unwrap();if r.pc()==p {r.set_pc(p+4);}steps+=1;assert!(steps<64);}
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,r.x,"enabled={enabled} sf={wide} cond={cond} a={a:x} b={b:x}");assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());assert_eq!(c.pc(),PC+40);assert_eq!(c.exit_reason,EXIT_SVC);checked+=1;
            }}
        }}}
        eprintln!("leafflags compare/branch cases={checked}");
    }

    #[test]
    fn leafflags_private_diamond_kills_before_observer_or_public_exit() {
        if disabled_classes()!=0||!super::super::precise_invalidation_enabled() {return;}
        let mut checked=0;
        for unsafe_path in 0..3 {for callret in [false,true] {for enabled in [false,true] {
            let target=PC+0x4000;let site=PC+8;
            let leaf=[0xb9400008,0xb9400029,0x6b09011f,0x1a9fd7e8,0x5a9fa100,0xd65f03c0];
            let words=[0xaa1403e0,0xaa1503e1,0xd63f0adf,
                if unsafe_path==1 {0x34000100}else {0x34000060}, // CBZ to public exit or private arm
                if unsafe_path==2 {0x9a8ea1ab}else {0x9100056b},
                0x14000002,0xd100056b,0x7100001f,0x9a8ea1ac,0x710006f7,0x54fffec1,0xd4001001];
            let read=|p:u64|if p>=target {leaf.get(((p-target)/4)as usize).copied()}else {p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied()};
            super::super::bliccache::set_hint_for_test(site,target);
            let mut u=Unit::new(StubKind::Patchable,false);u.leafinline=true;u.leafflags=enabled;u.callret=callret;u.loopra=true;u.loopra2=false;u.inline_ok=true;
            for p in [PC,PC+44] {let l=u.a.create_label();u.labels.insert(p,l);}
            emit_block(&mut u,PC,&read);emit_block(&mut u,PC+44,&read);
            assert_eq!(u.inline_flag_elisions,if enabled&&!callret&&!ibtc2_enabled()&&unsafe_path==0 {1}else {0});
            let(host,_)=assemble(u,0);
            for (a,b) in [(0u64,0u64),(1,2),(2,1),(0x80000000,1)] {for initial in [0,5,10,15] {
                let data=[a,b];let mut c=cpu();for n in 0..32 {c.x[n]=0xfedc123400000000+n as u64;c.v[n]=[n as u64,0xabcd];}
                c.x[20]=data.as_ptr()as u64;c.x[21]=unsafe {data.as_ptr().add(1)}as u64;c.x[22]=target;c.x[23]=2;c.set_nzcv(initial);
                let mut r=cpu();r.x=c.x;r.v=c.v;r.set_nzcv(initial);let mut steps=0;
                loop {let p=r.pc();if p==PC+44 {break;}interp::exec(&mut r,read(p).unwrap()).unwrap();if r.pc()==p {r.set_pc(p+4);}steps+=1;assert!(steps<256);}
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,r.x,"path={unsafe_path} ret={callret} enabled={enabled} a={a:x} b={b:x} initial={initial}");assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());assert_eq!(c.exit_reason,EXIT_SVC);checked+=1;
            }}
        }}}
        for w in [0x69000000,0x29400000,0xb9c00000,0xbd400008,0xd4001001,0x1e602800] {
            let m=std::collections::BTreeMap::from([(PC,w),(PC+4,0x7100001f)]);
            assert!(!leaf_private_flags_killed(PC,&m),"helper/reserved/FP {w:08x}");
        }
        let m=std::collections::BTreeMap::from([(PC,0xb9400008),(PC+4,0x7100001f)]);assert!(leaf_private_flags_killed(PC,&m));
        eprintln!("leafflags private CFG cases={checked}");
    }

    #[test]
    fn leafflags_callee_readers_private_and_cloned_killers() {
        if disabled_classes()!=0||!super::super::precise_invalidation_enabled() {return;}
        let mut checked=0;
        for private in [false,true] {for callret in [false,true] {for enabled in [false,true] {for wide in [false,true] {for suffix in 0..3 {for killer in 0..3 {
            let sf=(wide as u32)<<31;let target=PC+0x2000;let site=PC+8;
            let mut leaf=vec![if wide {0xf9400008}else {0xb9400008},if wide {0xf9400029}else {0xb9400029},sf|0x6b09011f];
            if suffix==1 {leaf.push(0xd2800011);} // MOV X17,#0 destroys host fusion
            if suffix==2 {leaf.push(0xd503201f);} // unsupported transparent suffix still declines
            leaf.extend([sf|0x1a9fd7e8,sf|0x5a9fa100,0xd65f03c0]);
            let first=match killer {0=>0x7100001f,1=>0x31001c0f,_=>0x11001c0f}; // CMP; ADDS; ADD(!S)
            let mut words=vec![0xaa1403e0,0xaa1503e1,0xd63f0adf,first,0x9a8ea1ac]; // BLRAAZ x22, then CSEL x12
            if private {words.extend([0x710006f7,0x54ffff41]);} // branch at24 to header
            let exit=PC+words.len()as u64*4;words.push(0xd4001001);
            let read=|p:u64|if p>=target {leaf.get(((p-target)/4)as usize).copied()}else {p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied()};
            super::super::bliccache::set_hint_for_test(site,target);
            let mut u=Unit::new(StubKind::Patchable,false);u.leafinline=true;u.leafflags=enabled;u.callret=callret;u.loopra=true;u.loopra2=false;u.inline_ok=true;u.funra=false;u.leafra=false;
            let entries=if private {vec![0,7]}else {vec![0,3,4]};
            for &i in &entries {let l=u.a.create_label();u.labels.insert(PC+i*4,l);}
            for &i in &entries {emit_block(&mut u,PC+i*4,&read);}
            assert_eq!(u.inline_flag_elisions,if enabled&&!callret&&!ibtc2_enabled()&&suffix==0&&killer!=2 {1}else {0},"private={private} callret={callret} enabled={enabled} suffix={suffix} killer={killer}");
            assert_eq!(u.insns_fallback,0);let(host,_)=assemble(u,0);
            for a in [0u64,1,0x80000000,u64::MAX] {for b in [0u64,2,u64::MAX] {for initial in [0,5,10,15] {
                let data=[a,b];let mut c=cpu();for n in 0..32 {c.x[n]=0xfeed123400000000+n as u64;c.v[n]=[n as u64,0x12345678];}
                c.x[20]=data.as_ptr()as u64|0xab00000000000000;c.x[21]=unsafe {data.as_ptr().add(1)}as u64;c.x[22]=target;c.x[23]=2;c.set_nzcv(initial);
                let mut r=cpu();r.x=c.x;r.v=c.v;r.set_nzcv(initial);let mut steps=0;
                loop {let p=r.pc();if p==exit {break;}interp::exec(&mut r,read(p).unwrap()).unwrap();if r.pc()==p {r.set_pc(p+4);}steps+=1;assert!(steps<256);}
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,r.x,"private={private} callret={callret} enabled={enabled} sf={wide} suffix={suffix} killer={killer} a={a:x} b={b:x} flags={initial}");
                assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());assert_eq!(c.pc(),exit+4);assert_eq!(c.exit_reason,EXIT_SVC);checked+=1;
            }}}
        }}}}}}
        eprintln!("leafflags directed cases={checked}");
    }

    #[test]
    fn leafinline_restricted_worker_can_observe_but_never_inline() {
        if !super::super::bliccache::enabled() {return;}
        let mut u=Unit::new(StubKind::Patchable,false);u.leafinline=true;u.inline_ok=false;u.no_internal_jumps=true;
        let l=u.a.create_label();u.labels.insert(PC,l);emit_block(&mut u,PC,&|p|if p==PC {Some(0xd63f02c0)}else {None});
        assert!(u.region_exits.is_empty());assert!(u.inline_leaf_calls.is_empty());let(host,_)=assemble(u,0);
        let mut c=cpu();c.x[22]=PC+0x7000;c.x[30]=0xabcdef;
        unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
        assert_eq!(c.exit_reason,EXIT_BLICCACHE);assert_eq!(c.exit_data,PC);assert_eq!(c.pc(),PC+0x7000);assert_eq!(c.x[30],PC+4);
    }


    #[test]
    fn inlinewide_bounded_many_block_call_loop_state_and_guards() {
        if disabled_classes()!=0||!super::super::precise_invalidation_enabled(){return;}
        let target=PC+0x4000;let site=PC+12;let mut checked=0;
        let leaf=[0xb9400008,0xb9400029,0x6b09011f,0x1a9fd7e8,0x5a9fa100,0xd65f03c0];
        for cells in [27,32] {for enabled in [false,true] {
            // Independently authored branch diamonds, no Apple bytes. One
            // original native comparator call per iteration, public entry at0.
            let mut words=vec![0xf9000272,0xaa1403e0,0xaa1503e1,0xd63f0adf];
            for i in 0..cells {words.extend([0x7100005f|((i as u32)<<10),0x54000040,0x9100054a,0xd503201f]);}
            words.push(0xf10006f7);let at=words.len();words.push(0x54000001|(((-(at as i32)as u32)&0x7ffff)<<5));let exit=PC+words.len()as u64*4;words.push(0xd4001001);
            let read=|p:u64|if p>=target {leaf.get(((p-target)/4)as usize).copied()}else {p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied()};
            super::super::bliccache::set_hint_for_test(site,target);
            let l=super::super::loops::allocation_with_calls(PC,&read,&|p,_|p==site).unwrap();assert!(l.reducible());assert!(l.starts.len()>32);assert!(l.words.len()<256);
            let mut u=Unit::new(StubKind::Patchable,false);u.inlinewide=enabled;u.inlinegp=false;u.leafinline=true;u.leafflags=true;u.loopra=true;u.loopra2=true;u.inline_ok=true;u.funra=false;u.leafra=false;u.callret=false;
            let entries:Vec<_>=l.starts.iter().copied().chain(std::iter::once(exit)).collect();
            for &p in &entries {let label=u.a.create_label();u.labels.insert(p,label);}
            for &p in &entries {emit_block(&mut u,p,&read);}
            assert_eq!(u.inlinewide_emissions,if enabled&&cells==27 {1}else {0},"cells={cells} enabled={enabled}");assert_eq!(u.insns_fallback,0);
            if cells==32 {continue;}let(host,_)=assemble(u,0);
            for guard in 0..3 {for initial in 0..16 {for tagged in [false,true] {for count in [1,3] {
                let epoch=std::sync::atomic::AtomicU64::new(1);let data=[if initial&1==0 {u64::MAX}else {0},if initial&2==0 {1}else {0xffffffff}];let before=data;
                let mut c=cpu();for n in 0..32 {c.x[n]=0xfedc123480000000+n as u64*31;c.v[n]=[0x7ff0000000000042+n as u64,0xdeadbeef00000000+n as u64];}
                c.x[2]=initial as u64;c.x[18]=if guard==2 {2}else {1};c.x[19]=&epoch as *const _ as u64;c.x[20]=data.as_ptr()as u64|if tagged {0xab00000000000000}else {0};c.x[21]=unsafe {data.as_ptr().add(1)}as u64;c.x[22]=target+if guard==1 {4}else {0};c.x[23]=count;c.set_nzcv(initial);c.fpcr=(((initial%4)as u64)<<22)|if initial&8!=0 {1<<25}else {0};c.fpsr=0x0800009f;
                let mut r=cpu();r.x=c.x;r.v=c.v;r.fpcr=c.fpcr;r.fpsr=c.fpsr;r.set_nzcv(initial);let mut ended=false;
                for _ in 0..1000 {let at=r.pc();if guard!=0&&at==site {ended=true;break;}
                    match interp::exec(&mut r,read(at).unwrap()) {Ok(())=>{},Err(interp::Exit::Svc(0x80))=>{ended=true;break;},Err(e)=>panic!("{e:?}")}
                }assert!(ended);epoch.store(1,std::sync::atomic::Ordering::SeqCst);c.epoch_ptr=&epoch as *const _ as u64;
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,r.x,"enabled={enabled} guard={guard} count={count} NZCV={initial}");assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());assert_eq!(c.pc(),r.pc());assert_eq!(c.fpcr,r.fpcr);assert_eq!(c.fpsr,r.fpsr);assert_eq!(data,before);assert_eq!(c.exit_reason,match guard {0=>EXIT_SVC,1=>EXIT_LEAFINLINE_MISS,_=>EXIT_CONTINUE});checked+=1;
            }}}}
        }}eprintln!("inlinewide full-state cases={checked}; over64 and OFF rejected");
    }

    #[test]
    fn inlinegp_canonical_short_leaf_all_state_and_precall_guards() {
        if disabled_classes()!=0||!super::super::precise_invalidation_enabled(){return;}
        let mut checked=0;
        for wide in [false,true] {for read_lr in [false,true] {for after in 0..3 {for callret in [false,true] {
            let target=PC+0x4000;let site=PC+12;let sf=(wide as u32)<<31;
            let mut leaf=vec![if wide {0xf9400008}else {0xb9400008},if wide {0xf9400029}else {0xb9400029},sf|0x6b09011f,sf|0x1a9fd7e8,sf|0x5a9fa100];
            if read_lr {leaf.push(0xaa1e03ec);}leaf.push(0xd65f03c0);
            let mut words=vec![0xf9000272,0xaa1403e0,0xaa1503e1,0xd63f0adf];
            let entries=match after {
                0=>{words.extend([0x7100001f,0x5400006c,0x52800170,0x14000002,0x528002d0,0xd4001001]);vec![0,4,6,8,9]},
                1=>{words.extend([0x36f80060,0x52800170,0x14000002,0x528002d0,0xd4001001]);vec![0,4,5,7,8]},
                _=>{words.extend([0x9a8ea1ad,0xd4001001]);vec![0,4,5]},
            };
            let read=|p:u64|if p>=target {leaf.get(((p-target)/4)as usize).copied()}else {p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied()};
            super::super::bliccache::set_hint_for_test(site,target);
            let mut u=Unit::new(StubKind::Patchable,false);u.inlinegp=true;u.leafinline=true;u.leafflags=true;u.loopra=true;u.loopra2=true;u.inline_ok=true;u.callret=callret;u.funra=false;u.leafra=false;
            for &i in &entries {let l=u.a.create_label();u.labels.insert(PC+4*i,l);}
            for &i in &entries {emit_block(&mut u,PC+4*i,&read);}
            assert_eq!(u.inlinegp_emissions,1,"wide={wide} lr={read_lr} after={after}");assert!(u.region_exits.is_empty(),"local scope must not leak an assignment");assert!(u.region.is_none());assert_eq!(u.insns_fallback,0);
            let(host,_)=assemble(u,0);
            for guard in 0..3 {for initial in 0..16 {for (a,b)in [(0u64,0u64),(0,u64::MAX),(u64::MAX,1),(0x80000000,0x7fffffff)] {for tagged in [false,true] {
                let epoch=std::sync::atomic::AtomicU64::new(1);let data=[a,b];let before=data;
                let mut c=cpu();for n in 0..32 {c.x[n]=0xfedc123480000000+n as u64*31;c.v[n]=[0x7ff0000000000042+n as u64,0xdeadbeef00000000+n as u64];}
                c.x[18]=if guard==2 {2}else {1};c.x[19]=&epoch as *const _ as u64;c.x[20]=data.as_ptr()as u64|if tagged {0xab00000000000000}else {0};c.x[21]=unsafe {data.as_ptr().add(1)}as u64|if tagged {0xff00000000000000}else {0};c.x[22]=target+if guard==1 {4}else {0};
                c.fpcr=(((initial%4)as u64)<<22)|if initial&4!=0 {1<<24}else {0}|if initial&8!=0 {1<<25}else {0};c.fpsr=0x0800009f;c.set_nzcv(initial);
                let mut r=cpu();r.x=c.x;r.v=c.v;r.fpcr=c.fpcr;r.fpsr=c.fpsr;r.set_nzcv(initial);let mut ended=false;
                for _ in 0..64 {let at=r.pc();if guard!=0&&at==site {ended=true;break;}
                    match interp::exec(&mut r,read(at).unwrap()) {Ok(())=>{},Err(interp::Exit::Svc(0x80))=>{ended=true;break;},Err(e)=>panic!("{e:?}")}
                }assert!(ended);
                epoch.store(1,std::sync::atomic::Ordering::SeqCst);c.epoch_ptr=&epoch as *const _ as u64;
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,r.x,"wide={wide} lr={read_lr} after={after} callret={callret} guard={guard} flags={initial}");assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());assert_eq!(c.fpcr,r.fpcr);assert_eq!(c.fpsr,r.fpsr);assert_eq!(c.pc(),r.pc());assert_eq!(data,before);
                assert_eq!(c.exit_reason,match guard {0=>EXIT_SVC,1=>EXIT_LEAFINLINE_MISS,_=>EXIT_CONTINUE});assert_eq!(epoch.load(std::sync::atomic::Ordering::SeqCst),if guard==2 {2}else {1});checked+=1;
            }}}}
        }}}}
        eprintln!("inlinegp full-state cases={checked}");
    }

    #[test]
    fn leafinline_native_parent_frame_is_not_a_callee_marker() {
        if disabled_classes()!=0||!super::super::precise_invalidation_enabled() {return;}
        let target=PC+0x4000;let site=PC+28;
        let words=[0x94000004,0xd4001001,0xd503201f,0xd503201f,0xaa1e03f8,
            0xaa1403e0,0xaa1503e1,0xd63f02c0,0x0b00014a,0x710006f7,0x54ffff61,0xaa1803fe,0xd65f03c0];
        let leaf=[0xb9400008,0xb9400029,0x6b09011f,0x1a9fd7e8,0x5a9fa100,0xd65f03c0];
        let read=|p:u64|if p>=target {leaf.get(((p-target)/4)as usize).copied()}else {p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied()};
        super::super::bliccache::set_hint_for_test(site,target);
        let mut u=Unit::new(StubKind::Patchable,false);u.leafinline=true;u.loopra=true;u.loopra2=false;u.inline_ok=true;u.callret=true;u.funra=false;u.leafra=false;
        for i in [0,1,4,5,11] {let l=u.a.create_label();u.labels.insert(PC+i*4,l);}
        for i in [0,1,4,5,11] {emit_block(&mut u,PC+i*4,&read);}
        assert_eq!(u.region_exits.len(),1);
        assert!(u.a.instructions().iter().filter(|i|i.memory_base()==iced_x86::Register::R15&&i.memory_displacement64()==off::ENTRY_EPOCH as u64).count()>=3,"outer continuation plus inline pre/post-call epoch checks");
        let(host,_)=assemble(u,0);
        for (a,b) in [(0u64,0u64),(1,2),(2,1),(0x80000000,1)] {
            let data=[a,b];let mut c=cpu();for n in 0..32 {c.x[n]=0x1234567800000000+n as u64;c.v[n]=[n as u64,0xabcd];}
            c.x[20]=data.as_ptr()as u64;c.x[21]=unsafe {data.as_ptr().add(1)}as u64;c.x[22]=target;c.x[23]=3;c.x[10]=7;
            let mut r=cpu();r.x=c.x;r.v=c.v;r.set_nzcv(c.nzcv());let mut steps=0;
            loop {let p=r.pc();if p==PC+4 {break;}interp::exec(&mut r,read(p).unwrap()).unwrap();if r.pc()==p {r.set_pc(p+4);}steps+=1;assert!(steps<256);}
            unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
            assert_eq!(c.exit_reason,EXIT_SVC);assert_eq!(c.pc(),PC+8);assert_eq!(c.x,r.x);assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());
        }
    }

    #[test]
    fn leafinline_epoch_bail_before_lr_and_leaf_proof_barriers() {
        if disabled_classes()!=0||!super::super::precise_invalidation_enabled() {return;}
        let target=PC+0x2000;let site=PC+16;
        let leaf=[0xb9400008,0xb9400029,0x6b09011f,0x1a9fd7e8,0x5a9fa100,0xd65f03c0];
        let read_leaf=|p:u64|p.checked_sub(target).and_then(|o|leaf.get((o/4)as usize)).copied();
        assert!(inline_leaf_candidate(target,&read_leaf));
        let mut changed=leaf.to_vec();changed.insert(5,0xaa1f03fe);assert!(!inline_leaf_candidate(target,&|p|p.checked_sub(target).and_then(|o|changed.get((o/4)as usize)).copied()));
        changed[5]=0xd4001001;assert!(!inline_leaf_candidate(target,&|p|p.checked_sub(target).and_then(|o|changed.get((o/4)as usize)).copied()));
        // Single-thread stand-in epoch. JIT entry samples1; original guest STR
        // changes it to2 before the call guard. LR/callee effects must not run.
        let epoch=std::sync::atomic::AtomicU64::new(1);
        let words=[0xf9000272,0xaa1403e0,0xaa1503e1,0xd503201f,0xd63f02c0,0x0b00014a,0x710006f7,0x54ffff21,0xd4001001];
        let read=|p:u64|if p>=target {read_leaf(p)}else {p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied()};
        super::super::bliccache::set_hint_for_test(site,target);
        let mut u=Unit::new(StubKind::Patchable,false);u.leafinline=true;u.loopra=true;u.loopra2=false;u.inline_ok=true;
        let l=u.a.create_label();u.labels.insert(PC,l);emit_block(&mut u,PC,&read);assert_eq!(u.region_exits.len(),1);
        let(host,_)=assemble(u,0);let data=[1u64,2];let mut c=cpu();for n in 0..32 {c.x[n]=0xfedc123400000000+n as u64;}
        c.x[18]=2;c.x[19]=&epoch as *const _ as u64;c.x[20]=data.as_ptr()as u64;c.x[21]=unsafe {data.as_ptr().add(1)}as u64;c.x[22]=target;c.x[23]=2;
        let mut r=cpu();r.x=c.x;r.v=c.v;r.set_nzcv(c.nzcv());for &w in &words[..4] {let p=r.pc();interp::exec(&mut r,w).unwrap();r.set_pc(p+4);}
        epoch.store(1,std::sync::atomic::Ordering::SeqCst);c.epoch_ptr=&epoch as *const _ as u64;
        unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
        assert_eq!(c.exit_reason,EXIT_CONTINUE);assert_eq!(c.pc(),site);assert_eq!(c.x,r.x);assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());
        assert_eq!(c.entry_epoch,1);assert_eq!(epoch.load(std::sync::atomic::Ordering::SeqCst),2);
    }

    #[test]
    fn leafinline_private_loop_original_words_guards_and_state() {
        if disabled_classes()!=0||!super::super::precise_invalidation_enabled() {return;}
        let mut checked=0;
        for kind in [StubKind::Plain,StubKind::Patchable] {for form in 0..4 {let direct=form==0;for wide in [false,true] {for rn in [22,30] {
            if direct&&rn==30 {continue;}
            let header=PC;let target=PC+0x1000;let site=PC+8;
            super::super::bliccache::set_hint_for_test(site,target);
            let sf=(wide as u32)<<31;
            let leaf=[(if wide {0xf9400008}else {0xb9400008}),(if wide {0xf9400029}else {0xb9400029}),sf|0x6b09011f,sf|0x1a9fd7e8,sf|0x5a9fa100,0xd65f03c0];
            let call=if direct {0x94000000|(((target-site)/4)as u32)}else {(match form {1=>0xd63f0000,2=>0xd63f081f,_=>0xd63f0c1f})|(rn<<5)};
            // Restore the target in x30 before a BLR x30 on each iteration.
            let prefix=if rn==30 {0xaa1603fe}else {0xd503201f};
            let words=[prefix,0xaa1403e0,0xaa1503e1,call,0x0b00014a,0x710006f7,0x54ffff41,0xd4001001];
            let site=PC+12;super::super::bliccache::set_hint_for_test(site,target);
            // Fix direct displacement for this actual call position.
            let mut words=words;if direct {words[3]=0x94000000|(((target-site)/4)as u32);}
            let read=|p:u64|if p>=target {leaf.get(((p-target)/4)as usize).copied()}else {p.checked_sub(header).and_then(|o|words.get((o/4)as usize)).copied()};
            let mut u=Unit::new(kind,false);u.leafinline=true;u.loopra=true;u.loopra2=false;u.inline_ok=true;
            let exit=PC+28;for p in [header,exit] {let l=u.a.create_label();u.labels.insert(p,l);}
            // Plain/AOT must never consume process-local target hints.
            emit_block(&mut u,header,&read);
            let inlined=kind==StubKind::Patchable;
            if inlined {assert_eq!(u.region_exits.len(),1);assert!(u.insns_translated>words.len()as u64);assert_eq!(u.insns_fallback,0);}
            if !inlined {continue;}
            emit_block(&mut u,exit,&read);let(host,_)=assemble(u,0);
            for a in [0u64,1,0x7fffffff,0x80000000,u64::MAX] {for b in [0u64,1,u64::MAX] {for count in [1,3] {for mismatch in [false,true] {
                let data=[a,b];let mut c=cpu();for n in 0..32 {c.x[n]=0x1234567800000000+n as u64;c.v[n]=[n as u64,0xabcdef];}
                c.x[20]=data.as_ptr()as u64|0xab00000000000000;c.x[21]=unsafe {data.as_ptr().add(1)}as u64;
                c.x[22]=target+if mismatch&&!direct {4}else {0};c.x[23]=count;c.x[10]=7;
                let mut r=cpu();r.x=c.x;r.v=c.v;r.set_nzcv(c.nzcv());let mut steps=0;
                loop {let p=r.pc();if p==exit {break;}if mismatch&&!direct&&p==site {break;}
                    let w=read(p).expect("reference instruction");interp::exec(&mut r,w).unwrap();if r.pc()==p {r.set_pc(p+4);}steps+=1;assert!(steps<256);
                }
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,r.x,"direct={direct} wide={wide} rn={rn} a={a:x} b={b:x} n={count} mismatch={mismatch}");
                assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());
                assert_eq!(c.exit_reason,if mismatch&&!direct {EXIT_LEAFINLINE_MISS}else {EXIT_SVC});
                assert_eq!(c.pc(),if mismatch&&!direct {site}else {exit+4});checked+=1;
            }}}}
        }}}}
        eprintln!("leafinline directed cases={checked}");
    }

    #[test]
    fn csetpair_tautology_outputs_flags_observer_aliases_and_barriers() {
        if disabled_classes()!=0 {return;}
        for f in 0..16 {let mut c=cpu();c.set_nzcv(f);for cond in 0..16 {assert_eq!(condition_bits(cond,f),c.cond(cond));}}
        let mut checked=0;let mut accepted=0;
        for wide in [false,true] {for kind in [StubKind::Plain,StubKind::Patchable] {for setter in 0..3 {
        for (c1,c2) in [(13,10),(9,2),(0,1),(4,5),(6,7),(11,10),(0,0),(2,2),(14,15),(15,14)] {for (t,d) in [(8,0),(8,8),(0,9),(31,0)] {
            let sf=(wide as u32)<<31;
            let set=sf|match setter {0=>0x6b09011f,1=>0x2b09011f,_=>0x6a09011f};
            let first=sf|0x1a9f07e0|(c1<<12)|t;
            let next=sf|0x5a9f0000|(c2<<12)|(t<<5)|d;
            let k=match setter {0=>FuseKind::Sub,1=>FuseKind::Add,_=>FuseKind::Logic};
            let expect=t!=31&&(0..16).all(|f|condition_bits(c1,f)||condition_bits(c2,f))&&x86_cc(k,c1^1).is_some()&&x86_cc(k,c2^1).is_some();
            let mut probe=Unit::new(kind,false);probe.csetpair=true;probe.look=vec![next];
            {let mut e=E{u:&mut probe,pc:PC+4,fuse_in:Some(k),scalar_inputs:None};assert_eq!(e.cset_pair(first),expect);}
            if expect {accepted+=1;assert!(probe.skip_next);assert_eq!(probe.insns_translated,1);}
            let mut cut=Unit::new(kind,false);cut.csetpair=true;cut.look=vec![next];let l=cut.a.create_label();cut.labels.insert(PC+8,l);
            {let mut e=E{u:&mut cut,pc:PC+4,fuse_in:Some(k),scalar_inputs:None};assert!(!e.cset_pair(first));}
            // The following CSEL must observe original guest flags, not the
            // host-only Boolean SUB. Every live intermediate is checked too.
            let words=[set,first,next,sf|0x1a8e01ac|(c1<<12),0xd4001001];
            let read=|p:u64|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.csetpair=true;u.csetfast=false;u.loopra=false;u.loopra2=false;
            let l=u.a.create_label();u.labels.insert(PC,l);emit_block(&mut u,PC,&read);
            assert_eq!(u.insns_translated,5);assert_eq!(u.insns_fallback,0);let(host,_)=assemble(u,0);
            for a in [0,1,0x7fffffff,0x80000000,0xffffffff,0x7fffffffffffffff,0x8000000000000000,u64::MAX] {for b in [0,1,0x80000000,u64::MAX] {
                let mut c=cpu();for n in 0..32 {c.x[n]=0xfedcba9800000000+n as u64;c.v[n]=[n as u64,0x9876];}c.x[8]=a;c.x[9]=b;
                let mut r=cpu();r.x=c.x;r.v=c.v;r.set_nzcv(c.nzcv());
                for &w in &words[..4] {let p=r.pc();interp::exec(&mut r,w).unwrap();if r.pc()==p {r.set_pc(p+4);}}
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,r.x,"sf={wide} setter={setter} cond={c1},{c2} td={t},{d} a={a:x} b={b:x}");
                assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());assert_eq!(c.exit_reason,EXIT_SVC);checked+=1;
            }}
        }}}}}
        assert!(accepted>0);eprintln!("csetpair directed cases={checked} accepted-shapes={accepted}");
        for barrier in 0..5 {
            let mut u=Unit::new(StubKind::Plain,false);u.csetpair=barrier!=0;
            u.look=vec![match barrier {1=>0xda9fa100,2=>0x5a9f0120,3=>0xd4001001,_=>0x5a9fa100}];
            let mut e=E{u:&mut u,pc:PC,fuse_in:if barrier==4 {None}else {Some(FuseKind::Sub)},scalar_inputs:None};
            assert!(!e.cset_pair(0x1a9fd7e8));
        }
        // Private recursive-function-style packed flags must survive the
        // Boolean SUB, and the next select must use those flags, not host SUB.
        for wide in [false,true] {for kind in [StubKind::Plain,StubKind::Patchable] {
            let sf=(wide as u32)<<31;
            let words=[sf|0x6b09011f,sf|0x1a9fd7e8,sf|0x5a9fa100,sf|0x1a8ea1ac,0xd4001001];
            let read=|p:u64|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.csetpair=true;u.loopra=true;
            let l=u.a.create_label();u.labels.insert(PC,l);
            let mut reg=[0xff;NLOOPREGS];reg[..6].copy_from_slice(&[8,9,0,12,13,14]);
            let r=LoopRa{pc:PC,fast:u.a.create_label(),exit:u.a.create_label(),reg,dirty:0,
                vreg:[0xff;NVREGS],xreg:[0xff;NVREGS],flags:9};
            u.region_exits.push(r);u.region_labels.insert(PC,r.fast);
            emit_block_inner(&mut u,PC,&read,Some(r));u.region=None;u.region_labels.clear();
            assert_eq!(u.insns_fallback,0);let(host,_)=assemble(u,0);
            for (a,b) in [(0,0),(1,2),(2,1),(0x8000000000000000,u64::MAX)] {
                let mut c=cpu();for n in 0..32 {c.x[n]=0xfeedbeef00000000+n as u64;}c.x[8]=a;c.x[9]=b;
                let mut reference=cpu();reference.x=c.x;
                for &w in &words[..4] {interp::exec(&mut reference,w).unwrap();}
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,reference.x);assert_eq!(c.nzcv(),reference.nzcv());assert_eq!(c.exit_reason,EXIT_SVC);
            }
        }}
        // MAX_BLOCK_INSNS and labelled interior entries cannot be consumed.
        for prefix in [0,94] {for wide in [false,true] {
            let sf=(wide as u32)<<31;let mut words=vec![0xd503201f;prefix];words.extend([sf|0x6b09011f,sf|0x1a9fd7e8,sf|0x5a9fa100,0xd4001001]);
            let entry=prefix+2;let read=|p:u64|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied();
            let mut u=Unit::new(StubKind::Plain,false);u.csetpair=true;u.csetfast=true;u.loopra=false;u.loopra2=false;
            for i in [0,entry] {let l=u.a.create_label();u.labels.insert(PC+i as u64*4,l);}
            for i in [0,entry] {emit_block(&mut u,PC+i as u64*4,&read);}
            assert_eq!(u.insns_translated,words.len()as u64);let(host,_)=assemble(u,0);
            let mut c=cpu();c.x[8]=1;c.x[9]=2;
            unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
            assert_eq!(c.exit_reason,EXIT_SVC);assert_eq!(c.x[0],if wide {u64::MAX}else {u32::MAX as u64});assert_eq!(c.x[8],0);
        }}
    }

    #[test]
    fn csetfast_conditions_aliases_width_and_fused_successor() {
        if disabled_classes()!=0 {return;}
        let mut checked=0;
        for wide in [false,true] {for kind in [StubKind::Plain,StubKind::Patchable] {for mapped in [false,true] {for cond in 0..16 {for dest in [0,8,9,31] {for setter in 0..3 {
            let sf=(wide as u32)<<31;
            let first=sf|match setter {0=>0x6b09011f,1=>0x2b09011f,_=>0x6a09011f};
            let leaf=[0xb940000a,0xb940002b,first,sf|0x1a9f07e0|(cond<<12)|dest,sf|0x5a9f0100|((cond^1)<<12),0xd65f03c0];
            let read=|p:u64|p.checked_sub(PC).and_then(|o|leaf.get((o/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.leafra=mapped;u.csetfast=true;u.csetpair=false;u.loopra=mapped;u.loopra2=false;u.funra=false;u.callret=false;
            let l=u.a.create_label();u.labels.insert(PC,l);emit_block(&mut u,PC,&read);
            assert_eq!(u.insns_fallback,0);
            // Logical/add setters deliberately don't meet leafra's narrow CMP
            // whitelist. CMP cases test the mapped destination as well.
            if mapped&&setter==0 {assert_eq!(u.region_exits.len(),1);}
            let k=match setter {0=>FuseKind::Sub,1=>FuseKind::Add,_=>FuseKind::Logic};
            if dest!=31&&x86_cc(k,cond^1).is_some() {
                assert!(u.a.instructions().iter().any(|i|i.op0_kind()==iced_x86::OpKind::Register&&format!("{:?}",i.mnemonic()).starts_with("Set")),"direct register SETcc must actually be selected");
            }
            let(host,_)=assemble(u,0);
            for a in [0,1,0x7fffffff,0x80000000,0xffffffff,0x7fffffffffffffff,0x8000000000000000,u64::MAX] {for b in [0,1,0x80000000,u64::MAX] {
                let memory=[3u64,5];let mut c=cpu();for n in 0..32 {c.x[n]=0x123456789abcdef0+n as u64;c.v[n]=[n as u64,0x1234];}
                c.x[0]=memory.as_ptr()as u64;c.x[1]=unsafe {memory.as_ptr().add(1)}as u64;c.x[8]=a;c.x[9]=b;c.x[30]=PC+0x800;
                let mut r=cpu();r.x=c.x;r.v=c.v;r.set_nzcv(c.nzcv());
                for w in leaf {let p=r.pc();interp::exec(&mut r,w).unwrap();if r.pc()==p {r.set_pc(p+4);}}
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,r.x,"mapped={mapped} sf={wide} setter={setter} cond={cond} d={dest} a={a:x} b={b:x}");
                assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());assert_eq!(c.pc(),r.pc());checked+=1;
            }}
        }}}}}}
        eprintln!("csetfast directed cases={checked}");
    }

    #[test]
    fn leafra_differential_public_return_liveins_aliases_and_native_parent() {
        if disabled_classes()!=0 {return;}
        let patterns=[0,1,0x7fffffff,0x80000000,0xffffffff,0x8000000000000000,0xffffffffffffffff];
        let mut checked=0;
        for wide in [false,true] {for kind in [StubKind::Plain,StubKind::Patchable] {for callret in [false,true] {for cond in 0..16 {for shape in 0..6 {
            let sf=(wide as u32)<<31;
            let load=if wide {0xf9400000}else {0xb9400000};
            let cmp=sf|0x6b09011f;
            let mut leaf=vec![load|if shape==2 {0x3e8}else {8},load|0x29,cmp];
            match shape {
                0|1=>{leaf.push(sf|0x1a9f07e8|(cond<<12));leaf.push(sf|0x5a9f0100|((cond^1)<<12));}
                2=>{ // Adjacent plain CSEL including destructive aliases.
                    leaf.push(sf|0x1a890108|(cond<<12));
                    leaf.push(sf|0x1a880129|(cond<<12));
                }
                3=>{
                    let (d,n,m)=[(0,8,9),(8,9,8),(9,9,9),(31,31,31)][(cond&3)as usize];
                    leaf.push(sf|0x1a800000|((cond&1)<<30)|(((cond>>1)&1)<<10)|(m<<16)|(cond<<12)|(n<<5)|d);
                }
                4=>{ // W self-definitions zero the full X; MOVK is a read.
                    leaf=vec![0x2a0803e8,0xf2ffffe8,0xaa1f03e9,0xeb09011f,0x9a890100|(cond<<12)];
                }
                _=>{ // Return to a different LR: never use the native parent.
                    leaf.extend([0xd2a4001e,0xf280901e,0x52824688,0x72bfffe8,0x92ffffff,0x9a880100|(cond<<12)]);
                }
            }
            leaf.push(0xd65f03c0);
            let mut words=vec![0x94000004,0xd4001001,0xd503201f,0xd503201f];words.extend(&leaf);
            let read=|p:u64|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied();
            let plan=leaf::plan(PC+16,&read,&|_|false).unwrap();
            if shape==0 {assert_eq!(plan.livein,(1<<0)|(1<<1)|(1<<30));}
            if shape==5 {assert_eq!(plan.livein&(1<<30),0);}
            let mut u=Unit::new(kind,false);u.leafra=true;u.csetfast=true;u.csetpair=true;u.loopra=true;u.loopra2=false;u.funra=false;u.callret=callret;u.selectra=true;
            for i in [0,1,4] {let l=u.a.create_label();u.labels.insert(PC+i*4,l);}
            for i in [0,1,4] {emit_block(&mut u,PC+i*4,&read);}
            assert_eq!(u.region_exits.len(),1);assert_eq!(u.region_exits[0].dirty&!plan.used,0);
            assert_eq!(u.function_entry,None);assert_eq!(u.leaf_livein,None);
            assert_eq!(u.insns_fallback,0);assert_eq!(u.insns_translated,leaf.len()as u64+2);
            assert_eq!(u.extra_guest_range,Some((PC+16,PC+words.len()as u64*4)));
            let entry=u.labels[&PC];let continuation=u.labels[&(PC+4)];u.finish();
            let base=crate::jit::code_cache().alloc(65536);
            let res=u.a.assemble_options(base,iced_x86::BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
            assert!(res.inner.code_buffer.len()<65536);
            unsafe {std::ptr::copy_nonoverlapping(res.inner.code_buffer.as_ptr(),base as *mut u8,res.inner.code_buffer.len());}
            for s in &u.finished_stubs {if kind==StubKind::Patchable {unsafe {(res.label_ip(&s.slot).unwrap()as *mut u64).write_unaligned(res.label_ip(&s.cont).unwrap());}}}
            let host=res.label_ip(&entry).unwrap();let cont=res.label_ip(&continuation).unwrap();
            for &a in &patterns {for &b in &patterns {
                let data=[a,b];let tag=if (a^b)&1!=0 {0xab00000000000000}else {0};let mut c=cpu();
                for n in 0..32 {c.x[n]=0xfeed012345678900+n as u64;c.v[n]=[n as u64,0x12345678+n as u64];}
                c.x[0]=data.as_ptr()as u64|tag;c.x[1]=unsafe {data.as_ptr().add(1)}as u64|tag;c.x[31]=c.x[0];c.x[30]=PC+0x800;c.x[8]=a;
                c.set_nzcv(((a^b)&15)as u32);
                let mut r=cpu();r.x=c.x;r.v=c.v;r.set_nzcv(c.nzcv());
                loop {let p=r.pc();if p==PC+0x480 {break;}
                    match interp::exec(&mut r,read(p).unwrap()) {
                        Err(interp::Exit::Svc(_))=>{r.set_pc(p+4);break;},Ok(())=>{},e=>panic!("{e:?}")
                    }
                    if r.pc()==p {r.set_pc(p+4);}
                }
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                if callret&&shape!=5 {assert_eq!(c.exit_reason,EXIT_SVC,"must take matching native parent, after writeback");}
                if c.exit_reason==EXIT_CONTINUE&&c.pc()==PC+4 {unsafe {crate::jit::attesor_jit_enter(&mut *c,cont);}}
                assert_eq!(c.x,r.x,"wide={wide} callret={callret} shape={shape} cond={cond} a={a:x} b={b:x}");
                assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());assert_eq!(c.pc(),r.pc());
                assert_eq!(c.exit_reason,if shape==5 {EXIT_CONTINUE}else {EXIT_SVC});checked+=1;
            }}
        }}}}}
        eprintln!("leafra directed cases={checked}");
    }

    #[test]
    fn vtrace_ssa_live_versions_rounding_nan_aliases_and_skip_accounting() {
        if !has_avx()||disabled_classes()!=0 {return;}
        let mut checked=0;
        for wide in [false,true] {
        let values:Vec<u64>=if wide {vec![0,0x8000000000000000,1,1f64.to_bits(),(-1f64).to_bits(),0x7fefffffffffffff,
            0x7ff0000000000000,0xfff0000000000000,0x7ff8000000000042,0x7ff0000000000073,0xfff0000000000084]}
            else {vec![0,0x80000000,1,1f32.to_bits()as u64,(-1f32).to_bits()as u64,0x7f7fffff,0x7f800000,0xff800000,0x7fc00042,0x7f800073,0xff800084]};
        for high in [false,true].into_iter().filter(|&h|!h||has_avx512()) {
        for nan512 in [false,true].into_iter().filter(|&h|!h||has_avx512()) {
        for known in [false,true] {for kind in [StubKind::Plain,StubKind::Patchable] {for shape in 0..4 {
            let shift=if high {22}else {0};let ty=(wide as u32)<<22;
            let op=|code:u32,d:u32,n:u32,m:u32|if code==4 {0x1e21c000|ty|((n+shift)<<5)|(d+shift)}
                else {0x1e200800|ty|(code<<12)|((m+shift)<<16)|((n+shift)<<5)|(d+shift)};
            let load=|r:u32,base:u32|(if wide {0xfd400000}else {0xbd400000})|(base<<5)|r;
            let three=vec![op(0,2,0,0),op(0,3,1,1),op(2,2,2,3)];
            let five=vec![op(2,3,0,1),op(4,4,3,3),op(0,3,3,4),op(1,3,2,3),op(0,0,0,3)];
            let mut words=Vec::new();if high {for r in 0..8 {words.push(load(r,2));}}
            if known {for r in 0..3 {words.push(load(r+shift,r+3));}}
            match shape {
                0=>words.extend(three.clone()),
                1=>words.extend([op(3,1,1,2),op(0,3,0,0),op(0,4,1,1),op(2,3,3,4)]),
                _=>{words.extend(five);if shape==3 {words.push(0x910004c6);words.extend(three);words.push(0x910004e7);}}
            }
            words.push(0xf100054a);words.push(0x54000001|((((-(words.len()as i32))as u32)&0x7ffff)<<5));
            let exit=PC+words.len()as u64*4;words.push(0xd4001001);
            let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.loopra=true;u.loopra2=false;u.vloop=true;u.vchain=true;u.vscalar=true;u.vpair=true;u.vtrace=true;u.avx512=high;u.avx512_nan=nan512;
            for p in [PC,exit] {let l=u.a.create_label();u.labels.insert(p,l);}
            emit_block(&mut u,PC,&read);emit_block(&mut u,exit,&read);
            assert_eq!(u.insns_translated,words.len()as u64);assert_eq!(u.cold_ranges.len(),if shape==3 {2}else {1});
            for(range,live)in &u.cold_ranges {check_cold_registers(&u.a.instructions()[range.clone()],*live).unwrap();}
            let(host,_)=assemble(u,0);
            for(ia,&a)in values.iter().enumerate() {for(ib,&b)in values.iter().enumerate() {for(ic,&v)in values.iter().enumerate() {
                let pad=if wide {1.25f64.to_bits()}else {1.25f32.to_bits()as u64};let mut c=cpu();
                for r in 0..32 {c.v[r]=[0xdeadbeef00000000+r as u64,0xfeedbeef00000000+r as u64];}
                for(r,value)in [(shift,a),(shift+1,b),(shift+2,v)] {c.v[r as usize][0]=if wide {value}else {0x1234000000000000|value};}
                c.x[2]=&pad as *const _ as u64;c.x[3]=&a as *const _ as u64;c.x[4]=&b as *const _ as u64;c.x[5]=&v as *const _ as u64;
                if ia&1!=0 {for n in 2..6 {c.x[n]|=0xab00000000000000;}}
                c.x[10]=1+((ia+ib+ic)%2)as u64;c.fpcr=(((ia+ib+ic)%4)as u64)<<22;
                let mut reference=cpu();reference.x=c.x;reference.v=c.v;reference.fpcr=c.fpcr;
                interp::sys_sync_host_fp_mode(c.fpcr);
                for _ in 0..128 {let p=reference.pc();let w=read(p).unwrap();
                    if matches!(interp::exec(&mut reference,w),Err(interp::Exit::Svc(_))) {break;}
                    if reference.pc()==p {reference.set_pc(p+4);}}
                interp::sys_sync_host_fp_mode(c.fpcr);unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.v,reference.v,"wide={wide} high={high} known={known} shape={shape} fpcr={:x} inputs={a:x},{b:x},{v:x}",c.fpcr);
                assert_eq!(c.x,reference.x);assert_eq!(c.nzcv(),reference.nzcv());checked+=1;
            }}}
            let mut guarded=cpu();guarded.fpcr=1<<25;guarded.x[10]=1;guarded.v[0]=[0x1234,0x5678];let before=guarded.v;
            unsafe {crate::jit::attesor_jit_enter(&mut *guarded,host);}
            assert_eq!(guarded.exit_reason,EXIT_INTERP);assert_eq!(guarded.pc(),PC);assert_eq!(guarded.v,before);
        }}}}}}
        interp::sys_sync_host_fp_mode(0);eprintln!("FP SSA trace directed cases={checked}");
    }

    #[test]
    fn vpair_rejects_boundaries_missing_inputs_and_nonpropagating_shapes() {
        if !has_avx()||disabled_classes()!=0 {return;}
        for reject in 0..9 {
            let mut u=Unit::new(StubKind::Plain,false);u.vchain=true;u.vpair=reject!=1;
            let mut vreg=[0xff;NVREGS];vreg[..4].copy_from_slice(&[0,1,2,3]);
            if reject==5 {vreg[3]=0xff;}
            let r=LoopRa{pc:PC,fast:u.a.create_label(),exit:u.a.create_label(),reg:[0xff;NLOOPREGS],dirty:0,vreg,xreg:[0xff;NVREGS],flags:0xff};u.region=Some(r);
            u.look=vec![0x1e621862]; // DIV D2,D3,D2 consumes MUL D2,D0,D1
            if reject==2 {let l=u.a.create_label();u.labels.insert(PC+4,l);}
            if reject==3 {u.look[0]^=1<<22;}
            if reject==4 {u.look[0]=0x1e612803;}
            if reject==6 {u.region=None;}
            if reject==8 {u.look[0]=0x1e626862;}
            let first=if reject==7 {0x9e670002}else {0x1e610802};
            let mut e=E{u:&mut u,pc:PC,fuse_in:None,scalar_inputs:None};
            assert_eq!(e.scalar_pair(first),reject==0,"reject={reject}");
        }
    }

    #[test]
    fn vpair_all_scalar_operations_aliases_nan_width_and_high_bank() {
        if !has_avx()||disabled_classes()!=0 {return;}
        let mut checked=0;
        for wide in [false,true] {
        let values:Vec<u64>=if wide {vec![0,0x8000000000000000,1,1f64.to_bits(),(-1f64).to_bits(),0x7fefffffffffffff,
            0x7ff0000000000000,0xfff0000000000000,0x7ff8000000000042,0x7ff0000000000073,0xfff0000000000084]}
            else {vec![0,0x80000000,1,1f32.to_bits()as u64,(-1f32).to_bits()as u64,0x7f7fffff,0x7f800000,0xff800000,0x7fc00042,0x7f800073,0xff800084]};
        for high in [false,true].into_iter().filter(|&h|!h||has_avx512()) {
        for nan512 in [false,true].into_iter().filter(|&h|!h||has_avx512()) {
        for kind in [StubKind::Plain,StubKind::Patchable] {
        for a in 0..5 {for b in 0..5 {
        for(t,d,n,m,p,q)in[(2,3,0,1,2,3),(0,0,0,1,0,1),(2,3,0,1,0,2),(2,2,0,1,2,2)] {
            if b==4&&p!=t {continue;}
            if a==0&&b==2&&t!=d&&((p==t)^(q==t)) {continue;}
            let shift=if high {22}else {0};let(t,d,n,m,p,q)=(t+shift,d+shift,n+shift,m+shift,p+shift,q+shift);
            let ty=(wide as u32)<<22;
            let op=|code:u32,d:u32,n:u32,m:u32|if code==4 {0x1e21c000|ty|(n<<5)|d}
                else {0x1e200800|ty|(code<<12)|(m<<16)|(n<<5)|d};
            let mut words=Vec::new();if high {for r in 0..8 {words.push((if wide {0xfd400040}else {0xbd400040})|r);}}
            words.extend([op(a,t,n,m),op(b,d,p,q),0xf100054a]);
            words.push(0x54000001|((((-(words.len()as i32))as u32)&0x7ffff)<<5));
            let exit=PC+words.len()as u64*4;words.push(0xd4001001);
            let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.loopra=true;u.loopra2=false;u.vloop=true;u.vchain=true;u.vpair=true;u.avx512=high;u.avx512_nan=nan512;
            for p in [PC,exit] {let l=u.a.create_label();u.labels.insert(p,l);}
            emit_block(&mut u,PC,&read);emit_block(&mut u,exit,&read);
            assert_eq!(u.cold_ranges.len(),1,"ops={a},{b}");
            for(range,live)in &u.cold_ranges {check_cold_registers(&u.a.instructions()[range.clone()],*live).unwrap();}
            let(host,_)=assemble(u,0);
            for(ia,&va)in values.iter().enumerate() {for(ib,&vb)in values.iter().enumerate() {
                let mut c=cpu();for r in 0..32 {c.v[r]=[0xdeadbeef00000000+r as u64,0xfeedbeef00000000+r as u64];}
                c.v[n as usize][0]=if wide {va}else {0xabc0000000000000|va};
                c.v[m as usize][0]=if wide {vb}else {0xabc0000000000000|vb};
                if d!=n&&d!=m {let v=values[(ia+ib)%values.len()];c.v[d as usize][0]=if wide {v}else {0x1234000000000000|v};}
                let pad=if wide {1.25f64.to_bits()}else {1.25f32.to_bits()as u64};c.x[2]=&pad as *const _ as u64;c.x[10]=1+((ia+ib)%2)as u64;
                let mut reference=cpu();reference.x=c.x;reference.v=c.v;interp::sys_sync_host_fp_mode(0);
                for _ in 0..64 {let p=reference.pc();let w=read(p).unwrap();
                    if matches!(interp::exec(&mut reference,w),Err(interp::Exit::Svc(_))) {break;}
                    if reference.pc()==p {reference.set_pc(p+4);}}
                interp::sys_sync_host_fp_mode(0);unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.v,reference.v,"wide={wide} high={high} nan512={nan512} ops={a},{b} tdpq={t},{d},{p},{q} ab={va:x},{vb:x}");
                assert_eq!(c.x,reference.x);assert_eq!(c.nzcv(),reference.nzcv());checked+=1;
            }}
        }}}}}}}
        interp::sys_sync_host_fp_mode(0);eprintln!("general scalar pair cases={checked}");
    }

    #[test]
    fn fmovra_typed_ranking_direct_bits_width_xzr_and_high_bank() {
        if disabled_classes()!=0 {return;}
        for invalid in [0x1e260000,0x9e660000,0x9e270000,0x1e670000,0x9ee70000] {
            assert!(gpr_to_fp_access(invalid).is_none(),"{invalid:08x}");
        }
        let mut checked=0;
        for high in [false,true].into_iter().filter(|&h|!h||has_avx512()) {
        for wide in [false,true] {for zero in [false,true] {for on in [false,true] {
        for kind in [StubKind::Plain,StubKind::Patchable] {
            let d=if high {24}else {4};let acc=if high {25}else {0};let rn=if zero {31}else {5};
            let transfer=(if wide {0x9e670000}else {0x1e270000})|(rn<<5)|d;
            let a=gpr_to_fp_access(transfer).unwrap();assert_eq!(a.xr,if zero {0}else {1<<5});assert_eq!(a.vw,1<<d);
            let load=|r:u32|(if wide {0xfd400040}else {0xbd400040})|r;
            let mut words=vec![0xf100087f,0];
            if high {for r in 0..8 {words.push(load(r));}}
            words.extend([0x910003e6,0x910003ff,transfer,0x1e202800|((wide as u32)<<22)|(d<<16)|(acc<<5)|acc]);
            let join=words.len();words[1]=0x54000000|(((join-1)as u32)<<5);
            words.push(0xf1000463);words.push(0x54000001|((((-(words.len()as i32))as u32)&0x7ffff)<<5));
            let exit=PC+words.len()as u64*4;words.push(0xd4001001);
            let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.loopra=true;u.loopra2=true;u.vloop=true;u.vloop2=true;u.vchain=true;u.vscalar=true;u.fmovra=on;u.avx512=high;
            for p in [PC,exit] {let l=u.a.create_label();u.labels.insert(p,l);}
            emit_block(&mut u,PC,&read);emit_block(&mut u,exit,&read);
            let region=u.region_exits[0];assert!(region.slot(31).is_some());
            if !zero {assert_eq!(region.slot(5).is_some(),on);}
            if on&&!zero {
                let dest=iced_x86::Register::from(VREGS[region.vslot(d).unwrap()]);
                let g=region.slot(5).unwrap();let src=if wide {iced_x86::Register::from(LOOPREGS[g])}else {iced_x86::Register::from(LOOPREGS32[g])};
                assert!(u.a.instructions().iter().any(|i|i.op0_register()==dest&&i.op1_register()==src));
            }
            let(host,_)=assemble(u,0);
            let values=if wide {vec![0,0x8000000000000000,1,1.25f64.to_bits(),0x7ff0000000000000,0x7ff0000000000042,0xfff8000000000031,u64::MAX]}
                else {vec![0,0x80000000,1,1.25f32.to_bits()as u64,0x7f800000,0x7f800042,0xffc00031,u32::MAX as u64]};
            for value in values {
                let pad=if wide {1f64.to_bits()}else {1f32.to_bits()as u64};let mut c=cpu();
                for r in 0..32 {c.v[r]=[r as u64*13,0xfeedbeef00000000+r as u64];}
                c.v[acc as usize][0]=if wide {0.5f64.to_bits()}else {0x1234000000000000|0.5f32.to_bits()as u64};
                c.x[2]=(&pad as *const _ as u64)|0xab00000000000000;c.x[3]=3;c.x[5]=if wide {value}else {0xdeadbeef00000000|value};c.x[31]=0xab12345676543210;
                let mut reference=cpu();reference.x=c.x;reference.v=c.v;interp::sys_sync_host_fp_mode(0);
                for _ in 0..100 {let p=reference.pc();let w=read(p).unwrap();
                    if matches!(interp::exec(&mut reference,w),Err(interp::Exit::Svc(_))) {break;}
                    if reference.pc()==p {reference.set_pc(p+4);}}
                interp::sys_sync_host_fp_mode(0);unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,reference.x);assert_eq!(c.v,reference.v,"high={high} WIDE={wide} zero={zero} on={on} value={value:x}");assert_eq!(c.nzcv(),reference.nzcv());checked+=1;
            }
        }}}}}
        interp::sys_sync_host_fp_mode(0);eprintln!("typed GP-FP direct cases={checked}");
    }

    #[test]
    fn xspill_pressure_diamond_width_alias_sp_memory_and_public_exit() {
        if disabled_classes()!=0 {return;}
        let mut checked=0;
        for high in [false,true].into_iter().filter(|&h|!h||has_avx512()) {
        for kind in [StubKind::Plain,StubKind::Patchable] {for lazy in [false,true] {
            let mut words=vec![0xf1000b9f,0]; // CMP X28,#2; B.EQ join
            for n in 0..24u32 {
                let m=(n+1)%24;
                words.push(0x0b000000|(((n%2)as u32)<<31)|(m<<16)|(n<<5)|n);
            }
            words.extend([0xf2a00014|(0x8123<<5),0x52800015|(0x4321<<5),
                0xf84087fa,0xd10023ff,0xf90007f4,0xaa0003e0]);
            let join=words.len();words[1]=0x54000000|(((join-1)as u32)<<5);
            words.push(0xf100079c); // SUBS X28,#1
            words.push(0x54000001|((((-(words.len()as i32))as u32)&0x7ffff)<<5));
            let exit=PC+words.len()as u64*4;words.push(0xd4001001);
            let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.loopra=true;u.loopra2=true;u.xspill=true;u.cfgflags=lazy;u.regflags=false;u.avx512=high;
            for p in [PC,exit] {let l=u.a.create_label();u.labels.insert(p,l);}
            emit_block(&mut u,PC,&read);emit_block(&mut u,exit,&read);
            let r=u.region_exits[0];let count=r.xreg.iter().filter(|&&n|n!=0xff).count();
            assert!(count>=8);assert_eq!(r.vreg,[0xff;NVREGS]);
            assert!(r.xreg.iter().filter(|&&n|n!=0xff).all(|n|!r.reg.contains(n)));
            if high {assert!(count>8);}else {assert_eq!(count,8);}
            assert_eq!(u.insns_fallback,0);
            let(host,_)=assemble(u,0);
            for seed in 0..64u64 {
                let original=[seed.wrapping_mul(7919),0x123456789abcdef0];let mut mem=original;
                let mut c=cpu();for n in 0..32 {c.x[n]=(0xfedcba9876543210u64^seed.wrapping_mul(192847)).wrapping_add(n as u64*7919);c.v[n]=[n as u64*31,0xfeed0000+n as u64];}
                c.x[28]=3;c.x[31]=(&mut mem as *mut _ as u64)|if seed&1!=0 {0xab00000000000000}else {0};c.set_nzcv(9);
                let mut reference=cpu();reference.x=c.x;reference.v=c.v;reference.set_nzcv(9);
                let mut done=false;
                for _ in 0..200 {let p=reference.pc();let w=read(p).unwrap();
                    if matches!(interp::exec(&mut reference,w),Err(interp::Exit::Svc(_))) {done=true;break;}
                    if reference.pc()==p {reference.set_pc(p+4);}}
                assert!(done);let expected=mem;mem=original;
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,reference.x,"high={high} lazy={lazy} seed={seed}");
                assert_eq!(c.v,reference.v);assert_eq!(c.nzcv(),reference.nzcv());assert_eq!(mem,expected);checked+=1;
            }
        }}}
        eprintln!("XMM GPR spill directed cases={checked}");
    }

    #[target_feature(enable="avx512f")]
    unsafe fn xspill_clobber_high() {
        std::arch::asm!("vpxord zmm16,zmm16,zmm16","vpxord zmm17,zmm17,zmm17", "vpxord zmm18,zmm18,zmm18","vpxord zmm19,zmm19,zmm19",
            "vpxord zmm20,zmm20,zmm20","vpxord zmm21,zmm21,zmm21","vpxord zmm22,zmm22,zmm22","vpxord zmm23,zmm23,zmm23",
            "vpxord zmm24,zmm24,zmm24","vpxord zmm25,zmm25,zmm25","vpxord zmm26,zmm26,zmm26","vpxord zmm27,zmm27,zmm27",
            "vpxord zmm28,zmm28,zmm28","vpxord zmm29,zmm29,zmm29","vpxord zmm30,zmm30,zmm30","vpxord zmm31,zmm31,zmm31",
            out("zmm16") _,out("zmm17") _,out("zmm18") _,out("zmm19") _,out("zmm20") _,out("zmm21") _,out("zmm22") _,out("zmm23") _,
            out("zmm24") _,out("zmm25") _,out("zmm26") _,out("zmm27") _,out("zmm28") _,out("zmm29") _,out("zmm30") _,out("zmm31") _, options(nostack));
    }

    extern "C" fn xspill_observer(c:*mut Cpu,insn:u32,_pc:u64)->u64 {
        let c=unsafe {&mut *c};c.x[30]=(c.x[20]==0x123456789abcdef0&&c.x[31]==0xab00000000001234)as u64;
        for n in 0..23 {c.x[n]=c.x[n].wrapping_add(77);}
        c.x[31]=c.x[31].wrapping_add(8);c.set_nzcv(9);
        unsafe {std::arch::asm!("pxor xmm8,xmm8","pxor xmm9,xmm9","pxor xmm10,xmm10","pxor xmm11,xmm11",
            "pxor xmm12,xmm12","pxor xmm13,xmm13","pxor xmm14,xmm14","pxor xmm15,xmm15",
            out("xmm8") _,out("xmm9") _,out("xmm10") _,out("xmm11") _,out("xmm12") _,out("xmm13") _,out("xmm14") _,out("xmm15") _, options(nostack));
            if has_avx512() {xspill_clobber_high();}}
        (insn&1)as u64
    }

    #[test]
    fn xspill_helper_canonicalization_clobber_reload_and_status() {
        for high in [false,true].into_iter().filter(|&h|!h||has_avx512()) {for status in [0,1] {
            let mut u=Unit::new(StubKind::Plain,false);let mut entry=u.a.create_label();u.a.set_label(&mut entry).unwrap();u.labels.insert(PC,entry);
            let mut xreg=[0xff;NVREGS];let count=if high {24}else {8};
            for n in 0..count {xreg[n]=n as u8;}xreg[count-2]=20;xreg[count-1]=31;
            // No duplicate guest locations (high bank already includes X20).
            if high {xreg[20]=22;}
            assert_eq!(xreg.iter().filter(|&&n|n==20).count(),1);
            let r=LoopRa{pc:PC,fast:u.a.create_label(),exit:u.a.create_label(),reg:[0xff;NLOOPREGS],dirty:u32::MAX,vreg:[0xff;NVREGS],xreg,flags:0xff};
            u.region=Some(r);u.region_exits.push(r);r.reload(&mut u.a);
            let mut e=E{u:&mut u,pc:PC,fuse_in:None,scalar_inputs:None};
            e.st_imm(20,0x123456789abcdef0);e.mov_imm(rax,0xab00000000001234);e.stx(31,rax,true);
            e.fallback(status);e.ldx_nf(rax,20);e.stx(25,rax,false);e.st_pc_and_exit(PC+4);
            let(host,_)=assemble(u,0);let mut c=cpu();for n in 0..32 {c.x[n]=n as u64*1234;c.v[n]=[n as u64,0xfeed+n as u64];}let before=c.v;
            c.helpers[HELPER_INTERP]=xspill_observer as *const () as u64;c.x[25]=42;
            let mut expected=c.x;expected[20]=0x123456789abcdef0;expected[31]=0xab00000000001234;
            for n in 0..23 {expected[n]=expected[n].wrapping_add(77);}expected[31]+=8;expected[30]=1;
            if status==0 {expected[25]=expected[20];}
            unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
            assert_eq!(c.x[30],1);assert_eq!(c.x[20],0x123456789abcdef0+77);assert_eq!(c.x[31],0xab0000000000123c);
            assert_eq!(c.x[25],if status==0 {c.x[20]}else {42});assert_eq!(c.v,before);assert_eq!(c.nzcv(),9);
            assert_eq!(c.x,expected);
        }}
    }

    #[test]
    fn xspill_v_bank_and_observer_eligibility_barriers() {
        use std::collections::{BTreeMap,BTreeSet};
        let base:Vec<u32>=(0..24).map(|n|0x8b000000|(((n+1)%24)<<16)|(n<<5)|n).collect();
        let mut u=Unit::new(StubKind::Plain,false);u.xspill=true;u.loopra=true;
        for extra in [0xd503201f,0x1e612800,0xfd400001,0x3dc00000,0x94000000,0xd51b4200,0xc85f7c01] {
            let words:BTreeMap<_,_>=base.iter().copied().chain([extra]).enumerate().map(|(n,w)|(PC+n as u64*4,w)).collect();
            let l=super::super::loops::Loop{header:PC,words,starts:BTreeSet::from([PC]),backedges:vec![]};
            let mut reg=[0xff;NLOOPREGS];for(n,r)in reg.iter_mut().enumerate(){*r=n as u8;}
            assert_eq!(xspill_layout(&u,&l,&reg)[0]!=0xff,extra==0xd503201f,"{extra:08x}");
        }
    }

    #[test]
    fn cfgflags_edge_sink_conditions_private_diamonds_and_public_state() {
        use iced_x86::Mnemonic;
        if disabled_classes()!=0 {return;}
        let mut checked=0;
        for kind in [StubKind::Plain,StubKind::Patchable] {
        for on in [false,true] {for cond in 0..14 {for wide in [false,true] {
        for (writer,flag_kind) in [(0x6b01001fu32,FuseKind::Sub),(0x2b01001f,FuseKind::Add),(0x6a01001f,FuseKind::Logic)] {
            // Setter; B.cond public; CBZ to join; ADD; join: ADD; B header; SVC.
            let words=[writer|((wide as u32)<<31),0x540000a0|cond,0xb4000042,0x91000463,0x91000400,0x17fffffb,0xd4001001];
            let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.loopra=true;u.loopra2=true;u.cfgflags=on;u.regflags=false;
            for p in [PC,PC+24] {let l=u.a.create_label();u.labels.insert(p,l);}
            emit_block(&mut u,PC,&read);emit_block(&mut u,PC+24,&read);
            assert!(!u.region_exits.is_empty());
            let inst=u.a.instructions();
            let branch=inst.iter().position(|i|i.flow_control()==iced_x86::FlowControl::ConditionalBranch).unwrap();
            let store=inst.iter().position(|i|i.mnemonic()==Mnemonic::Sets).unwrap();
            assert_eq!(store>branch,on&&x86_cc(flag_kind,cond).is_some(),"kind={flag_kind:?} cond={cond}");
            let(host,_)=assemble(u,0);
            for a in [0,1,5,u64::MAX,1<<63] {for b in [0,1,5,u64::MAX,1<<63] {for c2 in [0,1] {
                let mut c=cpu();c.x[0]=a;c.x[1]=b;c.x[2]=c2;c.x[3]=0xfedcba9800000000;c.set_nzcv(9);
                let mut reference=cpu();reference.x=c.x;reference.set_nzcv(9);let mut finished=false;
                for _ in 0..128 {let p=reference.pc();let w=read(p).unwrap();
                    if matches!(interp::exec(&mut reference,w),Err(interp::Exit::Svc(_))) {finished=true;break;}
                    if reference.pc()==p {reference.set_pc(p+4);}}
                if !finished {continue;}
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,reference.x,"on={on} cond={cond} a={a:x} b={b:x}");
                assert_eq!(c.nzcv(),reference.nzcv());checked+=1;
            }}}
        }}}}}
        eprintln!("CFG flag sink cases={checked}");
    }

    #[test]
    fn cfgflags_observer_budget_and_fpcr_entry_barriers() {
        for observer in [0xd4001001,0xd53b4200,0x9a010000,0x1e612000,0x1e612800] {
            let words=[observer,0xeb01001f];
            assert!(!private_flags_dead_at(PC,&|p|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied()));
        }
        let long=vec![0xd503201fu32;129];
        assert!(!private_flags_dead_at(PC,&|p|p.checked_sub(PC).and_then(|v|long.get((v/4)as usize)).copied()));
        // The private body starts with a writer, but its FPCR guard precedes it.
        let words=[0xeb01001f,0x54000080,0xeb03005f,0x1e612800,0x17fffffc,0xd4001001];
        let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();
        let mut u=Unit::new(StubKind::Plain,false);u.loopra=true;u.loopra2=true;u.cfgflags=true;u.regflags=false;
        for p in [PC,PC+20] {let l=u.a.create_label();u.labels.insert(p,l);}
        emit_block(&mut u,PC,&read);emit_block(&mut u,PC+20,&read);
        assert!(!u.region_exits.is_empty());let(host,_)=assemble(u,0);
        let mut c=cpu();c.fpcr=1<<25;c.x[0]=0;c.x[1]=1;c.x[2]=0;c.x[3]=0;
        unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
        assert_eq!(c.exit_reason,EXIT_INTERP);assert_eq!(c.pc(),PC+8);assert_eq!(c.nzcv(),0b1000);
    }

    #[test]
    fn vscalar_direct_alias_nan_width_high_and_flags() {
        if !has_avx()||disabled_classes()!=0 {return;}
        let mut cases=0;
        for double in [false,true] {
        let patterns:Vec<u64>=if double {vec![0,0x8000000000000000,1,0x3ff0000000000000,
            0x7fefffffffffffff,0x7ff0000000000000,0xfff0000000000000,
            0x7ff8000000000042,0x7ff0000000000073,0xfff0000000000084]}
            else {vec![0,0x80000000,1,0x3f800000,0x7f7fffff,0x7f800000,0xff800000,0x7fc00042,0x7f800073,0xff800084]};
        for high in [false,true].into_iter().filter(|&h|!h||has_avx512()) {
        for nan512 in [false,true].into_iter().filter(|&h|!h||has_avx512()) {
        for kind in [StubKind::Plain,StubKind::Patchable] {
        for known in [false,true] {
        for opcode in 0..5 {
        for(d,n,m)in[(0,1,2),(1,1,2),(2,1,2),(1,1,1),(0,1,1)] {
            let shift=if high {22}else {0};let(d,n,m)=(d+shift,n+shift,m+shift);
            let ty=(double as u32)<<22;
            let load=|reg:u32,base:u32|(if double {0xfd400000}else {0xbd400000})|(base<<5)|reg;
            let mut words=Vec::new();
            if high {for r in 0..8 {words.push(load(r,2));}}
            if known {words.push(load(n,0));words.push(load(m,1));}
            let alu=if opcode==4 {0x1e21c000|ty|(n<<5)|d}
                else {0x1e200800|ty|(opcode<<12)|(m<<16)|(n<<5)|d};
            words.push(alu);words.push(0xf100054a);
            words.push(0x54000001|((((-(words.len()as i32))as u32)&0x7ffff)<<5));
            let exit=PC+words.len()as u64*4;words.push(0xd4001001);
            let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.loopra=true;u.loopra2=false;u.vloop=true;u.vchain=true;u.vscalar=true;
            u.avx512=high;u.avx512_nan=nan512;
            for p in [PC,exit] {let l=u.a.create_label();u.labels.insert(p,l);}
            emit_block(&mut u,PC,&read);emit_block(&mut u,exit,&read);
            assert!(u.region_exits[0].vslot(d).is_some());
            let independent=d!=n&&(opcode==4||d!=m);
            assert_eq!(!u.resident_cold_ranges.is_empty(),independent);
            for(range,live,out)in &u.resident_cold_ranges {
                check_cold_registers_with_output(&u.a.instructions()[range.clone()],*live,Some(*out)).unwrap();
                assert!(check_cold_registers(&u.a.instructions()[range.clone()],*live).is_err());
            }
            let(host,_)=assemble(u,0);
            for &a in &patterns {for &b in &patterns {
                let mut c=cpu();
                for r in 0..32 {c.v[r]=[0xfeedbeef00000000+r as u64,0x1234567800000000+r as u64];}
                c.v[n as usize][0]=if double {a}else {0xdeadbeef00000000|a};
                c.v[m as usize][0]=if double {b}else {0xdeadbeef00000000|b};
                let padding=if double {1.25f64.to_bits()}else {1.25f32.to_bits()as u64};
                c.x[0]=&a as *const _ as u64;c.x[1]=&b as *const _ as u64;c.x[2]=&padding as *const _ as u64;
                c.x[10]=1+(a&1);
                let mut reference=cpu();reference.x=c.x;reference.v=c.v;
                interp::sys_sync_host_fp_mode(0);
                for _ in 0..96 {let p=reference.pc();let w=read(p).unwrap();
                    if matches!(interp::exec(&mut reference,w),Err(interp::Exit::Svc(_))) {break;}
                    if reference.pc()==p {reference.set_pc(p+4);}}
                interp::sys_sync_host_fp_mode(0);
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.v,reference.v,"D={double} high={high} nan512={nan512} known={known} op={opcode} dnm={d},{n},{m} ab={a:x},{b:x}");
                assert_eq!(c.x,reference.x);assert_eq!(c.nzcv(),reference.nzcv());cases+=1;
            }}
            let mut guarded=cpu();guarded.fpcr=1<<25;guarded.x[10]=1;guarded.v[0]=[0x1234,0x5678];let before=guarded.v;
            unsafe {crate::jit::attesor_jit_enter(&mut *guarded,host);}
            assert_eq!(guarded.exit_reason,EXIT_INTERP);assert_eq!(guarded.pc(),PC);assert_eq!(guarded.v,before);
        }}}}}}}
        interp::sys_sync_host_fp_mode(0);eprintln!("resident scalar directed cases={cases}");
    }

    #[test]
    fn regflags_rejects_other_native_flag_writers_and_carry_readers() {
        for middle in [0x1e612000,0xd51b4200,0x9a010000,0xf1000400] {
            // CMP; CBZ exit; unproven instruction; B header; SVC.
            let words=[0xf100041f,0xb4000061,middle,0x17fffffd,0xd4001001];
            let mut u=Unit::new(StubKind::Plain,false);u.loopra=true;u.loopra2=true;u.regflags=true;
            let l=u.a.create_label();u.labels.insert(PC,l);
            emit_block(&mut u,PC,&|p|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied());
            assert_eq!(u.region_exits.len(),1);
            assert!(u.region_exits[0].packed_flags().is_none(),"{middle:08x}");
        }
    }

    #[test]
    fn vloop2_diamond_gpr_to_fp_alias_nan_and_public_exit() {
        if disabled_classes()!=0 {return;}
        for kind in [StubKind::Plain,StubKind::Patchable] {
        for packed in [false,true] {
        for double in [false,true] {
            let ty=(double as u32)<<22;
            let words=[0xf100087f,0x54000080,
                if double {0x9e6700a1}else {0x1e2700a1}, // FMOV D/S1,X/W5
                0x1e220821|ty,0x1e212800|ty,
                if packed {0xd1000463}else {0xf1000463},
                if packed {0xb5ffff43}else {0x54ffff41},
                if double {0xfd000000}else {0xbd000000},0xd4001001];
            let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.vloop=true;u.vloop2=true;u.vchain=true;u.loopra=true;u.loopra2=true;
            u.regflags=packed;
            for offset in [0,28] {let l=u.a.create_label();u.labels.insert(PC+offset,l);}
            emit_block(&mut u,PC,&read);emit_block(&mut u,PC+28,&read);
            assert!(u.region_exits[0].vslot(0).is_some());
            assert_eq!(u.region_exits[0].packed_flags().is_some(),packed);
            let(host,_)=assemble(u,0);
            let values=if double {vec![1.25f64.to_bits(),0x8000000000000000,0x7ff0000000000000,0x7ff0000000000042]}
                else {vec![1.25f32.to_bits()as u64,0x80000000,0x7f800000,0x7f800042]};
            for value in values {
                let mut out=0u64;let mut c=cpu();
                for n in 0..32 {c.v[n]=[n as u64,0xfeedbeef00000000+n as u64];}
                c.v[0][0]=if double {0.5f64.to_bits()}else {0x1234000000000000|0.5f32.to_bits()as u64};
                c.v[2][0]=if double {2f64.to_bits()}else {0x1234000000000000|2f32.to_bits()as u64};
                c.x[0]=&mut out as *mut _ as u64;c.x[3]=3;c.x[5]=value;
                let mut reference=cpu();reference.x=c.x;reference.v=c.v;
                interp::sys_sync_host_fp_mode(0);
                for _ in 0..64 {
                    let p=reference.pc();let w=read(p).unwrap();
                    if matches!(interp::exec(&mut reference,w),Err(interp::Exit::Svc(_))) {break;}
                    if reference.pc()==p {reference.set_pc(p+4);}
                }
                let expected=out;out=0;
                interp::sys_sync_host_fp_mode(0);
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(out,expected);assert_eq!(c.v,reference.v);assert_eq!(c.x,reference.x);assert_eq!(c.nzcv(),reference.nzcv());
            }
            let mut c=cpu();c.fpcr=1<<25;c.x[3]=1;c.v[0]=[1,2];let before=c.v;
            unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
            assert_eq!(c.exit_reason,EXIT_INTERP);assert_eq!(c.pc(),PC+8);assert_eq!(c.v,before);
            assert_eq!(c.nzcv(),0b1000);
        }}}
        interp::sys_sync_host_fp_mode(0);
    }

    #[test]
    fn vloop2_gpr_transfer_full_bits_and_extended_xmm() {
        for high in [false,true].into_iter().filter(|&h|!h||has_avx512()) {
        for duplicate in [false,true] {
        for value in [0,0x8000000000000000,0x7ff0000000000042,u64::MAX] {
            let mut u=Unit::new(StubKind::Plain,false);u.avx512=high;
            let mut entry=u.a.create_label();u.a.set_label(&mut entry).unwrap();u.labels.insert(PC,entry);
            let mut vreg=[0xff;NVREGS];vreg[if high {9}else {0}]=20;
            let region=LoopRa{pc:PC,fast:u.a.create_label(),exit:u.a.create_label(),reg:[0xff;NLOOPREGS],dirty:0,vreg,xreg:[0xff;NVREGS],flags:0xff};
            u.region=Some(region);u.region_exits.push(region);
            let mut e=E{u:&mut u,pc:PC,fuse_in:None,scalar_inputs:None};
            e.mov_imm(rax,value);e.vst_rax(20,duplicate);e.st_pc_and_exit(PC+4);
            let(host,_)=assemble(u,0);let mut c=cpu();c.v[20]=[0x1234,0x5678];
            unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
            assert_eq!(c.v[20],[value,if duplicate {value}else {0}]);
        }}}
    }

    #[test]
    fn selectra_group_aliases_zero_register_width_and_live_flag_chain() {
        if disabled_classes()!=0 {return;}
        use iced_x86::Register;
        for kind in [StubKind::Plain,StubKind::Patchable] {
        for sf in [false,true] {
        for mixed in [false,true] {
        for cond in 0..14 {
        for (d,n,m) in [(0,1,2),(0,0,2),(0,1,0),(0,0,0),(0,1,1),(31,0,1),(0,31,1),(0,1,31),(0,31,31),(1,1,0)] {
            let select=|d:u32,n:u32,m:u32,c:u32|0x1a800000u32|((sf as u32)<<31)|(m<<16)|(c<<12)|(n<<5)|d;
            let words=[0xeb05009f,select(d,n,m,cond),select(6,d,6,cond)^if mixed {0x80000000}else {0},0xf100079c,0x54ffff81,0xd4001001];
            let mut u=Unit::new(kind,false);u.loopra=true;u.loopra2=false;u.selectra=true;
            for offset in [0,20] {let l=u.a.create_label();u.labels.insert(PC+offset,l);}
            let read=|p:u64|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied();
            emit_block(&mut u,PC,&read);emit_block(&mut u,PC+20,&read);
            assert!(!u.a.instructions().iter().any(|i|i.mnemonic()==iced_x86::Mnemonic::Cmovne && i.op0_register().full_register()==Register::RCX));
            let(host,_)=assemble(u,0);
            for a in [0,1,u64::MAX,1<<63,i64::MAX as u64] {
            for b in [0,1,u64::MAX,1<<63,i64::MAX as u64] {
                let mut c=cpu();for r in 0..32 {c.x[r]=0xfedcba9800000000+r as u64*79;}
                c.x[4]=a;c.x[5]=b;c.x[28]=1+(a&1);
                let mut reference=cpu();reference.x=c.x;
                for _ in 0..32 {
                    let p=reference.pc();let w=read(p).unwrap();
                    if matches!(interp::exec(&mut reference,w),Err(interp::Exit::Svc(_))) {break;}
                    if reference.pc()==p {reference.set_pc(p+4);}
                }
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,reference.x,"sf={sf} cond={cond} regs={d},{n},{m} compare={a:x},{b:x}");
                assert_eq!(c.nzcv(),reference.nzcv());
            }}
        }}}}}
    }

    #[test]
    fn funra_edge_prefix_and_paths_reject_observers_reserved_missing_cycles() {
        use std::collections::{BTreeMap,BTreeSet};
        let make=|words:&[u32]|super::super::loops::Loop{header:PC,words:words.iter().enumerate().map(|(i,&w)|(PC+i as u64*4,w)).collect(),starts:BTreeSet::from([PC]),backedges:vec![]};
        for (w,ok) in [(0xd503201f,true),(0xa9be4ff4,true),(0x910043fd,true),
            (0xa94003e0,false),(0xad0007e0,false),(0x290007e0,false),(0xd51b4400,false),
            (0x9a010000,false),(0xf9000001,false),(0x1e612000,false),(0x94000000,false),
            (0x54000020,false),(0xd65f03c0,false),(0x1ac20800,false)] {
            assert_eq!(fun_edges::prefix(&make(&[w,0x7100081f])).is_some(),ok,"prefix={w:x}");
        }
        assert!(fun_edges::prefix(&make(&[])).is_none());
        let mut long=vec![0xd503201f;MAX_BLOCK_INSNS as usize];long.push(0x7100081f);assert!(fun_edges::prefix(&make(&long)).is_none());
        let prefix=BTreeMap::from([(PC,0x7100081f)]);
        for words in [vec![0x14000000],vec![0x94000001],vec![0x14000001],
            vec![0xb4000040,0x7100081f,0xd65f03c0],vec![0xf9000001,0x7100081f],vec![0xd51b4200,0x7100081f]] {
            assert!(fun_edges::dead(&make(&words),PC,Some(&prefix)).is_none(),"path={words:x?}");
        }
        assert!(fun_edges::dead(&make(&[0x94000000]),PC,None).is_none());
        assert!(fun_edges::dead(&make(&[0x94000000]),PC,Some(&prefix)).is_some());
    }

    #[test]
    fn funra_edge_all_conditions_live_edges_and_stack_guard_clobbers() {
        if disabled_classes()!=0 {return;}
        let mut checked=0;
        for kind in [StubKind::Plain,StubKind::Patchable] {for stackfast in [false,true] {for sf in [false,true] {for dead_yes in [false,true] {for cond in 0..14 {
            let cmp=if sf {0xeb02003f}else {0x6b02003f};let kill=if sf {0xeb04007f}else {0x6b04007f};
            let words=if dead_yes {vec![0xa9bf7bf3,0x910003e9,cmp,0x54000060|cond,0xa8c17bf3,0xd65f03c0,kill,0xb4000080,0xd1000400,0x97fffff7,0x91000400,0xa8c17bf3,0xd65f03c0]}
                else {vec![0xa9bf7bf3,0x910003e9,cmp,0x540000e0|cond,kill,0xb40000a0,0xd1000400,0x97fffff9,0x91000400,0x14000001,0xa8c17bf3,0xd65f03c0]};
            let ret=PC+words.len()as u64*4;let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.funedge=true;u.funra=true;u.funflags=true;u.funstack=stackfast;u.loopra=true;
            let l=u.a.create_label();u.labels.insert(PC,l);emit_block(&mut u,PC,&read);assert!(u.funedge_emissions>0);assert_eq!(u.insns_fallback,0);let(host,_)=assemble(u,0);
            for a in [0,1,u64::MAX,1<<63,i64::MAX as u64,0x80000000,0x7fffffff] {for b in [0,1,u64::MAX,1<<63,i64::MAX as u64,0x80000000,0x7fffffff] {for nv in 0..16 {
                let mut stack=[0xfeed123456789abcu64;64];let initial=stack;
                let mut c=cpu();for n in 0..32 {c.x[n]=0xfedcba9800000000+n as u64;c.v[n]=[0x7ff0000000000042+n as u64,0x8000000000000000+n as u64];}
                c.x[0]=0;c.x[1]=a;c.x[2]=b;c.x[3]=a.wrapping_add(1);c.x[4]=b.wrapping_sub(1);c.x[30]=ret;
                c.x[31]=(stack.as_mut_ptr()as u64+512)|if nv&1!=0 {0xab00000000000000}else {0};c.set_nzcv(nv);
                let mut reference=cpu();reference.x=c.x;reference.v=c.v;reference.set_nzcv(nv);let mut steps=0;
                while reference.pc()!=ret {let p=reference.pc();interp::exec(&mut reference,read(p).unwrap()).unwrap();if reference.pc()==p {reference.set_pc(p+4);}steps+=1;assert!(steps<32);}
                let expected=stack;stack=initial;std::hint::black_box(&mut stack);
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,reference.x,"sf={sf} cond={cond} dead_yes={dead_yes} a={a:x} b={b:x}");assert_eq!(c.v,reference.v);assert_eq!(c.nzcv(),reference.nzcv());assert_eq!(c.pc(),ret);assert_eq!(c.exit_reason,EXIT_CONTINUE);assert_eq!(stack,expected);checked+=1;
            }}}
        }}}}}eprintln!("funedge all-condition/full-state/full-stack cases={checked}");
    }

    #[test]
    fn funra_movflags_all_conditions_raw_state_and_stack_guard_clobbers() {
        if disabled_classes()!=0 {return;}
        let mut checked=0;
        for kind in [StubKind::Plain,StubKind::Patchable] {for stackfast in [false,true] {for sf in [false,true] {for dead_yes in [false,true] {for cond in 0..14 {
            let cmp=if sf {0xeb02003f}else {0x6b02003f};let kill=if sf {0xeb04007f}else {0x6b04007f};
            let words=if dead_yes {vec![0xa9bf7bf3,0x910003e9,cmp,0x54000060|cond,0xa8c17bf3,0xd65f03c0,kill,0xb4000080,0xd1000400,0x97fffff7,0x91000400,0xa8c17bf3,0xd65f03c0]}
                else {vec![0xa9bf7bf3,0x910003e9,cmp,0x540000e0|cond,kill,0xb40000a0,0xd1000400,0x97fffff9,0x91000400,0x14000001,0xa8c17bf3,0xd65f03c0]};
            let mut words:Vec<u32>=words;words.insert(3,if sf {0xaa0103ea}else {0x2a0103ea}); // MOV X/W10,X/W1 between CMP and B.cond
            for w in &mut words {if *w&0xfc000000==0x94000000 {*w=0x94000000|((*w&0x03ffffff).wrapping_sub(1)&0x03ffffff);}}
            let ret=PC+words.len()as u64*4;let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.movflags=true;u.funedge=false;u.funra=true;u.funflags=true;u.funstack=stackfast;u.loopra=true;
            let l=u.a.create_label();u.labels.insert(PC,l);emit_block(&mut u,PC,&read);assert!(u.movflags_emissions>0);assert_eq!(u.insns_fallback,0);let(host,_)=assemble(u,0);
            for a in [0,1,u64::MAX,1<<63,i64::MAX as u64,0x80000000,0x7fffffff] {for b in [0,1,u64::MAX,1<<63,i64::MAX as u64,0x80000000,0x7fffffff] {for nv in 0..16 {
                let mut stack=[0xfeed123456789abcu64;64];let initial=stack;
                let mut c=cpu();for n in 0..32 {c.x[n]=0xfedcba9800000000+n as u64;c.v[n]=[0x7ff0000000000042+n as u64,0x8000000000000000+n as u64];}
                c.x[0]=0;c.x[1]=a;c.x[2]=b;c.x[3]=a.wrapping_add(1);c.x[4]=b.wrapping_sub(1);c.x[30]=ret;
                c.x[31]=(stack.as_mut_ptr()as u64+512)|if nv&1!=0 {0xab00000000000000}else {0};c.set_nzcv(nv);
                let mut reference=cpu();reference.x=c.x;reference.v=c.v;reference.set_nzcv(nv);let mut steps=0;
                while reference.pc()!=ret {let p=reference.pc();interp::exec(&mut reference,read(p).unwrap()).unwrap();if reference.pc()==p {reference.set_pc(p+4);}steps+=1;assert!(steps<32);}
                let expected=stack;stack=initial;std::hint::black_box(&mut stack);
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,reference.x,"sf={sf} cond={cond} dead_yes={dead_yes} a={a:x} b={b:x}");assert_eq!(c.v,reference.v);assert_eq!(c.nzcv(),reference.nzcv());assert_eq!(c.pc(),ret);assert_eq!(c.exit_reason,EXIT_CONTINUE);assert_eq!(stack,expected);checked+=1;
            }}}
        }}}}}eprintln!("movflags all-condition/full-state/full-stack cases={checked}");
    }

    #[test]
    fn funra_edge_deep_ras_and_new_callee_flags_on_epoch_exit() {
        use std::sync::atomic::{AtomicU64,Ordering};
        if disabled_classes()!=0 {return;}
        for inline_leaf in [false,true] {
        for epoch_case in [false,true] {for kind in [StubKind::Plain,StubKind::Patchable] {for tagged in [false,true] {for stackfast in [false,true] {
            let words=if epoch_case {vec![0xa9bf7bf3,0x910003e9,0x7100001f,0x540000a0,0xd1000400,0x97fffffb,0x91000400,0x14000002,0xf9000272,0xa8c17bf3,0xd65f03c0]}
                else {vec![0xa9bf7bf3,0x910003e9,0x7100001f,0x54000080,0xd1000400,0x97fffffb,0x91000400,0xa8c17bf3,0xd65f03c0]};
            let cont=PC+24;let ret=PC+words.len()as u64*4;let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.recleaf=inline_leaf;u.funra=true;u.funflags=true;u.funedge=true;u.funstack=stackfast;u.loopra=true;
            let l=u.a.create_label();u.labels.insert(PC,l);emit_block(&mut u,PC,&read);assert!(u.funedge_emissions>0);assert_eq!(u.recleaf_emissions>0,inline_leaf);assert_eq!(u.insns_fallback,0);let(host,_)=assemble(u,0);
            let mut resume=Unit::new(kind,false);resume.funra=false;resume.funedge=false;let l=resume.a.create_label();resume.labels.insert(cont,l);emit_block(&mut resume,cont,&read);let(continuation,_)=assemble(resume,6);
            let epoch=AtomicU64::new(1);let mut stack:Vec<u64>=(0..4096).map(|n|0xfeed000000000000+n).collect();let initial=stack.clone();
            let mut c=cpu();for n in 0..32 {c.x[n]=0xfeed000000000000+n as u64;c.v[n]=[n as u64,0xfff0000000000042+n as u64];}
            c.x[0]=if epoch_case {1}else {300};c.x[18]=2;c.x[19]=&epoch as *const _ as u64;c.x[30]=ret;c.x[31]=(stack.as_mut_ptr()as u64+stack.len()as u64*8)|if tagged {0xab00000000000000}else {0};c.set_nzcv(13);c.fpcr=if tagged {0x04c00000}else {0x00400000};c.fpsr=0x9f;c.epoch_ptr=&epoch as *const _ as u64;
            let mut reference=cpu();reference.x=c.x;reference.v=c.v;reference.set_nzcv(13);reference.fpcr=c.fpcr;reference.fpsr=c.fpsr;let mut steps=0;
            loop {let p=reference.pc();interp::exec(&mut reference,read(p).unwrap()).unwrap();if reference.pc()==p {reference.set_pc(p+4);}steps+=1;assert!(steps<10000);if reference.pc()==if epoch_case {cont}else {ret} {break;}}
            let expected=stack.clone();stack.copy_from_slice(&initial);epoch.store(1,Ordering::SeqCst);
            unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
            if epoch_case {assert_eq!(epoch.load(Ordering::SeqCst),2);assert_eq!(c.entry_epoch,1);assert_eq!(c.pc(),cont);assert_eq!(c.nzcv(),6,"callee NEW flags, not incoming13 or caller2");}
            else {let mut exits=0;while c.pc()!=ret {assert_eq!(c.pc(),cont);unsafe {crate::jit::attesor_jit_enter(&mut *c,continuation);}exits+=1;assert!(exits<400);}assert!(exits>0);}
            if inline_leaf {
                if epoch_case {assert!(c.aot_loop_regions>=0x100000,"actual inline path");}
                else {assert!(c.aot_loop_regions>=0x100&&c.aot_loop_regions<0x100000,"RAS-full MUST reject inline leaf");}
            }
            assert_eq!(c.fpcr,reference.fpcr);assert_eq!(c.fpsr,reference.fpsr);
            assert_eq!(c.x,reference.x);assert_eq!(c.v,reference.v);assert_eq!(c.nzcv(),reference.nzcv());assert_eq!(c.pc(),reference.pc());assert_eq!(c.exit_reason,EXIT_CONTINUE);assert_eq!(stack,expected);
        }}}}}
    }

    #[test]
    fn funra_recleaf_snapshot_prefix_operands_unknown_cycles_rejected() {
        use std::collections::BTreeSet;
        let base=[0xa9bf7bf3,0x910003e9,0x7100001f,0x54000080,0xd1000400,0x97fffffb,0x91000400,0xa8c17bf3,0xd65f03c0];
        let make=|w:&[u32]|super::super::loops::Loop{header:PC,words:w.iter().enumerate().map(|(i,&w)|(PC+i as u64*4,w)).collect(),starts:BTreeSet::from([PC,PC+16,PC+24,PC+28]),backedges:vec![]};
        assert!(rec_leaf::plan(&make(&base)).is_some());
        for (index,word) in [(1,0x910003e0), // prefix overwrites predicate X0
            (0,0xa94003e0), // duplicate pair fallback
            (0,0xad0007e0), // SIMD pair
            (1,0xd51b4400), // system/observer
            (2,0x710003ff), // predicate SP modified by original prologue
            (7,0x14000000), // leaf cycle
            (7,0x94000000), // leaf call
            (7,0xd4001001), // leaf observer
            (7,0xf98003e0), // unsupported PRFM family
            (8,0xd65f0000)] { // other return register
            let mut w=base;w[index]=word;assert!(rec_leaf::plan(&make(&w)).is_none(),"index={index} word={word:x}");
        }
        let mut l=make(&base);l.words.remove(&(PC+28));assert!(rec_leaf::plan(&l).is_none());
    }

    #[test]
    fn funra_recleaf_ret_preserves_raw_tagged_targets_and_parent_marker() {
        if disabled_classes()!=0 {return;}
        for kind in [StubKind::Plain,StubKind::Patchable] {for inline_leaf in [false,true] {for stackfast in [false,true] {for tagged_sp in [false,true] {for tagged_target in [false,true] {
            // Original callee deliberately rewrites its SAVED LR slot before
            // LDP/RET. If low bits match a parent continuation, raw tag MUST NOT
            // be silently normalized or consumed by a native RET fast path.
            let words=[0xa9bf7bf3,0x910003e9,0x7100001f,0x540000a0,0xd1000400,0x97fffffb,0x91000400,0x14000002,0xf90007f2,0xa8c17bf3,0xd65f03c0];
            let cont=PC+24;let ret=PC+44;let target=if tagged_target {0xab00000000000000|cont}else {ret+4};
            let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.recleaf=inline_leaf;u.funra=true;u.loopra=true;u.funflags=true;u.funstack=stackfast;
            let l=u.a.create_label();u.labels.insert(PC,l);emit_block(&mut u,PC,&read);assert_eq!(u.recleaf_emissions>0,inline_leaf);assert_eq!(u.insns_fallback,0);let(host,_)=assemble(u,0);
            let mut stack=[0xfeed123456789abcu64;64];let initial=stack;let mut c=cpu();for n in 0..32 {c.x[n]=0xfedcba9800000000+n as u64;c.v[n]=[n as u64,0x7ff0000000000042+n as u64];}
            c.x[0]=1;c.x[18]=target;c.x[30]=ret;c.x[31]=(stack.as_mut_ptr()as u64+512)|if tagged_sp {0xab00000000000000}else {0};c.set_nzcv(13);c.fpcr=0x04c00000;c.fpsr=0x9f;
            let mut reference=cpu();reference.x=c.x;reference.v=c.v;reference.set_nzcv(13);reference.fpcr=c.fpcr;reference.fpsr=c.fpsr;let mut steps=0;
            while reference.pc()!=target {let p=reference.pc();interp::exec(&mut reference,read(p).unwrap()).unwrap();if reference.pc()==p {reference.set_pc(p+4);}steps+=1;assert!(steps<32);}
            let expected=stack;stack=initial;std::hint::black_box(&mut stack);
            unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
            if inline_leaf {assert!(c.aot_loop_regions>=0x100000);}
            assert_eq!(c.pc(),target);assert_eq!(c.x,reference.x);assert_eq!(c.v,reference.v);assert_eq!(c.nzcv(),reference.nzcv());assert_eq!(c.fpcr,reference.fpcr);assert_eq!(c.fpsr,reference.fpsr);assert_eq!(c.exit_reason,EXIT_CONTINUE);assert_eq!(c.exit_data,0);assert_eq!(stack,expected);
        }}}}}
    }

    #[test]
    fn funra_recleaflazy_proof_rejects_readers_writers_and_live_continuations() {
        let good=[0xa9bf7bf3,0x910003e9,0x7100001f,0x540000a0,0xd1000400,0x97fffffb,0x71000e7f,0x91000400,0xa8c17bf3,0xd65f03c0];
        let planning=|w:&[u32]| {
            let read=|p:u64|p.checked_sub(PC).and_then(|v|w.get((v/4)as usize)).copied();
            functions::recursive(PC,&read).and_then(|l|rec_leaf::plan(&l))
        };
        assert!(!rec_leaf::witnesses(&planning(&good).unwrap()).is_empty());
        for (index,word) in [(2,0x6b02001f), // RR: no single-operand recipe
            (6,0xd503201f), // continuation RET before kill
            (6,0x14000000), // cycle
            (6,0x54000020), // NZCV reader before kill
            (6,0xf9000270), // uncertain memory before kill
            (8,0x71000e7f), // leaf writes NZCV
            (8,0x54000020)] { // leaf reads NZCV
            let mut w=good;w[index]=word;let p=planning(&w).expect("eager101 fallback still valid");
            assert!(rec_leaf::witnesses(&p).is_empty(),"index={index} word={word:x}");
        }
        let mut cut=functions::recursive(PC,&|p|p.checked_sub(PC).and_then(|v|good.get((v/4)as usize)).copied()).unwrap();
        cut.words.remove(&(PC+24));assert!(rec_leaf::witnesses(&rec_leaf::plan(&cut).unwrap()).is_empty());
    }

    #[test]
    fn funra_recleaflazy_all_conditions_cold_observers_full_state() {
        use std::sync::atomic::{AtomicU64,Ordering};
        if disabled_classes()!=0 {return;}
        let mut checked=0;
        for sf in [false,true] {for cond in 0..14 {for mode in 0..3 {for kind in [StubKind::Plain,StubKind::Patchable] {for lazy in [false,true] {
            let cmp=if sf {0xf100081f}else {0x7100081f};
            // Root chooses nonleaf, then MOV X0,X18 selects an arbitrary input
            // for the actual inlined leaf. Leaf changes X0 AFTER CMP: repair
            // MUST use the saved operand, not its current value.
            let store=if mode==2 {0xf90007f0}else {0xf9000270};
            let words=[0xa9bf7bf3,0x910003e9,cmp,0x540000a0|cond,0xaa1203e0,0x97fffffb,0x71000e7f,0x14000003,0x91004400,store,0xa8c17bf3,0xd65f03c0];
            let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();let cont=PC+24;let ret=PC+48;
            let values=[0u64,1,2,3,u64::MAX,1<<63,i64::MAX as u64,0x80000000,0x7fffffff,0xffffffff,0xab00000000000001];
            let leaf=|value:u64| {let mut p=cpu();p.x[0]=value;interp::exec(&mut p,cmp).unwrap();p.cond(cond)};
            let Some(root)=values.iter().copied().find(|&v|!leaf(v))else {continue;};
            let mut u=Unit::new(kind,false);u.recleaf=true;u.recleaflazy=lazy;u.funra=true;u.funflags=true;u.funstack=true;u.loopra=true;
            let l=u.a.create_label();u.labels.insert(PC,l);emit_block(&mut u,PC,&read);assert_eq!(u.recleaf_lazy_emissions>0,lazy);assert_eq!(u.insns_fallback,0);let(host,_)=assemble(u,0);
            for value in values.into_iter().filter(|&v|leaf(v)) {for nv in 0..16 {
                let epoch=AtomicU64::new(1);let mut data=0u64;let mut stack=[0xfeed123456789abcu64;64];let initial=stack;
                let raw_target=if nv&1!=0 {0xab00000000000000|cont}else {ret+4};
                let mut c=cpu();for n in 0..32 {c.x[n]=0xfedcba9800000000+n as u64;c.v[n]=[0x7ff0000000000042+n as u64,0xfeed000000000000+n as u64];}
                c.x[0]=root;c.x[18]=value;c.x[16]=if mode==2 {raw_target}else {2};
                c.x[19]=if mode==1 {&epoch as *const _ as u64}else {&mut data as *mut _ as u64};
                c.x[30]=ret;c.x[31]=(stack.as_mut_ptr()as u64+512)|if nv&1!=0 {0xab00000000000000}else {0};
                c.set_nzcv(nv);c.fpcr=(nv as u64&3)<<22|if nv&4!=0 {1<<24}else {0};c.fpsr=0x9f;c.epoch_ptr=&epoch as *const _ as u64;
                let mut r=cpu();r.x=c.x;r.v=c.v;r.set_nzcv(nv);r.fpcr=c.fpcr;r.fpsr=c.fpsr;
                let stop=if mode==1 {cont}else if mode==2 {raw_target}else {ret};let mut steps=0;
                while r.pc()!=stop {let p=r.pc();interp::exec(&mut r,read(p).unwrap()).unwrap();if r.pc()==p {r.set_pc(p+4);}steps+=1;assert!(steps<64);}
                let expected_stack=stack;let expected_data=data;stack=initial;std::hint::black_box(&mut stack);data=0;std::hint::black_box(&mut data);epoch.store(1,Ordering::SeqCst);
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert!(c.aot_loop_regions>=if lazy {0x300000}else {0x100000},"actual snapshot path");
                if mode==1 {assert_eq!(epoch.load(Ordering::SeqCst),2);assert_eq!(c.entry_epoch,1);}
                assert_eq!(c.x,r.x,"sf={sf} cond={cond} mode={mode} lazy={lazy} operand={value:x}");assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv(),"cold flags operand={value:x} AFTER source ADD");assert_eq!(c.fpcr,r.fpcr);assert_eq!(c.fpsr,r.fpsr);assert_eq!(c.pc(),stop);assert_eq!(c.exit_reason,EXIT_CONTINUE);assert_eq!(c.exit_data,0);assert_eq!(stack,expected_stack);assert_eq!(data,expected_data);checked+=1;
            }}
        }}}}}eprintln!("recleaflazy full-state/FPCR/FPSR/stack observer cases={checked}");
    }

    #[test]
    fn funra_recleaflazy_multiple_sites_eager_return_and_ras_full() {
        if disabled_classes()!=0 {return;}
        for deep in [false,true] {for kind in [StubKind::Plain,StubKind::Patchable] {for lazy in [false,true] {for tagged in [false,true] {
            let words=if deep {vec![0xa9bf7bf3,0x910003e9,0x7100001f,0x540000a0,0xd1000400,0x97fffffb,0x71000e7f,0x91000400,0xa8c17bf3,0xd65f03c0]}
                else {vec![0xa9bf7bf3,0x910003e9,0x7100001f,0x540000c0,0xd1000400,0x97fffffb,0x97fffffa,0x91000400,0x14000001,0xa8c17bf3,0xd65f03c0]};
            let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();let ret=PC+words.len()as u64*4;
            let mut u=Unit::new(kind,false);u.recleaf=true;u.recleaflazy=lazy;u.funra=true;u.funflags=true;u.funstack=true;u.loopra=true;
            let l=u.a.create_label();u.labels.insert(PC,l);emit_block(&mut u,PC,&read);assert_eq!(u.recleaf_lazy_emissions,if lazy {1}else {0});assert_eq!(u.insns_fallback,0);let(host,_)=assemble(u,0);
            // The first of two callsites is lazy (next is exact self call);
            // the second MUST be eager because ADD;RET never kills flags.
            let mut resume=Unit::new(kind,false);resume.funra=false;let l=resume.a.create_label();resume.labels.insert(PC+24,l);emit_block(&mut resume,PC+24,&read);let(continuation,_)=assemble(resume,6);
            let mut stack:Vec<u64>=(0..4096).map(|n|0xfeed000000000000+n).collect();let initial=stack.clone();let mut c=cpu();for n in 0..32 {c.x[n]=0xfedcba9800000000+n as u64;c.v[n]=[n as u64,0x7ff0000000000042+n as u64];}
            c.x[0]=if deep {300}else {1};c.x[30]=ret;c.x[31]=(stack.as_mut_ptr()as u64+stack.len()as u64*8)|if tagged {0xab00000000000000}else {0};c.set_nzcv(13);c.fpcr=0x04c00000;c.fpsr=0x9f;
            let mut r=cpu();r.x=c.x;r.v=c.v;r.set_nzcv(13);r.fpcr=c.fpcr;r.fpsr=c.fpsr;let mut steps=0;
            while r.pc()!=ret {let p=r.pc();interp::exec(&mut r,read(p).unwrap()).unwrap();if r.pc()==p {r.set_pc(p+4);}steps+=1;assert!(steps<10000);}
            let expected=stack.clone();stack.copy_from_slice(&initial);unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
            let mut exits=0;while c.pc()!=ret {assert!(deep);assert_eq!(c.pc(),PC+24);unsafe {crate::jit::attesor_jit_enter(&mut *c,continuation);}exits+=1;assert!(exits<400);}
            if deep {assert!(exits>0);assert!(c.aot_loop_regions>=0x100&&c.aot_loop_regions<0x100000,"full RAS refuses inline/snapshot");}
            else {assert!(c.aot_loop_regions>=if lazy {0x400000}else {0x200000});}
            assert_eq!(c.x,r.x);assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());assert_eq!(c.fpcr,r.fpcr);assert_eq!(c.fpsr,r.fpsr);assert_eq!(c.exit_reason,EXIT_CONTINUE);assert_eq!(stack,expected);
        }}}}
    }

    #[test]
    fn funra_stack_proof_rejects_unknown_writers_and_complete_span() {
        let mut u=Unit::new(StubKind::Plain,false);u.funstack=true;u.function_entry=Some(PC);
        let mut r=LoopRa{pc:PC,fast:u.a.create_label(),exit:u.a.create_label(),reg:[0xff;NLOOPREGS],dirty:u32::MAX,vreg:[0xff;NVREGS],xreg:[0xff;NVREGS],flags:0xff};
        r.reg[0]=31;u.region=Some(r);u.region_labels.insert(PC,r.fast);
        for (extra,accept) in [(0x910043fd,true), // ADD FP,SP,#16, not a SP writer
            (0x910043ff,true), // ADD SP,SP,#16, affine
            (0x110003ff,false), // WSP zero extension cannot preserve raw tags
            (0x9100001f,false), // ADD SP,X0,#0, arbitrary replacement
            (0x8b2063ff,false), // extended ADD may write SP
            (0xf9000001,false), // non-SP memory could invoke a helper
            (0xd51b4400,false), // system/observer barrier
            (0xa94003e0,false), // duplicate load destinations
            (0xad0007e0,false)] { // SIMD pair not in this GPR proof
            let words=[0xa9be4ff4,extra,0xd65f03c0];
            let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();
            assert_eq!(fun_stack::plan(&u,PC,&read).is_some(),accept,"word={extra:x}");
        }
    }

    #[test]
    fn funra_stack_actual_range_guards_near_bit56_no_data_deref() {
        let mask=0x00ff_ffff_ffff_ffffu64;let mut cases=0;
        for kind in [StubKind::Plain,StubKind::Patchable] {for (lo,hi) in [(-32,0),(0,32),(-512,511)] {
            let mut u=Unit::new(kind,false);let mut entry=u.a.create_label();u.a.set_label(&mut entry).unwrap();u.labels.insert(PC,entry);
            let mut slow=u.a.create_label();u.a.mov(rcx,qword_ptr(r15+off::X+31*8)).unwrap();
            fun_stack::guard(&mut u,rcx,lo,hi,slow);
            u.a.mov(qword_ptr(r15+off::AOT_LOOP_REGIONS),1i32).unwrap();u.a.mov(qword_ptr(r15+off::EXIT_REASON),EXIT_SVC as i32).unwrap();u.a.jmp(u.exit_label).unwrap();
            u.a.set_label(&mut slow).unwrap();u.a.mov(qword_ptr(r15+off::AOT_LOOP_REGIONS),2i32).unwrap();u.a.mov(qword_ptr(r15+off::EXIT_REASON),EXIT_SVC as i32).unwrap();u.a.jmp(u.exit_label).unwrap();let(host,_)=assemble(u,0);
            for raw in [0,1,31,32,511,512,mask-hi as u64,mask-hi as u64+1,mask,u64::MAX,0xab00000000008000] {
                let mut c=cpu();c.x[31]=raw;c.set_nzcv(15);let x=c.x;
                let expected=raw as i128+lo as i128>=0&&raw as u128+hi as u128<=mask as u128;
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}assert_eq!(c.aot_loop_regions,if expected {1}else {2});assert_eq!(c.x,x);assert_eq!(c.nzcv(),15);cases+=1;
            }
        }}eprintln!("funstack actual range-guard cases={cases}; no guest data dereferenced");
    }

    #[test]
    fn funra_stack_deep_ras_overflow_and_epoch_canonical_full_state() {
        use std::sync::atomic::{AtomicU64,Ordering};
        if disabled_classes()!=0 {return;}
        for epoch_case in [false,true] {for kind in [StubKind::Plain,StubKind::Patchable] {for tagged in [false,true] {
            // Linear recursion exercises >256 native-frame limit, NOT Fibonacci
            // replacement. The second source changes epoch inside an open call.
            let words=if epoch_case {vec![0xa9bf7bf3,0x910003e9,0xb40000a0,0xd1000400,0xf9000272,0x97fffffb,0x91000400,0xa8c17bf3,0xd65f03c0]}
                else {vec![0xa9bf7bf3,0x910003e9,0xb4000080,0xd1000400,0x97fffffc,0x91000400,0xa8c17bf3,0xd65f03c0]};
            let cont=PC+if epoch_case {24}else {20};let return_pc=PC+words.len()as u64*4;
            let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.funra=true;u.funstack=true;u.loopra=true;u.funflags=true;
            let l=u.a.create_label();u.labels.insert(PC,l);emit_block(&mut u,PC,&read);
            assert!(u.funstack_emissions>=2,"epoch={epoch_case} emissions={} regions={} recursive={}",u.funstack_emissions,u.region_exits.len(),functions::recursive(PC,&read).is_some());assert_eq!(u.insns_fallback,0);let(host,_)=assemble(u,0);
            // Dispatcher reentry after RAS saturation is the ORIGINAL native
            // continuation, not bypassing the marker/return/epoch contract.
            let mut continuation=Unit::new(kind,false);continuation.funra=false;continuation.funstack=false;
            let l=continuation.a.create_label();continuation.labels.insert(cont,l);emit_block(&mut continuation,cont,&read);
            let(resume,_)=assemble(continuation,((cont-PC)/4)as usize);
            let epoch=AtomicU64::new(1);let mut stack:Vec<u64>=(0..4096).map(|n|0xfeed000000000000+n).collect();let initial=stack.clone();
            let mut c=cpu();for n in 0..32 {c.x[n]=0xfeed000000000000+n as u64;c.v[n]=[n as u64,0x12345678+n as u64];}
            c.x[0]=if epoch_case {1}else {300};c.x[18]=2;c.x[19]=&epoch as *const _ as u64;c.x[30]=return_pc;
            c.x[31]=(stack.as_mut_ptr()as u64+stack.len()as u64*8)|if tagged {0xab00000000000000}else {0};c.set_nzcv(13);c.epoch_ptr=&epoch as *const _ as u64;
            let mut reference=cpu();reference.x=c.x;reference.v=c.v;reference.set_nzcv(c.nzcv());
            let mut steps=0;
            loop {
                let p=reference.pc();interp::exec(&mut reference,read(p).unwrap()).unwrap();if reference.pc()==p {reference.set_pc(p+4);}
                steps+=1;assert!(steps<10000);
                if reference.pc()==if epoch_case {cont}else {return_pc} {break;}
            }
            let expected_stack=stack.clone();stack.copy_from_slice(&initial);epoch.store(1,Ordering::SeqCst);
            unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
            if epoch_case {assert_eq!(epoch.load(Ordering::SeqCst),2);assert_eq!(c.entry_epoch,1);assert_eq!(c.pc(),cont);}
            else {let mut exits=0;while c.pc()!=return_pc {assert_eq!(c.pc(),cont);unsafe {crate::jit::attesor_jit_enter(&mut *c,resume);}exits+=1;assert!(exits<400);}assert!(exits>0,"deep case must hit the existing RAS boundary");}
            assert_eq!(c.x,reference.x,"epoch={epoch_case} tagged={tagged}");assert_eq!(c.v,reference.v);assert_eq!(c.nzcv(),reference.nzcv());assert_eq!(c.pc(),reference.pc());assert_eq!(c.exit_reason,EXIT_CONTINUE);assert_eq!(stack,expected_stack,"every guest stack byte");
        }}}
    }

    #[test]
    fn funra_packed_cmp_flags_all_conditions_and_canonical_roundtrip() {
        for cond in 0..16 {
            let mut u=Unit::new(StubKind::Plain,false);
            let mut entry=u.a.create_label();u.a.set_label(&mut entry).unwrap();u.labels.insert(PC,entry);
            let region=LoopRa{pc:PC,fast:u.a.create_label(),exit:u.a.create_label(),reg:[0xff;NLOOPREGS],
                dirty:0,vreg:[0xff;NVREGS],xreg:[0xff;NVREGS],flags:7};
            u.region=Some(region);u.region_exits.push(region);region.reload(&mut u.a);
            let mut e=E{u:&mut u,pc:PC,fuse_in:None,scalar_inputs:None};
            e.cond_to_al(cond);a!(e,movzx(eax,al));e.stx(0,rax,false);e.st_pc_and_exit(PC+4);
            let(host,_)=assemble(u,0);
            for nzcv in 0..16 {
                let mut c=cpu();c.set_nzcv(nzcv);let expected=c.cond(cond)as u64;
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x[0],expected,"NZCV={nzcv:x} cond={cond}");assert_eq!(c.nzcv(),nzcv);
            }
        }
    }

    #[test]
    fn funra_recursive_gpr_state_returns_tags_and_marker_boundary() {
        if disabled_classes()!=0 {return;}
        // Ordinary clang-shaped recursive GPR code. It executes the original
        // guest prologue, arithmetic, recursive BL and epilogue unchanged.
        let original=[0xa9be4ff4,0xa9017bfd,0x910043fd,0x7100081f,0x5400006a,
            0x52800013,0x1400000b,0x52800013,0xaa0003f4,0x51000680,0x97fffff6,
            0xaa0003e8,0x51000a80,0x0b130113,0x71000e9f,0xaa0003f4,0x54ffff28,
            0x0b130000,0xa9417bfd,0xa8c24ff4,0xd65f03c0];
        for lazy in [false,true] {
        for leaf_fast in [false,true] {
        for movfast in [false,true] {
        for edgefast in [false,true] {
        for stackfast in [false,true] {
        for kind in [StubKind::Plain,StubKind::Patchable] {
        for packed in [false,true] {
        for outside_callret in [false,true] {
        for tamper in [false,true] {
            let mut words=original.to_vec();
            if tamper {words.insert(words.len()-1,0x1000005e);} // ADR LR, outside
            let return_pc=PC+words.len()as u64*4;
            let read=|p:u64|p.checked_sub(PC).and_then(|n|words.get((n/4)as usize)).copied();
            assert!(functions::recursive(PC,&read).is_some());
            let mut u=Unit::new(kind,false);u.recleaflazy=lazy;u.recleaf=leaf_fast;u.movflags=movfast;u.funedge=edgefast;u.funstack=stackfast;u.funra=true;u.funflags=packed;u.loopra=true;u.callret=outside_callret;
            let entry=u.a.create_label();u.labels.insert(PC,entry);
            emit_block(&mut u,PC,&read);
            assert_eq!(u.funstack_emissions>0,stackfast);
            assert_eq!(u.funedge_emissions>0,edgefast&&packed);
            assert_eq!(u.movflags_emissions>0,movfast);
            assert_eq!(u.recleaf_emissions>0,leaf_fast);
            assert_eq!(u.recleaf_lazy_emissions>0,lazy&&leaf_fast&&packed);
            assert_eq!(u.insns_fallback,0);
            assert_eq!(u.region_exits.len(),1);assert_eq!(u.function_entry,None);
            assert_eq!(u.region_exits[0].packed_flags().is_some(),packed);
            assert_eq!(u.extra_guest_range,Some((PC,return_pc)));
            let(host,_)=assemble(u,0);
            for n in [0,1,2,5,10,16,0x80000000,0xffffffff] {
                let mut stack:Vec<u64>=(0..2048).map(|i|0xfeed000000000000u64+i).collect();
                let initial_stack=stack.clone();
                let top=stack.as_mut_ptr()as u64+stack.len()as u64*8;
                let mut c=cpu();
                for i in 0..32 {c.x[i]=0x1234567890000000+i as u64;c.v[i]=[i as u64,0xfeed+i as u64];}
                c.x[0]=n;c.x[30]=return_pc;c.x[31]=top|if n%2!=0 {0xab00000000000000}else {0};
                c.set_nzcv(15);
                let mut r=cpu();r.x=c.x;r.v=c.v;r.set_nzcv(c.nzcv());
                let mut steps=0;
                while r.pc()!=return_pc {
                    let p=r.pc();let w=read(p).expect("reference outside function");
                    interp::exec(&mut r,w).unwrap();if r.pc()==p {r.set_pc(p+4);}
                    steps+=1;assert!(steps<100000);
                }
                let expected_stack=stack.clone();stack.copy_from_slice(&initial_stack);
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                if leaf_fast&&n>=2&&n<0x80000000 {assert!(c.aot_loop_regions>=0x100000,"actual leaf inlining must execute");}
                assert_eq!(c.pc(),return_pc,"n={n} tamper={tamper}");
                assert_eq!(c.x,r.x,"n={n} tamper={tamper} caller_callret={outside_callret}");
                assert_eq!(stack,expected_stack,"full guest stack bytes stackfast={stackfast}");
                assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());
                assert_eq!(c.exit_reason,EXIT_CONTINUE);
            }
        }}}}}}}}}
    }

    #[test]
    fn vchain_observation_barriers_and_scalar_zero_width() {
        if !has_avx() || disabled_classes()!=0 {return;}
        for reject in 0..7 {
            let mut u=Unit::new(StubKind::Plain,false);u.vchain=reject!=1;
            let mut vreg=[0xff;NVREGS];vreg[..4].copy_from_slice(&[0,1,2,3]);
            if reject==6 {vreg[0]=0xff;}
            u.region=Some(LoopRa{pc:PC,fast:u.a.create_label(),exit:u.a.create_label(),
                reg:[0xff;NLOOPREGS],dirty:0,vreg,xreg:[0xff;NVREGS],flags:0xff});
            u.look=vec![0x1e622863]; // FADD D3,D3,D2
            if reject==2 {let l=u.a.create_label();u.labels.insert(PC+4,l);}
            if reject==3 {u.look[0]^=1<<22;} // mixed S/D
            if reject==4 {u.look[0]=0x1e622862;} // both writes target D2
            if reject==5 {u.region=None;}
            let mut e=E{u:&mut u,pc:PC,fuse_in:None,scalar_inputs:None};
            assert_eq!(e.vchain_pair(0x1e610802),reject==0);
        }
        let mut u=Unit::new(StubKind::Plain,false);u.vchain=true;
        let mut e=E{u:&mut u,pc:PC,fuse_in:None,scalar_inputs:None};
        e.scalar_zero(2,false);assert_eq!(e.u.vzero,[4,4]);
        e.scalar_zero(2,true);assert_eq!(e.u.vzero,[0,4]);
        // A D write does not prove that bits 63:32 are zero for a later S op.
        e.scalar_zero(3,true);assert_eq!(e.u.vzero,[0,12]);
    }

    #[test]
    fn vchain_separate_rounding_nan_alias_width_and_high_bank() {
        if !has_avx() || disabled_classes()!=0 {return;}
        use iced_x86::{Mnemonic,Register};
        let mut checked=0u64;
        for double in [false,true] {
        let patterns:Vec<u64>=if double {vec![0,0x8000000000000000,1,0x0010000000000000,
            0x3ff0000000000000,0xbff0000000000000,0x3ff0000000000001,0x3feffffffffffffe,
            0x7fefffffffffffff,0xffefffffffffffff,0x7ff0000000000000,0xfff0000000000000,
            0x7ff8000000000042,0x7ff0000000000073,0xfff0000000000084]}
        else {vec![0,0x80000000,1,0x00800000,0x3f800000,0xbf800000,0x3f800001,0x3f7ffffe,
            0x7f7fffff,0xff7fffff,0x7f800000,0xff800000,0x7fc00042,0x7f800073,0xff800084]};
        for high in [false,true].into_iter().filter(|&h|!h||has_avx512()) {
        for known in [false,true] {
        for kind in [StubKind::Plain,StubKind::Patchable] {
        for (t,d,n,m,o) in [(2,3,0,1,3),(0,3,0,1,3),(1,0,0,1,0),(2,0,0,0,1),(2,1,0,1,0)] {
        for tmp_first in [false,true] {
            let shift=if high {24}else {0};let(t,d,n,m,o)=(t+shift,d+shift,n+shift,m+shift,o+shift);
            let ty=(double as u32)<<22;
            let ldr=|reg:u32,base:u32|(if double {0xfd400000u32}else {0xbd400000u32})|(base<<5)|reg;
            let mut words=Vec::new();
            if high {for r in 0..8 {words.push(ldr(r,2));}}
            if known {words.push(ldr(n,0));words.push(ldr(m,1));}
            words.push(0x1e200800|ty|(m<<16)|(n<<5)|t);
            let(an,am)=if tmp_first {(t,o)}else {(o,t)};
            words.push(0x1e202800|ty|(am<<16)|(an<<5)|d);
            words.push(0xf100054a); // SUBS X10,X10,#1
            words.push(0x54000001|((((-(words.len()as i32))as u32)&0x7ffff)<<5));
            let exit=PC+words.len()as u64*4;words.push(0xd4001001);
            let read=|p:u64|p.checked_sub(PC).and_then(|v|words.get((v/4)as usize)).copied();
            let mut u=Unit::new(kind,false);u.loopra=true;u.loopra2=false;u.vloop=true;u.vchain=true;u.avx512=high;
            for pc in [PC,exit] {let l=u.a.create_label();u.labels.insert(pc,l);}
            emit_block(&mut u,PC,&read);emit_block(&mut u,exit,&read);
            let add=if double {Mnemonic::Vaddsd}else {Mnemonic::Vaddss};
            assert!(u.a.instructions().iter().any(|i|i.mnemonic()==add && i.op0_register()==Register::XMM1));
            if !jit_off("tbimask") {
                assert_eq!(u.a.instructions().iter().filter(|i|i.mnemonic()==Mnemonic::Mov && i.op0_register()==Register::R11).count(),1,
                    "the scalar pair must not reload its unchanged TBI mask per backedge");
            }
            let(host,_)=assemble(u,0);
            for (ia,&a) in patterns.iter().enumerate() {
            for (ib,&b) in patterns.iter().enumerate() {
            for (ic,&c0) in patterns.iter().enumerate() {
                let mut c=cpu();
                for i in 0..32 {c.v[i]=[0xfeedbeef00000000+i as u64,0x123456789abcdef0+i as u64];}
                for (reg,value) in [(n,a),(m,b),(o,c0)] {
                    c.v[reg as usize][0]=if double {value}else {0xfeedbeef00000000|value};
                }
                let padding=if double {1f64.to_bits()}else {1f32.to_bits()as u64};
                c.x[0]=&a as *const _ as u64;c.x[1]=&b as *const _ as u64;c.x[2]=&padding as *const _ as u64;
                if ic%2!=0 {for reg in 0..3 {c.x[reg]|=0xab00000000000000;}}
                c.x[10]=1+((ia+ib+ic)%2)as u64;
                let mut reference=cpu();reference.x=c.x;reference.v=c.v;
                interp::sys_sync_host_fp_mode(0);
                for _ in 0..128 {
                    let index=((reference.pc()-PC)/4)as usize;
                    if matches!(interp::exec(&mut reference,words[index]),Err(interp::Exit::Svc(_))) {break;}
                    if reference.pc()==PC+index as u64*4 {reference.set_pc(reference.pc()+4);}
                }
                interp::sys_sync_host_fp_mode(0);
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.v,reference.v,"double={double} high={high} known={known} regs={t},{d},{n},{m},{o} tmp_first={tmp_first} inputs={a:x},{b:x},{c0:x}");
                assert_eq!(c.x,reference.x);assert_eq!(c.nzcv(),reference.nzcv());
                checked+=1;
            }}}
        }}}}}}
        eprintln!("vchain directed differential cases={checked}");
        interp::sys_sync_host_fp_mode(0);
    }

    #[test]
    fn vloop_mapping_capacity_and_observer_barriers() {
        if disabled_classes()!=0 {return;}
        let mut words=vec![0xfd400009]; // high-bank scalar load after allocation
        for n in 1..24 {words.push(0x1e602800 | (n<<16));} // FADD D0,D0,Dn
        words.push(0xf1000421); // SUBS X1,X1,#1
        words.push(0x54000001 | (((-(words.len()as i32))as u32 & 0x7ffff)<<5));
        let read=|p:u64|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied();
        let mut u=Unit::new(StubKind::Patchable,false);u.vloop=true;
        let l=u.a.create_label();u.labels.insert(PC,l);
        u.avx512=true;
        let layout=vloop_layout(&u,PC,&read);
        assert_eq!(layout,(0..24).map(|x|x as u8).collect::<Vec<_>>().as_slice());
        assert!(high_xmm(VREGS[layout.iter().position(|&x|x==9).unwrap()]));
        u.avx512=false;assert_eq!(vloop_layout(&u,PC,&read),[0xff;NVREGS]);
        u.avx512=true;u.commpage_checks=true;assert_eq!(vloop_layout(&u,PC,&read),[0xff;NVREGS]);
        u.commpage_checks=false;u.no_internal_jumps=true;assert_eq!(vloop_layout(&u,PC,&read),[0xff;NVREGS]);
        u.no_internal_jumps=false;
        let join=u.a.create_label();u.labels.insert(PC+8,join);
        assert_eq!(vloop_layout(&u,PC,&read),[0xff;NVREGS]);
        u.labels.remove(&(PC+8));
        let original=words[1];words[1]=0xd51b4400; // MSR FPCR must reject the contract
        assert_eq!(vloop_layout(&u,PC,&|p|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied()),[0xff;NVREGS]);
        words[1]=original;u.vloop=false;
        assert_eq!(vloop_layout(&u,PC,&|p|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied()),[0xff;NVREGS]);
    }

    #[test]
    fn vloop_nested_scalar_contract_survives_gpr_outer_allocation() {
        // Outer count X4; reset inner X3 each iteration. The GPR-only outer
        // clone must not swallow the inner loop's resident V0/V1 mapping.
        let words = [0xd2800083,0x14000001,0xfd400001,0x1e612800,
            0xf1000463,0x54ffffa1,0xf1000484,0x54ffff21,0xfd000040,0xd4001001];
        if disabled_classes()!=0 {return;}
        for kind in [StubKind::Plain,StubKind::Patchable] {
        for resident in [false,true] {
            let mut u=Unit::new(kind,false);u.loopra=true;u.loopra2=true;u.vloop=resident;
            for offset in [0,8,24,32] {let label=u.a.create_label();u.labels.insert(PC+offset,label);}
            let read=|p:u64|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied();
            emit_block(&mut u,PC,&read);
            assert_eq!(u.region_exits.len(),if resident {0}else {1});
            for offset in [8,24,32] {emit_block(&mut u,PC+offset,&read);}
            assert!(u.region_exits.iter().any(|r|r.pc==PC+8 && r.vslot(0).is_some()==resident));
            let (host,_)=assemble(u,0);
            let source=1.25f64;let mut out=0u64;let mut c=cpu();let mut r=cpu();
            for s in [&mut c,&mut r] {
                for i in 0..32 {s.v[i]=[i as u64*37,0xfeed+i as u64];}
                s.v[0]=[0.5f64.to_bits(),0x1234];s.x[0]=&source as *const _ as u64;
                s.x[2]=&mut out as *mut _ as u64;s.x[4]=3;
            }
            interp::sys_sync_host_fp_mode(0);
            for _ in 0..256 {
                let index=((r.pc()-PC)/4)as usize;
                if matches!(interp::exec(&mut r,words[index]),Err(interp::Exit::Svc(_))) {break;}
                if r.pc()==PC+index as u64*4 {r.set_pc(r.pc()+4);}
            }
            let expected=out;out=0;
            unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
            assert_eq!(out,expected);assert_eq!(out,15.5f64.to_bits());
            assert_eq!(c.x,r.x);assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());
        }}
    }

    #[test]
    fn vloop_fp_recurrence_load_store_nan_public_exit_and_fpcr() {
        // LDR D1,[X0]; LDR D2,[X1]; FMUL D1,D1,D2; FADD D0,D0,D1;
        // ADD X0,#8; ADD X1,#8; SUBS X3,#1; B.NE header; STR D0,[X2]; SVC.
        let words = [0xfd400001,0xfd400022,0x1e620821,0x1e612800,
            0x91002000,0x91002021,0xf1000463,0x54ffff21,0xfd000040,0xd4001001];
        let on = std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|x| x=="vloop")
            && !jit_off("vloop") && disabled_classes()==0;
        for high in [false,true].into_iter().filter(|&h| !h || has_avx512()) {
        for kind in [StubKind::Plain,StubKind::Patchable] {
        for exceptional in [false,true] {
            let mut u=Unit::new(kind,false);u.loopra=true;u.loopra2=false;u.avx512=high;
            let label=u.a.create_label();u.labels.insert(PC,label);
            let exit=u.a.create_label();u.labels.insert(PC+32,exit);
            emit_block(&mut u,PC,&|p|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied());
            assert_eq!(u.region_exits[0].vslot(0).is_some(),on);
            emit_block(&mut u,PC+32,&|p|p.checked_sub(PC).and_then(|o|words.get((o/4)as usize)).copied());
            let (host,_)=assemble(u,0);
            let a=[1.0,if exceptional {f64::from_bits(0x7ff0000000000042)} else {-2.0},3.0,-0.0];
            let b=[2.0,4.0,-5.0,0.0];let mut out=0u64;
            let mut c=cpu();let mut r=cpu();
            for s in [&mut c,&mut r] {
                for i in 0..32 {s.v[i]=[i as u64*7919,0x12340000+i as u64];}
                s.v[0]=[0.5f64.to_bits(),0xfeed];
                s.x[0]=a.as_ptr()as u64;s.x[1]=b.as_ptr()as u64;s.x[2]=&mut out as *mut _ as u64;s.x[3]=4;
            }
            interp::sys_sync_host_fp_mode(0);
            for _ in 0..100 {
                let index=((r.pc()-PC)/4)as usize;
                if matches!(interp::exec(&mut r,words[index]),Err(interp::Exit::Svc(_))) {break;}
                if r.pc()==PC+index as u64*4 {r.set_pc(r.pc()+4);}
            }
            let expected=out;out=0xdead;
            interp::sys_sync_host_fp_mode(0);
            unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
            assert_eq!(out,expected,"exceptional={exceptional}");
            assert_eq!(c.x,r.x);assert_eq!(c.v,r.v);assert_eq!(c.nzcv(),r.nzcv());
            // A non-native FPCR must exit before modifying even the cached V bank.
            let mut guarded=cpu();guarded.fpcr=1<<25;guarded.x[3]=1;
            guarded.v[0]=[0x1234,0x5678];let before=guarded.v;
            unsafe {crate::jit::attesor_jit_enter(&mut *guarded,host);}
            assert_eq!(guarded.exit_reason,EXIT_INTERP);assert_eq!(guarded.v,before);
        }}}
        interp::sys_sync_host_fp_mode(0);
    }

    #[test]
    fn g4_isel2_isa_alias_width_count_flags_and_regions() {
        use iced_x86::Mnemonic;
        let has=|k|std::env::var(k).unwrap_or_default().split(',').any(|t|t=="isel2");
        let on=has("ATTESOR_JIT_ON")&&!has("ATTESOR_JIT_OFF");
        let selected=crate::jit::isel2::selected();
        assert_eq!(selected.bmi2,on&&std::is_x86_feature_detected!("bmi2"));
        assert_eq!(selected.lzcnt,on&&std::is_x86_feature_detected!("lzcnt"));
        assert_eq!(crate::jit::isel2::Selection{bmi2:true,lzcnt:true}.mask(),3);
        assert_eq!(crate::jit::isel2_mask(),selected.mask());
        eprintln!("isel2 selected: bmi2={} lzcnt={}",selected.bmi2,selected.lzcnt);
        for sf in [false,true] {
        for region in [false,true] {
        for kind in [StubKind::Plain,StubKind::Patchable] {
        for (opcode,mnemonic) in [(8,Mnemonic::Shlx),(9,Mnemonic::Shrx),(10,Mnemonic::Sarx),(4,Mnemonic::Lzcnt),(5,Mnemonic::Lzcnt)] {
        for (rd,rn,rm) in [(0,1,2),(1,1,2),(2,1,2),(0,1,1),(31,1,2),(0,31,2),(0,1,31)] {
            let shift=opcode>=8;
            let w=if shift {0x1ac00000|(rm<<16)|(opcode<<10)|(rn<<5)|rd}
                else {0x5ac00000|(opcode<<10)|(rn<<5)|rd} | ((sf as u32)<<31);
            let words=if region {vec![w,0xd100079c,0xb5ffffdc,0xd4001001]} else {vec![w,0xd4001001]};
            let entries=if region {vec![0,3]} else {vec![0]};
            let mut u=Unit::new(kind,false);u.loopra=region;u.loopra2=false;u.regioncache=false;
            for &i in &entries {let l=u.a.create_label();u.labels.insert(PC+i*4,l);}
            let read=|p:u64|p.checked_sub(PC).and_then(|o|words.get((o/4) as usize)).copied();
            emit_block(&mut u,PC,&read);
            if region {assert!(u.region.is_some());}
            let active=if shift {selected.bmi2}else{selected.lzcnt};
            assert_eq!(u.a.instructions().iter().any(|i|i.mnemonic()==mnemonic),active,"opcode={opcode} sf={sf} region={region}");
            for &i in entries.iter().skip(1) {emit_block(&mut u,PC+i*4,&read);}
            let (host,_)=assemble(u,0);
            for value in [0,1,u64::MAX,0x80000000,0x8000000000000000,0xdeadbeef01234567] {
            for count in [0,31,32,63,64,u64::MAX] {
                let mut c=cpu();c.x[0]=99;c.x[1]=value;c.x[2]=count;c.x[28]=2;c.set_nzcv(11);
                let mut r=crate::jit::cpu_clone(&c);
                for _ in 0..12 {let w=read(r.pc()).unwrap();if let Err(attesor_core::interp::Exit::Svc(_))=attesor_core::interp::exec(&mut r,w){break;}}
                unsafe{crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,r.x,"op={opcode} rd={rd} rn={rn} rm={rm} value={value:x} count={count}");
                assert_eq!(c.nzcv(),r.nzcv());assert_eq!(c.pc(),r.pc());assert_eq!(c.exit_reason,EXIT_SVC);
            }}
        }}}}}
    }

    #[test]
    fn g4_flagreg_public_exit_matches_interpreter() {
        let has = |k| std::env::var(k).unwrap_or_default().split(',').any(|t| t == "flagreg");
        let enabled = has("ATTESOR_JIT_ON") && !has("ATTESOR_JIT_OFF");
        assert_eq!(crate::jit::flagreg::enabled(), enabled);
        for kind in [StubKind::Plain, StubKind::Patchable] {
        for sf in [false,true] {
        for shift in [0,1] {
        for rn in [0,2,17] {
        for (branch,trips) in [(0xb5ffffc2,3), (0x35ffffc2,3), (0x370fffc2,2), (0x360fffc2,3)] {
        for value in [0,1,u64::MAX,0x80000000,0x8000000000000000,0xdeadbeef00000001] {
            let cmp = 0x7100001f | ((sf as u32)<<31) | (shift<<22) | (1<<10) | (rn<<5);
            let words = [0xd1000442,cmp,branch,0xd4001001]; // SUB X2,#1; CMP; CB/TB backedge
            let u=unit(&words,&[0],kind,false,false);
            assert_eq!(u.loop_exit_compare.is_some(),enabled,"must exercise lazy plan");
            let (host,_)=assemble(u,0);
            let mut c=cpu();c.x[0]=value;c.x[17]=value;c.x[2]=trips;c.set_nzcv(15);
            let mut r=crate::jit::cpu_clone(&c);
            let read=|p:u64| words[((p-PC)/4) as usize];
            let mut steps=0;
            while r.pc()!=PC+12 { let w=read(r.pc());attesor_core::interp::exec(&mut r,w).unwrap();steps+=1;assert!(steps<30); }
            unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
            assert_eq!(c.x,r.x);assert_eq!(c.nzcv(),r.nzcv(),"sf={sf} shift={shift} rn={rn} value={value:x}");
            assert_eq!(c.pc(),PC+12);assert_eq!(c.exit_reason,EXIT_CONTINUE);
        }}}}}}
        // Verification/public labels prevent the deferred proof from being used.
        let words=[0xd1000442,0xf100041f,0xb5ffffc2,0xd4001001];
        assert!(unit(&words,&[0],StubKind::Plain,true,false).loop_exit_compare.is_none());
        assert!(unit(&words,&[0,1],StubKind::Plain,false,false).loop_exit_compare.is_none());
    }

    #[test]
    fn multi_block_private_entries_helpers_and_public_joins() {
        // A diamond with an early exit in either arm, a helper in one arm,
        // and FP in the other. All public entries remain canonical, including
        // AOT entries in the middle of the private clone.
        let words = [
            0x91000400, // add x0,x0,#1
            0x360000c2, // tbz w2,#0,+24 -> other arm (7)
            0x8b000021, // add x1,x1,x0
            0x1ac04021, // crc32b w1,w1,w0 (helper)
            0xb4000123, // cbz x3,+36 -> side svc (13)
            0x14000004, // b latch (9)
            0xd503201f, // unreachable
            0xcb000021, // sub x1,x1,x0
            0x1e612800, // fadd d0,d0,d1
            0xf1000442, // subs x2,x2,#1
            0x54fffec1, // b.ne header
            0xd4001001, // svc normal
            0xd503201f,
            0xd4001021, // svc side
        ];
        for kind in [StubKind::Plain, StubKind::Patchable] {
        for commpage in [false, true] {
        for entry in [0,2,7,9] {
        for trips in [1,2,7] {
        for side in [0,1] {
            let entries = [0,2,5,7,9,11,13];
            let mut u = Unit::new(kind, commpage); u.loopra = true; u.loopra2 = true;
            for &i in &entries { let l = u.a.create_label(); u.labels.insert(PC+i*4,l); }
            let read = |p: u64| p.checked_sub(PC).and_then(|o| words.get((o/4) as usize)).copied();
            for &i in &entries { emit_block(&mut u, PC+i*4, &read); }
            assert_eq!(u.labels.len(), entries.len());
            assert!(u.extra_guest_range.is_some());
            assert_eq!(u.region_exits[0].dirty, u32::MAX);
            assert!(u.region_labels.is_empty());
            let (host,_) = assemble(u, entry as usize);
            let mut c = cpu(); c.set_pc(PC+entry*4); c.x[0]=11; c.x[1]=17; c.x[2]=trips; c.x[3]=side;
            c.v[0]=[1f64.to_bits(),0]; c.v[1]=[2f64.to_bits(),0]; c.set_nzcv(11);
            let mut r = crate::jit::cpu_clone(&c);
            for _ in 0..1000 {
                let w=read(r.pc()).unwrap();
                if let Err(attesor_core::interp::Exit::Svc(_))=attesor_core::interp::exec(&mut r,w) { break; }
            }
            unsafe { crate::jit::attesor_jit_enter(&mut *c,host); }
            assert_eq!(c.x,r.x,"entry={entry} trips={trips} side={side}");
            assert_eq!(c.v,r.v); assert_eq!(c.nzcv(),r.nzcv()); assert_eq!(c.pc(),r.pc());
            assert_eq!(c.exit_reason,EXIT_SVC);
        }}}}}
    }

    #[test]
    fn multi_block_verification_and_fpcr_guard_are_canonical() {
        let words = [0x91000400,0x36000042,0xd503201f,0x1e612800,0xf1000442,0x54ffff61,0xd4001001];
        for verify in [false,true] {
            let mut u=Unit::new(StubKind::Plain,false);u.loopra2=true;u.loopra=true;u.no_internal_jumps=verify;
            let l=u.a.create_label();u.labels.insert(PC,l);
            emit_block(&mut u,PC,&|p| p.checked_sub(PC).and_then(|o| words.get((o/4) as usize)).copied());
            assert_eq!(u.extra_guest_range.is_some(),!verify);
            if verify { continue; }
            let (host,_)=assemble(u,0);
            let mut c=cpu();c.x[0]=41;c.x[2]=2;c.fpcr=1<<25;
            unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
            assert_eq!(c.exit_reason,EXIT_INTERP);
            // The header's integer write must be visible when the private FP
            // block hands off, even though it was not emitted in that block.
            assert_eq!(c.x[0],42);assert_eq!(c.x[2],2);assert_eq!(c.pc(),PC+12);
        }
    }

    #[test]
    fn q_register_offset_scale16_matches_interpreter() {
        // Minimized from loops seed 0x1234, case 89: STR Q with LSL #4.
        // Also cover loads, UXTW with stale upper bits, signed extends, both
        // scale choices, helper checks and both relocatable region tail kinds.
        for kind in [StubKind::Plain, StubKind::Patchable] {
        for commpage in [false, true] {
        for option in [2,3,6,7] {
        for scaled in [false, true] {
        for load in [false, true] {
            let w = 0x3ca0_0800 | ((load as u32) << 22) | (29 << 16)
                | (option << 13) | ((scaled as u32) << 12) | (20 << 5);
            let words = [w,0xf100_079c,0x54ff_ffc1,0xd400_1001];
            let u = unit(&words,&[0,3],kind,false,commpage);
            assert_eq!(u.region_exits.len(),1);
            assert_eq!(u.insns_fallback,0);
            assert!(u.a.instructions().iter().all(|i| i.memory_index_scale() <= 8));
            let (host,_) = assemble(u,0);
            let mut memory: Vec<u8> = (0..256).map(|n| (n ^ (n >> 4)) as u8).collect();
            let original = memory.clone();
            let mut c = cpu(); c.x[20] = (memory.as_mut_ptr() as u64 + 128) | 0xab00_0000_0000_0000;
            c.x[29] = match option {2 => 0xfeed_beef_0000_0003,6 => 0xfeed_beef_ffff_fffd,7 => (-3i64) as u64,_ => 3};
            c.x[28] = 2; c.v[0] = [0x0123_4567_89ab_cdef,0xfedc_ba98_7654_3210]; c.set_nzcv(15);
            let mut reference = cpu(); reference.x=c.x; reference.v=c.v; reference.set_nzcv(c.nzcv());
            for _ in 0..16 {
                let w=words[((reference.pc()-PC)/4) as usize];
                match attesor_core::interp::exec(&mut reference,w) {
                    Ok(())=>{},Err(attesor_core::interp::Exit::Svc(_))=>break,Err(e)=>panic!("{e:?}"),
                }
            }
            let expected = memory.clone(); memory.copy_from_slice(&original);
            unsafe { crate::jit::attesor_jit_enter(&mut *c,host); }
            assert_eq!(memory,expected,"memory option={option} scaled={scaled} load={load}");
            assert_eq!(c.x,reference.x,"GPR option={option} scaled={scaled} load={load}");
            assert_eq!(c.v,reference.v,"SIMD option={option} scaled={scaled} load={load}");
            assert_eq!(c.nzcv(),reference.nzcv()); assert_eq!(c.pc(),reference.pc());
            assert_eq!(c.exit_reason,EXIT_SVC);
        }}}}}
    }

    #[test]
    fn w_zero_and_bit_branches_ignore_dirty_upper_halves() {
        for kind in [StubKind::Plain, StubKind::Patchable] {
            // ADD X leaves a nonzero high half while its low half first wraps
            // to zero. CBZ W must take exactly one backedge, then fall through.
            // TBZ W31 likewise observes bit 31, never the dirty X sign bit.
            for words in [
                [0x9100_0400,0x34ff_ffe0,0xd400_1001],
                [0xd261_0000,0x36ff_ffe0 | (31 << 19),0xd400_1001],
            ] {
                let u=unit(&words,&[0,2],kind,false,false);
                assert_eq!(u.region_exits.len(),1);
                let (host,_) = assemble(u,0);
                let mut c=cpu(); c.x[0]=0xdead_beef_ffff_ffff; c.set_nzcv(11);
                let mut reference=cpu(); reference.x=c.x; reference.set_nzcv(c.nzcv());
                for _ in 0..8 {
                    let w=words[((reference.pc()-PC)/4) as usize];
                    match attesor_core::interp::exec(&mut reference,w) {
                        Ok(())=>{},Err(attesor_core::interp::Exit::Svc(_))=>break,Err(e)=>panic!("{e:?}"),
                    }
                }
                unsafe {crate::jit::attesor_jit_enter(&mut *c,host);}
                assert_eq!(c.x,reference.x); assert_eq!(c.nzcv(),reference.nzcv());
                assert_eq!(c.pc(),reference.pc()); assert_eq!(c.exit_reason,EXIT_SVC);
            }
        }
    }

    #[test]
    fn plain_regions_relocate_and_match_interpreter() {
        // Exercise a public successor and an out-of-unit Plain stub, plus a
        // helper barrier and the FPCR guard (which can exit before any writes).
        for words in [
            vec![0x9100_0400, 0xf100_0631, 0x54ff_ffc1, 0xd400_1001],
            vec![0xd280_00e0, 0x1ac1_4000, 0xf100_0631, 0x54ff_ffa1, 0xd400_1001], // CRC32B helper
            vec![0x9100_0400, 0x1e61_2800, 0xf100_0631, 0x54ff_ffa1, 0xd400_1001],
        ] {
            for internal_exit in [false, true] {
                let svc = words.len() - 1;
                let entries = if internal_exit { vec![0, svc] } else { vec![0] };
                let mut u = unit(&words, &entries, StubKind::Plain, false, false);
                assert_eq!(u.region_exits.len(), 1);
                u.finish();
                assert!(!u.a.instructions().iter().any(|i| i.has_lock_prefix()));
                let label = u.labels[&PC];
                let mut first_bytes = None;
                for _ in 0..2 {
                    let host = crate::jit::code_cache().alloc(65536);
                    let res = u.a.assemble_options(host, iced_x86::BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
                    if let Some(bytes) = &first_bytes { assert_eq!(bytes, &res.inner.code_buffer); }
                    else { first_bytes = Some(res.inner.code_buffer.clone()); }
                    unsafe { std::ptr::copy_nonoverlapping(res.inner.code_buffer.as_ptr(), host as *mut u8, res.inner.code_buffer.len()); }
                    for count in [1, 2, 31] {
                        let mut c = cpu(); c.x[0] = 19; c.x[17] = count;
                        c.v[0][0] = 2f64.to_bits(); c.v[1][0] = 3f64.to_bits(); c.set_nzcv(11);
                        let mut reference = cpu(); reference.x = c.x; reference.v = c.v; reference.set_nzcv(c.nzcv());
                        loop {
                            let w = words[((reference.pc() - PC) / 4) as usize];
                            match attesor_core::interp::exec(&mut reference, w) {
                                Ok(()) => {}, Err(attesor_core::interp::Exit::Svc(_)) => break,
                                Err(e) => panic!("reference: {e:?}"),
                            }
                        }
                        unsafe { crate::jit::attesor_jit_enter(&mut *c, res.label_ip(&label).unwrap()); }
                        assert_eq!(c.x, reference.x); assert_eq!(c.v, reference.v);
                        assert_eq!(c.nzcv(), reference.nzcv());
                        assert_eq!(c.pc(), reference.pc() - if internal_exit { 0 } else { 4 });
                        assert_eq!(c.exit_reason, if internal_exit { EXIT_SVC } else { EXIT_CONTINUE });
                        if words.contains(&0x1e61_2800) {
                            c.set_pc(PC); c.fpcr = 1 << 25;
                            let before = c.x;
                            unsafe { crate::jit::attesor_jit_enter(&mut *c, res.label_ip(&label).unwrap()); }
                            assert_eq!(c.x, before); assert_eq!(c.pc(), PC); assert_eq!(c.exit_reason, EXIT_INTERP);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn short_loop_uses_one_backedge_and_skips_only_dead_exit_flags() {
        // The scalar marking loop extracted from bench.c. CMP after the loop
        // models the next flag writer in the outer loop; SVC keeps flags live.
        for kind in [StubKind::Plain, StubKind::Patchable] {
            for dead in [false, true] {
                let mut words = vec![0x3824_6818, 0x8b0e_0084, 0xeb14_009f, 0x54ff_ffa3];
                if dead { words.push(0xeb07_00bf); } // cmp x5,x7
                words.push(0xd400_1001);
                let u = unit(&words, &[0], kind, false, false);
                assert_eq!(u.loop_exit_flags.is_none(), dead);
                // Only the final successor transfer remains unconditional.
                assert_eq!(u.a.instructions().iter().filter(|i| i.mnemonic() == iced_x86::Mnemonic::Jmp).count(), 1);
                assert_eq!(u.a.instructions().iter().filter(|i| i.mnemonic() == iced_x86::Mnemonic::Sets).count(), if dead { 0 } else { 1 });
                let (host, _) = assemble(u, 0);
                for count in [1, 2, 97] {
                    let mut buf = [0u8; 128];
                    let mut c = cpu(); c.x[0] = buf.as_mut_ptr() as u64;
                    c.x[4] = 0; c.x[14] = 1; c.x[20] = count; c.x[24] = 1;
                    c.x[5] = 7; c.x[7] = 9; c.set_nzcv(15);
                    let mut reference = cpu(); reference.x = c.x; reference.set_nzcv(c.nzcv());
                    // Execute through the successor flag writer before SVC.
                    loop {
                        let w = words[((reference.pc() - PC) / 4) as usize];
                        if w == 0xd400_1001 { break; }
                        attesor_core::interp::exec(&mut reference, w).unwrap();
                    }
                    let expected = buf; buf.fill(0);
                    unsafe { crate::jit::attesor_jit_enter(&mut *c, host); }
                    assert_eq!(c.x, reference.x); assert_eq!(buf, expected);
                    if !dead { assert_eq!(c.nzcv(), reference.nzcv()); }
                    // Both normal exits are outside this unit. The dispatcher
                    // executes the successor and must observe its final flags.
                    assert_eq!(c.pc(), PC + 16);
                    if dead { attesor_core::interp::exec(&mut c, words[4]).unwrap(); }
                    assert_eq!(c.nzcv(), reference.nzcv());
                }
            }
        }
    }

    extern "C" fn mutate(c: *mut Cpu, _: u32, _: u64) -> u64 {
        let c = unsafe { &mut *c };
        for i in 0..10 {
            assert_eq!(c.x[i], i as u64 + c.x[30] * (i as u64 + 2) + 1);
            c.x[i] += i as u64 + 1;
        }
        c.x[30] += 1;
        if c.x[29] == 2 { c.fpcr = 1 << 25; }
        if c.x[29] == 1 { c.exit_reason = EXIT_SVC; c.exit_data = 0x82; return 1; }
        0
    }

    #[test]
    fn ten_registers_helpers_pending_and_cold_fpcr_exit() {
        for mode in 0..3 {
            let mut words: Vec<u32> = (0..10).map(|n| 0x9100_0400 | (n << 5) | n).collect();
            words.extend([0x1e61_2800, 0, 0xf100_079c, 0x54ff_fe61, 0xd400_1001]);
            // subs x28,x28,#1; b.ne -13 insns (back to word zero)
            let u = unit(&words, &[0, 14], StubKind::Patchable, false, false);
            assert_eq!(u.region_exits.len(), 1);
            assert_eq!(u.region_exits[0].reg, [0,1,2,3,4,5,6,7,8,9]);
            let (host, _) = assemble(u, 0);
            let mut c = cpu();
            for i in 0..10 { c.x[i] = i as u64; }
            c.x[28] = 3; c.x[29] = mode;
            c.v[0][0] = 2f64.to_bits(); c.v[1][0] = 3f64.to_bits();
            c.helpers[HELPER_INTERP] = mutate as *const () as u64;
            unsafe { crate::jit::attesor_jit_enter(&mut *c, host); }
            let passes = if mode == 0 { 3 } else { 1 };
            for i in 0..10 { assert_eq!(c.x[i], i as u64 + passes * (i as u64 + 2)); }
            assert_eq!(c.x[30], passes);
            assert_eq!(c.v[0][0], (2.0 + 3.0 * passes as f64).to_bits());
            assert_eq!(c.exit_reason, if mode == 2 { EXIT_INTERP } else { EXIT_SVC });
            if mode == 2 { assert_eq!(c.pc(), PC); }
        }
    }

    #[test]
    fn region_exit_stub_and_patched_successor_see_dirty_values() {
        let words = [0x9100_0400, 0xf100_0631, 0x54ff_ffc1, 0xd400_1001];
        // Use x17 as counter and x0 as accumulator. The exit is outside the unit.
        let u = unit(&words, &[0], StubKind::Patchable, false, false);
        assert!(u.region.is_some());
        let (host, slots) = assemble(u, 0);
        assert_eq!(slots.len(), 1);
        let mut c = cpu(); c.x[17] = 7;
        unsafe { crate::jit::attesor_jit_enter(&mut *c, host); }
        assert_eq!(c.x[0], 7); assert_eq!(c.x[17], 0); assert_eq!(c.pc(), PC + 12);
        assert_eq!(c.exit_reason, EXIT_CONTINUE);
        let (successor, _) = assemble(unit(&words, &[3], StubKind::Patchable, false, false), 3);
        unsafe { (slots[0] as *mut u64).write_unaligned(successor); }
        c.x[0] = 0; c.x[17] = 9;
        unsafe { crate::jit::attesor_jit_enter(&mut *c, host); }
        assert_eq!(c.x[0], 9); assert_eq!(c.x[17], 0); assert_eq!(c.exit_reason, EXIT_SVC);
    }

    #[test]
    fn commpage_helper_preserves_ten_registers_and_pair_addresses() {
        // A custom redirection helper maps the synthetic commpage address into
        // a private mapping. It also stresses calls from pre/post and pair accesses.
        extern "C" fn map(_: u64) -> u64 {
            unsafe { std::arch::asm!("xor rdx,rdx", "xor rsi,rsi", "xor rdi,rdi", "xor r8,r8", "xor r9,r9", "xor r10,r10",
                out("rdx") _, out("rsi") _, out("rdi") _, out("r8") _, out("r9") _, out("r10") _); }
            BUFFER.load(std::sync::atomic::Ordering::Relaxed)
        }
        static BUFFER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut memory = [11u64, 13, 17, 19];
        BUFFER.store(memory.as_mut_ptr() as u64, std::sync::atomic::Ordering::Relaxed);
        let mut words: Vec<u32> = (0..10).map(|n| 0x9100_0400 | (n << 5) | n).collect();
        // ldp x24,x25,[x20],#16; ldr x26,[x20,#8]!; subs x28,x28,#1; b.ne loop; svc
        words.extend([0xa8c1_6698, 0xf840_8e9a, 0xf100_079c, 0x54ff_fe61, 0xd400_1001]);
        let u = unit(&words, &[0,14], StubKind::Patchable, false, true);
        assert_eq!(u.region_exits[0].reg, [0,1,2,3,4,5,6,7,8,9]);
        let (host, _) = assemble(u, 0);
        let mut c = cpu(); c.x[20] = 0x0000_000f_ffff_0000; c.x[28] = 5;
        c.helpers[HELPER_COMMPAGE] = map as *const () as u64;
        unsafe { crate::jit::attesor_jit_enter(&mut *c, host); }
        for i in 0..10 { assert_eq!(c.x[i], 5); }
        assert_eq!(c.x[20], 0x0000_000f_ffff_0000 + 5 * 24);
        assert_eq!((c.x[24], c.x[25], c.x[26]), (11,11,11));
        assert_eq!(c.exit_reason, EXIT_SVC);
    }

    #[test]
    fn sp_zero_immediates_high_registers_and_spills_match_interpreter() {
        let regs: Vec<u32> = (0..7).chain(16..20).chain(24..27).collect();
        let mut words = vec![0xd280_001a]; // movz x26,#0 (assigned immediate write)
        for &n in &regs { words.push(0x9100_0000 | ((n + 1) << 10) | (n << 5) | n); }
        words.extend([0x9100_43ff, 0x9100_03f8, 0x8b1a_03f9, 0xf100_079c]);
        let back = -(words.len() as i32) as u32;
        words.push(0xb500_001c | ((back & 0x7ffff) << 5));
        let svc = words.len(); words.push(0xd400_1001);
        let u = unit(&words, &[0, svc], StubKind::Patchable, false, false);
        let r = u.region_exits[0];
        assert!(r.reg.contains(&31) && r.reg.contains(&26));
        assert!(regs.iter().any(|&n| !r.reg.contains(&(n as u8))));
        let (host, _) = assemble(u, 0);
        for count in [1,2,17] {
            let mut c = cpu();
            c.x = std::array::from_fn(|n| u64::MAX - n as u64 * 12345);
            c.x[28] = count; c.set_nzcv(11);
            let mut reference = cpu(); reference.x = c.x; reference.set_nzcv(c.nzcv());
            for _ in 0..1000 {
                let w = words[((reference.pc() - PC) / 4) as usize];
                match attesor_core::interp::exec(&mut reference, w) {
                    Ok(()) => {},
                    Err(attesor_core::interp::Exit::Svc(_)) => break,
                    Err(e) => panic!("reference: {e:?}"),
                }
            }
            unsafe { crate::jit::attesor_jit_enter(&mut *c, host); }
            assert_eq!(c.x, reference.x); assert_eq!(c.nzcv(), reference.nzcv());
            assert_eq!(c.pc(), reference.pc()); assert_eq!(c.exit_reason, EXIT_SVC);
        }
    }

    #[test]
    fn direct_operands_aliases_widths_and_fused_selects_match_interpreter() {
        let mut cases = Vec::new();
        for sf in [0, 1u32 << 31] {
            for d in [0, 1, 31] { for n in [0, 1, 31] { for m in [0, 1, 31] {
                for op in 0..4 {
                    cases.push(sf | 0x0a00_0000 | (op << 29) | (m << 16) | (n << 5) | d);
                    cases.push(sf | 0x0b00_0000 | (op << 29) | (m << 16) | (n << 5) | d);
                    cases.push(sf | 0x0b20_0000 | (op << 29) | (m << 16) | (6 << 13) | (2 << 10) | (n << 5) | d);
                    cases.push(sf | 0x1a80_0000 | ((op >> 1) << 30) | ((op & 1) << 10) | (m << 16) | (n << 5) | d);
                }
                for opcode in 8..=11 {
                    cases.push(sf | 0x1ac0_0000 | (m << 16) | (opcode << 10) | (n << 5) | d);
                }
                for lsb in [0,7] {
                    cases.push(sf | 0x1380_0000 | ((sf >> 31) << 22) | (m << 16) | (lsb << 10) | (n << 5) | d);
                }
                if m == 0 {
                    for op in 0..4 {
                        cases.push(sf | 0x1100_0400 | (op << 29) | (n << 5) | d);
                        cases.push(sf | 0x1200_1c00 | ((sf >> 31) << 22) | (op << 29) | (n << 5) | d);
                    }
                    for op in [0,2,3] { cases.push(sf | 0x1280_2460 | (op << 29) | d); }
                }
                // Shift/invert with Rd=Rm, and a multiply-add whose addend aliases Rd.
                cases.push(sf | 0x4a40_2000 | (m << 16) | (n << 5) | d);
                cases.push(sf | 0x0a20_0c00 | (m << 16) | (n << 5) | d);
                cases.push(sf | 0x1b00_0000 | (m << 16) | (d << 10) | (n << 5) | d);
                for opc in 0..3 {
                    for (r,s) in [(0,31),(8,15),(17,3)] {
                        cases.push(sf | 0x1300_0000 | (opc << 29) | ((sf >> 31) << 22) | (r << 16) | (s << 10) | (n << 5) | d);
                    }
                }
            } } }
        }
        for (i,w) in cases.into_iter().enumerate() {
            // CMP -> candidate exercises both native flags and fused selects.
            // SUB without S + CBNZ preserves the candidate's final NZCV.
            let words = [0xeb12_023f,w,0xd100_079c,0xb5ff_ffbc,0xd400_1001];
            let u = unit(&words,&[0,4],StubKind::Patchable,false,false);
            assert_eq!(u.region_exits.len(),1,"case {i}: {w:08x}");
            let (host,_) = assemble(u,0);
            for nzcv in [0,15] {
                let mut c = cpu();
                c.x = std::array::from_fn(|n| 0xabcd_ef01_9876_5432u64.wrapping_mul(n as u64 + 1));
                c.x[28] = 3; c.x[17] = if nzcv == 0 {7} else {15}; c.x[18] = 7;
                c.set_nzcv(nzcv as u32);
                let mut reference = cpu(); reference.x = c.x; reference.set_nzcv(c.nzcv());
                loop {
                    let w = words[((reference.pc() - PC) / 4) as usize];
                    match attesor_core::interp::exec(&mut reference,w) {
                        Ok(()) => {}, Err(attesor_core::interp::Exit::Svc(_)) => break,
                        Err(e) => panic!("case {i}: {w:08x}: {e:?}"),
                    }
                }
                unsafe { crate::jit::attesor_jit_enter(&mut *c,host); }
                assert_eq!(c.x,reference.x,"case {i}: {w:08x}");
                assert_eq!(c.nzcv(),reference.nzcv(),"NZCV case {i}: {w:08x}");
                assert_eq!(c.exit_reason,EXIT_SVC);
            }
        }
    }

    #[test]
    fn mask_survives_helper_backedge_and_normal_code_has_no_locked_counter() {
        extern "C" fn clobber(_: *mut Cpu, _: u32, _: u64) -> u64 {
            unsafe { std::arch::asm!("xor r11,r11",out("r11") _); }
            0
        }
        // LDRB; helper; SUBS; B.NE. The helper poisons the mask after the last
        // memory access, so the backedge must restore it before the next load.
        let words = [0x3940_0280,0,0xf100_079c,0x54ff_ffa1,0xd400_1001];
        let u = unit(&words,&[0,4],StubKind::Patchable,false,false);
        assert!(!u.a.instructions().iter().any(|i| i.has_lock_prefix()));
        let (host,_) = assemble(u,0);
        let byte = 0xabu8;
        let mut c = cpu(); c.x[20] = (&byte as *const u8 as u64) | 0xab00_0000_0000_0000; c.x[28] = 7;
        c.helpers[HELPER_INTERP] = clobber as *const () as u64;
        unsafe { crate::jit::attesor_jit_enter(&mut *c,host); }
        assert_eq!(c.x[0],0xab); assert_eq!(c.x[28],0); assert_eq!(c.exit_reason,EXIT_SVC);

        let crc = [0x3869_6a6a,0x4a08_014a,0x9240_1d4a,0xb86a_7aea,0x4a48_2148,0x9100_0529,0xf160_013f,0x54ff_ff21];
        let u = unit(&crc,&[0],StubKind::Patchable,false,false);
        assert_eq!(u.a.instructions().iter().filter(|i| i.mnemonic()==iced_x86::Mnemonic::Mov && i.op0_register()==iced_x86::Register::R11).count(),1);
        // Pair/pre/post addressing uses registers, including with commpage checks.
        for commpage in [false,true] {
            let words = [0xa8c1_6698,0xf840_8e9a,0xf100_079c,0x54ff_ffa1];
            let u = unit(&words,&[0],StubKind::Patchable,false,commpage);
            if !commpage {
                assert!(!u.a.instructions().iter().any(|i| matches!(i.mnemonic(),iced_x86::Mnemonic::Push|iced_x86::Mnemonic::Pop)));
            }
        }
    }

    #[test]
    fn verification_and_scratch_templates_do_not_enter_regions() {
        let words = [0x9100_0400, 0xf100_0631, 0x54ff_ffc1, 0xd400_1001];
        assert!(unit(&words, &[0], StubKind::Plain, false, false).region.is_some());
        assert!(unit(&words, &[0], StubKind::Patchable, true, false).region.is_none());
        for scratch in [0xc8a0_7c22, 0xf820_0022, 0x5e00_4000, 0x5e00_5000] {
            let words = [0x9100_0400, scratch, 0xf100_0631, 0x54ff_ffa1];
            assert!(unit(&words, &[0], StubKind::Patchable, false, false).region.is_none());
        }
    }
}

#[cfg(test)]
mod vdead_tests {
    use super::*;
    use attesor_core::{cpu::Cpu, interp};
    const PC: u64 = 0x1800_0000;

    #[test]
    fn forward_proof_stops_at_every_observer_and_boundary() {
        let labels = HashMap::new();
        let dead = |words: &[u32], room, labels: &HashMap<u64, CodeLabel>| {
            v_store_dead_at(0, PC, room, labels, &|p| words.get(((p - PC) / 4) as usize).copied())
        };
        let kill = 0x1e622800; // fadd d0,d0,d2: reads old V0 through scalar_inputs
        assert!(dead(&[0x1e602803, kill], 2, &labels));
        assert!(dead(&[0x4e228420], 1, &labels)); // add v0.16b,v1.16b,v2.16b
        assert!(dead(&[0x0e228420], 1, &labels)); // 64-bit vector writer zeroes upper
        assert!(dead(&[0xfd400020], 1, &labels)); // ldr d0,[x1] zeroes upper bits
        for barrier in [0xd4001001, 0x94000001, 0x14000001, 0,
                        0xf9c00021, 0xa9400421, // reserved single / duplicate GPR pair -> fallback
                        0xfd000020, // str d0,[x1], reads Cpu, not V cache
                        0xad000420, // stp q0,q1,[x1]
                        0x4e228400, 0x4e228403, // direct-memory vector input aliases
                        0x6e621c20, // BSL reads old destination
                        0x4e081c20, // ins v0.d[0],x1 (partial)
                        0x4d408020, // lane load / structure family (unknown)
                        0x9e660001, // fmov x1,d0 (direct memory reader)
                        0x1e624801] { // scalar max/min: direct memory + cold path
            assert!(!dead(&[barrier, kill], 2, &labels), "{barrier:08x}");
        }
        assert!(!dead(&[kill], 0, &labels));
        let mut u = Unit::new(StubKind::Plain, false);
        let l = u.a.create_label(); u.labels.insert(PC, l);
        assert!(!dead(&[kill], 1, &u.labels));
    }

    #[test]
    fn register_only_aliases_nan_eviction_and_loop_aot_match_interpreter() {
        for kind in [StubKind::Plain, StubKind::Patchable] {
            for looped in [false, true] {
                // More than eight pending dead values forces dirty eviction.
                let mut words: Vec<u32> = (0..12).map(|d| 0x1e622820 | d).collect();
                for d in 0..12 { words.push(0x1e602800 | (d << 16) | (d << 5) | d); }
                for d in 0..12 { words.push(0x4e3f87c0 | d); } // add vD.16b,v30.16b,v31.16b
                // A memory-reading store is a barrier. The final writes must be visible.
                words.push(0xfd000280); // str d0,[x20]
                if looped {
                    words.push(0xf10006b5); // subs x21,x21,#1
                    words.push(0x54000001 | (((-(words.len() as i32)) as u32 & 0x7ffff) << 5));
                }
                words.push(0xd4001001);
                let mut u = Unit::new(kind, false);
                let l = u.a.create_label(); u.labels.insert(PC, l);
                let exit_pc = PC + (words.len() as u64 - 1) * 4;
                if looped { let l = u.a.create_label(); u.labels.insert(exit_pc, l); }
                emit_block(&mut u, PC, &|p| words.get(((p - PC) / 4) as usize).copied());
                if looped { emit_block(&mut u, exit_pc, &|p| words.get(((p - PC) / 4) as usize).copied()); }
                u.finish();
                let host = crate::jit::code_cache().alloc(65536);
                let bytes = u.a.assemble(host).unwrap(); assert!(bytes.len() < 65536);
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), host as *mut u8, bytes.len()); }
                for nan in [false, true] {
                    let mut memory = 0u64;
                    let mut c = Cpu::new(); crate::jit::setup_cpu(&mut c);
                    c.set_pc(PC); c.x[20] = &mut memory as *mut _ as u64; c.x[21] = 3;
                    for i in 0..32 { c.v[i] = [if nan { 0x7ff0000000000001 + i as u64 } else { (i as f64 + 0.25).to_bits() }, !0]; }
                    let mut r = Cpu::new(); r.x = c.x; r.v = c.v; r.set_pc(PC);
                    for _ in 0..500 {
                        let w = words[((r.pc() - PC) / 4) as usize];
                        match interp::exec(&mut r, w) { Ok(()) => {}, Err(interp::Exit::Svc(_)) => break, Err(e) => panic!("{e:?}") }
                    }
                    let expected = memory; memory = 0;
                    unsafe { crate::jit::attesor_jit_enter(&mut *c, host); }
                    assert_eq!(c.v, r.v); assert_eq!(c.x, r.x); assert_eq!(c.nzcv(), r.nzcv());
                    assert_eq!(memory, expected); assert_eq!(c.exit_reason, EXIT_SVC);
                }
            }
        }
    }
}

#[cfg(test)]
mod avx512_tests {
    use super::*;
    use attesor_core::{cpu::Cpu, interp};
    use iced_x86::{EncodingKind, Register};
    const PC: u64 = 0x2800_0000;

    #[test]
    fn macos_detection_uses_kernel_policy_not_eager_xcr0() {
        let required = (1 << 16) | (1 << 31) | (1 << 30) | (1 << 17);
        // No XCR0 input: lazy state (0x7) must not veto kernel-approved hardware.
        assert!(avx512_macos_policy(required, [true; 4]));
        for i in [16, 31, 30, 17] {
            assert!(!avx512_macos_policy(required & !(1 << i), [true; 4]));
        }
        for i in 0..4 {
            let mut os = [true; 4]; os[i] = false;
            assert!(!avx512_macos_policy(required, os));
        }
        eprintln!("AVX512 runtime detection: {}", has_avx512());
    }

    // Test-only execution authorization, not a product feature override. Set only
    // after an independent EVEX probe succeeds on this host. Some sandboxes deny
    // sysctl queries even though the kernel correctly promotes AVX-512 threads.
    pub(super) fn execution_available() -> bool {
        has_avx512() || (std::env::var("ATTESOR_TEST_AVX512").as_deref() == Ok("1")
            && avx512_macos_policy(std::arch::x86_64::__cpuid_count(7, 0).ebx, [true; 4]))
    }

    fn compile(words: &[u32], extended: bool, kind: StubKind) -> Unit {
        compile_features(words, extended, false, kind)
    }

    fn compile_features(words: &[u32], extended: bool, nan: bool, kind: StubKind) -> Unit {
        let mut u = Unit::new(kind, false);
        // Encoder-only tests may select the new bank even on unsupported hosts.
        // Execution tests below must separately check has_avx512(). No user-facing
        // override can bypass the host/OS detector.
        u.avx512 = extended;
        u.avx512_nan = nan;
        u.no_internal_jumps = true;
        let l = u.a.create_label(); u.labels.insert(PC, l);
        emit_block(&mut u, PC, &|p| words.get(((p-PC)/4) as usize).copied());
        u.finish();
        u
    }

    fn assert_xmm_only(u: &Unit) {
        for i in u.a.instructions() {
            for op in 0..i.op_count() {
                let r = i.op_register(op);
                assert!(!r.is_ymm() && !r.is_zmm(), "wider-than-128-bit operand: {i:?}");
            }
        }
        for (range, live) in &u.cold_ranges {
            check_cold_registers(&u.a.instructions()[range.clone()], *live).unwrap();
        }
    }

    fn pressure_words(single: bool, op: u32, aliases: bool) -> Vec<u32> {
        // 28 dirty values exceed *both* cache sizes, so xmm31 and dirty
        // eviction are mandatory. The second wave reads old values and leaves
        // its results observable in Cpu (do not overwrite them with constants).
        let ty = if single { 0 } else { 1 << 22 };
        let mut words: Vec<u32> = (0..28).map(|d| 0x1e20_2800 | ty | (31 << 16) | (d << 5) | d).collect();
        for d in 0..28 {
            let m = if aliases { d } else { (d+1)%28 };
            words.push(0x1e20_0800 | ty | (op << 12) | (m << 16) | (d << 5) | d);
        }
        words.push(0xd400_1001);
        words
    }

    #[test]
    fn all_high_register_transfers_encode_without_wide_vectors() {
        let mut u = Unit::new(StubKind::Plain, false); u.avx512 = true;
        let mut e = E { u: &mut u, pc: PC, fuse_in: None, scalar_inputs: None };
        for r in VREGS.into_iter().skip(BASE_NVREGS) {
            e.cache_copy(r, xmm0);
            e.cache_copy(xmm6, r);
            e.cache_low64(xmm0, r);
            e.fld_source(xmm1, FpSource { cached: Some(r), offset: 0 }, false);
        }
        assert_xmm_only(&u);
        let evex = u.a.instructions().iter().filter(|i| i.encoding() == EncodingKind::EVEX).count();
        assert_eq!(evex, 16*4);
        u.a.assemble(0x1000).unwrap(); // Invalid high-register legacy forms fail here.
    }

    #[test]
    fn pressure_uses_every_high_slot_and_fallback_uses_none() {
        for single in [false, true] { for op in 0..4 { for aliases in [false, true] {
            for kind in [StubKind::Plain, StubKind::Patchable] {
                let words = pressure_words(single, op, aliases);
                for extended in [false, true] {
                    let mut u = compile(&words, extended, kind);
                    assert_xmm_only(&u);
                    let mut high = [false; 16];
                    for i in u.a.instructions() {
                        for op in 0..i.op_count() {
                            let r = i.op_register(op);
                            if r.is_xmm() && r.number() >= 16 {
                                high[r.number()-16] = true;
                                assert_eq!(i.encoding(), EncodingKind::EVEX);
                            }
                        }
                    }
                    assert_eq!(high, [extended; 16]);
                    if extended {
                        assert!(u.a.instructions().iter().any(|i| i.mnemonic() == iced_x86::Mnemonic::Vmovdqu64
                            && i.op0_kind() == iced_x86::OpKind::Memory), "missing dirty high-bank spill");
                    }
                    u.a.assemble(0x1000).unwrap();
                }
            }
        }}}
    }

    #[test]
    fn cold_contract_protects_all_extended_cache_registers() {
        for reg in VREGS.into_iter().skip(BASE_NVREGS) {
            let mut a = CodeAssembler::new(64).unwrap();
            a.vmovdqa64(xmm7, reg).unwrap();
            assert!(check_cold_registers(a.instructions(), true).is_ok());
            a.vmovdqa64(reg, xmm7).unwrap();
            assert!(check_cold_registers(a.instructions(), true).is_err());
        }
    }

    #[test]
    fn pressure_results_match_interpreter_with_sensitivity_mutant() {
        if !execution_available() {
            eprintln!("AVX512_EXECUTION_UNAVAILABLE: high-bank differential and execution sensitivity NOT RUN (fallback pressure still tested)");
        }
        // Keep the deliberately broken code confined to this test's private
        // assembler: corrupt the xmm31 cache copy. Correct and mutant runs must
        // respectively match and disagree with the *same* interpreter oracle.
        for single in [false, true] { for op in 0..4 { for aliases in [false, true] {
            for kind in [StubKind::Plain, StubKind::Patchable] {
                let words = pressure_words(single, op, aliases);
                for extended in [false, true].into_iter().filter(|&extended| !extended || execution_available()) {
                let mut mutant_mismatches = 0;
                for mutant in [false, true].into_iter().filter(|&mutant| !mutant || extended) {
                    let mut u = compile(&words, extended, kind);
                    if mutant {
                        let mut insns = u.a.instructions().to_vec();
                        let copy = insns.iter_mut().find(|i| i.op0_register() == Register::XMM31
                            && i.mnemonic() == iced_x86::Mnemonic::Vmovdqa64).unwrap();
                        copy.set_op1_register(Register::XMM5);
                        // Preserve labels and constant-pool relocations by changing
                        // only the encoded operand after normal assembly below.
                        let host = crate::jit::code_cache().alloc(65536);
                        let result = u.a.assemble_options(host, iced_x86::BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
                        let idx = u.a.instructions().iter().position(|i| i.op0_register() == Register::XMM31
                            && i.mnemonic() == iced_x86::Mnemonic::Vmovdqa64).unwrap();
                        let offset = result.inner.new_instruction_offsets[idx] as usize;
                        let mut encoder = iced_x86::Encoder::new(64);
                        encoder.encode(copy, host+offset as u64).unwrap();
                        let broken = encoder.take_buffer();
                        let mut bytes = result.inner.code_buffer;
                        bytes[offset..offset+broken.len()].copy_from_slice(&broken);
                        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(),host as *mut u8,bytes.len()); }
                        mutant_mismatches += execute_cases(host, &words, single, true);
                    } else {
                        let host = crate::jit::code_cache().alloc(65536);
                        let bytes = u.a.assemble(host).unwrap();
                        assert!(bytes.len()<65536);
                        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(),host as *mut u8,bytes.len()); }
                        assert_eq!(execute_cases(host, &words, single, false), 0);
                    }
                }
                if extended {
                    assert!(mutant_mismatches > 0, "insensitive pressure oracle");
                    eprintln!("AVX512 sensitivity: single={single} op={op} aliases={aliases} mismatch={mutant_mismatches}");
                }
                }
            }
        }}}
    }

    #[test]
    fn nan_templates_encode_xmm_only_and_preserve_cold_registers() {
        for double in [false, true] {
            let ty = if double { 1 << 22 } else { 0 };
            for (name, word) in [("add", 0x1e222820), ("max", 0x1e224820),
                                 ("maxnm", 0x1e226820), ("fma", 0x1f020c20)] {
                if name == "fma" && !has_fma() { continue; }
                let words = [word | ty, 0xd4001001];
                let mut before = compile_features(&words, false, false, StubKind::Plain);
                let mut after = compile_features(&words, false, true, StubKind::Plain);
                assert_xmm_only(&after);
                let cold_count = |u: &Unit| u.cold_ranges.iter().map(|(r,_)| r.len()).sum::<usize>();
                eprintln!("AVX512 static fixup: double={double} template={name} cold_instructions={} -> {}",
                    cold_count(&before), cold_count(&after));
                assert!(cold_count(&after) < cold_count(&before));
                assert!(after.a.instructions().iter().any(|i| i.encoding() == EncodingKind::EVEX));
                before.a.assemble(0).unwrap(); after.a.assemble(0).unwrap();
            }
        }
        // k1 is scratch only; no other opmask is admitted by the cold ABI.
        let mut a = CodeAssembler::new(64).unwrap();
        a.kmovw(k1, eax).unwrap();
        assert!(check_cold_registers(a.instructions(), true).is_ok());
        a.kmovw(k2, eax).unwrap();
        assert!(check_cold_registers(a.instructions(), true).is_err());
    }

    #[test]
    fn native_feature_selftest_matrix() {
        if !execution_available() {
            eprintln!("AVX512_EXECUTION_UNAVAILABLE: native feature matrix NOT RUN");
            return;
        }
        struct Reset;
        impl Drop for Reset { fn drop(&mut self) { TEST_AVX512_FEATURES.with(|f| f.set(None)); } }
        let _reset = Reset;
        for features in [(true, false), (false, true), (true, true)] {
            TEST_AVX512_FEATURES.with(|f| f.set(Some(features)));
            eprintln!("AVX512 native selftest features={features:?} seed=1 count=20000");
            for class in ["fpblocks", "fpnan", "0E20D400:0E20D400", "0F800000:0F800000", "1F000000:1F000000", "loops"] {
                crate::jit::selftest::run_selftest(class, 20000, Some(1));
            }
        }
    }

    #[test]
    fn nan_range_results_and_mutants_match_interpreter() {
        if !execution_available() {
            eprintln!("AVX512_EXECUTION_UNAVAILABLE: NaN/range differential and sensitivity NOT RUN");
            return;
        }
        let mut total = 0;
        for double in [false, true] {
            let ty = if double { 1 << 22 } else { 0 };
            let mut words = Vec::new();
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
                if has_fma() {
                    for base in [0x0f801000, 0x0f805000, 0x0e20cc00, 0x0ea0cc00] {
                        words.push(base | qbit | ty | (2 << 16) | (1 << 5));
                    }
                }
            }
            // Cartesian triples plus rotated packed lanes cover both NaN passes,
            // FMA addend priority, all signed-zero ties, payload/sign preservation,
            // exact negative indefinite inputs, and denormals with FZ on/off.
            let values: &[u64] = if double {
                &[0, 0x8000000000000000, 1, 0x8000000000000001, 0x0010000000000000,
                  0x3ff0000000000000, 0xbff0000000000000, 0x7ff0000000000000,
                  0xfff0000000000000, 0x7ff8000000001234, 0xfff8000000005678,
                  0xfff8000000000000, 0x7ff0000000001234, 0xfff0000000005678]
            } else {
                &[0, 0x80000000, 1, 0x80000001, 0x00800000, 0x3f800000, 0xbf800000,
                  0x7f800000, 0xff800000, 0x7fc01234, 0xffc05678, 0xffc00000,
                  0x7f801234, 0xff805678]
            };
            for word in words {
                for mutant in [0, 1, 2] {
                    let mut u = compile_features(&[word, 0xd4001001], true, true, StubKind::Plain);
                    assert_xmm_only(&u);
                    let target = u.a.instructions().iter().position(|i| match mutant {
                        1 => matches!(i.mnemonic(), iced_x86::Mnemonic::Vfixupimmps | iced_x86::Mnemonic::Vfixupimmpd),
                        2 => matches!(i.mnemonic(), iced_x86::Mnemonic::Vrangeps | iced_x86::Mnemonic::Vrangepd),
                        _ => false,
                    });
                    if mutant != 0 && target.is_none() { continue; }
                    let host = crate::jit::code_cache().alloc(16384);
                    let res = u.a.assemble_options(host, iced_x86::BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
                    let mut bytes = res.inner.code_buffer.clone();
                    if let Some(idx) = target {
                        let mut broken = u.a.instructions()[idx];
                        if mutant == 1 {
                            broken.set_op1_register(Register::XMM0); // lose original NaN payload/priority
                            let table = if double { 0x22 } else { 0x0000_0022_0000_0022 };
                            let label = u.constant_pool.iter().find(|(_, v)| *v == table).unwrap().0;
                            broken.set_memory_displacement64(res.label_ip(&label).unwrap());
                        }
                        else { broken.set_immediate8(broken.immediate8() ^ 1); } // invert min/max
                        let offset = res.inner.new_instruction_offsets[idx] as usize;
                        let mut encoder = iced_x86::Encoder::new(64);
                        encoder.encode(&broken, host + offset as u64).unwrap();
                        let patch = encoder.take_buffer();
                        bytes[offset..offset+patch.len()].copy_from_slice(&patch);
                    }
                    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), host as *mut u8, bytes.len()); }
                    let mut mismatches = 0;
                    for mode in [0, 1 << 24] {
                    for ia in 0..values.len() { for ib in 0..values.len() { for ic in 0..values.len() {
                        let mut c = Cpu::new(); crate::jit::setup_cpu(&mut c);
                        c.set_pc(PC); c.fpcr = mode; c.set_nzcv(11);
                        for (r, j) in [(0,ic), (1,ia), (2,ib), (3,ic)] {
                            let at = |n: usize| values[n % values.len()];
                            c.v[r] = if double { [at(j),at(j+3)] }
                                else { [at(j)|(at(j+1)<<32), at(j+2)|(at(j+3)<<32)] };
                        }
                        let mut expected = Cpu::new(); expected.v = c.v;
                        expected.fpcr = mode; expected.set_pc(PC); expected.set_nzcv(11);
                        interp::sys_sync_host_fp_mode(mode);
                        interp::exec(&mut expected, word).unwrap();
                        interp::sys_sync_host_fp_mode(mode);
                        unsafe { crate::jit::attesor_jit_enter(&mut *c, host); }
                        assert_eq!(c.exit_reason, EXIT_SVC);
                        assert_eq!(c.x, expected.x); assert_eq!(c.nzcv(), expected.nzcv());
                        assert_eq!(c.fpcr, mode);
                        if c.v != expected.v {
                            mismatches += 1;
                            assert!(mutant != 0, "word={word:08x} mode={mode:x} a={:x} b={:x} c={:x}: {:?} != {:?}",
                                values[ia], values[ib], values[ic], c.v, expected.v);
                        }
                        if mutant == 0 { total += 1; }
                    }}}}
                    if mutant != 0 {
                        assert!(mismatches > 0, "insensitive mutant={mutant} word={word:08x}");
                        eprintln!("AVX512 NaN sensitivity: mutant={mutant} word={word:08x} mismatch={mismatches}");
                    }
                }
            }
        }
        interp::sys_sync_host_fp_mode(0);
        eprintln!("AVX512 NaN/range directed: cases={total} mismatch=0 FZ=off/on");
    }

    fn execute_cases(host: u64, words: &[u32], single: bool, mutant: bool) -> usize {
        let values: &[u64] = if single {
            &[0x3f800001, 0x40000001, 0x7fc01234, 0xff801234, 0x7f800000, 0, 1, 0x80000000]
        } else {
            &[0x3ff0000000000001, 0x4000000000000001, 0x7ff8000000001234,
              0xfff0000000001234, 0x7ff0000000000000, 0, 1, 0x8000000000000000]
        };
        let mut mismatches = 0;
        for seed in [1u64, 0x12345678, 0xfeedbeef] {
            for a in values { for b in values {
                let mut c = Cpu::new(); crate::jit::setup_cpu(&mut c); c.set_pc(PC);
                // Distinct bit patterns in every guest register detect register
                // swaps as well as corruption; identical accumulators would hide them.
                for n in 0..32 { c.v[n] = [a.wrapping_add(n as u64 * ((seed & 15)+1)), !seed]; }
                c.v[30] = [*a, !seed]; c.v[31] = [*b, !seed];
                let mut reference = Cpu::new(); reference.v = c.v; reference.set_pc(PC);
                for &w in words {
                    match interp::exec(&mut reference, w) {
                        Ok(()) => {}, Err(interp::Exit::Svc(_)) => break, Err(e) => panic!("{w:08x}: {e:?}"),
                    }
                }
                unsafe { crate::jit::attesor_jit_enter(&mut *c, host); }
                assert_eq!(c.exit_reason, EXIT_SVC);
                assert_eq!(c.x, reference.x); assert_eq!(c.nzcv(), reference.nzcv());
                assert_eq!(c.fpcr, reference.fpcr);
                if c.v != reference.v {
                    mismatches += 1;
                    if !mutant { panic!("single={single} seed={seed:x} a={a:x} b={b:x}: {:?} != {:?}", c.v, reference.v); }
                }
            }}
        }
        mismatches
    }
}


#[cfg(test)]
mod acceptance_switch_tests {
    use super::*;

    #[test]
    fn vchain_switches_in_fresh_processes() {
        const CHILD:&str="ATTESOR_VCHAIN_SWITCH_CHILD";
        if let Ok(expected)=std::env::var(CHILD) {
            let u=Unit::new(StubKind::Plain,false);
            assert_eq!(format!("{}{}",u.vloop as u8,u.vchain as u8),expected);return;
        }
        for(on,off,expected)in[("","","00"),("vchain","","01"),("vloop,vchain","","11"),
            ("vloop,vchain","vchain","10"),("vloop,vchain","vloop,vchain","00"),
            ("xvloop,xvchain","","00"),("vloop,vchain","xvchain","11")] {
            let p=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","jit::emit::acceptance_switch_tests::vchain_switches_in_fresh_processes"])
                .env(CHILD,expected).env("ATTESOR_JIT_ON",on).env("ATTESOR_JIT_OFF",off)
                .env_remove("ATTESOR_JIT_DISABLE").output().unwrap();
            assert!(p.status.success(),"ON={on} OFF={off}: {}{}",String::from_utf8_lossy(&p.stdout),String::from_utf8_lossy(&p.stderr));
        }
    }

    #[test]
    fn on_off_precedence_in_fresh_processes() {
        const CHILD: &str = "ATTESOR_ACCEPT_SWITCH_CHILD";
        if let Ok(expected) = std::env::var(CHILD) {
            let mut u = Unit::new(StubKind::Plain, false);
            let actual = format!("{}{}", u.regioncache as u8, u.loopra as u8);
            assert_eq!(actual, expected, "ON={:?} OFF={:?}", std::env::var("ATTESOR_JIT_ON"), std::env::var("ATTESOR_JIT_OFF"));
            let pc = 0x1000;
            let words = [0x9100_0400, 0xf100_079c, 0x54ff_ffc1, 0xd400_1001];
            let label = u.a.create_label(); u.labels.insert(pc, label);
            emit_block(&mut u, pc, &|p| p.checked_sub(pc).and_then(|n| words.get((n/4) as usize)).copied());
            assert_eq!(u.region.is_some(), u.loopra);
            assert_eq!(u.loop_cache.is_some(), !u.loopra && u.regioncache);
            return;
        }
        let exe = std::env::current_exe().unwrap();
        for (on, off, expected) in [
            ("", "", "00"), ("regioncache", "", "10"), ("loopra", "", "01"),
            ("regioncache,loopra", "", "11"),
            ("regioncache,loopra", "regioncache,loopra", "00"),
            ("regioncache", "loopra", "10"), ("regioncache", "loopra,regioncache", "00"),
            ("xregioncache,xloopra", "", "00"),
            ("regioncache,loopra", "regioncache", "01"),
            ("regioncache,loopra", "loopra", "10"),
        ] {
            let result = std::process::Command::new(&exe)
                .args(["--exact", "jit::emit::acceptance_switch_tests::on_off_precedence_in_fresh_processes", "--nocapture"])
                .env(CHILD, expected).env("ATTESOR_JIT_ON", on).env("ATTESOR_JIT_OFF", off)
                .env_remove("ATTESOR_JIT_DISABLE").output().unwrap();
            assert!(result.status.success(), "ON={on} OFF={off}: {}{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
        }
    }
}
