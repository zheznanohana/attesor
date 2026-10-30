//! JIT runtime: code cache, dispatcher, block chaining, indirect-branch
//! cache, invalidation. Blocks can also come from the AOT cache (`aot.rs`).

mod code_cache_alloc;
mod sharedtrans;
mod isel2;
pub(crate) fn isel2_mask() -> u32 { isel2::selected().mask() }
pub(crate) mod flagreg;
pub mod cost;
pub mod emit;
mod fastenc;
mod bgtrans;
mod tiered;
mod bliccache;
mod rel32chain;
mod fix2_diag;
pub(crate) fn fastenc_enabled() -> bool { fastenc::enabled() }
pub mod selftest;
pub mod loops;
pub(crate) mod offline;

use emit::{StubKind, Unit, EXIT_CONTINUE, EXIT_PENDING, EXIT_SVC, HELPER_COMMPAGE, HELPER_INTERP};
use iced_x86::BlockEncoderOptions;
use attesor_core::cpu::Cpu;
use attesor_core::interp::{self, Exit};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};

/// Cheap multiplicative hasher for u64 keys (guest pcs / host addresses).
#[derive(Default, Clone, Copy)]
pub struct Fx(u64);
impl std::hash::Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, b: &[u8]) {
        for &x in b {
            self.0 = (self.0.rotate_left(5) ^ x as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
    fn write_u64(&mut self, i: u64) {
        self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}
type FastMap<K, V> = HashMap<K, V, std::hash::BuildHasherDefault<Fx>>;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

/// Entries in each thread's indirect-branch translation cache.
pub const IBTC_SIZE: usize = 32768;
pub const IBTC_GUEST_SHIFT: u32 = 2;
pub const IBTC_ENTRY_BYTES: usize = std::mem::size_of::<[u64; 2]>();
pub const IBTC2_WAYS: usize = 2;
pub const IBTC2_SETS: usize = IBTC_SIZE / IBTC2_WAYS;
pub const IBTC2_SET_BYTES: usize = IBTC_ENTRY_BYTES * IBTC2_WAYS;
const IBTC_EMPTY: [u64; 2] = [1, 0]; // unaligned guest PC cannot be a valid target
const _: () = assert!(IBTC_SIZE.is_power_of_two() && IBTC2_SETS.is_power_of_two());
const _: () = assert!(IBTC_ENTRY_BYTES == 16 && IBTC2_SET_BYTES == 32);

/// One allocation is shared by generated lookups and dispatcher fills on this
/// native thread only. ibtc2 retains the original total entry budget: adjacent
/// pairs are ways of one set, not two independent direct-mapped tables.
struct IndirectCache {
    entries: Box<[[u64; 2]]>,
    /// Next victim on a full-set miss: deterministic round-robin (not LRU).
    /// Hits refresh in place without changing this cursor; empty ways win first.
    victims: Option<Box<[u8]>>,
}

impl IndirectCache {
    fn new() -> Self {
        Self {
            entries: vec![IBTC_EMPTY; IBTC_SIZE].into_boxed_slice(),
            victims: emit::ibtc2_enabled().then(|| vec![0; IBTC2_SETS].into_boxed_slice()),
        }
    }

    fn insert(&mut self, pc: u64, host: u64) {
        let Some(victims) = self.victims.as_mut() else {
            self.entries[((pc >> IBTC_GUEST_SHIFT) as usize) & (IBTC_SIZE - 1)] = [pc, host];
            return;
        };
        if pc & 3 != 0 || host == 0 { return; }
        let set = ((pc >> IBTC_GUEST_SHIFT) as usize) & (IBTC2_SETS - 1);
        let ways = &mut self.entries[set * IBTC2_WAYS..(set + 1) * IBTC2_WAYS];
        if let Some(e) = ways.iter_mut().find(|e| e[0] == pc && e[1] != 0) {
            *e = [pc, host];
            return;
        }
        let way = ways.iter().position(|e| e[1] == 0).unwrap_or(victims[set] as usize);
        ways[way] = [pc, host];
        victims[set] = ((way + 1) % IBTC2_WAYS) as u8;
    }

    fn clear(&mut self) {
        self.entries.fill(IBTC_EMPTY); // includes both ways in every set
        if let Some(victims) = self.victims.as_mut() { victims.fill(0); }
    }
}

const CODE_CACHE_SIZE: u64 = 4 << 30;

std::arch::global_asm!(
    ".globl _attesor_jit_enter",
    ".globl _attesor_jit_exit",
    ".p2align 4",
    "_attesor_jit_enter:",
    "push rbp",
    "mov rbp, rsp",
    "push rbx",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "sub rsp, 8",
    "mov r15, rdi",
    "mov [r15 + {host_sp}], rsp",
    "mov qword ptr [r15 + {ras_depth}], 0",
    "mov rax, [r15 + {epoch_ptr}]",
    "mov rax, [rax]",
    "mov [r15 + {entry_epoch}], rax",
    "jmp rsi",
    ".p2align 4",
    "_attesor_jit_exit:",
    // translated guest calls may have pushed host frames: drop them all
    "mov rsp, [r15 + {host_sp}]",
    "add rsp, 8",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
    host_sp = const attesor_core::cpu::off::HOST_SP,
    ras_depth = const attesor_core::cpu::off::RAS_DEPTH,
    epoch_ptr = const attesor_core::cpu::off::EPOCH_PTR,
    entry_epoch = const attesor_core::cpu::off::ENTRY_EPOCH,
);

extern "C" {
    #[link_name = "attesor_jit_enter"]
    fn jit_enter_raw(cpu: *mut Cpu, code: u64);
    fn attesor_jit_exit();
}

#[inline(always)]
unsafe fn attesor_jit_enter(cpu: *mut Cpu, code: u64) {
    // Observe graph publication under a read lease, then serialize BEFORE
    // fetching any generated instruction. The lease spans the complete run.
    let _graph = rel32chain::enabled().then(|| chain_graph().read().unwrap());
    if rel32chain::enabled() { fix2_diag::add(0); rel32chain::serialize_before_entry(); }
    jit_enter_raw(cpu, code);
}

pub struct CodeCache {
    base: u64,
    end: u64,
    /// Code grows up from `base`.
    code_ptr: AtomicU64,
    /// Chaining slots grow down from `end`.
    slot_ptr: AtomicU64,
}

static CODE: OnceLock<CodeCache> = OnceLock::new();
#[derive(Default)]
struct BlockRegistry {
    entries: FastMap<u64, u64>,
    /// Populated only with preciseinvalidate. Shares the existing publication
    /// lock instead of changing every default-path map value or adding a lock.
    external: FastMap<u64, ExternalBlock>,
    // Bounded tombstones also protect a late/concurrent disk loader. Only
    // preciseinvalidate populates these; no additional default-path work.
    external_invalidations: Vec<(u64, u64)>,
}

struct ExternalBlock {
    host: u64,
    // Only the closed-image AOT builder may supply a whole-image dependency.
    // Legacy external registrations have no proof and retain the global latch.
    image: Option<(u64, u64)>,
}

impl BlockRegistry {
    fn note_external_invalidation(&mut self, mut lo: u64, mut hi: u64) {
        self.external_invalidations.retain(|&(a, b)| {
            if a <= hi && b >= lo { lo = lo.min(a); hi = hi.max(b); false } else { true }
        });
        self.external_invalidations.push((lo, hi));
        if self.external_invalidations.len() > 1024 {
            // Exhausting the precision budget must fail closed, not forget an
            // old invalidation and permit a late stale image to be registered.
            self.external_invalidations.clear();
            self.external_invalidations.push((0, u64::MAX));
        }
    }
}

/// guest pc -> host code and optional external-registration provenance.
static BLOCKS: RwLock<Option<BlockRegistry>> = RwLock::new(None);
/// In precise mode untracked legacy AOT cannot be proven current after an invalidation.
/// Access is coordinated by the existing BLOCKS lock; no new fork-owned lock.
static EXTERNAL_INVALIDATED: AtomicBool = AtomicBool::new(false);
static TRANSLATE: Mutex<()> = Mutex::new(());
// rel32 graph publication lease. Never wait for executing guests to quiesce:
// a busy graph retains its dispatcher fallback and can retry next time.
static CHAIN_GRAPH: OnceLock<std::sync::atomic::AtomicPtr<RwLock<()>>> = OnceLock::new();
fn chain_graph() -> &'static RwLock<()> {
    let graph = CHAIN_GRAPH.get_or_init(|| std::sync::atomic::AtomicPtr::new(Box::into_raw(Box::new(RwLock::new(())))));
    // Graphs are permanent, including the orphaned parent graph after fork.
    unsafe { &*graph.load(Ordering::Acquire) }
}
static EPOCH: AtomicU64 = AtomicU64::new(0);
pub static JIT_ENABLED: AtomicBool = AtomicBool::new(true);

/// One translated unit: the guest range it was built from and where its code lives.
struct UnitRec {
    entry: u64,
    host: u64,
    /// Guest byte range [lo, hi) the unit's instructions may come from.
    lo: u64,
    hi: u64,
    alive: bool,
    // Stable data address embedded in published runtime-only entry guards.
    alive_guard: Option<Box<AtomicU64>>,
    /// Guard entry used only by immutable incoming rel32 edges.
    guard_host: u64,
    host_bytes: u64,
    /// All actual reader requests, or None for a conservative hull.
    source_ranges: Option<Box<[(u64, u64)]>>,
}

/// Bookkeeping that makes invalidation precise: which units cover which guest
/// pages, which chained exit slots jump into which unit, and each slot's
/// default (return-to-dispatcher) continuation.
#[derive(Default)]
struct JitIndex {
    units: Vec<UnitRec>,
    invalidated_entries: FastMap<u64, ()>,
    /// 16 KiB guest page -> unit ids overlapping it.
    pages: FastMap<u64, Vec<u32>>,
    /// Conservative dependencies too wide to enumerate into a bounded index.
    wide_units: Vec<u32>,
    /// unit entry host address -> chain slots currently pointing at it.
    incoming: FastMap<u64, Vec<IncomingLink>>,
    guarded_entries: FastMap<u64, u32>,
    slot_cont: FastMap<u64, u64>,
    /// Separate from interpreter HOT; removed with the matching SMC generation.
    cold: FastMap<u64, tiered::Cold>,
}

enum IncomingLink {
    Indirect(u64),
    ImmutableRel32 { _site: u64 },
}

static INDEX: Mutex<Option<JitIndex>> = Mutex::new(None);
static SHARED_HITS: AtomicU64 = AtomicU64::new(0);
static SHARED_LOCAL: AtomicU64 = AtomicU64::new(0);
static STATS_BLOCKS: AtomicU64 = AtomicU64::new(0);
static STATS_INSNS: AtomicU64 = AtomicU64::new(0);
static STATS_FALLBACK: AtomicU64 = AtomicU64::new(0);
/// Only the loops self-test requests generated region-entry instrumentation.
static COUNT_LOOP_REGIONS: AtomicBool = AtomicBool::new(false);
/// Loopra public region entries actually executed (not compilations or backedges).
/// Ordinary loops and the write-through regioncache do not count.
static STATS_LOOP_REGIONS: AtomicU64 = AtomicU64::new(0);
static STATS_MULTI_LOOP_REGIONS: AtomicU64 = AtomicU64::new(0);
static STATS_AOT_LOOP_REGIONS: AtomicU64 = AtomicU64::new(0);

pub fn stats_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ATTESOR_STATS").is_some())
}

static STATS_TRANSLATE_NS: AtomicU64 = AtomicU64::new(0);
static STATS_INTERP_BLOCKS: AtomicU64 = AtomicU64::new(0);
static STATS_JIT_ENTRIES: AtomicU64 = AtomicU64::new(0);
static STATS_QUICK_BLOCKS: AtomicU64 = AtomicU64::new(0);
static STATS_BG_ADOPTIONS: AtomicU64 = AtomicU64::new(0);
static STATS_TIER_PROMOTIONS: AtomicU64 = AtomicU64::new(0);

/// Executions of a block in the interpreter before it is JIT-compiled.
fn hot_threshold() -> u32 {
    static T: OnceLock<u32> = OnceLock::new();
    *T.get_or_init(|| std::env::var("ATTESOR_JIT_THRESHOLD").ok().and_then(|v| v.parse().ok()).unwrap_or(24))
}

thread_local! {
    static HOT: RefCell<FastMap<u64, u32>> = RefCell::new(FastMap::default());
    static PENDING: Cell<Option<Exit>> = const { Cell::new(None) };
    static IBTC: RefCell<Option<IndirectCache>> = const { RefCell::new(None) };
    static MY_EPOCH: Cell<u64> = const { Cell::new(0) };
}

/// Effective allocation policy, also used to isolate persistent AOT identity.
/// A rejected superpage request still belongs to the opt-in configuration.
pub(crate) fn hugepages_enabled() -> bool {
    code_cache_alloc::enabled()
}

pub fn code_cache() -> &'static CodeCache {
    CODE.get_or_init(|| {
        let p = code_cache_alloc::allocate(CODE_CACHE_SIZE as usize);
        assert!(p != libc::MAP_FAILED, "cannot allocate the JIT code cache: {}", std::io::Error::last_os_error());
        let base = p as u64;
        CodeCache { base, end: base + CODE_CACHE_SIZE, code_ptr: AtomicU64::new(base), slot_ptr: AtomicU64::new(base + CODE_CACHE_SIZE) }
    })
}

impl CodeCache {
    /// Reserve `len` bytes of code space (16-byte aligned).
    pub fn alloc(&self, len: u64) -> u64 {
        let len = (len + 15) & !15;
        let p = self.code_ptr.fetch_add(len, Ordering::SeqCst);
        if p + len > self.slot_ptr.load(Ordering::SeqCst) {
            panic!("attesor: JIT code cache exhausted");
        }
        p
    }
    pub fn cursor(&self) -> u64 {
        self.code_ptr.load(Ordering::SeqCst)
    }
    pub fn contains(&self, a: u64) -> bool {
        a >= self.base && a < self.end
    }
}

pub fn lookup(pc: u64) -> Option<u64> {
    BLOCKS.read().unwrap().as_ref().and_then(|registry| registry.entries.get(&pc).copied())
}

/// Register externally produced code (AOT) for `pc`.
pub fn register_block(pc: u64, host: u64) -> bool {
    register_aot_block(pc, host, None)
}

pub(crate) fn aot_image_allowed(image: Option<(u64, u64)>) -> bool {
    if !precise_invalidation_enabled() { return true; }
    match image {
        None => !EXTERNAL_INVALIDATED.load(Ordering::Relaxed),
        Some((lo, hi)) => lo < hi && BLOCKS.read().unwrap().as_ref().is_none_or(|registry|
            registry.external_invalidations.iter().all(|&(a, b)| lo >= b || hi <= a)),
    }
}

/// `image` is a dependency proof, not an entry-PC heuristic: the precise-mode
/// AOT builder refuses EVERY instruction read outside this exact image range.
pub(crate) fn register_aot_block(pc: u64, host: u64, image: Option<(u64, u64)>) -> bool {
    let mut g = BLOCKS.write().unwrap();
    // AOT loading holds IMAGES. Do not take TRANSLATE/INDEX here: translation
    // records observed PCs through IMAGES and that would invert its lock order.
    // Keep provenance in BLOCKS itself, atomically with entry publication.
    let precise = precise_invalidation_enabled();
    if precise && image.is_none() && EXTERNAL_INVALIDATED.load(Ordering::Relaxed) {
        return false;
    }
    let registry = g.get_or_insert_with(BlockRegistry::default);
    if precise {
        if let Some((lo, hi)) = image {
            if !(lo <= pc && pc < hi) || registry.external_invalidations.iter()
                .any(|&(a, b)| lo < b && hi > a) { return false; }
        }
    }
    if let std::collections::hash_map::Entry::Vacant(entry) = registry.entries.entry(pc) {
        entry.insert(host);
        if precise { registry.external.insert(pc, ExternalBlock { host, image }); }
        return true;
    }
    false
}

/// Adopt immutable, pointer-free Plain bytes with complete local source indexing.
/// No alive guard is advertised, so rel32chain cannot directly link into this unit.
fn register_plain_unit(
    _translation: &std::sync::MutexGuard<'_, ()>, unit: &sharedtrans::Plain,
) -> Option<u64> {
    let hi = unit.pc.checked_add(unit.source.len() as u64)?;
    if unit.source.is_empty() || unit.source.len() % 4 != 0 || unit.code.is_empty() { return None; }
    if !sharedtrans::source_unchanged(unit) { return None; }
    let (local, slot_offset, cont_offset) = sharedtrans::localize_exits(&unit.code)?;
    let at = code_cache().alloc(local.len() as u64);
    unsafe {
        std::ptr::copy_nonoverlapping(local.as_ptr(), at as *mut u8, local.len());
        std::ptr::write((at + slot_offset as u64) as *mut u64, at + cont_offset as u64);
    }
    let mut index = INDEX.lock().unwrap();
    let ix = index.get_or_insert_with(JitIndex::default);
    let mut blocks = BLOCKS.write().unwrap();
    let registry = blocks.get_or_insert_with(BlockRegistry::default);
    if registry.entries.contains_key(&unit.pc) { return None; }
    let id = u32::try_from(ix.units.len()).ok()?;
    ix.units.push(UnitRec {
        entry: unit.pc, host: at, lo: unit.pc, hi, alive: true,
        alive_guard: None, guard_host: 0, host_bytes: local.len() as u64,
        source_ranges: Some(vec![(unit.pc, hi)].into_boxed_slice()),
    });
    for page in (unit.pc >> 14)..=((hi - 1) >> 14) { ix.pages.entry(page).or_default().push(id); }
    ix.slot_cont.insert(at + slot_offset as u64, at + cont_offset as u64);
    ix.cold.remove(&unit.pc);
    if ix.invalidated_entries.remove(&unit.pc).is_some() {
        cost::add("retranslate.units", 1);
        cost::add("retranslate.host-bytes", unit.code.len() as u64);
    }
    registry.entries.insert(unit.pc, at);
    Some(at)
}

// ---------------- helpers called from generated code ----------------

static HIST_ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("ATTESOR_FALLBACK_HIST").is_some());
static HIST: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);
static HIST_N: AtomicU64 = AtomicU64::new(0);

/// Count executions of instructions that fall back to the interpreter, by mnemonic,
/// and periodically write the top of the histogram to /tmp/attesor-hist-<pid>.txt.
fn note_fallback_exec(insn: u32) {
    thread_local! { static TICK: Cell<u32> = const { Cell::new(0) }; }
    // Sample 1 in 16 executions to keep the overhead small.
    let t = TICK.with(|c| { c.set(c.get().wrapping_add(1)); c.get() });
    if t % 16 != 0 {
        return;
    }
    let text = attesor_core::disasm::disasm(insn);
    // mnemonic plus operand shape (register numbers and immediates collapsed)
    let mn: String = text.chars().map(|c| if c.is_ascii_digit() { 'N' } else { c }).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
    let mn = if mn.len() > 60 { mn[..60].to_string() } else { mn };
    let mut g = HIST.lock().unwrap();
    *g.get_or_insert_with(HashMap::new).entry(mn).or_insert(0) += 1;
    if HIST_N.fetch_add(1, Ordering::Relaxed) % 5_000 == 4_999 {
        let mut v: Vec<_> = g.as_ref().unwrap().iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        let mut out = String::new();
        for (k, n) in v.iter().take(60) {
            out.push_str(&format!("{n:>12} {k}\n"));
        }
        let _ = std::fs::write(format!("/tmp/attesor-hist-{}.txt", std::process::id()), out);
    }
}

extern "C" fn helper_interp(cpu: *mut Cpu, insn: u32, pc: u64) -> u64 {
    if *HIST_ON {
        note_fallback_exec(insn);
    }
    let cpu = unsafe { &mut *cpu };
    cpu.set_pc(pc);
    let _profile = crate::profile::enter(crate::profile::State::Interp);
    match interp::exec(cpu, insn) {
        Ok(()) => 0,
        Err(e) => {
            PENDING.with(|p| p.set(Some(e)));
            cpu.exit_reason = EXIT_PENDING;
            1
        }
    }
}

extern "C" fn helper_commpage(addr: u64) -> u64 {
    attesor_core::mem::host(addr) as u64
}

pub fn commpage_checks() -> bool {
    attesor_core::mem::COMMPAGE_REDIRECT.load(Ordering::Relaxed) != 0
}

/// Read guest instruction words, verifying page readability once per page.
pub fn guest_reader() -> impl Fn(u64) -> Option<u32> {
    let last_ok = Cell::new(u64::MAX);
    move |a: u64| {
        let page = a & !0xfff;
        if last_ok.get() != page {
            let mut b = [0u8; 4];
            if !crate::guestmem::read(a, &mut b) {
                return None;
            }
            last_ok.set(page);
        }
        Some(unsafe { std::ptr::read_unaligned(a as *const u32) })
    }
}

// ---------------- translation ----------------

fn verify_mode() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| std::env::var_os("ATTESOR_JIT_VERIFY").is_some())
}

/// Keep dependency collection bounded. Losing precision is safe; losing a
/// source dependency is not. Once full, retain only the complete read hull.
const MAX_SOURCE_WORDS: usize = 8192;

pub(crate) fn precise_invalidation_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let has = |name| std::env::var(name).unwrap_or_default()
            .split(',').any(|s| s == "preciseinvalidate");
        has("ATTESOR_JIT_ON") && !has("ATTESOR_JIT_OFF")
    })
}

#[derive(Default)]
struct SourceReads {
    words: BTreeSet<u64>,
    bounds: Option<(u64, u64)>,
    conservative: bool,
}

struct SourceDependencies {
    lo: u64,
    hi: u64,
    ranges: Option<Box<[(u64, u64)]>>,
}

impl SourceReads {
    fn note(&mut self, pc: u64) {
        let Some(end) = pc.checked_add(4) else {
            self.bounds = Some((0, u64::MAX));
            self.conservative = true;
            self.words.clear();
            return;
        };
        self.bounds = Some(self.bounds.map_or((pc, end), |(lo, hi)| (lo.min(pc), hi.max(end))));
        if self.conservative { return; }
        if self.words.len() >= MAX_SOURCE_WORDS && !self.words.contains(&pc) {
            self.conservative = true;
            self.words.clear();
        } else {
            self.words.insert(pc);
        }
    }

    fn finish(self) -> Option<SourceDependencies> {
        let (lo, hi) = self.bounds?;
        let ranges = if self.conservative { None } else {
            let mut ranges: Vec<(u64, u64)> = Vec::new();
            for pc in self.words {
                // note() accepted only checked four-byte words. Merge both
                // adjacent and overlapping reads without widening holes.
                let end = pc + 4;
                if let Some(last) = ranges.last_mut().filter(|last| pc <= last.1) {
                    last.1 = last.1.max(end);
                } else {
                    ranges.push((pc, end));
                }
            }
            Some(ranges.into_boxed_slice())
        };
        Some(SourceDependencies { lo, hi, ranges })
    }
}

fn tiered_enabled() -> bool { tiered::enabled() && !verify_mode() }

fn emit_unit_with_dependencies(pc: u64, slot_padding: usize, quick: bool, guard: Option<u64>) -> (Unit, Option<SourceDependencies>, Option<iced_x86::code_asm::CodeLabel>) {
    let mut u = Unit::new(StubKind::Patchable, commpage_checks());
    u.slot_padding = slot_padding;
    u.quick = quick;
    u.no_internal_jumps = verify_mode() || quick;
    u.inline_ok = !verify_mode() && !quick;
    if quick { u.callret = false; }
    let l = u.a.create_label();
    u.labels.insert(pc, l);
    let reader = guest_reader();
    let dependencies = RefCell::new(SourceReads::default());
    let track = precise_invalidation_enabled();
    let read = |address| {
        // Failed reads affect generated fallback paths too. Record the request,
        // not a later re-read or a sampler's snapshot of mutable guest text.
        if track { dependencies.borrow_mut().note(address); }
        reader(address)
    };
    emit::emit_block(&mut u, pc, &read);
    // call continuations queued by translated BL/BLR (bounded by the emitter)
    let mut i = 0;
    while i < u.inline_queue.len() {
        let next = u.inline_queue[i];
        emit::emit_block(&mut u, next, &read);
        i += 1;
    }
    u.finish();
    // Keep OFF's body/loop/slot layout: only immutable incoming edges visit
    // this cold appendix. A prefix shifted every hot entry and loop alignment.
    let guard_label = guard.map(|alive| {
        let mut entry = u.a.create_label();
        u.a.set_label(&mut entry).unwrap();
        rel32chain::emit_guard(&mut u.a, l, pc, alive);
        entry
    });
    (u, if track { dependencies.into_inner().finish() } else { None }, guard_label)
}

fn translate(pc: u64) -> u64 { translate_requested(pc, None) }

fn translate_requested(pc: u64, expected: Option<(u64, u64)>) -> u64 {
    let runtime = cost::enabled() && !crate::symbols::is_image_pc(pc);
    let _profile = crate::profile::enter(if runtime { crate::profile::State::TranslateRuntime } else { crate::profile::State::Translate });
    let _timer = cost::Timer::new(if runtime { "translate.runtime.ns" } else { "translate.image.ns" });
    cost::add(if runtime { "translate.runtime.requests" } else { "translate.image.requests" }, 1);
    let t0 = std::time::Instant::now();
    let r = translate_inner(pc, runtime, expected);
    STATS_TRANSLATE_NS.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
    r
}

fn slot_reuse() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let has = |name| std::env::var(name).unwrap_or_default().split(',').any(|s| s == "slotreuse");
        has("ATTESOR_JIT_ON") && !has("ATTESOR_JIT_OFF")
    })
}

fn translate_inner(pc: u64, runtime: bool, expected: Option<(u64, u64)>) -> u64 {
    // Take/release IMAGES before TRANSLATE (the disk loader uses IMAGES -> INDEX).
    // Cold/tier-promotion units keep their existing accounting and guards.
    let identity = (sharedtrans::available() && !tiered_enabled() && expected.is_none())
        .then(|| crate::aot::shared_translation_identity(pc)).flatten();
    let _g = TRANSLATE.lock().unwrap();
    let current = lookup(pc);
    let promote = expected.is_some_and(|(host, epoch)| {
        current == Some(host) && EPOCH.load(Ordering::SeqCst) == epoch
            && INDEX.lock().unwrap().as_ref().is_some_and(|ix|
                ix.cold.get(&pc).is_some_and(|c| c.host == host && c.executions >= tiered::threshold()))
    });
    if !promote {
        if let Some(host) = current { return host; }
    }
    if let Some((key, image_end)) = identity {
        if let Some(unit) = sharedtrans::prepare(&key, pc, image_end) {
            if let Some(host) = register_plain_unit(&_g, &unit) {
                if unit.shared { fix2_diag::add(7); SHARED_HITS.fetch_add(1, Ordering::Relaxed); }
                else { fix2_diag::add(8); SHARED_LOCAL.fetch_add(1, Ordering::Relaxed); }
                STATS_BLOCKS.fetch_add(1, Ordering::Relaxed);
                STATS_INSNS.fetch_add((unit.source.len() / 4) as u64, Ordering::Relaxed);
                cost::add("translate.host-bytes", unit.code.len() as u64);
                cost::add(if runtime { "translate.runtime.units" } else { "translate.image.units" }, 1);
                cost::add(if runtime { "translate.runtime.host-bytes" } else { "translate.image.host-bytes" }, unit.code.len() as u64);
                crate::aot::note_translated(pc);
                return host;
            }
            // A concurrent AOT publication is authoritative; never overwrite it.
            if let Some(host) = lookup(pc) { return host; }
        }
    }
    let quick = tiered_enabled() && !promote;
    let retranslated = cost::enabled() && INDEX.lock().unwrap().as_ref().is_some_and(|ix| ix.invalidated_entries.contains_key(&pc));
    let _retranslate_timer = retranslated.then(|| cost::Timer::new("retranslate.ns"));
    let cc = code_cache();
    let ip = cc.cursor();
    let mut prefix = 0u64;
    let alive_guard = rel32chain::enabled().then(|| Box::new(AtomicU64::new(1)));
    let guard = alive_guard.as_ref().map(|cell| &**cell as *const AtomicU64 as u64);
    // Slots must be 8-byte aligned for atomic patches. The default path
    // regenerates with internal padding; slotreuse moves the entire unit.
    let (mut u, mut source, mut guard_label) = {
        let _timer = cost::Timer::new("translate.emit.ns");
        emit_unit_with_dependencies(pc, 0, quick, guard)
    };
    let mut res = {
        let _timer = cost::Timer::new("translate.assemble.ns");
        u.assemble_options(ip, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS | if slot_reuse() { BlockEncoderOptions::RETURN_RELOC_INFOS } else { 0 }).expect("assemble")
    };
    if let Some(first) = u.first_slot {
        let at = res.label_ip(&first).expect("slot label");
        if at % 8 != 0 {
            let pad = (8 - (at % 8)) as usize;
            cost::add("translate.alignment-retries", 1);
            let _timer = cost::Timer::new("translate.alignment-retry.ns");
            if slot_reuse() {
                // Moving the WHOLE unit preserves every unit-local relative
                // displacement (including RIP-relative slots/constants). Keep the
                // first assembly's bytes and add prefix to all resolved host labels.
                // There must be no external branch relocation requiring rebasing.
                assert!(res.inner.reloc_infos.is_empty());
                prefix = pad as u64;
                assert_eq!((res.label_ip(&first).unwrap() + prefix) % 8, 0);
                cost::add("translate.reused-emission", 1);
            } else {
                (u, source, guard_label) = emit_unit_with_dependencies(pc, pad, quick, guard);
                res = u.assemble_options(ip, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).expect("assemble");
            }
        }
    }
    STATS_BLOCKS.fetch_add(1, Ordering::Relaxed);
    STATS_INSNS.fetch_add(u.insns_translated, Ordering::Relaxed);
    STATS_FALLBACK.fetch_add(u.insns_fallback, Ordering::Relaxed);
    let mut bytes = std::borrow::Cow::Borrowed(res.inner.code_buffer.as_slice());
    let base = cc.alloc(bytes.len() as u64 + prefix);
    assert_eq!(base, ip, "code cache raced");
    let at = base + prefix;
    let mut ix_guard = INDEX.lock().unwrap();
    let ix = ix_guard.get_or_insert_with(JitIndex::default);
    for s in &u.finished_stubs {
        let cont = res.label_ip(&s.cont).expect("stub label") + prefix;
        let slot = res.label_ip(&s.slot).expect("slot label") + prefix;
        assert_eq!(slot % 8, 0, "unaligned chain slot");
        ix.slot_cont.insert(slot, cont);
        if rel32chain::enabled() && !verify_mode() && !quick {
            if let Some(target) = lookup(s.target) {
                if let Some(&id) = ix.guarded_entries.get(&target) {
                    let unit = &ix.units[id as usize];
                    let cold = ix.cold.get(&s.target).is_some_and(|c| c.host == target);
                    if target < at && unit.alive && unit.alive_guard.is_some() && !cold {
                        let offset = res.inner.new_instruction_offsets[s.link_site];
                        if offset == u32::MAX { continue; } // encoder rewrote instruction
                        let site = at + u64::from(offset);
                        if rel32chain::link_unpublished(bytes.to_mut(), at, site, unit.guard_host) {
                            fix2_diag::add(3);
                            ix.incoming.entry(target).or_default().push(IncomingLink::ImmutableRel32 { _site: site });
                        }
                    }
                }
            }
        }
    }
    // Only unpublished Vec bytes are changed; no executable instruction patch API.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), at as *mut u8, bytes.len()) };
    for s in &u.finished_stubs {
        let cont = res.label_ip(&s.cont).expect("stub label") + prefix;
        let slot = res.label_ip(&s.slot).expect("slot label") + prefix;
        unsafe { std::ptr::write_volatile(slot as *mut u64, cont) };
    }
    crate::aot::note_translated(pc);
    let mut g = BLOCKS.write().unwrap();
    let published = g.as_ref().and_then(|m| m.entries.get(&pc).copied());
    // AOT registration does not hold TRANSLATE. Never replace an external
    // publication that won while this unit was being assembled. The abandoned
    // code allocation is retained, just like all other old generated code.
    if published != current {
        return published.expect("only AOT insertion can race the translation lock");
    }
    let (mut lo, mut hi) = (pc, pc);
    for &lpc in u.labels.keys() {
        lo = lo.min(lpc);
        hi = hi.max(lpc);
    }
    if let Some((a, b)) = u.extra_guest_range { lo = lo.min(a); hi = hi.max(b); }
    // The last block can run up to MAX_BLOCK_INSNS instructions past its label.
    let hi = hi.saturating_add((emit::MAX_BLOCK_INSNS as u64 + 1) * 4);
    let (lo, hi, source_ranges) = match source {
        Some(source) => (source.lo, source.hi, source.ranges),
        None => (lo, hi, None),
    };
    let id = ix.units.len() as u32;
    if ix.invalidated_entries.remove(&pc).is_some() {
        cost::add("retranslate.units", 1);
        cost::add("retranslate.host-bytes", bytes.len() as u64);
    }
    cost::add("translate.host-bytes", bytes.len() as u64);
    cost::add(if runtime { "translate.runtime.units" } else { "translate.image.units" }, 1);
    cost::add(if runtime { "translate.runtime.host-bytes" } else { "translate.image.host-bytes" }, bytes.len() as u64);
    // Canonical dispatcher / mutable-slot entries retain OFF's ordinary entry.
    // Only immutable edges need the permanent revocation/epoch appendix.
    let public = res.label_ip(&u.labels[&pc]).expect("public body") + prefix;
    if alive_guard.is_some() { ix.guarded_entries.insert(public, id); }
    let guard_host = guard_label.map_or(0, |l| res.label_ip(&l).expect("guard entry") + prefix);
    ix.units.push(UnitRec { alive_guard, guard_host, source_ranges, entry: pc, host: public, lo, hi, alive: true, host_bytes: bytes.len() as u64 });
    if precise_invalidation_enabled() {
        if let Some(ranges) = &ix.units[id as usize].source_ranges {
            let mut pages = BTreeSet::new();
            for &(lo, hi) in ranges.iter() {
                pages.extend((lo >> 14)..=((hi - 1) >> 14));
            }
            for page in pages { ix.pages.entry(page).or_default().push(id); }
        } else if ((hi - 1) >> 14) - (lo >> 14) >= MAX_SOURCE_WORDS as u64 {
            ix.wide_units.push(id);
        } else {
            for page in (lo >> 14)..=((hi - 1) >> 14) {
                ix.pages.entry(page).or_default().push(id);
            }
        }
    } else {
        for page in (lo >> 14)..=((hi - 1) >> 14) {
            ix.pages.entry(page).or_default().push(id);
        }
    }
    if quick {
        ix.cold.insert(pc, tiered::Cold { host: public, executions: 0 });
    } else { ix.cold.remove(&pc); }
    if promote {
        if let Some(&id) = ix.guarded_entries.get(&current.unwrap()) {
            if let Some(alive) = &ix.units[id as usize].alive_guard { alive.store(0, Ordering::SeqCst); }
        }
        if let Some(slots) = ix.incoming.remove(&current.unwrap()) {
            for link in slots {
                if let IncomingLink::Indirect(slot) = link {
                    if let Some(&cont) = ix.slot_cont.get(&slot) {
                        unsafe { std::ptr::write_volatile(slot as *mut u64, cont) };
                    }
                }
            }
        }
    }
    g.get_or_insert_with(BlockRegistry::default).entries.insert(pc, public);
    if quick { STATS_QUICK_BLOCKS.fetch_add(1, Ordering::Relaxed); }
    if promote { STATS_TIER_PROMOTIONS.fetch_add(1, Ordering::Relaxed); }
    if promote { EPOCH.fetch_add(1, Ordering::SeqCst); }
    drop(g);
    drop(ix_guard);
    if bgtrans::enabled() {
        let targets: Vec<_> = u.finished_stubs.iter().map(|s| s.target).collect();
        bgtrans::enqueue(&targets, bgtrans::generation(), commpage_checks());
    }
    public
}

fn lookup_or_translate(pc: u64) -> u64 {
    lookup(pc).or_else(|| adopt_background(pc)).unwrap_or_else(|| translate(pc))
}

/// Adopt only a fully assembled worker result. Neither an unavailable result
/// nor a busy translation lock makes the dispatcher wait for speculative work.
fn adopt_background(pc: u64) -> Option<u64> {
    let staged = bgtrans::take(pc)?;
    let _translate = TRANSLATE.try_lock().ok()?;
    if let Some(host) = lookup(pc) { return Some(host); }
    if staged.generation != bgtrans::generation()
        || staged.commpage != commpage_checks()
        || bgtrans::snapshot(staged.page).as_ref() != Some(&staged.source) {
        return None;
    }
    // The restricted worker assembles only internal RIP-relative references at
    // base zero and rejects external relocation records. The cache base is
    // 16-byte aligned, preserving every validated slot alignment.
    let at = code_cache().alloc(staged.code.len() as u64);
    unsafe {
        std::ptr::copy_nonoverlapping(staged.code.as_ptr(), at as *mut u8, staged.code.len());
    }
    crate::aot::note_translated(pc);
    let mut guard = INDEX.lock().unwrap();
    let ix = guard.get_or_insert_with(JitIndex::default);
    // Match the foreground publisher: an AOT insertion can race TRANSLATE.
    // Recheck while holding publication lock; do not overwrite its newer entry.
    let mut blocks = BLOCKS.write().unwrap();
    if let Some(host) = blocks.as_ref().and_then(|m| m.entries.get(&pc).copied()) { return Some(host); }
    for &(slot, cont) in &staged.slots {
        let (slot, cont) = (at + slot, at + cont);
        unsafe { std::ptr::write_volatile(slot as *mut u64, cont); }
        ix.slot_cont.insert(slot, cont);
    }
    // The whole copied page is a dependency, including words consulted only by
    // lookahead/flag-liveness analysis, not just the emitted instruction range.
    let (lo, hi) = (staged.page, staged.page + bgtrans::PAGE_BYTES as u64);
    let id = ix.units.len() as u32;
    ix.units.push(UnitRec { alive_guard: None, guard_host: 0, entry: pc, host: at, lo, hi, alive: true,
        host_bytes: staged.code.len() as u64, source_ranges: Some(vec![(lo, hi)].into_boxed_slice()) });
    for page in (lo >> 14)..=((hi - 1) >> 14) {
        ix.pages.entry(page).or_default().push(id);
    }
    STATS_BLOCKS.fetch_add(1, Ordering::Relaxed);
    STATS_INSNS.fetch_add(staged.insns, Ordering::Relaxed);
    STATS_FALLBACK.fetch_add(staged.fallback, Ordering::Relaxed);
    // Publish LAST, with all slots, invalidation and ownership records in place.
    blocks.get_or_insert_with(BlockRegistry::default).entries.insert(pc, at);
    STATS_BG_ADOPTIONS.fetch_add(1, Ordering::Relaxed);
    Some(at)
}

/// Guest fork pauses and drains one bounded emitter unit; async AOT saving can
/// skip a busy attempt. Dispatcher adoption never waits for background results.
pub fn bgtrans_fork_guard(wait: bool) -> Result<impl Drop, ()> {
    bgtrans::fork_guard(wait)
}

/// Fork children never wait for or reuse a parent's vanished speculative worker.
pub fn bgtrans_after_fork_child() {
    // Only the caller survives fork, outside generated execution. Inherited
    // leases of vanished threads must not poison the child's publication gate.
    if let Some(graph) = CHAIN_GRAPH.get() {
        graph.store(Box::into_raw(Box::new(RwLock::new(()))), Ordering::Release);
    }
    bgtrans::after_fork_child();
}

/// Drop the translations built from guest code in [addr, addr+len) (self-modifying code,
/// JIT-generated code being rewritten). Chained jumps into dropped units are reset to
/// return to the dispatcher; threads currently inside old code leave it through their
/// exit slots.
pub fn invalidate_range(addr: u64, len: u64) { invalidate_for(addr, len, "other"); }

pub fn invalidate_for(addr: u64, len: u64, source: &'static str) {
    // Metric keys are a closed set; syscall sources such as "munmap" are accounted under "other".
    let source = if matches!(source, "ic-ivau" | "mprotect-exec") { source } else { "other" };
    let _profile = crate::profile::enter(crate::profile::State::Invalidate);
    let _timer = cost::Timer::new("invalidate.ns");
    if cost::enabled() {
        cost::add(&format!("invalidate.{source}.calls"), 1);
        cost::add(&format!("invalidate.{source}.requested-bytes"), len);
    }
    let _g = TRANSLATE.lock().unwrap();
    // Even no-victim/early-return invalidations retire speculative snapshots.
    bgtrans::invalidate();
    let mut ix_guard = INDEX.lock().unwrap();
    let precise = precise_invalidation_enabled();
    let ix = if precise {
        ix_guard.get_or_insert_with(JitIndex::default)
    } else {
        let Some(ix) = ix_guard.as_mut() else { return };
        ix
    };
    // Protection and mapping operations affect pages, not just the caller's
    // nominal bytes. Keep them conservative even with precise source ranges.
    // Guest pages are 16 KiB, a superset of the Intel host's 4 KiB pages.
    let page_operation = matches!(source, "mprotect-exec" | "mprotect" | "munmap" | "mmap-fixed");
    let requested_end = addr.checked_add(len.max(1));
    let bounds = if precise && page_operation {
        requested_end.and_then(|end| end.checked_add(0x3fff))
            .map(|end| (addr & !0x3fff, end & !0x3fff))
    } else {
        requested_end.map(|end| (addr, end))
    };
    // A wrapped request has no trustworthy upper bound. Default behavior stays
    // unchanged; the opt-in path conservatively retires all registered code.
    let invalidate_all = precise && bounds.is_none();
    let (addr, end) = if precise {
        bounds.unwrap_or((0, u64::MAX))
    } else {
        (addr, addr.saturating_add(len.max(1)))
    };
    let (p0, p1) = (addr >> 14, (end - 1) >> 14);
    let mut victims: Vec<u32> = if precise { ix.wide_units.clone() } else { Vec::new() };
    if p1 - p0 < ix.pages.len() as u64 {
        for p in p0..=p1 {
            if let Some(v) = ix.pages.get(&p) {
                victims.extend(v.iter().copied());
            }
        }
    } else {
        for (p, v) in ix.pages.iter() {
            if *p >= p0 && *p <= p1 {
                victims.extend(v.iter().copied());
            }
        }
    }
    victims.retain(|&id| {
        let u = &ix.units[id as usize];
        u.alive && (invalidate_all || (u.lo < end && u.hi > addr
            && (!precise || page_operation || u.source_ranges.as_ref().is_none_or(|ranges|
                ranges.iter().any(|&(lo, hi)| lo < end && hi > addr)))))
    });
    if victims.is_empty() && !precise {
        return;
    }
    victims.sort_unstable();
    victims.dedup();
    if cost::enabled() { cost::add(&format!("invalidate.{source}.units"), victims.len() as u64); }
    // Revoke every target before withdrawing public entries or publishing EPOCH.
    for &id in &victims {
        if let Some(alive) = &ix.units[id as usize].alive_guard { alive.store(0, Ordering::SeqCst); }
    }
    let mut blocks = BLOCKS.write().unwrap();
    let mut retired = Vec::with_capacity(victims.len());
    if precise {
        // Legacy external entries lack dependencies: retain their one-way
        // global rejection policy. Closed-image AOT has a conservative image
        // dependency, so unrelated dyld mappings need not throw all AOT away.
        EXTERNAL_INVALIDATED.store(true, Ordering::Relaxed);
        let registry = blocks.get_or_insert_with(BlockRegistry::default);
        registry.note_external_invalidation(addr, end);
        registry.external.retain(|&pc, external| {
            let revoke = external.image.is_none_or(|(lo, hi)| lo < end && hi > addr);
            if revoke { retired.push((pc, external.host)); }
            !revoke
        });
    }
    if victims.is_empty() && retired.is_empty() { return; }
    if std::env::var_os("ATTESOR_TRACE").is_some() {
        eprintln!("[attesor] invalidating {} units, {} external entries ({:#x}+{:#x})",
            victims.len(), retired.len(), addr, len);
    }
    for id in victims {
        let (entry, host) = {
            let u = &mut ix.units[id as usize];
            u.alive = false;
            cost::add("invalidate.units", 1);
            cost::add("invalidate.host-bytes", u.host_bytes);
            if cost::enabled() { ix.invalidated_entries.insert(u.entry, ()); }
            (u.entry, u.host)
        };
        retired.push((entry, host));
    }
    for (entry, host) in retired {
        if ix.cold.get(&entry).is_some_and(|c| c.host == host) { ix.cold.remove(&entry); }
        if let Some(registry) = blocks.as_mut() {
            if registry.entries.get(&entry) == Some(&host) {
                registry.entries.remove(&entry);
            }
        }
        if let Some(slots) = ix.incoming.remove(&host) {
            for link in slots {
                match link {
                    IncomingLink::Indirect(slot) => {
                        if let Some(cont) = ix.slot_cont.get(&slot) {
                            unsafe { std::ptr::write_volatile(slot as *mut u64, *cont) };
                        }
                    }
                    IncomingLink::ImmutableRel32 { .. } => {
                        // Already logically unlinked by the permanent target guard.
                        // The old branch/code/guard remain allocated forever.
                    }
                }
            }
        }
    }
    drop(blocks);
    // Stale pages entries (dead units) are dropped lazily.
    for p in p0..=p1.min(p0 + 64) {
        if let Some(v) = ix.pages.get_mut(&p) {
            let units = &ix.units;
            v.retain(|&id| units[id as usize].alive);
        }
    }
    if precise {
        let units = &ix.units;
        ix.wide_units.retain(|&id| units[id as usize].alive);
    }
    EPOCH.fetch_add(1, Ordering::SeqCst);
}

/// Point an exit slot straight at the block for `pc`, remembering the link so the
/// target can be unlinked if it is invalidated.
fn chain_slot(slot: u64, pc: u64, target: u64) {
    // Must precede TRANSLATE: generated helpers may invalidate while holding
    // a graph read lease. try_write never waits on a running/infinite guest.
    let _graph = if rel32chain::enabled() {
        let Ok(lease) = chain_graph().try_write() else { fix2_diag::add(9); return; };
        Some(lease)
    } else { None };
    let _g = TRANSLATE.lock().unwrap();
    if lookup(pc) != Some(target) {
        return;
    }
    let mut ix_guard = INDEX.lock().unwrap();
    let ix = ix_guard.get_or_insert_with(JitIndex::default);
    if ix.cold.get(&pc).is_some_and(|c| c.host == target) {
        return; // every cold entry must pass the canonical dispatcher
    }
    if !ix.slot_cont.contains_key(&slot) {
        return; // not a slot we own (AOT code)
    }
    fix2_diag::add(2);
    unsafe { &*(slot as *const AtomicU64) }.store(target, Ordering::Release);
    ix.incoming.entry(target).or_default().push(IncomingLink::Indirect(slot));
}

/// (blocks translated, ms translating, cold blocks interpreted, translated-block entries, fallback insns)
pub fn stats_snapshot() -> (u64, u64, u64, u64, u64) {
    (
        STATS_BLOCKS.load(Ordering::Relaxed),
        STATS_TRANSLATE_NS.load(Ordering::Relaxed) / 1_000_000,
        STATS_INTERP_BLOCKS.load(Ordering::Relaxed),
        STATS_JIT_ENTRIES.load(Ordering::Relaxed),
        STATS_FALLBACK.load(Ordering::Relaxed),
    )
}

pub fn flush_profile() {
    fix2_diag::flush();
    if sharedtrans::enabled() && std::env::var_os("ATTESOR_TRACE").is_some() {
        eprintln!("[attesor] sharedtrans: adopted={} local_plain={}",
            SHARED_HITS.load(Ordering::Relaxed), SHARED_LOCAL.load(Ordering::Relaxed));
    }
    bgtrans::stop();
    crate::aot::save_profile();
    if std::env::var_os("ATTESOR_STATS").is_some() {
        eprintln!(
            "[attesor] jit: {} blocks, {} insns translated ({} via interpreter helper), {:.1} ms translating; {} block entries; {} blocks interpreted (cold)",
            STATS_BLOCKS.load(Ordering::Relaxed),
            STATS_INSNS.load(Ordering::Relaxed),
            STATS_FALLBACK.load(Ordering::Relaxed),
            STATS_TRANSLATE_NS.load(Ordering::Relaxed) as f64 / 1e6,
            STATS_JIT_ENTRIES.load(Ordering::Relaxed),
            STATS_INTERP_BLOCKS.load(Ordering::Relaxed),
        );
        eprintln!("[attesor] aot: loop_regions={}", STATS_AOT_LOOP_REGIONS.load(Ordering::Relaxed));
        if bgtrans::enabled() {
            eprintln!("[attesor] bgtrans: adopted={}", STATS_BG_ADOPTIONS.load(Ordering::Relaxed));
        }
        if tiered_enabled() {
            eprintln!("[attesor] tiered: quick_blocks={} promotions={} threshold={}",
                STATS_QUICK_BLOCKS.load(Ordering::Relaxed),
                STATS_TIER_PROMOTIONS.load(Ordering::Relaxed), tiered::threshold());
        }
    }
}

/// Only a completed cold generation can request promotion. Never reuse legacy
/// interpreter HOT counts, which have different invalidation/lifetime semantics.
fn tiered_dispatch(pc: u64) -> u64 {
    let candidate = {
        let ix = INDEX.lock().unwrap();
        ix.as_ref().and_then(|ix| ix.cold.get(&pc))
            .filter(|c| c.executions >= tiered::threshold())
            .map(|c| (c.host, EPOCH.load(Ordering::SeqCst)))
    };
    match candidate {
        Some(expected) => translate_requested(pc, Some(expected)),
        None => lookup_or_translate(pc),
    }
}

fn note_cold_completion(pc: u64, host: u64) {
    if let Some(ix) = INDEX.lock().unwrap().as_mut() {
        if let Some(c) = ix.cold.get_mut(&pc).filter(|c| c.host == host) {
            c.executions = c.executions.saturating_add(1);
        }
    }
}

// ---------------- dispatcher ----------------

fn setup_cpu(cpu: &mut Cpu) {
    cpu.jit_exit = attesor_jit_exit as usize as u64;
    cpu.epoch_ptr = &EPOCH as *const AtomicU64 as u64;
    cpu.helpers[HELPER_INTERP] = helper_interp as usize as u64;
    cpu.helpers[HELPER_COMMPAGE] = helper_commpage as usize as u64;
    bliccache::setup(cpu);
    IBTC.with(|t| {
        let mut t = t.borrow_mut();
        if t.is_none() {
            *t = Some(IndirectCache::new());
        }
        cpu.ibtc = t.as_mut().unwrap().entries.as_mut_ptr() as u64;
    });
    if emit::ibtc2_enabled() { sync_ibtc_epoch(cpu); }
}

fn sync_ibtc_epoch(cpu: &mut Cpu) {
    let epoch = EPOCH.load(Ordering::Acquire);
    if MY_EPOCH.with(|e| e.get()) != epoch {
        ibtc_clear();
        MY_EPOCH.with(|e| e.set(epoch));
    }
    cpu.ibtc_epoch = epoch;
}

fn ibtc_insert(pc: u64, host: u64) {
    // Keep cold entries unreachable from other generated code, so dispatcher
    // completion counts do not miss chains, indirect hits, self loops or RETs.
    let _guard = if tiered_enabled() { Some(TRANSLATE.lock().unwrap()) } else { None };
    if tiered_enabled() {
        if lookup(pc) != Some(host) { return; }
        if INDEX.lock().unwrap().as_ref().is_some_and(|ix|
            ix.cold.get(&pc).is_some_and(|c| c.host == host)) { return; }
    }
    IBTC.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            t.insert(pc, host);
        }
    });
}

fn ibtc_clear() {
    IBTC.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            t.clear();
        }
    });
}

/// Production ibtc2 fill is serialized with invalidation. Reject a host selected
/// before it was invalidated/replaced, and cache only tracked, alive public JIT
/// entries. This snapshot's AOT registration has no UnitRec/range lifecycle, so
/// AOT entries remain dispatchable but are deliberately not cached by ibtc2.
fn ibtc2_fill(cpu: &mut Cpu, pc: u64, host: u64) -> bool {
    let _g = TRANSLATE.lock().unwrap();
    sync_ibtc_epoch(cpu);
    if host == 0 || lookup(pc) != Some(host) { return false; }
    let ix_guard = INDEX.lock().unwrap();
    let tracked = ix_guard.as_ref().is_some_and(|ix| {
        if ix.cold.get(&pc).is_some_and(|c| c.host == host) { return false; }
        ix.pages.get(&(pc >> 14)).is_some_and(|ids| ids.iter().any(|&id| {
            let u = &ix.units[id as usize];
            u.alive && u.entry == pc && u.host == host
        }))
    });
    // Already holding TRANSLATE: do not recursively take the tiered fill lock.
    if tracked {
        IBTC.with(|t| { if let Some(t) = t.borrow_mut().as_mut() { t.insert(pc, host); } });
    }
    true
}

fn cpu_clone(c: &Cpu) -> Box<Cpu> {
    let mut n = Cpu::new();
    n.x = c.x;
    n.set_pc(c.pc());
    n.set_nzcv(c.nzcv());
    n.v = c.v;
    n.fpcr = c.fpcr;
    n.fpsr = c.fpsr;
    n.tpidr_el0 = c.tpidr_el0;
    n.tpidrro_el0 = c.tpidrro_el0;
    n.excl_addr = c.excl_addr;
    n.excl_val = c.excl_val;
    n.excl_val2 = c.excl_val2;
    n.excl_size = c.excl_size;
    n
}

/// Run one block in the interpreter (reference), undo its memory writes,
/// then run the translated block and compare.
fn verify_block(cpu: &mut Cpu, host: u64) {
    use attesor_core::mem;
    mem::LOGGING_ENABLED.store(true, Ordering::Relaxed);
    let start_pc = cpu.pc();
    let mut r = cpu_clone(cpu);
    mem::log_start();
    let mut ref_exit = None;
    let mut executed = Vec::new();
    for _ in 0..emit::MAX_BLOCK_INSNS {
        crate::profile::set(crate::profile::State::Interp);
        let insn = unsafe { mem::r32(r.pc()) };
        executed.push((r.pc(), insn));
        let term = emit::is_terminator(insn);
        if let Err(e) = interp::exec(&mut r, insn) {
            ref_exit = Some(e);
            break;
        }
        if term {
            break;
        }
    }
    let log = mem::log_stop();
    // Final bytes written by the reference, then undo.
    let finals: Vec<(u64, Vec<u8>)> = log.iter().map(|(a, _, n)| {
        let mut b = vec![0u8; *n];
        unsafe { std::ptr::copy_nonoverlapping(mem::host(*a), b.as_mut_ptr(), *n) };
        (*a, b)
    }).collect();
    for (a, old, n) in log.iter().rev() {
        unsafe { std::ptr::copy_nonoverlapping(old.as_ptr(), mem::host(*a), *n) };
    }
    crate::profile::set(crate::profile::State::Jit);
    unsafe { attesor_jit_enter(cpu, host) };
    // Blocks with atomics race with other threads between the two runs.
    let racy = executed.iter().any(|&(_, i)| {
        (i & 0x3F00_0000 == 0x0800_0000) || (i & 0x3B20_0C00 == 0x3820_0000 && (i >> 26) & 1 == 0)
    });
    if racy {
        return;
    }
    let mut diffs = Vec::new();
    let jit_pending = cpu.exit_reason == EXIT_PENDING;
    if ref_exit.is_some() != (jit_pending || cpu.exit_reason == EXIT_SVC) {
        diffs.push(format!("exit: ref={:?} jit reason={}", ref_exit, cpu.exit_reason));
    }
    for i in 0..32 {
        if r.x[i] != cpu.x[i] {
            diffs.push(format!("x{i}: ref={:#x} jit={:#x}", r.x[i], cpu.x[i]));
        }
    }
    if r.nzcv() != cpu.nzcv() {
        diffs.push(format!("nzcv: ref={:x} jit={:x}", r.nzcv(), cpu.nzcv()));
    }
    if r.pc() != cpu.pc() && ref_exit.is_none() {
        diffs.push(format!("pc: ref={:#x} jit={:#x}", r.pc(), cpu.pc()));
    }
    for i in 0..32 {
        if r.v[i] != cpu.v[i] {
            diffs.push(format!("v{i}: ref={:x?} jit={:x?}", r.v[i], cpu.v[i]));
        }
    }
    for (a, want) in &finals {
        let mut got = vec![0u8; want.len()];
        unsafe { std::ptr::copy_nonoverlapping(mem::host(*a), got.as_mut_ptr(), want.len()) };
        if &got != want {
            diffs.push(format!("mem {:#x}: ref={:x?} jit={:x?}", a, want, got));
        }
    }
    if !diffs.is_empty() {
        eprintln!("\n[verify] MISMATCH in block at {:#x} {}", start_pc, crate::symbols::describe(start_pc));
        for (pc, insn) in &executed {
            eprintln!("    {:#x}: {:08x}  {}", pc, insn, attesor_core::disasm::disasm(*insn));
        }
        for d in diffs.iter().take(12) {
            eprintln!("  {d}");
        }
        std::process::exit(99);
    }
    if let Some(e) = PENDING.with(|p| p.take()) {
        PENDING.with(|p| p.set(Some(e)));
    }
}

/// Interpret one basic block. Returns false when the thread should stop.
fn interp_block(cpu: &mut Cpu) -> bool {
    let _profile = crate::profile::enter(crate::profile::State::Interp);
    for _ in 0..emit::MAX_BLOCK_INSNS {
        let insn = unsafe { attesor_core::mem::r32(cpu.pc()) };
        let term = emit::is_terminator(insn);
        if let Err(e) = interp::exec(cpu, insn) {
            return crate::engine::handle_exit(cpu, e);
        }
        if term {
            break;
        }
    }
    true
}

pub fn run(cpu: &mut Cpu) {
    if !JIT_ENABLED.load(Ordering::Relaxed) {
        crate::engine::interp_loop(cpu);
        return;
    }
    {
        let _profile = crate::profile::enter(crate::profile::State::Translate);
        crate::aot::ensure_loaded();
    }
    setup_cpu(cpu);
    bgtrans::start();
    let (mut last_pc, mut repeats) = (u64::MAX, 0u64);
    let mut pending_bliccache: Option<(u64, u64)> = None;
    loop {
        crate::profile::set(crate::profile::State::Dispatch);
        if cpu.pc() == last_pc {
            repeats += 1;
            if repeats == 20000 && std::env::var_os("ATTESOR_SPIN_DEBUG").is_some() {
                let found = lookup(cpu.pc());
                let hot = HOT.with(|h| h.borrow().get(&cpu.pc()).copied());
                let msg = format!("[spin] pc={:#x} lookup={:?} hot={:?} exit_reason={} exit_data={:#x} epoch={} my_epoch={}\n", cpu.pc(), found, hot, cpu.exit_reason, cpu.exit_data, EPOCH.load(Ordering::Relaxed), MY_EPOCH.with(|e| e.get()));
                unsafe { libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len()) };
                if let Some(h) = found {
                    let bytes = unsafe { std::slice::from_raw_parts(h as *const u8, 160) };
                    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                    let m2 = format!("[spin] code {hex}\n");
                    unsafe { libc::write(2, m2.as_ptr() as *const libc::c_void, m2.len()) };
                }
            }
        } else {
            last_pc = cpu.pc();
            repeats = 0;
        }
        let epoch = EPOCH.load(if rel32chain::enabled() { Ordering::Acquire } else { Ordering::Relaxed });
        if MY_EPOCH.with(|e| e.get()) != epoch {
            ibtc_clear();
            MY_EPOCH.with(|e| e.set(epoch));
        }
        let pc = cpu.pc();
        // Consume before cold interpretation: its successor is not this call target.
        let cache_site = pending_bliccache.take().filter(|&(_, target)| target == pc).map(|(site, _)| site);
        let host = if tiered_enabled() {
            tiered_dispatch(pc)
        } else {
            match lookup(pc).or_else(|| adopt_background(pc)) {
                Some(h) => h,
                None => {
                    // Tiering: interpret cold blocks, compile hot ones.
                    let n = HOT.with(|h| {
                        let mut h = h.borrow_mut();
                        let c = h.entry(pc).or_insert(0);
                        *c += 1;
                        *c
                    });
                    if n == 1 {
                        crate::aot::note_translated(pc);
                    }
                    if n < hot_threshold() {
                        STATS_INTERP_BLOCKS.fetch_add(1, Ordering::Relaxed);
                        if !interp_block(cpu) {
                            return;
                        }
                        continue;
                    }
                    translate(pc)
                }
            }
        };
        // Promotion may have advanced EPOCH after the top-of-loop observation.
        if tiered_enabled() {
            let epoch = EPOCH.load(Ordering::SeqCst);
            if MY_EPOCH.with(|e| e.get()) != epoch {
                ibtc_clear();
                MY_EPOCH.with(|e| e.set(epoch));
            }
        }
        if !verify_mode() && emit::ibtc2_enabled() && !ibtc2_fill(cpu, pc, host) {
            continue;
        }
        if let Some(site) = cache_site {
            bliccache::publish(site, pc, host);
        }
        STATS_JIT_ENTRIES.fetch_add(1, Ordering::Relaxed);
        crate::profile::set(crate::profile::State::Jit);
        if verify_mode() {
            verify_block(cpu, host);
        } else {
            if !emit::ibtc2_enabled() { ibtc_insert(pc, host); }
            unsafe { attesor_jit_enter(cpu, host) };
            if tiered_enabled() && cpu.exit_reason != emit::EXIT_INTERP {
                note_cold_completion(pc, host);
            }
            if stats_enabled() {
                STATS_AOT_LOOP_REGIONS.fetch_add(std::mem::take(&mut cpu.aot_loop_regions), Ordering::Relaxed);
            }
        }
        crate::profile::set(crate::profile::State::Dispatch);
        match cpu.exit_reason {
            emit::EXIT_BLICCACHE => {
                pending_bliccache = Some((cpu.exit_data, cpu.pc()));
            }
            EXIT_CONTINUE => {
                let slot = cpu.exit_data;
                if slot != 0 { fix2_diag::add(1); }
                if slot != 0 && !verify_mode() {
                    // Chain: point the exit slot straight at the target block.
                    let target = lookup_or_translate(cpu.pc());
                    chain_slot(slot, cpu.pc(), target);
                }
            }
            EXIT_SVC => {
                let imm = cpu.exit_data as u16;
                if !crate::engine::handle_exit(cpu, Exit::Svc(imm)) {
                    return;
                }
            }
            EXIT_PENDING => {
                let e = PENDING.with(|p| p.take()).expect("pending exit");
                if !crate::engine::handle_exit(cpu, e) {
                    return;
                }
            }
            emit::EXIT_INTERP => {
                if !interp_block(cpu) {
                    return;
                }
            }
            r => panic!("bad exit reason {r}"),
        }
        cpu.exit_reason = EXIT_CONTINUE;
        cpu.exit_data = 0;
    }
}

#[cfg(test)]
mod precise_invalidation_tests {
    use super::*;

    #[test]
    fn external_tombstones_are_bounded_and_fail_closed() {
        let mut registry = BlockRegistry::default();
        for i in 0..1025 { registry.note_external_invalidation(i * 32, i * 32 + 4); }
        assert_eq!(registry.external_invalidations, [(0, u64::MAX)]);
        registry.note_external_invalidation(100, 104);
        assert_eq!(registry.external_invalidations, [(0, u64::MAX)]);
    }

    #[test]
    fn source_reads_holes_budget_and_overflow() {
        let mut reads = SourceReads::default();
        // Failed reads must be noted by the wrapper just like successful reads.
        reads.note(0x1000); reads.note(0x1004); reads.note(0x9000);
        let deps = reads.finish().unwrap();
        assert_eq!((deps.lo, deps.hi), (0x1000, 0x9004));
        assert_eq!(deps.ranges.unwrap().as_ref(), &[(0x1000, 0x1008), (0x9000, 0x9004)]);
        let mut reads = SourceReads::default();
        for pc in 0..=MAX_SOURCE_WORDS { reads.note(pc as u64 * 8); }
        reads.note(0x1000_0000_0000);
        let deps = reads.finish().unwrap();
        assert!(deps.ranges.is_none());
        assert_eq!(deps.hi, 0x1000_0000_0004);
        let mut reads = SourceReads::default();
        reads.note(u64::MAX - 1); reads.note(0x1000);
        let deps = reads.finish().unwrap();
        assert_eq!((deps.lo, deps.hi), (0, u64::MAX));
        assert!(deps.ranges.is_none());
    }

    // Run in fresh processes with default, ON, and ON+OFF. This exercises the
    // actual publisher/invalidation/chain/epoch path, not a separate model.
    #[test]
    fn runtime_directed_and_stale_code_sensitivity() {
        // Other directed fixtures now perform real invalidations first. Give
        // this external-AOT lifecycle test a clean initial latch and restore the
        // process state afterwards; production's one-way latch is unchanged.
        struct ExternalState(bool);
        impl Drop for ExternalState {
            fn drop(&mut self) { EXTERNAL_INVALIDATED.store(self.0, Ordering::SeqCst); }
        }
        let _external = ExternalState(EXTERNAL_INVALIDATED.swap(false, Ordering::SeqCst));
        let has = |name| std::env::var(name).unwrap_or_default().split(',').any(|s| s == "preciseinvalidate");
        let precise = has("ATTESOR_JIT_ON") && !has("ATTESOR_JIT_OFF");
        assert_eq!(precise_invalidation_enabled(), precise);
        let raw = unsafe { libc::mmap(std::ptr::null_mut(), 0x10000, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
        assert_ne!(raw, libc::MAP_FAILED);
        let pc = (raw as u64 + 0x3fff) & !0x3fff;
        let neighbor = pc + 0x100; // Inside the old 388-byte conservative hull.
        let write = |at: u64, n: u32| unsafe {
            *(at as *mut u32) = 0xd2800000 | (n << 5); // MOV X0,#n
            *((at + 4) as *mut u32) = 0xd4001001;
        };
        let execute = |host| {
            let mut cpu = Cpu::new(); cpu.set_pc(pc); setup_cpu(&mut cpu);
            unsafe { attesor_jit_enter(&mut *cpu, host); }
            cpu.x[0]
        };
        write(pc, 7); write(neighbor, 11);
        let host = translate(pc);
        assert_eq!(execute(host), 7);
        let other = translate(neighbor);
        let external_pc = pc + 0x8000;
        register_block(external_pc, host + 1);
        assert_eq!(lookup(external_pc), Some(host + 1));
        let epoch = EPOCH.load(Ordering::SeqCst);
        invalidate_range(neighbor, 4);
        assert_eq!(lookup(pc), if precise { Some(host) } else { None }, "byte invalidation must preserve a dependency-free neighbor only in precise mode");
        assert_eq!(lookup(neighbor), None);
        assert!(EPOCH.load(Ordering::SeqCst) > epoch);
        assert_eq!(lookup(external_pc), if precise { None } else { Some(host + 1) });
        register_block(external_pc + 4, host + 2);
        assert_eq!(lookup(external_pc + 4).is_none(), precise, "external AOT must stay disabled after first invalidation");
        let host = translate(pc);
        let other_new = translate(neighbor);
        assert_ne!(other_new, other);
        let mut slot = Box::new(0x1234u64);
        let slot_addr = &mut *slot as *mut u64 as u64;
        INDEX.lock().unwrap().as_mut().unwrap().slot_cont.insert(slot_addr, 0x1234);
        chain_slot(slot_addr, pc, host);
        assert_eq!(*slot, if tiered_enabled() { 0x1234 } else { host });
        write(pc, 42);
        // Sensitivity proof: omit invalidation deliberately and observe stale 7,
        // rather than claiming that mere metadata/lookup changes prove execution.
        assert_eq!(execute(host), 7);
        assert_ne!(execute(host), 42, "stale-code mutation must be detected");
        invalidate_range(pc, 4);
        assert_eq!(*slot, 0x1234, "incoming slot must be unlinked");
        assert_eq!(lookup(pc), None);
        assert_eq!(lookup(neighbor), Some(other_new));
        let fresh = translate(pc);
        assert_ne!(fresh, host);
        assert_eq!(execute(fresh), 42);
        // Page operations stay conservative even for a nominal one-byte request.
        invalidate_for(pc, 1, "mprotect");
        assert_eq!(lookup(pc), None);
        assert_eq!(lookup(neighbor).is_none(), precise);
        // Exercise the actual BSD dispatch, including failures that must not
        // invalidate. Every mapping belongs to this test's private arena.
        let syscall = |number, args: [u64; 8]| {
            let mut cpu = Cpu::new(); cpu.x[..8].copy_from_slice(&args); cpu.x[16] = number;
            crate::syscalls::handle_svc(&mut cpu);
            (cpu.cf, cpu.x[0])
        };
        let before = translate(pc);
        assert_ne!(syscall(74, [pc + 1, 4096, 3, 0, 0, 0, 0, 0]).0, 0);
        assert_eq!(lookup(pc), Some(before), "failed mprotect must not invalidate");
        assert_ne!(syscall(73, [pc + 1, 4096, 0, 0, 0, 0, 0, 0]).0, 0);
        assert_eq!(lookup(pc), Some(before), "failed munmap must not invalidate");
        assert_ne!(syscall(197, [pc, 0x4000, 3, 0x12, u64::MAX, 0, 0, 0]).0, 0);
        assert_eq!(lookup(pc), Some(before), "failed MAP_FIXED must not invalidate");
        assert_eq!(syscall(74, [pc, 0x4000, 3, 0, 0, 0, 0, 0]).0, 0);
        assert_eq!(lookup(pc).is_none(), precise, "successful non-exec mprotect");
        translate(pc);
        assert_eq!(syscall(197, [pc, 0x4000, 3, 0x1012, u64::MAX, 0, 0, 0]), (0, pc));
        assert_eq!(lookup(pc).is_none(), precise, "successful MAP_FIXED");
        write(pc, 42); write(neighbor, 11);
        translate(pc);
        assert_eq!(syscall(73, [pc, 0x4000, 0, 0, 0, 0, 0, 0]).0, 0);
        assert_eq!(lookup(pc).is_none(), precise, "successful munmap");
        // Replace only our own just-unmapped reservation for the overflow test.
        assert_eq!(syscall(197, [pc, 0x4000, 3, 0x1012, u64::MAX, 0, 0, 0]), (0, pc));
        write(pc, 42); write(neighbor, 11);
        if precise {
            translate(pc); translate(neighbor);
            invalidate_range(u64::MAX - 1, 8);
            assert_eq!(lookup(pc), None); assert_eq!(lookup(neighbor), None);
        }
        INDEX.lock().unwrap().as_mut().unwrap().slot_cont.remove(&slot_addr);
        invalidate_range(raw as u64, 0x10000);
        if let Some(registry) = BLOCKS.write().unwrap().as_mut() {
            // Also discard the test-only fake external entries in default mode.
            registry.entries.retain(|&pc, _| pc < raw as u64 || pc >= raw as u64 + 0x10000);
            registry.external.retain(|&pc, _| pc < raw as u64 || pc >= raw as u64 + 0x10000);
        }
        unsafe { libc::munmap(raw, 0x10000); }
    }
}

#[cfg(test)]
mod tiered_tests {
    use super::*;

    #[test]
    fn directed_promotion_smc_indirect_and_generation() {
        let has = |name| std::env::var(name).unwrap_or_default().split(',').any(|s| s == "tiered");
        assert_eq!(tiered_enabled(), has("ATTESOR_JIT_ON") && !has("ATTESOR_JIT_OFF") && !verify_mode());
        let threshold = std::env::var("ATTESOR_JIT_TIER_THRESHOLD").ok()
            .and_then(|s| s.parse::<u32>().ok()).filter(|&n| n != 0).unwrap_or(32);
        assert_eq!(tiered::threshold(), threshold);
        if !tiered_enabled() { return; }
        assert!(threshold <= 1000, "directed test requires a bounded test threshold");
        let p = unsafe { libc::mmap(std::ptr::null_mut(), 0x10000, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0) };
        assert_ne!(p, libc::MAP_FAILED);
        let pc = (p as u64 + 0x3fff) & !0x3fff;
        let write = |at: u64, words: &[u32]| unsafe {
            std::ptr::copy_nonoverlapping(words.as_ptr(), at as *mut u32, words.len());
        };
        write(pc, &[0x91000400, 0x14000001, 0xd4001001]); // ADD X0,#1; B next; SVC
        let execute = |at, host| {
            let mut cpu = Cpu::new(); cpu.set_pc(at); cpu.x[0] = 10; setup_cpu(&mut cpu);
            unsafe { attesor_jit_enter(&mut *cpu, host); }
            (cpu.x[0], cpu.pc(), cpu.exit_reason, cpu.exit_data)
        };
        let cold = tiered_dispatch(pc);
        assert_eq!(INDEX.lock().unwrap().as_ref().unwrap().cold[&pc].executions, 0);
        assert_eq!(execute(pc, cold), (11, pc + 8, EXIT_CONTINUE, 0));
        ibtc_clear(); ibtc_insert(pc, cold);
        IBTC.with(|t| assert_ne!(t.borrow().as_ref().unwrap().entries[if emit::ibtc2_enabled() { ((pc >> 2) as usize & (IBTC2_SETS - 1)) * 2 } else { ((pc >> 2) as usize) & (IBTC_SIZE - 1) }], [pc, cold]));
        let mut slot = Box::new(0x1234u64);
        let slot_at = &mut *slot as *mut u64 as u64;
        INDEX.lock().unwrap().as_mut().unwrap().slot_cont.insert(slot_at, 0x1234);
        chain_slot(slot_at, pc, cold); assert_eq!(*slot, 0x1234, "cold target must not be chained");
        for _ in 0..threshold - 1 { note_cold_completion(pc, cold); }
        assert_eq!(tiered_dispatch(pc), cold, "must not promote early");
        // Counts are global, not thread-local. A late old-host return must not
        // update any future generation of this PC.
        std::thread::spawn(move || note_cold_completion(pc, cold)).join().unwrap();
        let epoch = EPOCH.load(Ordering::SeqCst);
        EPOCH.fetch_add(1, Ordering::SeqCst);
        assert_eq!(translate_requested(pc, Some((cold, epoch))), cold, "stale epoch must cancel promotion");
        // Deliberate stale-code sensitivity control, then promote from fresh
        // guest reads. This private mutation intentionally omits IC invalidation.
        write(pc, &[0x91000800]); // ADD X0,#2
        assert_eq!(execute(pc, cold).0, 11);
        let full = tiered_dispatch(pc);
        assert_ne!(full, cold);
        assert_eq!(execute(pc, full).0, 12, "promotion must re-read instructions, not clone cold bytes");
        assert!(EPOCH.load(Ordering::SeqCst) > epoch);
        assert!(!INDEX.lock().unwrap().as_ref().unwrap().cold.contains_key(&pc));
        chain_slot(slot_at, pc, full); assert_eq!(*slot, full);
        invalidate_range(pc, 4); assert_eq!(*slot, 0x1234);
        let next = tiered_dispatch(pc); assert_ne!(next, full);
        note_cold_completion(pc, cold);
        assert_eq!(INDEX.lock().unwrap().as_ref().unwrap().cold[&pc].executions, 0);
        invalidate_range(pc, 4);
        assert!(!INDEX.lock().unwrap().as_ref().unwrap().cold.contains_key(&pc));

        // Populate an actual IBTC hit to the next block. Quick BR and RET must
        // return before following it, otherwise completion accounting is lost.
        let other = pc + 0x100;
        write(other, &[0x91018c00, 0xd4001001]); // ADD X0,#99; SVC
        let other_host = tiered_dispatch(other);
        for branch in [0xd61f0020, 0xd65f0020] { // BR X1 / RET X1
            write(pc, &[branch]);
            invalidate_range(pc, 4);
            let host = tiered_dispatch(pc);
            let mut cpu = Cpu::new(); setup_cpu(&mut cpu); cpu.set_pc(pc); cpu.x[0] = 10; cpu.x[1] = other;
            IBTC.with(|t| t.borrow_mut().as_mut().unwrap().insert(other, other_host));
            unsafe { attesor_jit_enter(&mut *cpu, host); }
            assert_eq!((cpu.x[0], cpu.pc(), cpu.exit_reason, cpu.exit_data), (10, other, EXIT_CONTINUE, 0));
        }
        INDEX.lock().unwrap().as_mut().unwrap().slot_cont.remove(&slot_at);
        invalidate_range(p as u64, 0x10000);
        ibtc_clear();
        unsafe { libc::munmap(p, 0x10000); }
        eprintln!("tiered directed: cold/full execution, threshold, cross-thread counts, stale epoch/host, SMC, chain, BR/RET passed; stale-code oracle 11 != fresh 12");
    }
}

#[cfg(test)]
mod bgtrans_tests {
    use super::*;

    #[test]
    fn bgtrans_worker_adoption_smc_fork_and_sensitivity() {
        let has = |name| std::env::var(name).unwrap_or_default().split(',').any(|s| s == "bgtrans");
        let diagnostics = ["ATTESOR_LOOPRA_BISECT", "ATTESOR_LOOPRA_LOG", "ATTESOR_LOOPRA_ALLOW"]
            .iter().any(|k| std::env::var_os(k).is_some());
        let expected = has("ATTESOR_JIT_ON") && !has("ATTESOR_JIT_OFF") && !verify_mode()
            && !loops::census_enabled() && !diagnostics;
        assert_eq!(bgtrans::enabled(), expected);
        if !expected { return; }
        crate::aot::set_enabled(false); // no checkpoint threads/cache writes in fork child
        bgtrans::start();
        let p = unsafe { libc::mmap(std::ptr::null_mut(), 0x10000,
            libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0) };
        assert_ne!(p, libc::MAP_FAILED);
        let pc = p as u64;
        let write = |at: u64, words: &[u32]| unsafe {
            std::ptr::copy_nonoverlapping(words.as_ptr(), at as *mut u32, words.len());
        };
        let wait_ready = |at| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !bgtrans::ready_for_test(at) {
                assert!(std::time::Instant::now() < deadline, "actual QoS worker did not produce a result");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        let execute = |at, host| {
            let mut cpu = Cpu::new(); setup_cpu(&mut cpu); cpu.set_pc(at);
            unsafe { attesor_jit_enter(&mut *cpu, host); }
            (cpu.x[0], cpu.pc(), cpu.exit_reason)
        };
        write(pc, &[0xd28000e0, 0x14000001, 0xd4001001]); // MOV X0,#7; B next; SVC
        // No INDEX victim exists yet. A no-victim invalidation must nevertheless
        // reject an already-completed speculative result with unchanged bytes.
        let generation = bgtrans::generation();
        bgtrans::enqueue(&[pc], generation, commpage_checks());
        wait_ready(pc);
        invalidate_range(pc, 4);
        assert!(bgtrans::generation() > generation);
        assert!(adopt_background(pc).is_none());
        bgtrans::enqueue(&[pc], bgtrans::generation(), commpage_checks());
        wait_ready(pc);
        let before = STATS_BG_ADOPTIONS.load(Ordering::Relaxed);
        // Use the real lookup/adopt integration, not a direct staged-byte call.
        let host = lookup_or_translate(pc);
        assert_eq!(STATS_BG_ADOPTIONS.load(Ordering::Relaxed), before + 1);
        assert_eq!(execute(pc, host), (7, pc + 8, EXIT_CONTINUE));
        // Actual foreground publication -> finished_stubs -> queue -> worker ->
        // lookup adoption, with no explicit enqueue of this target by the test.
        let origin = pc + 0x200;
        let target = origin + 8;
        write(origin, &[0x14000002, 0xd503201f, 0xd2800060, 0xd4001001]);
        let origin_host = translate(origin);
        if tiered_enabled() {
            for _ in 0..tiered::threshold() { note_cold_completion(origin, origin_host); }
            assert_ne!(tiered_dispatch(origin), origin_host);
        }
        wait_ready(target);
        let count = STATS_BG_ADOPTIONS.load(Ordering::Relaxed);
        let successor = lookup_or_translate(target);
        assert_eq!(STATS_BG_ADOPTIONS.load(Ordering::Relaxed), count + 1);
        assert_eq!(execute(target, successor), (3, target + 8, EXIT_SVC));
        let mut slot = Box::new(0x1234u64);
        let slot_at = &mut *slot as *mut u64 as u64;
        INDEX.lock().unwrap().as_mut().unwrap().slot_cont.insert(slot_at, 0x1234);
        chain_slot(slot_at, pc, host); assert_eq!(*slot, host);
        // Entire snapshot page, including lookahead-only bytes, is a dependency.
        invalidate_range(pc + 0xf00, 4);
        assert_eq!(lookup(pc), None); assert_eq!(*slot, 0x1234);

        // The generation remains current, but source bytes differ. Omitting the
        // source revalidation would publish/exe stale 7 instead of fresh 42.
        bgtrans::stage_for_test(pc, bgtrans::generation(), commpage_checks());
        write(pc, &[0xd2800540]); // MOV X0,#42
        assert!(adopt_background(pc).is_none(), "changed source must never be adopted");
        assert_eq!(execute(pc, host).0, 7, "sensitivity control: stale bytes really execute differently");
        let fresh = translate(pc);
        assert_eq!(execute(pc, fresh).0, 42);
        invalidate_range(pc, 4);
        bgtrans::stage_for_test(pc, bgtrans::generation(), !commpage_checks());
        assert!(adopt_background(pc).is_none(), "commpage mode mismatch must be rejected");
        bgtrans::stage_for_test(pc, bgtrans::generation(), commpage_checks());
        {
            let _busy = TRANSLATE.lock().unwrap();
            assert!(adopt_background(pc).is_none(), "dispatcher must try-lock, not wait");
        }
        assert!(lookup(pc).is_none());

        // A live emitter makes optional AOT fork skip, but guest fork waits for
        // the gate and keeps the parent's worker available. Child must disable
        // inherited worker primitives and still execute foreground translations.
        let (ready, started) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _gate = bgtrans::emission_for_test(); ready.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(80));
        });
        started.recv().unwrap();
        assert!(bgtrans_fork_guard(false).is_err());
        let child = crate::aot::fork_guest();
        assert!(child >= 0);
        if child == 0 {
            let ok = !bgtrans::enabled() && execute(pc, translate(pc)).0 == 42;
            unsafe { libc::_exit(if ok { 0 } else { 1 }); }
        }
        holder.join().unwrap();
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert_eq!(status, 0, "fork child must translate without inherited queue locks");
        assert!(bgtrans::enabled());
        drop(bgtrans_fork_guard(false).unwrap()); // parent gate released
        bgtrans::enqueue(&[pc], bgtrans::generation(), commpage_checks());
        wait_ready(pc);
        let parent = lookup_or_translate(pc);
        assert_eq!(execute(pc, parent).0, 42);
        INDEX.lock().unwrap().as_mut().unwrap().slot_cont.remove(&slot_at);
        invalidate_range(pc, 0x10000);
        bgtrans::stop();
        crate::aot::set_enabled(true);
        unsafe { libc::munmap(p, 0x10000); }
        eprintln!("bgtrans directed: real worker/adoption, relocated slots, no-victim generation, whole-page SMC, content/mode/try-lock rejection, guest/optional fork passed; stale 7 != fresh 42 detected");
    }
}

#[cfg(test)]
mod port_g3_tests;

#[cfg(test)]
mod acceptance_diagnostic_switch_tests {
    use super::*;
    #[test]
    fn exact_on_and_off_precedence() {
        const CHILD: &str = "ATTESOR_ACCEPT_DIAG_CHILD";
        if let Ok(expected) = std::env::var(CHILD) {
            assert_eq!(format!("{}{}{}", cost::probes() as u8, loops::census_enabled() as u8, slot_reuse() as u8), expected);
            return;
        }
        for (on, off, expected) in [
            ("", "", "000"), ("jitcost", "", "100"), ("loopcensus", "", "010"),
            ("slotreuse", "", "001"), ("jitcost,loopcensus,slotreuse", "", "111"),
            ("jitcost,loopcensus,slotreuse", "jitcost,loopcensus,slotreuse", "000"),
            ("jitcost,loopcensus,slotreuse", "jitcost", "011"),
            ("jitcost,loopcensus,slotreuse", "loopcensus", "101"),
            ("jitcost,loopcensus,slotreuse", "slotreuse", "110"),
            ("xjitcost,xloopcensus,xslotreuse, jitcost, loopcensus, slotreuse", "", "000"),
        ] {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "jit::acceptance_diagnostic_switch_tests::exact_on_and_off_precedence", "--nocapture"])
                .env(CHILD, expected).env("ATTESOR_JIT_ON", on).env("ATTESOR_JIT_OFF", off).output().unwrap();
            assert!(result.status.success(), "ON={on} OFF={off}: {}{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
        }
    }
}
