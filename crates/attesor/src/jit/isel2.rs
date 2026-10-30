//! Opt-in host ISA selection. This snapshot is shared by emission and AOT identity:
//! the environment switch alone is not sufficient to identify host-specific code.

use std::sync::OnceLock;

#[derive(Clone, Copy, Default)]
pub(super) struct Selection {
    pub bmi2: bool,
    pub lzcnt: bool,
}

impl Selection {
    /// Actual selected extensions, not all CPUID capabilities. Reserve distinct
    /// bits for each extension so a host lacking one never loads its AOT code.
    pub fn mask(self) -> u32 {
        self.bmi2 as u32 | (self.lzcnt as u32) << 1
    }
}

/// Like the existing JIT switches, names are exact comma-separated tokens.
/// OFF wins over ON, and neither extension is selected by default.
pub(super) fn selected() -> Selection {
    static SELECTION: OnceLock<Selection> = OnceLock::new();
    *SELECTION.get_or_init(|| {
        let contains = |name| std::env::var(name).unwrap_or_default().split(',').any(|s| s == "isel2");
        if !contains("ATTESOR_JIT_ON") || contains("ATTESOR_JIT_OFF") {
            return Selection::default();
        }
        Selection {
            bmi2: std::is_x86_feature_detected!("bmi2"),
            lzcnt: std::is_x86_feature_detected!("lzcnt"),
        }
    })
}
