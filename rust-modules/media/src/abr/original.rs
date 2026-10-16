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
/// is still only evidence about that prefix, so everything after admission remains an observed
/// trial under [`OriginalModeController`]; no invented multiplier turns it into a capacity claim.
pub fn original_sustainable(
    source_kbps: u32,
    measured_kbps: u32,
    complete: bool,
    _policy: &AbrPolicy,
) -> bool {
    source_kbps > 0 && complete && measured_kbps >= source_kbps
}

/// **Retired by N13.** Kept as `#[cfg(test)]` because two tests derive their expectations from it,
/// which is the point: the new duration must be *the same rule on the right clock*, and a test that
/// says so has to be able to name the old one.
///
/// Six windows of [`ORIGINAL_WINDOW_US`] ACTIVE BODY-READ time was the rule, and its own two doc
/// comments disagreed about the unit — "about four and a half seconds of real transfer" in one
/// place and "about nine seconds" of wall clock in another, for the same counter. Under
/// backpressure, the healthy full-buffer case, a 750 ms active window spans unbounded wall time, so
/// the second reading was not merely imprecise: the rule named no duration at all.
///
/// `AbrPolicy::sustained_unsafe_deficit_ms` carries the same 4 500 ms onto the WALL clock. The
/// conversion is deliberately 1:1 in the number and **is not 1:1 in the world** — under
/// backpressure the wall interval is longer, so the new rule is at least as patient as the old one
/// and usually more so. Which is the safe direction (it delays a visible switch, never hastens
/// one), and the observed ratio is an M2 measurement this project has not taken.
#[cfg(any(test, feature = "test-support"))]
pub const ORIGINAL_DEFICIT_WINDOWS: u8 = 6;

