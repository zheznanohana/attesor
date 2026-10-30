//! Guest sampling: atomic CPU PC loads and a Rust-only state word. Generated code
//! already publishes PC with aligned x86 MOV stores. jitcost adds opt-in phase probes.
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub static ENABLED: AtomicBool = AtomicBool::new(false);
#[derive(Clone, Copy)]
#[repr(u8)]
pub enum State { Dispatch, Jit, Interp, Translate, Mach, Bsd, Gpu, Idle, Machdep, Invalidate, Indirect, CallReturn, TranslateRuntime }
const STATES: [&str; 13] = ["dispatch", "jit", "interp", "translate", "syscall-mach", "syscall-bsd", "gpu", "idle", "syscall-machdep", "invalidate", "indirect", "call-return", "translate-runtime"];
thread_local! { static CURRENT: Cell<*const AtomicU64> = const { Cell::new(std::ptr::null()) }; }

#[inline(always)]
pub fn set(state: State) {
    if ENABLED.load(Ordering::Relaxed) {
        CURRENT.with(|c| {
            let p = c.get();
            if !p.is_null() { unsafe { (*p).store(state as u64, Ordering::Relaxed) }; }
        });
    }
}
pub struct Guard(Option<(*const AtomicU64, u64)>);
impl Drop for Guard {
    #[inline]
    fn drop(&mut self) {
        if let Some((p, old)) = self.0 { unsafe { (*p).store(old, Ordering::Relaxed) }; }
    }
}
#[inline(always)]
pub fn enter(state: State) -> Guard {
    if !ENABLED.load(Ordering::Relaxed) { return Guard(None); }
    CURRENT.with(|c| {
        let p = c.get();
        if p.is_null() { return Guard(None); }
        let old = unsafe { (*p).load(Ordering::Relaxed) };
        unsafe { (*p).store(state as u64, Ordering::Relaxed) };
        Guard(Some((p, old)))
    })
}
// Low byte is the broad state; class and number share the same atomic publication.
// This prevents samples combining a new state with a previous syscall identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Class { Bsd = 1, Mach, Machdep, Gpu, Selector }
impl Class {
    fn name(self) -> &'static str {
        match self { Self::Bsd => "bsd", Self::Mach => "mach", Self::Machdep => "machdep",
            Self::Gpu => "gpu-op", Self::Selector => "gpu-selector" }
    }
    fn parse(s: &str) -> Option<Self> {
        [Self::Bsd, Self::Mach, Self::Machdep, Self::Gpu, Self::Selector].into_iter().find(|c| c.name() == s)
    }
}
type Key = (Class, u32);
fn packed(state: State, (class, number): Key) -> u64 {
    state as u64 | (class as u64) << 8 | (number as u64) << 16
}
fn identity(word: u64) -> Option<Key> {
    let class = match (word >> 8) & 255 { 1 => Class::Bsd, 2 => Class::Mach, 3 => Class::Machdep,
        4 => Class::Gpu, 5 => Class::Selector, _ => return None };
    Some((class, (word >> 16) as u32))
}
fn call_name((class, n): Key) -> String {
    match class {
        Class::Bsd => crate::syscalls::syscall_name(n as u64).into(),
        Class::Mach => crate::syscalls::mach_name(n).into(),
        Class::Machdep => match n { 2 => "set_tpidrro_el0", 3 => "get_tpidrro_el0", _ => "?" }.into(),
        Class::Gpu => match n { 1 => "rpc", 2 => "free", 3 => "event", 4 => "batch", 5 => "defsel", _ => "?" }.into(),
        Class::Selector => "?".into(),
    }
}
#[derive(Clone, Default)]
struct Call {
    name: String,
    samples: u64,
    idle_samples: u64,
    calls: u64,
    idle_calls: u64,
    ns: u64,
    host_ns: u64,
}
struct Active { key: Key, start: u64, host_start: Option<u64>, host_ns: u64, idle: bool }
impl Active {
    fn host_begin(&mut self, start: u64) { self.host_start = Some(start); }
    fn host_end(&mut self, end: u64) {
        if let Some(start) = self.host_start.take() { self.host_ns += end.saturating_sub(start); }
    }
}
#[derive(Default)]
struct CallData {
    calls: BTreeMap<Key, Call>,
    active: BTreeMap<u64, Active>,
    next_id: u64,
    mmap_fast: (u64, u64), // first attempts, aligned successes (profiling only)
}
static CALL_DATA: Mutex<CallData> = Mutex::new(CallData {
    calls: BTreeMap::new(), active: BTreeMap::new(), next_id: 0, mmap_fast: (0, 0),
});
thread_local! { static ACTIVE: Cell<u64> = const { Cell::new(0) }; }

// mach_absolute_time is a cheap user-space clock on this host. No clock or TLS
// access is performed in the disabled path. Initialize its scale before sampling.
#[repr(C)]
struct Timebase { numer: u32, denom: u32 }
extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
}
static TIMEBASE: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
#[inline]
fn now() -> u64 {
    let &(numer, denom) = TIMEBASE.get_or_init(|| {
        let mut tb = Timebase { numer: 0, denom: 0 };
        unsafe { mach_timebase_info(&mut tb); }
        (tb.numer as u64, tb.denom as u64)
    });
    (unsafe { mach_absolute_time() } as u128 * numer as u128 / denom as u128) as u64
}

pub struct CallGuard { id: u64, previous: u64, state: Guard }
impl Drop for CallGuard {
    fn drop(&mut self) {
        let end = now();
        let mut d = CALL_DATA.lock().unwrap();
        if let Some(a) = d.active.remove(&self.id) { Data::add_active(&mut d.calls, &a, end); }
        drop(d);
        ACTIVE.with(|a| a.set(self.previous));
        // The state guard restores the complete previous identity on drop.
        let _ = &self.state;
    }
}
/// Called only on the enabled branch in handle_svc (and the optional bridge hook).
pub fn enter_call(state: State, class: Class, number: u32) -> CallGuard {
    let key = (class, number);
    let id = {
        let mut d = CALL_DATA.lock().unwrap();
        d.calls.entry(key).or_insert_with(|| Call { name: call_name(key), ..Call::default() });
        d.next_id += 1;
        let id = d.next_id;
        d.active.insert(id, Active { key, start: now(), host_start: None, host_ns: 0,
            idle: matches!(state, State::Idle) });
        id
    };
    let previous = ACTIVE.with(|a| a.replace(id));
    let state = CURRENT.with(|c| {
        let p = c.get();
        if p.is_null() { return Guard(None); }
        let old = unsafe { (*p).load(Ordering::Relaxed) };
        let parent = if class == Class::Selector { ((old >> 16) & 0xffff) << 48 } else { 0 };
        unsafe { (*p).store(packed(state, key) | parent, Ordering::Relaxed) };
        Guard(Some((p, old)))
    });
    CallGuard { id, previous, state }
}

pub struct HostGuard(Option<u64>);
impl Drop for HostGuard {
    #[inline]
    fn drop(&mut self) {
        if let Some(id) = self.0 {
            let end = now();
            if let Some(a) = CALL_DATA.lock().unwrap().active.get_mut(&id) {
                a.host_end(end);
            }
        }
    }
}
/// Scope the actual raw host call, after argument conversion, before result conversion.
#[inline]
pub fn host() -> HostGuard {
    if !ENABLED.load(Ordering::Relaxed) { return HostGuard(None); }
    let id = ACTIVE.with(Cell::get);
    if id == 0 { return HostGuard(None); }
    if let Some(a) = CALL_DATA.lock().unwrap().active.get_mut(&id) {
        a.host_begin(now()); return HostGuard(Some(id));
    }
    HostGuard(None)
}

#[inline]
pub fn host_call<T>(f: impl FnOnce() -> T) -> T {
    let _host = host();
    f()
}

/// Non-fixed mmap first-try hit rate; failed attempts remain in the denominator.
pub fn mmap_fast_path(aligned: bool) {
    if !ENABLED.load(Ordering::Relaxed) { return; }
    let mut d = CALL_DATA.lock().unwrap();
    d.mmap_fast.0 += 1;
    d.mmap_fast.1 += u64::from(aligned);
}

/// The caller waits until the bridge has finished, so its CPU remains alive.
/// The bridge publishes selector states into that same atomic while it works.
pub fn current_word() -> u64 { CURRENT.with(|c| c.get() as u64) }
pub fn bridge_register(word: u64) { CURRENT.with(|c| c.set(word as *const AtomicU64)); }
thread_local! { static SELECTOR: std::cell::RefCell<Option<CallGuard>> = const { std::cell::RefCell::new(None) }; }
/// Optional local dylib API, not a wire-protocol change: event 0 begins, 1 ends.
pub extern "C" fn bridge_selector(event: u32, name: *const libc::c_char) {
    if event == 1 { SELECTOR.with(|g| g.borrow_mut().take()); return; }
    if name.is_null() { return; }
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
    let number = {
        let mut d = CALL_DATA.lock().unwrap();
        if let Some((&(_, n), _)) = d.calls.iter().find(|((c, _), v)| *c == Class::Selector && v.name == name) { n }
        else {
            let n = d.calls.keys().filter(|(c, _)| *c == Class::Selector).map(|(_, n)| *n).max().unwrap_or(0) + 1;
            d.calls.insert((Class::Selector, n), Call { name: name.into_owned(), ..Call::default() });
            n
        }
    };
    SELECTOR.with(|g| *g.borrow_mut() = Some(enter_call(State::Gpu, Class::Selector, number)));
}
pub fn bridge_coverage(available: bool) {
    if let Some(d) = DATA.lock().unwrap().as_mut() { d.bridge_selectors = Some(available); }
}

pub fn register(cpu: &attesor_core::cpu::Cpu) {
    if ENABLED.load(Ordering::Relaxed) {
        CURRENT.with(|c| c.set(&cpu.profile_state));
    }
}
pub fn unregister() { CURRENT.with(|c| c.set(std::ptr::null())); }

#[derive(Default)]
struct Counts {
    pcs: BTreeMap<u64, u64>,
    states: [u64; 13],
    // Full tables are persisted, even though the human-readable tables are truncated.
    images: BTreeMap<String, [u64; 2]>,
    symbols: BTreeMap<String, u64>,
}
struct Data {
    dir: PathBuf,
    name: String,
    args: Vec<String>,
    counts: Counts,
    stats_base: (u64, u64),
    stats_prior: (u64, u64),
    mmap_fast_prior: (u64, u64),
    metrics_prior: BTreeMap<String, u64>,
    metrics_base: BTreeMap<String, u64>,
    resolved: BTreeMap<u64, (String, String)>,
    calls: BTreeMap<Key, Call>,
    bridge_selectors: Option<bool>,
}
static DATA: Mutex<Option<Data>> = Mutex::new(None);

pub fn init(name: &str, args: &[String]) {
    let Some(dir) = std::env::var_os("ATTESOR_PROFILE").filter(|s| !s.is_empty()) else { return };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("attesor: profile directory: {e}"); return;
    }
    let mut data = Data { dir: dir.into(), name: name.into(), args: args.to_vec(),
        counts: Counts::default(), stats_base: (0, 0), stats_prior: (0, 0), mmap_fast_prior: (0, 0), metrics_prior: BTreeMap::new(), metrics_base: BTreeMap::new(), resolved: BTreeMap::new(), calls: BTreeMap::new(), bridge_selectors: None };
    // A successful exec of the same executable keeps its pid and report path.
    // Continue that snapshot instead of losing the previous execution epoch.
    data.resume();
    let mut live = CallData::default();
    // Reuse selector ids across successful same-pid exec snapshots.
    for (&key, c) in &data.calls {
        if key.0 == Class::Selector { live.calls.insert(key, Call { name: c.name.clone(), ..Call::default() }); }
    }
    *CALL_DATA.lock().unwrap() = live;
    *DATA.lock().unwrap() = Some(data);
    let _ = now();
    ENABLED.store(true, Ordering::Relaxed);
    unsafe { libc::atexit(on_exit); }
    start();
}
extern "C" fn on_exit() { flush(); }
fn start() {
    if let Err(e) = std::thread::Builder::new().name("guest-profiler".into()).spawn(|| {
        let mut next_flush = Instant::now() + Duration::from_secs(10);
        let mut next = Instant::now();
        loop {
            {
                // This gate also prevents inheriting sampler-owned locks at fork.
                let mut data = DATA.lock().unwrap();
                let Some(d) = data.as_mut() else { return };
                for (pc, state) in crate::engine::profile_samples() {
                    if let Some(key) = identity(state) {
                        let call = d.calls.entry(key).or_insert_with(|| Call { name: call_name(key), ..Call::default() });
                        call.samples += 1;
                        call.idle_samples += u64::from(state & 255 == State::Idle as u64);
                        if key.0 == Class::Selector && state >> 48 != 0 {
                            let parent = (Class::Gpu, (state >> 48) as u32);
                            d.calls.entry(parent).or_insert_with(|| Call { name: call_name(parent), ..Call::default() }).samples += 1;
                        }
                    }
                    let state = (state & 255) as usize;
                    d.counts.states[state] += 1;
                    if state == State::Jit as usize || state == State::Interp as usize {
                        if crate::jit::loops::census_enabled() { *d.counts.pcs.entry(pc).or_default() += 1; }
                        let (image, symbol) = d.resolved.entry(pc).or_insert_with(|| crate::symbols::attribute(pc));
                        d.counts.images.entry(image.clone()).or_default()[usize::from(state == State::Interp as usize)] += 1;
                        *d.counts.symbols.entry(format!("{image}`{symbol}")).or_default() += 1;
                    }
                }
                if Instant::now() >= next_flush {
                    d.write();
                    next_flush = Instant::now() + Duration::from_secs(10);
                }
            }
            next += Duration::from_millis(1);
            let now = Instant::now();
            if next < now { next = now; }
            std::thread::sleep(next.saturating_duration_since(now));
        }
    }) { eprintln!("attesor: profiler thread: {e}"); }
}
pub fn flush() {
    if ENABLED.load(Ordering::Relaxed) {
        if let Some(d) = DATA.lock().unwrap().as_ref() { d.write(); }
    }
}
// Held across every Attesor fork, including AOT workers. The timing lock is
// held across runtime locks only by this fork gate. Order DATA -> CALL_DATA -> CPUS -> AOT.
pub struct ForkGuard {
    data: MutexGuard<'static, Option<Data>>,
    calls: Option<MutexGuard<'static, CallData>>,
}
pub fn fork_guard() -> ForkGuard {
    let data = DATA.lock().unwrap();
    let calls = data.as_ref().map(|_| CALL_DATA.lock().unwrap());
    ForkGuard { data, calls }
}
impl ForkGuard {
    // fork_safely already holds CALL_DATA, so use its existing guard.
    pub fn host_begin(&mut self) {
        if let Some(calls) = self.calls.as_mut() {
            if let Some(a) = calls.active.get_mut(&ACTIVE.with(Cell::get)) { a.host_begin(now()); }
        }
    }
    pub fn host_end(&mut self) {
        if let Some(calls) = self.calls.as_mut() {
            if let Some(a) = calls.active.get_mut(&ACTIVE.with(Cell::get)) {
                a.host_end(now());
            }
        }
    }
    pub fn child(&mut self) {
        if let Some(d) = self.data.as_mut() {
            d.counts = Counts::default();
            d.calls.clear();
            let calls = self.calls.as_mut().unwrap();
            calls.calls.clear();
            calls.mmap_fast = (0, 0);
            d.mmap_fast_prior = (0, 0);
            let current = ACTIVE.with(Cell::get);
            calls.active.retain(|&id, _| id == current);
            for a in calls.active.values_mut() {
                a.start = now(); a.host_ns = 0; a.host_start = None;
            }
            d.metrics_prior.clear();
            d.metrics_base = crate::jit::cost::snapshot();
            d.stats_prior = (0, 0);
            let s = crate::jit::stats_snapshot();
            d.stats_base = (s.0, s.1);
        }
    }
}
pub fn after_fork() { if ENABLED.load(Ordering::Relaxed) { start(); } }
fn clean(s: &str) -> String { s.replace(['\t', '\n', '\r'], " ") }
fn pct(n: u64, total: u64) -> f64 { if total == 0 { 0.0 } else { n as f64 * 100.0 / total as f64 } }
impl Data {
    fn add_active(calls: &mut BTreeMap<Key, Call>, a: &Active, end: u64) {
        let c = calls.entry(a.key).or_insert_with(|| Call { name: call_name(a.key), ..Call::default() });
        c.calls += 1;
        c.idle_calls += u64::from(a.idle);
        let ns = end.saturating_sub(a.start);
        c.ns += ns;
        c.host_ns += (a.host_ns + a.host_start.map(|s| end.saturating_sub(s)).unwrap_or(0)).min(ns);
    }
    fn snapshot_calls(&self) -> BTreeMap<Key, Call> {
        let mut calls = self.calls.clone();
        let live = CALL_DATA.lock().unwrap();
        for (&key, c) in &live.calls {
            let dest = calls.entry(key).or_default();
            dest.name = c.name.clone();
            dest.calls += c.calls;
            dest.idle_calls += c.idle_calls;
            dest.ns += c.ns;
            dest.host_ns += c.host_ns;
        }
        let end = now();
        for a in live.active.values() { Self::add_active(&mut calls, a, end); }
        calls
    }
    fn path(&self) -> PathBuf {
        let name: String = self.name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
        self.dir.join(format!("{name}-{}.txt", std::process::id()))
    }
    fn resume(&mut self) {
        let Ok(text) = std::fs::read_to_string(self.path()) else { return };
        if !text.starts_with("Attesor guest profile v1\n") && !text.starts_with("Attesor guest profile v2\n") { return; }
        for line in text.lines() {
            let f: Vec<_> = line.split('\t').collect();
            match f.as_slice() {
                ["state", state, n] => {
                    if let (Some(i), Ok(n)) = (STATES.iter().position(|s| s == state), n.parse()) { self.counts.states[i] = n; }
                }
                ["pc", pc, n] => {
                    if let (Ok(pc), Ok(n)) = (u64::from_str_radix(pc, 16), n.parse()) { self.counts.pcs.insert(pc, n); }
                }
                ["image", image, jit, interp] => {
                    if let (Ok(j), Ok(i)) = (jit.parse(), interp.parse()) { self.counts.images.insert(image.to_string(), [j, i]); }
                }
                ["symbol", symbol, n] => {
                    if let Ok(n) = n.parse() { self.counts.symbols.insert(symbol.to_string(), n); }
                }
                ["call", class, number, name, samples, idle_samples, calls, idle_calls, ns, host_ns] => {
                    if let (Some(class), Ok(number), Ok(samples), Ok(idle_samples), Ok(calls), Ok(idle_calls), Ok(ns), Ok(host_ns)) =
                        (Class::parse(class), number.parse(), samples.parse(), idle_samples.parse(), calls.parse(), idle_calls.parse(), ns.parse(), host_ns.parse()) {
                        self.calls.insert((class, number), Call { name: name.to_string(), samples, idle_samples, calls, idle_calls, ns, host_ns });
                    }
                }
                ["jit-metric", key, n] => {
                    if let Ok(n) = n.parse() { self.metrics_prior.insert(key.to_string(), n); }
                }
                ["bridge-selectors", available] => { self.bridge_selectors = available.parse().ok(); }
                ["translation", blocks, ms] => {
                    if let (Ok(b), Ok(m)) = (blocks.parse(), ms.parse()) { self.stats_prior = (b, m); }
                }
                ["mmap-fast", attempts, hits] => {
                    if let (Ok(a), Ok(h)) = (attempts.parse::<u64>(), hits.parse::<u64>()) {
                        if h <= a { self.mmap_fast_prior = (a, h); }
                    }
                }
                _ => {}
            }
        }
    }
    fn write(&self) {
        let c = &self.counts;
        let total: u64 = c.states.iter().sum();
        let exec = c.states[1] + c.states[2];
        let stats = crate::jit::stats_snapshot();
        let blocks = stats.0.saturating_sub(self.stats_base.0) + self.stats_prior.0;
        let ms = stats.1.saturating_sub(self.stats_base.1) + self.stats_prior.1;
        let mut out = format!("Attesor guest profile v2\nProcess: {} (pid {})\nArgs: {}\nTotal samples: {total}\nTranslation: {blocks} blocks, {ms} ms\n\nState                 Samples   % total\n", clean(&self.name), std::process::id(), self.args.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(" "));
        let mut states: Vec<_> = c.states.iter().enumerate().collect();
        states.sort_by_key(|&(i, n)| (std::cmp::Reverse(*n), i));
        for (i, &n) in states { let _ = writeln!(out, "{:<20} {n:>8} {:>9.2}", STATES[i], pct(n, total)); }
        let calls = self.snapshot_calls();
        out.push_str("\nCalls (wall time incl. waits; host = raw kernel boundary; emulation = remainder)\n");
        for group in [0, 1, 2] {
            let rows: Vec<_> = calls.iter().filter(|((class, _), _)| match class {
                Class::Gpu => group == 1, Class::Selector => group == 2, _ => group == 0,
            }).collect();
            let samples: u64 = rows.iter().map(|(_, c)| c.samples).sum();
            out.push_str(match group { 1 => "GPU operations (caller wall time):\n", 2 => "GPU selectors (nested bridge execution; kernel split unavailable):\n", _ => "Syscalls (including idle):\n" });
            out.push_str("Class/number Name                              Samples % samples     Calls    Total ms     Host ms Emulation ms\n");
            for (&(class, n), c) in rows {
                let _ = writeln!(out, "{}/{n:<5} {:<34} {:>7} {:>9.2} {:>9} {:>11.3} {:>11} {:>12}",
                    class.name(), clean(&c.name), c.samples, pct(c.samples, samples), c.calls,
                    c.ns as f64 / 1e6,
                    if matches!(class, Class::Gpu | Class::Selector) { "n/a".into() } else { format!("{:.3}", c.host_ns as f64 / 1e6) },
                    if matches!(class, Class::Gpu | Class::Selector) { "n/a".into() } else { format!("{:.3}", c.ns.saturating_sub(c.host_ns) as f64 / 1e6) });
            }
        }
        out.push_str("\nClassified idle: class/number name, idle calls, idle samples\n");
        for (&(class, n), c) in &calls {
            if c.idle_calls != 0 || c.idle_samples != 0 {
                let _ = writeln!(out, "{}/{n} {}: {} calls, {} samples", class.name(), clean(&c.name), c.idle_calls, c.idle_samples);
            }
        }
        let _ = writeln!(out, "GPU selector hook: {}", match self.bridge_selectors { Some(true) => "available", Some(false) => "unavailable (rebuild bridge for selector detail)", None => "not loaded" });
        out.push_str("\nTop images (jit + interp)       Samples   % exec    % jit % interp\n");
        let mut images: Vec<_> = c.images.iter().collect();
        images.sort_by_key(|(name, n)| (std::cmp::Reverse(n[0] + n[1]), *name));
        for (name, n) in images.iter().take(40) {
            let sum = n[0] + n[1];
            let _ = writeln!(out, "{:<40} {sum:>8} {:>8.2} {:>8.2} {:>8.2}", clean(name), pct(sum, exec), pct(n[0], sum), pct(n[1], sum));
        }
        out.push_str("\nTop symbols (jit + interp)      Samples   % exec\n");
        let mut symbols: Vec<_> = c.symbols.iter().collect();
        symbols.sort_by_key(|(name, n)| (std::cmp::Reverse(**n), *name));
        for (name, &n) in symbols.iter().take(60) { let _ = writeln!(out, "{:<60} {n:>8} {:>8.2}", clean(name), pct(n, exec)); }
        out.push_str("\n# Complete merger records (tab separated; cumulative snapshot)\n");
        let _ = writeln!(out, "process\t{}\t{}", clean(&self.name), self.args.iter().map(|a| clean(a)).collect::<Vec<_>>().join("\t"));
        let _ = writeln!(out, "translation\t{blocks}\t{ms}");
        let (attempts, hits) = CALL_DATA.lock().unwrap().mmap_fast;
        let _ = writeln!(out, "mmap-fast\t{}\t{}", attempts + self.mmap_fast_prior.0, hits + self.mmap_fast_prior.1);
        for (i, &n) in c.states.iter().enumerate() { let _ = writeln!(out, "state\t{}\t{n}", STATES[i]); }
        for (&(class, number), c) in &calls {
            let _ = writeln!(out, "call\t{}\t{number}\t{}\t{}\t{}\t{}\t{}\t{}\t{}", class.name(), clean(&c.name), c.samples, c.idle_samples, c.calls, c.idle_calls, c.ns, c.host_ns);
        }
        if let Some(available) = self.bridge_selectors { let _ = writeln!(out, "bridge-selectors\t{available}"); }
        for (name, n) in &c.images { let _ = writeln!(out, "image\t{}\t{}\t{}", clean(name), n[0], n[1]); }
        for (pc, n) in &c.pcs { let _ = writeln!(out, "pc\t{pc:x}\t{n}"); }
        for (name, n) in &c.symbols { let _ = writeln!(out, "symbol\t{}\t{n}", clean(name)); }
        let mut metrics = self.metrics_prior.clone();
        for (key, n) in crate::jit::cost::snapshot() {
            *metrics.entry(key.clone()).or_default() += n.saturating_sub(*self.metrics_base.get(&key).unwrap_or(&0));
        }
        for (key, n) in ["branch.indirect", "branch.miss", "branch.call", "branch.return"].into_iter().zip(crate::engine::profile_jit_counts()) {
            *metrics.entry(key.into()).or_default() += n;
        }
        for (key, n) in metrics { let _ = writeln!(out, "jit-metric\t{key}\t{n}"); }
        let path = self.path();
        let tmp = path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, out).and_then(|_| std::fs::rename(&tmp, &path)) {
            eprintln!("attesor: profile {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod call_tests {
    use super::*;

    #[test]
    fn multiple_host_calls_accumulate_instead_of_counting_only_the_last() {
        let key = (Class::Bsd, 197);
        let mut a = Active { key, start: 100, host_start: None, host_ns: 0, idle: false };
        // mmap, fixed remap, prefix and suffix munmap within one guest mmap.
        for (start, end) in [(110, 130), (140, 170), (180, 190), (200, 240)] {
            a.host_begin(start);
            a.host_end(end);
        }
        assert_eq!(a.host_ns, 100);
        let mut calls = BTreeMap::new();
        Data::add_active(&mut calls, &a, 260);
        assert_eq!((calls[&key].calls, calls[&key].ns, calls[&key].host_ns), (1, 160, 100));
        // Snapshot a fifth unfinished host call, then finish it: no double count.
        a.host_begin(270);
        let mut snapshot = BTreeMap::new();
        Data::add_active(&mut snapshot, &a, 280);
        assert_eq!(snapshot[&key].host_ns, 110);
        a.host_end(300);
        let mut completed = BTreeMap::new();
        Data::add_active(&mut completed, &a, 310);
        assert_eq!(completed[&key].host_ns, 130);
    }

    #[test]
    fn atomic_identity_covers_classes_numbers_and_nested_gpu() {
        for class in [Class::Bsd, Class::Mach, Class::Machdep, Class::Gpu, Class::Selector] {
            for n in [0, 3, 363, 0x8000_0000, u32::MAX] {
                let word = packed(State::Idle, (class, n));
                assert_eq!(identity(word), Some((class, n)));
                assert_eq!(word & 255, State::Idle as u64);
            }
        }
        let word = packed(State::Gpu, (Class::Selector, 123)) | 4 << 48;
        assert_eq!(identity(word), Some((Class::Selector, 123)));
        assert_eq!(word >> 48, 4); // parent batch remains available to sampler
        assert_eq!(identity(State::Jit as u64), None);
    }

    #[test]
    fn unfinished_host_wait_snapshot_and_completion_do_not_double_count() {
        let key = (Class::Bsd, 363);
        let mut completed = BTreeMap::new();
        let mut a = Active { key, start: 100, host_start: Some(130), host_ns: 10, idle: true };
        let mut snapshot = completed.clone();
        Data::add_active(&mut snapshot, &a, 200);
        let c = &snapshot[&key];
        assert_eq!((c.calls, c.idle_calls, c.ns, c.host_ns), (1, 1, 100, 80));
        assert!(completed.is_empty());
        a.host_start = None;
        a.host_ns = 90;
        Data::add_active(&mut completed, &a, 250);
        let c = &completed[&key];
        assert_eq!((c.calls, c.idle_calls, c.ns, c.host_ns), (1, 1, 150, 90));
        a.start = 300; // clock/epoch reset: keep exported times nonnegative
        Data::add_active(&mut completed, &a, 250);
        assert_eq!(completed[&key].host_ns, 90);
    }

    #[test]
    fn snapshot_round_trip_preserves_calls_and_v1_remains_readable() {
        let dir = std::env::temp_dir().join(format!("attesor-profile-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let make = || Data { dir: dir.clone(), name: "roundtrip".into(), args: vec![],
            counts: Counts::default(), stats_base: (0, 0), stats_prior: (0, 0), mmap_fast_prior: (0, 0),
            metrics_prior: BTreeMap::new(), metrics_base: BTreeMap::new(), resolved: BTreeMap::new(), calls: BTreeMap::new(), bridge_selectors: Some(true) };
        let mut first = make();
        first.mmap_fast_prior = (20, 15);
        first.metrics_prior.insert("invalidate.ic-ivau.calls".into(), 17);
        first.calls.insert((Class::Bsd, 363), Call { name: "kevent".into(), samples: 4,
            idle_samples: 3, calls: 20, idle_calls: 20, ns: 900, host_ns: 600 });
        first.counts.states[State::Idle as usize] = 3;
        // Loop census PCs and the new JIT phases must survive in the same snapshot.
        first.counts.pcs.insert(0x1234, 7);
        first.counts.states[State::TranslateRuntime as usize] = 5;
        first.counts.states[State::Invalidate as usize] = 2;
        first.write();
        let mut resumed = make();
        resumed.resume();
        let c = &resumed.calls[&(Class::Bsd, 363)];
        assert_eq!((c.samples, c.idle_samples, c.calls, c.idle_calls, c.ns, c.host_ns), (4, 3, 20, 20, 900, 600));
        assert_eq!(resumed.bridge_selectors, Some(true));
        assert_eq!(resumed.mmap_fast_prior, (20, 15));
        assert_eq!(resumed.metrics_prior["invalidate.ic-ivau.calls"], 17);
        assert_eq!(resumed.counts.pcs[&0x1234], 7);
        assert_eq!(resumed.counts.states[State::TranslateRuntime as usize], 5);
        assert_eq!(resumed.counts.states[State::Invalidate as usize], 2);
        std::fs::write(first.path(), "Attesor guest profile v1\nstate\tjit\t42\ntranslation\t5\t7\n").unwrap();
        let mut legacy = make();
        legacy.resume();
        assert_eq!(legacy.counts.states[State::Jit as usize], 42);
        assert_eq!(legacy.stats_prior, (5, 7));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
