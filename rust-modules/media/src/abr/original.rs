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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceProbeState {
    /// No source experiment has been made in this HLS worker.
    Fresh,
    /// A previous experiment was insufficient. Retry only when the HLS conservative bound rises
    /// above both the source result and HLS's own recent estimate at that experiment.
    AwaitHlsAbove(u32),
    /// The source was selected or was not worth a reload. Neither conclusion becomes more
    /// informative by repeating the same request at this HLS operating point.
    Terminal,
    /// An upward HLS commit invalidated only the counterfactual side of the last mode comparison.
    /// Keep the completed source evidence, wait for one ordinary observation from the new live
    /// stream, then score the same source result against that operating point without another
    /// request. The `Direction::Down` branch instead retires terminal evidence and returns the gate
    /// to `Fresh`.
    ReconsiderAfterHlsCommit,
}

pub struct OriginalRecovery {
    source_kbps: u32,
    policy: AbrPolicy,
    /// Evidence about the source request specifically. Never seeded from HLS segments; see
    /// [`CapacityEstimate::demote_to_prior`].
    probe: CapacityEstimate,
    features: SourceFeatures,
    /// The switch history AS CAPTURED on the main thread when this worker started. It is the
    /// BASE, never mutated: [`OriginalRecovery::advance_to`] carries the worker's own clock and
    /// the two are combined at the point of use, so advancing cannot double-count however often
    /// the caller ticks it.
    history: TransitionHistory,
    /// Wall time since that capture. The visible-switch penalty HALVES every
    /// `visible_switch_decay_ms`, which is the whole of this controller's hysteresis — and until
    /// this field existed the caller passed a literal 0, so `since_last_ms` never advanced, the
    /// penalty never decayed, and Original stayed unreachable for the rest of a playback after
    /// two mode switches. The decay RATE is policy and is unchanged; this is only the clock that
    /// drives it, which was stopped.
    elapsed_ms: u64,
    probes: u8,
    source_probe_state: SourceProbeState,
    /// The last comparison this gate made, whole, for the event log — see [`ModeComparison`].
    /// Written only by a decision over completed source evidence: [`Self::observe_probe`] or
    /// [`Self::reconsider_after_hls_commit`]. `worth_probing` asks a hypothetical ("would a GOOD
    /// probe change anything") and publishing that beside a real decision is how a log acquires
    /// two numbers where one of them decided nothing.
    last_comparison: Option<ModeComparison>,
    /// **This playback's actuator set** — carried so both halves of the comparison can be scored
    /// on real alternatives (N14) and so Original's own quality can be scored against the SOURCE
    /// raster, which `HlsActuatorCatalog::source_raster` already holds. The catalog is `Copy` and
    /// the worker already owns one; taking it here beats adding a raster parameter to three
    /// methods and beats a second copy of a bound that can disagree with the ladder it bounds.
    catalog: HlsActuatorCatalog,
}

impl OriginalRecovery {
    pub fn new(
        source_kbps: u32,
        policy: AbrPolicy,
        features: SourceFeatures,
        history: TransitionHistory,
        catalog: HlsActuatorCatalog,
    ) -> Option<Self> {
        (source_kbps > 0).then_some(Self {
            source_kbps,
            policy,
            probe: CapacityEstimate::default(),
            features,
            history,
            elapsed_ms: 0,
            probes: 0,
            source_probe_state: SourceProbeState::Fresh,
            last_comparison: None,
            catalog,
        })
    }

    pub fn probes(&self) -> u8 {
        self.probes
    }

    /// **The comparison the verdict was actually taken on**, for the log — the estimate's central
    /// value, its uncertainty, how many probes are behind it, the discounted rate that is compared,
    /// and the requirement it is compared against.
    ///
    /// Without it the line reads `measured=42012kbps ... verdict=Insufficient` against a 25 Mbit/s
    /// file, which is not merely terse but actively misleading: it invites the reading that 42
    /// Mbit/s was judged too slow for a 25 Mbit/s source, when what happened is that
    /// `conservative_kbps` was 29 689 against a requirement of 34 106. The operative numbers were
    /// unreconstructable from the event log, and this gate is precisely where a run gets stuck —
    /// seven consecutive refusals on a healthy link, with nothing on the line to say which
    /// quantity was short. `[[silent-instrument-trap]]`: the instrument has to be able to show the
    /// thing before its silence means anything.
    pub fn basis(&self) -> (u32, u32, u32, u32, u32) {
        (
            self.probe.slow_kbps,
            self.probe.uncertainty_pm,
            self.probe.samples,
            self.probe.conservative_kbps(),
            source_requirement_kbps(self.source_kbps, &self.policy),
        )
    }

    /// The basis of the decision THIS probe reached, for one log line. `None` whenever the last
    /// probe did not reach one — before the first, and after either of `observe_probe`'s two early
    /// exits (`!observation.completed`, and a conservative rate under the requirement), which is
    /// why that function clears this on entry rather than only writing it on success. Saying "no
    /// comparison" beats publishing a stale one, and the difference is invisible from the log:
    /// `ff.rs` emits `abr: mode` on every probe result, so a value left behind reads as a decision
    /// that had just been taken.
    pub fn comparison(&self) -> Option<ModeComparison> {
        self.last_comparison
    }

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

