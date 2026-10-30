//! Dispatcher-owned cold-translation policy. No background compiler or guest pointers.
use std::sync::OnceLock;

/// Exact comma-separated token; OFF takes precedence over ON. Process immutable.
pub(super) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        let contains = |name| std::env::var(name).unwrap_or_default().split(',').any(|s| s == "tiered");
        contains("ATTESOR_JIT_ON") && !contains("ATTESOR_JIT_OFF")
    })
}

/// Completed cold-code entries before promotion at the next canonical dispatch.
/// Invalid/zero settings use the conservative default; unrelated to interpreter HOT.
pub(super) fn threshold() -> u32 {
    static THRESHOLD: OnceLock<u32> = OnceLock::new();
    *THRESHOLD.get_or_init(|| std::env::var("ATTESOR_JIT_TIER_THRESHOLD").ok()
        .and_then(|s| s.parse::<u32>().ok()).filter(|&n| n != 0).unwrap_or(32))
}

/// Host address is a generation token: code-cache allocations are never reused.
/// Owned by JitIndex; all accesses share its mutex, including SMC removal.
pub(super) struct Cold {
    pub host: u64,
    pub executions: u32,
}
