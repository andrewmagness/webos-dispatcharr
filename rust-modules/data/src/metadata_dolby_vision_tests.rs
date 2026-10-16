//! The frame-safety of reading the platform's Dolby Vision answer through the presentation
//! decision.

use super::*;

/// The platform's cached Dolby Vision answer and [`Dovi::presentation`] are readable inside a
/// `FrameScope`, where any blocking call panics under `cfg(test)`. The platform half of this
/// (the cache's own getters) is `devcaps::dv`'s `dv_caps_getters_are_frame_safe`; this half names
/// `Dovi` and `DvPresentation`, so it lives with the type that owns them. Prewarming the
/// diagnostic latches first mirrors boot, which resolves them outside the scope.
#[test]
fn dv_presentation_reads_are_frame_safe() {
    prewarm_dv_latches();
    let frame = plx_base::task::FrameScope::enter();
    let _ = plx_platform::devcaps::dv::capability();
    let dovi = Dovi {
        present: true,
        profile: 8,
        bl_compat: 1,
        el_present: false,
        ..Dovi::NONE
    };
    assert_eq!(
        dovi.presentation(true, plx_platform::devcaps::dv::DvCapability::Unknown, true),
        DvPresentation::NotDv,
    );
    drop(frame);
}
