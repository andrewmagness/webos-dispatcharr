use super::*;

/// The two states Auto can be in. They are not two ends of one ladder: Original is a different
/// pipeline with different costs and a different failure mode, and the whole reason this is an
/// enum rather than a top rung is that "the highest bitrate" and "no server video encoding at
/// all" are not comparable by bitrate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeKind {
    Original,
    Hls,
}

/// Visible mode switches already spent in this playback. Captured on the main thread when a worker
/// starts, then advanced by the worker's own elapsed time — there is no shared clock between them
/// and inventing one would be a race for no gain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransitionHistory {
    pub visible_switches: u32,
    pub since_last_ms: Option<u64>,
}

impl TransitionHistory {

}

/// One candidate state, scored. Kept as its component terms rather than a bare total because the
/// event log prints them: "Original lost" is not a diagnosis, "Original lost 40 of quality to 60
/// of transition cost with 90 s left" is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeUtility {
    pub quality: i64,
    pub features: i64,
    pub risk: i64,
    pub server: i64,
    pub transition: i64,
    pub total: i64,
}

/// **What this source carries that a transcode would destroy** — one value where there was one
/// boolean (N16).
///
/// `route::auto_original_features` returned `dovi.profile > 0 || immersive`, so the two collapsed
/// into a single flat bonus and an Atmos-only film bought two visible reloads for a benefit
/// inaudible on television speakers, priced identically to a Dolby Vision panel-mode change. They
/// are ORDERED, not equal; see `AbrPolicy::dv_bonus`.
///
/// Both are recorded off the Original CANDIDATE rather than inferred from the stream now playing,
/// which mid-HLS is a re-encode of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SourceFeatures {
    pub dv: bool,
    pub atmos: bool,
}

pub fn scaled(value: i64, scale_pm: i64) -> i64 {
    value * scale_pm / 1_000
}

/// **One whole mode comparison, for one event-log line** (§7.H).
///
/// `ModeUtility`'s own doc has always said the terms are kept apart "because the event log prints
/// them" — *"Original lost is not a diagnosis; Original lost 40 of quality to 60 of transition cost
/// with 90 s left is"*. That was aspirational: all three `choose_mode` call sites discarded the
/// reason and both utilities, so the one question an operator asks after a visible switch was
/// unanswerable from a log. This is the carrier that makes the sentence true.
///
/// Assembled in `abr/`, which never logs — the same division `ControllerTelemetry` keeps, and for
/// the same reason: the numbers printed are then provably the numbers used, rather than a
/// re-derivation at the call site against inputs that have since moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeComparison {
    pub chosen: ModeKind,
    pub reason: ModeReason,
    /// The mode that WON, decomposed.
    pub winner: ModeUtility,
    /// The mode that lost, when there was one. `None` only for `OriginalInfeasible`, where there
    /// was no second candidate to score rather than a second candidate that scored badly.
    pub loser: Option<ModeUtility>,
    /// The HLS operating point the comparison was actually against — the thing N14 site 1 was
    /// fabricating. Logged because "Original lost to HLS" means nothing without it.
    pub hls_rung: Rung,
    /// `benefit_scale_pm` at the moment of the decision, so a comparison taken near the end of a
    /// film is readable as one.
    pub scale_pm: i64,
}

/// Why a mode comparison came out the way it did — logged verbatim, so "why did Auto choose this"
/// is answerable from the event log alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeReason {
    /// Original wins on utility and its delivery evidence supports it.
    OriginalWorthIt,
    /// Original is technically impossible here (codec, burn-in, relay, no source URL).
    OriginalInfeasible,
    /// Feasible, but the sums say no — usually a visible switch costing more than the remaining
    /// playback can earn back.
    OriginalNotWorthIt,
}

