//! Opt-in mechanism counters, independent of STATS/PROFILE sharing policy.
use std::sync::{LazyLock, atomic::{AtomicU64, Ordering}};
static ON: LazyLock<bool> = LazyLock::new(|| std::env::var_os("ATTESOR_FIX2_COUNTERS").is_some());
const NAMES: [&str; 10] = ["entry_serializations", "slot_fallbacks", "slot_links", "rel32_links", "shared_attempts", "snapshot_reads", "mapping_opens", "shared_adoptions", "shared_local", "graph_busy"];
static COUNTS: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];
#[inline] pub(super) fn add(n: usize) { if *ON { COUNTS[n].fetch_add(1, Ordering::Relaxed); } }
pub(super) fn flush() {
    if !*ON { return; }
    for (name, count) in NAMES.iter().zip(COUNTS.iter()) { eprintln!("[fix2] {name}={}", count.load(Ordering::Relaxed)); }
    let (blocks, ms, cold, entries, _) = super::stats_snapshot();
    eprintln!("[fix2] blocks={blocks} translate_ms={ms} cold={cold} dispatcher_entries={entries} epoch={}", super::EPOCH.load(Ordering::Relaxed));
}
