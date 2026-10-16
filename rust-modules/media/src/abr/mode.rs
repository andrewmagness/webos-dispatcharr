
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

