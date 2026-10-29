//! Guest sampling: atomic CPU PC loads and a Rust-only state word. Generated code
//! already publishes PC with aligned x86 MOV stores; its bytes are unchanged.
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub static ENABLED: AtomicBool = AtomicBool::new(false);
#[derive(Clone, Copy)]
#[repr(u8)]
pub enum State { Dispatch, Jit, Interp, Translate, Mach, Bsd, Gpu, Idle }
const STATES: [&str; 8] = ["dispatch", "jit", "interp", "translate", "syscall-mach", "syscall-bsd", "gpu", "idle"];
thread_local! { static CURRENT: Cell<*const AtomicU8> = const { Cell::new(std::ptr::null()) }; }

#[inline(always)]
pub fn set(state: State) {
    if ENABLED.load(Ordering::Relaxed) {
        CURRENT.with(|c| {
            let p = c.get();
            if !p.is_null() { unsafe { (*p).store(state as u8, Ordering::Relaxed) }; }
        });
    }
}
pub struct Guard(Option<(*const AtomicU8, u8)>);
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
        unsafe { (*p).store(state as u8, Ordering::Relaxed) };
        Guard(Some((p, old)))
    })
}
pub fn register(cpu: &attesor_core::cpu::Cpu) {
    if ENABLED.load(Ordering::Relaxed) {
        CURRENT.with(|c| c.set(&cpu.profile_state));
    }
}
pub fn unregister() { CURRENT.with(|c| c.set(std::ptr::null())); }

#[derive(Default)]
struct Counts {
    states: [u64; 8],
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
    resolved: BTreeMap<u64, (String, String)>,
}
static DATA: Mutex<Option<Data>> = Mutex::new(None);

pub fn init(name: &str, args: &[String]) {
    let Some(dir) = std::env::var_os("ATTESOR_PROFILE").filter(|s| !s.is_empty()) else { return };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("attesor: profile directory: {e}"); return;
    }
    let mut data = Data { dir: dir.into(), name: name.into(), args: args.to_vec(),
        counts: Counts::default(), stats_base: (0, 0), stats_prior: (0, 0), resolved: BTreeMap::new() };
    // A successful exec of the same executable keeps its pid and report path.
    // Continue that snapshot instead of losing the previous execution epoch.
    data.resume();
    *DATA.lock().unwrap() = Some(data);
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
                    let state = state as usize;
                    d.counts.states[state] += 1;
                    if state == State::Jit as usize || state == State::Interp as usize {
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
// Held across all Attesor forks, including AOT worker forks. Lock order DATA -> CPUS -> AOT.
pub struct ForkGuard { data: MutexGuard<'static, Option<Data>> }
pub fn fork_guard() -> ForkGuard { ForkGuard { data: DATA.lock().unwrap() } }
impl ForkGuard {
    pub fn child(&mut self) {
        if let Some(d) = self.data.as_mut() {
            d.counts = Counts::default();
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
    fn path(&self) -> PathBuf {
        let name: String = self.name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
        self.dir.join(format!("{name}-{}.txt", std::process::id()))
    }
    fn resume(&mut self) {
        let Ok(text) = std::fs::read_to_string(self.path()) else { return };
        if !text.starts_with("Attesor guest profile v1\n") { return; }
        for line in text.lines() {
            let f: Vec<_> = line.split('\t').collect();
            match f.as_slice() {
                ["state", state, n] => {
                    if let (Some(i), Ok(n)) = (STATES.iter().position(|s| s == state), n.parse()) { self.counts.states[i] = n; }
                }
                ["image", image, jit, interp] => {
                    if let (Ok(j), Ok(i)) = (jit.parse(), interp.parse()) { self.counts.images.insert(image.to_string(), [j, i]); }
                }
                ["symbol", symbol, n] => {
                    if let Ok(n) = n.parse() { self.counts.symbols.insert(symbol.to_string(), n); }
                }
                ["translation", blocks, ms] => {
                    if let (Ok(b), Ok(m)) = (blocks.parse(), ms.parse()) { self.stats_prior = (b, m); }
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
        let mut out = format!("Attesor guest profile v1\nProcess: {} (pid {})\nArgs: {}\nTotal samples: {total}\nTranslation: {blocks} blocks, {ms} ms\n\nState                 Samples   % total\n", clean(&self.name), std::process::id(), self.args.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(" "));
        let mut states: Vec<_> = c.states.iter().enumerate().collect();
        states.sort_by_key(|&(i, n)| (std::cmp::Reverse(*n), i));
        for (i, &n) in states { let _ = writeln!(out, "{:<20} {n:>8} {:>9.2}", STATES[i], pct(n, total)); }
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
        for (i, &n) in c.states.iter().enumerate() { let _ = writeln!(out, "state\t{}\t{n}", STATES[i]); }
        for (name, n) in &c.images { let _ = writeln!(out, "image\t{}\t{}\t{}", clean(name), n[0], n[1]); }
        for (name, n) in &c.symbols { let _ = writeln!(out, "symbol\t{}\t{n}", clean(name)); }
        let path = self.path();
        let tmp = path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, out).and_then(|_| std::fs::rename(&tmp, &path)) {
            eprintln!("attesor: profile {}: {e}", path.display());
        }
    }
}
