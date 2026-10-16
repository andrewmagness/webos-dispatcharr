//! The player's transport-pause contract: `paused`, `viewer_paused`, `set_transport_paused` and
//! `transport_target` are the one door every surface reaches the player's pause through.
//! `app::lifecycle` re-exports them, so app code spells the names it always did. An app switch
//! does not resume a position: a live channel is parked on background and tuned again on
//! foreground (`app::run`).

use std::sync::atomic::Ordering::Relaxed;

// transport state — was the C playback globals; now crate::player (atomics)
#[inline]
pub fn paused() -> bool {
    crate::player::TX.paused.load(Relaxed)
}
#[inline]
pub fn set_paused(v: bool) {
    crate::player::TX.commit_paused(v)
}
/// PURE: the paused value a transport request settles on. `Some(true)` is the remote's PLAY key,
/// `Some(false)` its PAUSE, and `None` the single PLAYPAUSE toggle — which is the only one of the
/// three that needs the current state at all.
///
/// The player's `PlayerReq::Transport` answers the key ladder through this one function.
pub fn transport_target(play: Option<bool>, paused: bool) -> bool {
    match play {
        Some(true) => false,
        Some(false) => true,
        None => !paused,
    }
}

/// The transport state the VIEWER sees and their next press toggles.
pub fn viewer_paused() -> bool {
    paused()
}

/// Ask the synchronized player clock to commit a user Pause/Resume. The player publishes the feed
/// gate at the same accepted native boundary; keeping a second commit here used to leave a window
/// in which deadline accounting still treated an already-accepted Pause as active playback.
/// Resume during an internal HLS hold is accepted as a deferred transition: feeding reopens, while
/// measured re-prime owns both the eventual Starfish Play and the matching ACB Resume.
pub fn set_transport_paused(
    pa: &mut crate::player::adapter::PlayerAdapter,
    value: bool,
) -> bool {
    if paused() == value {
        return true;
    }
    if value {
        crate::player::pause(pa)
    } else {
        crate::player::resume(pa)
    }
}

#[cfg(test)]
mod transport_target_tests {
    /// The three keys, both ways round. A PLAY key on an already-playing transport is a no-op
    /// rather than a pause, and a PAUSE key on an already-paused one likewise — the property a
    /// per-caller `match` kept getting wrong, and the reason the player's transport and the detail
    /// page's trailer transport share this one function.
    #[test]
    fn only_the_toggle_depends_on_the_current_state() {
        for paused in [false, true] {
            assert!(!super::transport_target(Some(true), paused), "PLAY plays");
            assert!(super::transport_target(Some(false), paused), "PAUSE pauses");
        }
        assert!(super::transport_target(None, false), "PLAYPAUSE pauses a playing transport");
        assert!(!super::transport_target(None, true), "…and resumes a paused one");
    }
}

#[cfg(all(test, feature = "hostsim"))]
mod transport_pause_contract_tests {
    use super::{
        paused, set_transport_paused, };
    use std::sync::atomic::Ordering;

    #[test]
    fn a_refused_native_pause_or_play_cannot_diverge_the_feed_gate() {
        let _guard = plx_base::testlock::serial();
        let old_paused = crate::player::TX.paused.load(Ordering::Acquire);
        crate::player::TX.commit_paused(false);
        let old_rebuffering = crate::player::SHARED
            .hls_rebuffering
            .swap(false, Ordering::AcqRel);
        struct Restore {
            paused: bool,
            rebuffering: bool,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                crate::player::force_pause_result_for_test(None);
                crate::player::force_play_result_for_test(None);
                crate::player::TX.commit_paused(self.paused);
                crate::player::SHARED
                    .hls_rebuffering
                    .store(self.rebuffering, Ordering::Release);
            }
        }
        let _restore = Restore {
            paused: old_paused,
            rebuffering: old_rebuffering,
        };
        let mut pa = crate::player::adapter::PlayerAdapter::new(unsafe {
            plx_base::task::MainThread::assume()
        });

        crate::player::force_pause_result_for_test(Some(0));
        assert!(!set_transport_paused(&mut pa, true));
        assert!(
            !paused(),
            "feed must continue when the native clock refused Pause"
        );

        crate::player::force_pause_result_for_test(Some(1));
        assert!(set_transport_paused(&mut pa, true));
        assert!(paused());

        crate::player::force_play_result_for_test(Some(0));
        assert!(!set_transport_paused(&mut pa, false));
        assert!(
            paused(),
            "feed must remain stopped when the native clock refused Play"
        );

        crate::player::force_play_result_for_test(Some(1));
        assert!(set_transport_paused(&mut pa, false));
        assert!(!paused());
    }

}
