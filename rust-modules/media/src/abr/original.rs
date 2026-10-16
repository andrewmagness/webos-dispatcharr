use super::*;

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

