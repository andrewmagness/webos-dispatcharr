use super::*;

pub const SOURCE_PROBE_MIN_BYTES: usize = 512 * 1024;
pub const SOURCE_PROBE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// The finite source object a probe requests and the longest useful BODY time to wait for it.
///
/// The unclamped object is exactly one second of source media. Its sustainability question has a
/// coefficient-free deadline: if those bytes have not arrived within the amount of media they
/// represent, the response cannot establish `A <= D`. Connection setup is a distinct bounded
/// phase and is never subtracted from this interval. The byte clamps bound sampling noise and
/// memory, so their represented duration is recomputed rather than still called one second.
/// `max_budget_ms` is only the operational ceiling for a tiny source whose minimum sample spans
/// several seconds; waiting past it was already forbidden by [`AbrPolicy::probe_budget_ms`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceProbePlan {
    pub target_bytes: usize,
    pub budget_ms: u64,
}

pub fn source_probe_plan(source_kbps: u32, max_budget_ms: u64) -> Option<SourceProbePlan> {
    let rate = u64::from(source_kbps);
    if rate == 0 || max_budget_ms == 0 {
        return None;
    }
    let target_bytes = usize::try_from(rate)
        .unwrap_or(usize::MAX)
        .saturating_mul(125)
        .clamp(SOURCE_PROBE_MIN_BYTES, SOURCE_PROBE_MAX_BYTES);
    // kbps is bits/ms, so bits/kbps is already milliseconds. Round upward: truncating this
    // deadline would refuse a response at the exact conservation boundary.
    let represented_ms = u64::try_from(target_bytes)
        .unwrap_or(u64::MAX)
        .saturating_mul(8)
        .saturating_add(rate - 1)
        / rate;
    Some(SourceProbePlan {
        target_bytes,
        budget_ms: represented_ms.max(1).min(max_budget_ms),
    })
}

/// **Cold-start Original admission, and only that.** The measured source prefix must complete and
/// arrive no slower than the file's average consumption rate.  That is a physical conservation
/// test: the prefix contributes media at least as quickly as playback removes it.  A finite prefix
/// is still only evidence about that prefix; no invented multiplier turns it into a capacity claim.
pub fn original_sustainable(
    source_kbps: u32,
    measured_kbps: u32,
    complete: bool,
    _policy: &AbrPolicy,
) -> bool {
    source_kbps > 0 && complete && measured_kbps >= source_kbps
}

