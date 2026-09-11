//! Helper configuration — a separate file module the root declares with
//! `mod helpers;` and re-exports from with `pub use helpers::HelperConfig;`.

/// Tuning knobs threaded through resolution.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HelperConfig {
    /// Fail resolution instead of falling back to the first candidate.
    pub strict: bool,
    /// Upper bound on candidates considered per toolchain type (0 = no bound).
    pub max_candidates: usize,
}

impl HelperConfig {
    /// A permissive configuration for tests and spikes.
    pub fn permissive() -> Self {
        Self {
            strict: false,
            max_candidates: 0,
        }
    }
}
