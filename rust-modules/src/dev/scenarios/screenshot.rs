//! The screenshot pipeline's arms that are not about any one page: the still clock a settled
//! capture needs, and the simulator's clock stop that holds a moment of playback.
//!
//! Dev-only, like every arm in [`super`]: the reads go through `devtrig::read`, which is `None` at
//! compile time without the `devtriggers` feature.

use crate::app::run::Frame;
use crate::app::App;
#[cfg(feature = "hostsim")]
use plx_screens::registry::AppArg;

/// Per-arm latches, owned by [`super::Scenarios`].
#[derive(Default)]
pub(crate) struct ScreenshotArms {
    /// `clockstop`'s position, read once (`None` = not read yet), and whether it was reached.
    clockstop: Option<Option<u32>>,
    #[cfg(feature = "hostsim")]
    clockstop_reached: bool,
}

impl ScreenshotArms {
    /// Is any of these arms still on its way to its state? The settled capture waits while one is
    /// (`app::run`'s `shot::tick`): a screen can come to rest BEFORE an arm acts — a black player
    /// waiting on its seek — and a capture of that rest is the wrong picture. An arm that was never
    /// armed, reached its state or gave up (and logged it) is not pending. Simulator only, as the
    /// settled capture is.
    #[cfg(feature = "hostsim")]
    pub(crate) fn pending(&self) -> bool {
        self.clockstop.is_none() || (matches!(self.clockstop, Some(Some(_))) && !self.clockstop_reached)
    }
}

/// `/tmp/dispatcharrtv-stillclock=<ms>` — hold every free-running clock animator (`motion::Phase`:
/// spinners, stall timers) at `<ms>` elapsed. A waiting screen then draws one fixed picture and
/// comes to rest, which is what a settled capture needs; springs and ramps are untouched, they
/// settle on their own.
/// The hold itself only exists with `devtriggers` (`motion::hold_phase_clocks`), hence the gate on
/// the body rather than a second, empty twin of this function.
pub(crate) fn arm_stillclock() {
    #[cfg(feature = "devtriggers")]
    if let Some(v) = plx_base::devtrig::read("stillclock") {
        let ms = v.parse().unwrap_or(0);
        plx_machine::motion::hold_phase_clocks(Some(ms));
        plx_base::eventlog::log(&format!("motion: free-running clocks held at {ms} ms by /tmp/dispatcharrtv-stillclock"));
    }
}

/// `/tmp/dispatcharrtv-clockstop=<ms>` — simulator only: stop the clock sink at that movie position
/// and leave the transport PLAYING, so the player holds a moment of playback that pausing would
/// change (the Up Next tile, which exists only while playing). Logs once when the playhead is
/// there. The stop is a stall of the sink's own making (`ffi_host.rs::stop_clock_at`), re-armed
/// every frame because a Load (a seek) starts a fresh fed timeline.
pub(crate) fn clockstop_arm(app: &mut App, fr: &Frame) {
    #[cfg(feature = "hostsim")]
    {
        let arms = &mut app.scenarios.shots;
        if arms.clockstop.is_none() {
            arms.clockstop = Some(plx_base::devtrig::read("clockstop").map(|v| match v.parse::<u32>() {
                Ok(ms) => Some(ms),
                Err(_) => {
                    plx_base::eventlog::log(&format!("BADTRIGGER clockstop {v:?}: expected <ms>"));
                    None
                }
            }).unwrap_or(None));
        }
        let Some(Some(ms)) = arms.clockstop else { return };
        if !matches!(app.route(), AppArg::Player) {
            return;
        }
        let at = i64::from(ms) * 1_000_000;
        plx_media::player::stop_sim_clock_at(Some(at));
        let arms = &mut app.scenarios.shots;
        if arms.clockstop_reached {
            return;
        }
        if crate::app::playback::playpos() >= at {
            arms.clockstop_reached = true;
            plx_base::eventlog::log(&format!("clockstop: clock held at {ms} ms, still playing"));
        } else if fr.now.wrapping_sub(app.t0) > CLOCKSTOP_CEILING_MS {
            // Reached or not, the arm stops holding the capture back (`pending`).
            arms.clockstop_reached = true;
            plx_base::eventlog::log(&format!("clockstop: gave up; the playhead never reached {ms} ms"));
        }
    }
    #[cfg(not(feature = "hostsim"))]
    {
        let _ = fr;
        app.scenarios.shots.clockstop = Some(None);
    }
}

/// How long `clockstop` waits for the playhead: its position is usually reached through
/// `autoseek`, which only fires 12 s into a run.
#[cfg(feature = "hostsim")]
const CLOCKSTOP_CEILING_MS: u32 = 60_000;
