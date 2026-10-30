//! Profiling-only counters. Timings are nested elapsed thread wall time, not CPU time.
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::Instant;
// Atomic counters add no new fork-owned lock. Snapshot baselines handle child
// epochs; event timers never hold a guard across generated code or a syscall.
const KEYS: &[&str] = &[
    "translate.runtime.ns",
    "translate.image.ns",
    "translate.runtime.requests",
    "translate.image.requests",
    "translate.emit.ns",
    "translate.assemble.ns",
    "translate.alignment-retries",
    "translate.alignment-retry.ns",
    "translate.reused-emission",
    "translate.host-bytes",
    "translate.runtime.host-bytes",
    "translate.image.host-bytes",
    "translate.runtime.units",
    "translate.image.units",
    "retranslate.units",
    "retranslate.host-bytes",
    "retranslate.ns",
    "invalidate.ns",
    "invalidate.units",
    "invalidate.host-bytes",
    "invalidate.other.calls",
    "invalidate.other.requested-bytes",
    "invalidate.other.units",
    "invalidate.ic-ivau.calls",
    "invalidate.ic-ivau.requested-bytes",
    "invalidate.ic-ivau.units",
    "invalidate.mprotect-exec.calls",
    "invalidate.mprotect-exec.requested-bytes",
    "invalidate.mprotect-exec.units",
];
static METRICS: [std::sync::atomic::AtomicU64; KEYS.len()] =
    [const { std::sync::atomic::AtomicU64::new(0) }; KEYS.len()];
#[inline]
pub fn enabled() -> bool { crate::profile::ENABLED.load(Ordering::Relaxed) }
pub fn add(key: &str, n: u64) {
    if !enabled() { return; }
    let index = match key {
        "translate.runtime.ns" => 0,
        "translate.image.ns" => 1,
        "translate.runtime.requests" => 2,
        "translate.image.requests" => 3,
        "translate.emit.ns" => 4,
        "translate.assemble.ns" => 5,
        "translate.alignment-retries" => 6,
        "translate.alignment-retry.ns" => 7,
        "translate.reused-emission" => 8,
        "translate.host-bytes" => 9,
        "translate.runtime.host-bytes" => 10,
        "translate.image.host-bytes" => 11,
        "translate.runtime.units" => 12,
        "translate.image.units" => 13,
        "retranslate.units" => 14,
        "retranslate.host-bytes" => 15,
        "retranslate.ns" => 16,
        "invalidate.ns" => 17,
        "invalidate.units" => 18,
        "invalidate.host-bytes" => 19,
        "invalidate.other.calls" => 20,
        "invalidate.other.requested-bytes" => 21,
        "invalidate.other.units" => 22,
        "invalidate.ic-ivau.calls" => 23,
        "invalidate.ic-ivau.requested-bytes" => 24,
        "invalidate.ic-ivau.units" => 25,
        "invalidate.mprotect-exec.calls" => 26,
        "invalidate.mprotect-exec.requested-bytes" => 27,
        "invalidate.mprotect-exec.units" => 28,
        _ => panic!("unknown JIT metric: {key}"),
    };
    METRICS[index].fetch_add(n, Ordering::Relaxed);
}
pub fn snapshot() -> BTreeMap<String, u64> {
    KEYS.iter().zip(&METRICS).map(|(&key, n)| (key.into(), n.load(Ordering::Relaxed))).collect()
}
pub struct Timer(Option<(&'static str, Instant)>);
impl Timer {
    pub fn new(key: &'static str) -> Self { Self(enabled().then(|| (key, Instant::now()))) }
}
impl Drop for Timer {
    fn drop(&mut self) {
        if let Some((key, start)) = self.0 { add(key, start.elapsed().as_nanos() as u64); }
    }
}
/// Generated probes are deliberately opt-in, independently of ordinary sampling.
pub fn probes() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let has = |name| std::env::var(name).unwrap_or_default().split(',').any(|s| s == "jitcost");
        has("ATTESOR_JIT_ON") && !has("ATTESOR_JIT_OFF")
    })
}
