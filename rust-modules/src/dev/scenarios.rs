//! Every dev-trigger ARM — the code that reads `/tmp/plxnative-*` through [`plx_base::devtrig::flag`] /
//! [`plx_base::devtrig::read`] / [`plx_base::devtrig::latched_flag!`] and reacts to it — gathered on one file
//! (UI restructure spec v4 §3.3 step 2 / §11). [`Scenarios`] is the ONE struct, owned by `App` as
//! `app.scenarios`, that gathers every arm's own retry counter, oscillator phase and boot-time
//! latch, so a field used by no production code has exactly one home.
//!
//! **Entry points, called from the loop:** [`pre_boot`] (before SDL exists), the individual
//! boot-time arms below (each named for its trigger, called inline from `app::boot::construct`),
//! and [`each_frame`] (the per-frame scripts: the synthetic playback, seeks, pauses and the
//! player's panels).

use crate::app::App;
use crate::app::run::Frame;
use plx_screens::registry::AppArg;
use plx_machine::machine::{Key, Tick};
use std::os::raw::c_int;

pub(crate) mod screenshot;
#[cfg(feature = "devtriggers")]
pub(crate) mod phcount;
#[cfg(feature = "devtriggers")]
pub(crate) mod toast_probe;

/// The dev triggers read ONCE at boot and consulted by the loop after (each is documented where
/// it is READ, below).
pub(crate) struct DevFlags {
    /// `plxnative-nobudget`: read at boot, applied to the one `Budget` at boot, and kept here so
    /// a log reader can tell an A leg from a B leg by the flags the boot recorded.
    pub(crate) nobudget: bool,
}

/// Every dev-trigger arm's own retry counter, oscillator phase and one-shot latch.
/// `pub(crate)` throughout: `app::boot`/`app::run` read and write these fields directly through
/// `&mut App`, exactly as they read `App`'s own fields.
#[derive(Default)]
pub(crate) struct Scenarios {
    pub(crate) marker_tried: bool,
    pub(crate) auto_tried: bool,
    pub(crate) replay_left: u32,
    pub(crate) seek_tried: bool,
    pub(crate) seek_script: Vec<String>,
    pub(crate) seek_script_at: u32,
    pub(crate) seek_gap_ms: u32,
    pub(crate) seek_script_last: i64,
    pub(crate) quality_script: Vec<plx_plex::plex::session::PlaybackQuality>,
    pub(crate) quality_script_at: u32,
    pub(crate) quality_gap_ms: u32,
    pub(crate) quality_tried: bool,
    pub(crate) quality_playing_since: Option<u32>,
    pub(crate) menu_tried: bool,
    pub(crate) menupick_tried: bool,
    /// dev: the row `/tmp/plxnative-menupick` still owes the track menu, as its RAW second field
    /// (an absolute row number, or a named Audio-tab target such as `"boost"`/`"loudness"` — see
    /// `menupick_arm`'s doc): the panel opens and is picked on separate frames, so the pick is
    /// carried here until the surface it names exists.
    pub(crate) menupick_target: Option<String>,
    /// `/tmp/plxnative-subtiming` — see [`subtiming_arm`] and [`Subtiming`].
    pub(crate) subtiming: Subtiming,
    /// `/tmp/plxnative-submenuosc` — see [`submenuosc_arm`] and [`SubmenuOsc`].
    pub(crate) submenu_osc: SubmenuOsc,
    /// `/tmp/plxnative-moreosc` — see [`moreosc_arm`]; the same state as `submenu_osc`.
    pub(crate) more_osc: SubmenuOsc,
    pub(crate) pause_tried: bool,
    /// An armed Pause edge: (due at, hold ms, the media position it also waits for).
    pub(crate) pause_script: Option<(u32, Option<u32>, Option<u32>)>,
    pub(crate) pause_resume_at: Option<u32>,
    /// The boot-time trigger flags the loop consults every frame after.
    pub(crate) dev: DevFlags,
    /// The screenshot pipeline's arms (`plxnative-clockstop`) — see [`screenshot`].
    pub(crate) shots: screenshot::ScreenshotArms,
}

impl Default for DevFlags {
    fn default() -> Self { Self { nobudget: false } }
}

// =================================================================================================
// pre-SDL
// =================================================================================================

/// `/tmp/plxnative-stats` — force the Stats-for-nerds overlay on, before SDL or a screen exists,
/// so a playback test photographs the same ABR/pipeline evidence on every automated run rather
/// than depending on a previous manual toggle surviving into this session.
pub(crate) fn pre_boot() {
    if plx_base::devtrig::flag("stats") {
        crate::app::diagnostics::open();
    }
}

// =================================================================================================
// boot-time arms — each named for its trigger, called inline from `app::boot::construct` in the
// order the reads always ran in.
// =================================================================================================

/// `/tmp/plxnative-novsync` — uncap the swap interval so `fps=` reports the true GPU render rate.
pub(crate) fn novsync_armed() -> bool {
    plx_base::devtrig::flag("novsync")
}

plx_base::devtrig::latched_flag! { pub(crate) fn imagecache_stats_armed = "imagecache-stats"; }
plx_base::devtrig::latched_flag! {
    /// `/tmp/plxnative-imgtrace` — per-image poster timeline and every picture-lost event
    /// (`app/adapters/poster/trace.rs`). Absent in shipping builds.
    pub(crate) fn imgtrace_armed = "imgtrace";
}
plx_base::devtrig::latched_flag! {
    /// RAM-only control leg for cache performance comparisons; absent in shipping builds.
    pub(crate) fn imagecache_bypass_armed = "imagecache-bypass";
}

/// `/tmp/plxnative-stillclock=<ms>` — see [`screenshot::arm_stillclock`].
pub(crate) fn arm_stillclock() {
    screenshot::arm_stillclock();
}

/// `/tmp/plxnative-anim` — the animation-diagnostic overlay (off by default).
pub(crate) fn arm_anim() {
    if plx_base::devtrig::flag("anim") {
        plx_ui::anim::set_enabled(true);
    }
}

/// `/tmp/plxnative-glassload` — the backdrop-glass LOAD DIAL.
pub(crate) fn arm_glassload(glass: &mut plx_ui::frame::glass::GlassPlan) {
    if let Some(v) = plx_base::devtrig::read("glassload") {
        glass.configure_dial(&v);
    }
}

/// `/tmp/plxnative-navblur` — the blurred-route-transition prototype.
pub(crate) fn arm_navblur(glass: &mut plx_ui::frame::glass::GlassPlan) {
    if let Some(v) = plx_base::devtrig::read("navblur") {
        glass.configure_navblur(&v);
    }
}

/// `/tmp/plxnative-overdraw` — the CPU-side per-draw-class overdraw ledger.
pub(crate) fn arm_overdraw() {
    if plx_base::devtrig::flag("overdraw") {
        plx_gfx::overdraw::set_ledger(true);
    }
}

/// `/tmp/plxnative-drawmask=<classes>` — refuse every draw of the named classes.
pub(crate) fn arm_drawmask() {
    if let Some(spec) = plx_base::devtrig::read("drawmask") {
        plx_gfx::overdraw::set_mask(&spec);
    }
}

/// `/tmp/plxnative-profile` / `/tmp/plxnative-hwcnt` — the two GPU-time profilers. Both present
/// is refused; either alone arms its mode.
///
/// Gated on `devtriggers` at the item level rather than left to `devtrig::read` folding to `None`:
/// `devtrig::read` already makes this whole function inert in a release build, but the disabled-both
/// diagnostic line below spells out both trigger names in full, and a compiled-but-unreachable
/// function still carries its own string literals into `--no-default-features` bytes. Compiling
/// the function out entirely is what actually keeps `plxnative-profile`/`plxnative-hwcnt` out of
/// the binary `ci/check-package.py`'s dev-trigger-catalog check inspects.
#[cfg(feature = "devtriggers")]
pub(crate) fn arm_profile_hwcnt() {
    match (plx_base::devtrig::read("profile"), plx_base::devtrig::read("hwcnt")) {
        (Some(_), Some(_)) => {
            plx_base::eventlog::log("PROFILE disabled: remove either /tmp/plxnative-profile or /tmp/plxnative-hwcnt");
        }
        (Some(filter), None) => plx_ui::profile::set_enabled(&filter),
        (None, Some(filter)) => plx_ui::profile::set_hwcnt_enabled(&filter),
        (None, None) => {}
    }
}
#[cfg(not(feature = "devtriggers"))]
pub(crate) fn arm_profile_hwcnt() {}

/// `/tmp/plxnative-cpuprof` — the render thread's own per-phase CPU clock.
pub(crate) fn arm_cpuprof() {
    if plx_base::devtrig::flag("cpuprof") {
        plx_ui::profile::set_cpu_enabled();
    }
}

/// `/tmp/plxnative-noidle` — turn the whole-frame present gate off.
pub(crate) fn arm_noidle() {
    if plx_base::devtrig::flag("noidle") {
        plx_machine::idle::set_enabled(false);
        #[cfg(feature = "devtriggers")]
        plx_base::eventlog::log("idle: present gate DISABLED by /tmp/plxnative-noidle");
    }
}

/// `/tmp/plxnative-audioenh=off|boost|loudness` — force the PERSISTED Boost Dialog / Normalize
/// Loudness preference (issue #266) at boot, harness-only.
///
/// Every other boot override here (`dev::playback_quality_override`, `arm_glassload`, …) is
/// deliberately in-memory-only, so a test can never mutate a real person's saved preference. This
/// one is the one exception, and on purpose: `player::set_audio_enhancements` is the SAME call a
/// person's own track-menu pick makes (it both updates the live route and re-persists through the
/// storage worker), and an on-device harness case needs the persisted value itself pinned before
/// boot, not just the in-memory copy — one case proves the cold-start "a saved preference turns an
/// otherwise-direct-playable route into a remux" path (`route::plan`'s cold-start audio branch) by
/// booting with the preference already ON, and the very next boot must not inherit whatever a
/// FAILED previous case's toggle left behind. A toggle-based reset cannot promise that: if the
/// pick never lands, the preference is stuck at whatever the last successful toggle set it to.
/// Forcing the value at boot, every time, is what makes a case's starting preference a property of
/// the manifest instead of of history — the same argument `run_case`'s per-case viewOffset reset
/// already makes for resume position.
///
/// `off` clears both flags; `boost`/`loudness` sets exactly one (never both — no case here needs
/// both at once, and a value this test-only can grow a second name later without breaking the
/// existing ones). An unrecognised value is ignored rather than guessed at.
pub(crate) fn arm_audio_enhancements() {
    let Some(v) = plx_base::devtrig::read("audioenh") else { return };
    let a = match v.trim() {
        "off" => plx_plex::plex::AudioEnhancements::NONE,
        "boost" => plx_plex::plex::AudioEnhancements { boost_dialog: true, normalize_loudness: false },
        "loudness" => plx_plex::plex::AudioEnhancements { boost_dialog: false, normalize_loudness: true },
        #[allow(unused_variables)]
        other => {
            #[cfg(feature = "devtriggers")]
            plx_base::eventlog::log(&format!("audioenh: unrecognised value {other:?} — ignored"));
            return;
        }
    };
    plx_media::player::set_audio_enhancements(a);
    #[cfg(feature = "devtriggers")]
    plx_base::eventlog::log(&format!(
        "audioenh: forced boost_dialog={} normalize_loudness={} by /tmp/plxnative-audioenh",
        a.boost_dialog, a.normalize_loudness,
    ));
}

/// `/tmp/plxnative-nobudget` — the frame budget's A/B CONTROL LEG (spec §8.1, phase 11).
///
/// Present, admission is what it was before phase 11: the `Poster` quota of three per frame and
/// nothing else — no time ceiling, no solo rule, and a `Residency` upload (a backdrop, a hero
/// logo) spending one of those three exactly as it used to. It exists so a device A/B measures
/// this CHANGE and not the difference between two builds, and it is DIAG for the reason
/// `plxnative-drawmask` is: an A/B whose two legs boot to different screens has measured the
/// screen.
pub(crate) fn nobudget_armed() -> bool {
    plx_base::devtrig::flag("nobudget")
}

/// `/tmp/plxnative-framedrop[=<ms>]`.
pub(crate) fn framedrop_value() -> Option<String> {
    plx_base::devtrig::read("framedrop")
}
/// `/tmp/plxnative-framering[=<ms>]` — the frame-drop detector's context ring
/// (`diag::heartbeat::FrameRing`): write only frames of `<ms>` or more (default 17) and their
/// neighbours, instead of every frame. `None` when unarmed.
pub(crate) fn framering_ms() -> Option<f64> {
    plx_base::devtrig::read("framering").map(|s| s.parse().ok().filter(|v: &f64| *v > 0.0).unwrap_or(17.0))
}
/// `/tmp/plxnative-replay[=N]`'s raw content, for [`crate::app::boot::replay_budget`].
pub(crate) fn replay_trigger_value() -> Option<String> {
    plx_base::devtrig::read("replay")
}

fn script_step_due(now: u32, at: u32, gap_ms: u32) -> bool {
    (now.wrapping_sub(at) as i32) >= gap_ms as i32
}

#[cfg(test)]
mod script_schedule_tests {
    use super::script_step_due;

    #[test]
    fn a_delay_longer_than_the_gap_does_not_fire_at_once() {
        let (now, gap, delay) = (1_000_000u32, 300u32, 95_000u32);
        let at = now.wrapping_sub(gap).wrapping_add(delay);
        assert!(!script_step_due(now, at, gap), "the delayed step fired immediately");
        assert!(!script_step_due(now.wrapping_add(delay - 1), at, gap), "fired one ms early");
        assert!(script_step_due(now.wrapping_add(delay), at, gap), "never fired at the delay");
    }

    #[test]
    fn an_undelayed_script_still_fires_at_once_then_one_gap_apart() {
        let (now, gap) = (1_000_000u32, 300u32);
        let at = now.wrapping_sub(gap);
        assert!(script_step_due(now, at, gap), "the first step must fire on arming");
        assert!(!script_step_due(now.wrapping_add(gap - 1), now, gap), "second step fired early");
        assert!(script_step_due(now.wrapping_add(gap), now, gap), "second step never fired");
    }

    #[test]
    fn the_predicate_survives_the_tick_wrap() {
        let (gap, at) = (300u32, u32::MAX - 100);
        assert!(!script_step_due(at.wrapping_add(299), at, gap));
        assert!(script_step_due(at.wrapping_add(300), at, gap));
    }
}

/// Pin the player HUD up for a headless capture — the shared tail of the menu/menupick/autopause
/// arms. Stays defined in `app/run.rs` (it draws on that module's own HUD plumbing); called from
/// here at the exact points the arms always called it.
use crate::app::run::pin_headless_hud;

/// `/tmp/plxnative-playurl` (or `-autoplay`/`-h265`) — start the synthetic playback named by the
/// `plxnative-playurl` trigger once, two seconds into the run, with the transport pinned for a
/// headless capture. The engine reads the trigger itself when `route::url()` is empty.
fn autoplay_arm(app: &mut App, fr: &mut Frame) {
    use plx_screens::player::input::HUD_HEADLESS_MS;
    if !app.scenarios.auto_tried
        && !matches!(app.route(), AppArg::Player)
        && fr.now.wrapping_sub(app.t0) > 2000
    {
        app.scenarios.auto_tried = true;
        let playurl = plx_base::devtrig::flag("playurl");
        if plx_base::devtrig::flag("autoplay") || playurl || plx_base::devtrig::flag("h265") {
            plx_media::route::clear_url(&mut app.player.session);
            crate::app::playback::start_playback(&mut app.player.session,
                &mut app.adapters.player,
                0,
                crate::app::playback::Origin::Here,
                HUD_HEADLESS_MS,
                None,
                &mut app.pages,
                &mut app.bridge,
            );
        }
    }
}

fn autoseek_arm(app: &mut App, fr: &mut Frame) {
    if !app.scenarios.seek_tried
        && matches!(app.route(), AppArg::Player)
        && crate::app::playback::dur() > 0
        && fr.now.wrapping_sub(app.t0) > 12000
    {
        app.scenarios.seek_tried = true;
        if let Some(s) = plx_base::devtrig::read("autoseek") {
            let mut steps: Vec<String> = s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect();
            let mut first_delay_ms = 0u32;
            loop {
                let Some(head) = steps.first().cloned() else { break };
                if let Some(g) = head.strip_prefix("gap=") {
                    app.scenarios.seek_gap_ms = g.parse().unwrap_or(300).max(50);
                } else if let Some(d) = head.strip_prefix("delay=") {
                    first_delay_ms = d.parse().unwrap_or(0);
                } else {
                    break;
                }
                steps.remove(0);
            }
            if steps.is_empty() {
                steps.push("140".to_string());
            }
            app.scenarios.seek_script_last = plx_media::player::playpos_ns();
            app.scenarios.seek_script_at = fr.now.wrapping_sub(app.scenarios.seek_gap_ms).wrapping_add(first_delay_ms);
            app.scenarios.seek_script = steps;
        }
    }
    if !app.scenarios.seek_script.is_empty()
        && matches!(app.route(), AppArg::Player)
        && script_step_due(fr.now, app.scenarios.seek_script_at, app.scenarios.seek_gap_ms)
    {
        let step = app.scenarios.seek_script.remove(0);
        app.scenarios.seek_script_at = fr.now;
        let t = if let Some(r) = step.strip_prefix('+') {
            app.scenarios.seek_script_last + r.parse::<i64>().unwrap_or(0) * 1_000_000_000
        } else if let Some(r) = step.strip_prefix('-') {
            app.scenarios.seek_script_last - r.parse::<i64>().unwrap_or(0) * 1_000_000_000
        } else {
            step.parse::<i64>().unwrap_or(140) * 1_000_000_000
        }.max(0);
        app.scenarios.seek_script_last = t;
        plx_base::eventlog::log(&format!("autoseek: step → {}s ({} left)", t / 1_000_000_000, app.scenarios.seek_script.len()));
        crate::app::playback::request_seek(t);
    }
}

fn qualityswitch_arm(app: &mut App, fr: &mut Frame) {
    if !app.scenarios.quality_tried {
        const QUALITY_SWITCH_OBSERVE_MS: u32 = 12_000;
        let playing = matches!(app.route(), AppArg::Player)
            && crate::app::playback::dur() > 0
            && plx_media::player::is_playing(&app.player.session);
        if !playing {
            app.scenarios.quality_playing_since = None;
        } else {
            let since = *app.scenarios.quality_playing_since.get_or_insert(fr.now);
            if fr.now.wrapping_sub(since) >= QUALITY_SWITCH_OBSERVE_MS {
                app.scenarios.quality_tried = true;
                if let Some((gap, qs)) = super::quality_switch_script() {
                    app.scenarios.quality_gap_ms = gap;
                    app.scenarios.quality_script_at = fr.now.wrapping_sub(gap);
                    app.scenarios.quality_script = qs;
                }
            }
        }
    }
    if !app.scenarios.quality_script.is_empty()
        && matches!(app.route(), AppArg::Player)
        && script_step_due(fr.now, app.scenarios.quality_script_at, app.scenarios.quality_gap_ms)
    {
        let q = app.scenarios.quality_script.remove(0);
        app.scenarios.quality_script_at = fr.now;
        plx_base::eventlog::log(&format!("quality: switch → {} ({} left)", super::quality_wire_name(q), app.scenarios.quality_script.len()));
        plx_media::route::set_quality(&mut app.player.session, q);
    }
}

fn autopause_arm(app: &mut App, fr: &mut Frame) {
    if !app.scenarios.pause_tried && matches!(app.route(), AppArg::Player) && fr.now.wrapping_sub(app.t0) > 6000 {
        app.scenarios.pause_tried = true;
        if let Some(script) = super::pause_script() {
            app.scenarios.pause_script =
                Some((fr.now.wrapping_add(script.delay_ms), script.hold_ms, script.at_ms));
        }
    }
    if let Some((pause_at, hold_ms, at_ms)) = app.scenarios.pause_script {
        // In the simulator the clock sink is also told to stop on `at` exactly, so the pause
        // (accepted a little after the gate opens) freezes that position rather than wherever
        // scheduling had got to: the same frame and clock every run (`ffi_host.rs::stop_clock_at`).
        // Re-armed every frame until accepted: a Load in between rebases the fed timeline, and
        // the stop is kept in movie time, so re-arming is idempotent.
        #[cfg(feature = "hostsim")]
        if let Some(ms) = at_ms {
            plx_media::player::stop_sim_clock_at(Some(i64::from(ms) * 1_000_000));
        }
        let reached = at_ms.is_none_or(|ms| crate::app::playback::playpos() >= i64::from(ms) * 1_000_000);
        if matches!(app.route(), AppArg::Player) && script_step_due(fr.now, pause_at, 0) && reached {
            if crate::app::lifecycle::set_transport_paused(&mut app.adapters.player, true) {
                plx_base::eventlog::log(&format!(
                    "autopause: Pause accepted hold={}ms",
                    hold_ms.map_or_else(|| "forever".to_string(), |ms| ms.to_string()),
                ));
                app.scenarios.pause_script = None;
                app.scenarios.pause_resume_at = hold_ms.map(|hold| fr.now.wrapping_add(hold));
                #[cfg(feature = "hostsim")]
                plx_media::player::stop_sim_clock_at(None);
                pin_headless_hud(app, fr.now, None);
            }
        }
    }
    if let Some(resume_at) = app.scenarios.pause_resume_at {
        if matches!(app.route(), AppArg::Player) && script_step_due(fr.now, resume_at, 0) {
            if crate::app::lifecycle::set_transport_paused(&mut app.adapters.player, false) {
                plx_base::eventlog::log("autopause: Resume accepted");
                app.scenarios.pause_resume_at = None;
            }
        }
    }
}

fn menu_arm(app: &mut App, fr: &mut Frame) {
    if !app.scenarios.menu_tried && matches!(app.route(), AppArg::Player) && fr.now.wrapping_sub(app.t0) > 6000 {
        app.scenarios.menu_tried = true;
        if let Some(t) = plx_base::devtrig::read("menu") {
            crate::app::bridge::open_player_overlay(&mut app.player.session,
                app.bridge.metadata_view(),
                &mut app.pages,
                plx_screens::player::overlay::OverlayKind::Tracks { tab: t.parse::<c_int>().unwrap_or(0) },
            );
            pin_headless_hud(app, fr.now, None);
        }
        if plx_base::devtrig::flag("more") {
            crate::app::bridge::open_player_overlay(&mut app.player.session, app.bridge.metadata_view(), &mut app.pages, plx_screens::player::overlay::OverlayKind::More { quality: false });
            pin_headless_hud(app, fr.now, None);
        }
        if plx_base::devtrig::flag("info") {
            crate::app::bridge::open_player_overlay(&mut app.player.session, app.bridge.metadata_view(), &mut app.pages, plx_screens::player::overlay::OverlayKind::Info);
            pin_headless_hud(app, fr.now, Some(0));
        }
        if plx_base::devtrig::flag("chapters") {
            crate::app::bridge::open_player_overlay(&mut app.player.session, app.bridge.metadata_view(), &mut app.pages, plx_screens::player::overlay::OverlayKind::Chapters);
            pin_headless_hud(app, fr.now, Some(1));
        }
    }
}

/// `/tmp/plxnative-menupick=<tab>,<target>`: `target` is either an absolute `TableView` row
/// number (the original contract), on the Audio tab a NAMED target — `"boost"`/`"loudness"` —
/// resolved through the panel's own [`plx_appkit::track_menu::TrackRow`] identities
/// (`TrackMenuState::row_for_audio_target`), or on the Subtitles tab `"track:N"`, the N-th track in
/// page order (root tracks, then those behind Other languages), committed by its own index
/// (`TrackMenuState::sub_track_for_target` / `commit_sub_track`). A name survives a track-count change a hand-written
/// row number does not: `audio_enhancement_arm` below writes one instead of deriving the row
/// itself, which is the issue #266 PR4 fix this trigger inherits (see
/// `TrackMenuState::row_for_audio_target`'s doc for that history). An unrecognized name logs a
/// clear line and commits nothing, same shape as the existing "row already active" no-commit log.
fn menupick_arm(app: &mut App, fr: &mut Frame) {
    if !app.scenarios.menupick_tried && matches!(app.route(), AppArg::Player) && fr.now.wrapping_sub(app.t0) > 7000 {
        app.scenarios.menupick_tried = true;
        if let Some(s) = plx_base::devtrig::read("menupick") {
            let mut it = s.split(',');
            let tab = it.next().and_then(|x| x.trim().parse::<c_int>().ok()).unwrap_or(0);
            let target = it.next().map(|x| x.trim().to_string()).unwrap_or_else(|| "0".to_string());
            crate::app::bridge::open_player_overlay(&mut app.player.session, app.bridge.metadata_view(), &mut app.pages, plx_screens::player::overlay::OverlayKind::Tracks { tab });
            app.scenarios.menupick_target = Some(target);
        }
    }
    if let Some(target) = app.scenarios.menupick_target.take() {
        let meta = app.bridge.metadata_view();
        match crate::app::bridge::player_overlay_mut(&mut app.pages) {
            Some(surface) => match surface.resolve_menupick_track(&target) {
                Some(i) => match surface.pick_sub_track(meta, i) {
                    Some(commit) => crate::app::playback::commit_track(&mut app.player.session, commit),
                    None => plx_base::eventlog::log(&format!("menupick: track {i} gave no commit")),
                },
                None => match surface.resolve_menupick_row(&target) {
                    Some(row) => match surface.pick_track_row(meta, row) {
                        Some(commit) => crate::app::playback::commit_track(&mut app.player.session, commit),
                        // The menu's own on_ok treats picking the already-active row as a no-op: no
                        // commit, no route transition line. Without this, a manifest case whose row
                        // no longer differs from the start pick (e.g. #210's file-default rule) fails
                        // downstream as "no route transition" with nothing pointing back at menupick.
                        None => plx_base::eventlog::log(&format!("menupick: row {row} already active — no commit")),
                    },
                    None => plx_base::eventlog::log(&format!("menupick: unknown target {target:?} — no commit")),
                },
            },
            None => app.scenarios.menupick_target = Some(target),
        }
    }
}

/// `/tmp/plxnative-submenuosc=<period_ms>`'s own state — see [`submenuosc_arm`].
#[derive(Debug, Default)]
pub(crate) struct SubmenuOsc {
    /// The period, read once when the arm first runs (`None` = not read yet, `Some(0)` = not armed).
    period: Option<u32>,
    /// The frame clock of the last key.
    last: u32,
    /// Where in [`submenuosc_next`]'s cycle the next key is.
    step: u8,
    /// Whether the Subtitles ROOT offered Other languages when last seen there (a pushed page's
    /// own form has no such row, so the root is the only place to ask).
    has_other: bool,
}

/// What one tick of the drill-in oscillator does: seat the cursor on a row (if any), then press `key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SubmenuStep {
    seat: Option<plx_appkit::track_menu::TrackRow>,
    key: Key,
    next: u8,
}

/// The oscillator's script, as a pure function of (cycle step, tab, page depth, has Other
/// languages): Subtitles root -> Style -> Size picker -> back -> back -> Other languages -> back ->
/// (LEFT at the root) the Audio tab -> (RIGHT) the Subtitles tab, and round again. A step whose
/// preconditions do not hold (the menu is somewhere the script did not expect, e.g. a key was
/// dropped) pops a page or restarts the cycle rather than pressing blind, so it re-syncs instead
/// of drifting. `Other languages` is skipped when the item has none.
fn submenuosc_next(step: u8, tab: c_int, depth: usize, has_other: bool) -> SubmenuStep {
    use plx_appkit::track_menu::{StyleField, TrackRow};
    let press = |seat: Option<TrackRow>, key: Key, next: u8| SubmenuStep { seat, key, next };
    let left = Key::Left;
    let right = Key::Right;
    match (step, tab, depth) {
        (0, 1, 0) => press(Some(TrackRow::Style), right, 1),
        (1, 1, 1) => press(Some(TrackRow::OpenField(StyleField::Size)), right, 2),
        (2, 1, 2) => press(None, left, 3),
        (3, 1, 1) => press(None, left, if has_other { 4 } else { 6 }),
        (4, 1, 0) => press(Some(TrackRow::OpenOther), right, 5),
        (5, 1, 1) => press(None, left, 6),
        (6, 1, 0) => press(None, left, 7),
        (7, 0, 0) => press(None, right, 0),
        // Off script: climb out of any page, then rejoin the cycle at the Subtitles root.
        (_, _, d) if d > 0 => press(None, left, step),
        (_, 0, _) => press(None, right, 0),
        _ => press(None, left, 0),
    }
}

/// `/tmp/plxnative-submenuosc=<period_ms>` — the device frame-time scene for the Tracks panel's
/// drill-in animation (panel resize + page slide). With the Subtitles menu open
/// (`plxnative-menu=1`) it presses one REAL key per period through the dispatcher
/// ([`submenuosc_next`]'s cycle), so every push, pop and tab switch runs the production handlers
/// and the spring they start. Shaped like `navosc`/`modalosc`: it never opens or dismisses
/// anything except that a menu dismissed under it (a stray key) is reopened on the Subtitles tab.
/// The period must exceed the slide's settle time (~0.5 s) to measure transitions, or be shorter
/// to measure interrupted ones; the manifest scene uses 900 ms.
fn submenuosc_arm(app: &mut App, fr: &mut Frame) {
    let period = *app.scenarios.submenu_osc.period.get_or_insert_with(|| {
        plx_base::devtrig::read("submenuosc").and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(0)
    });
    if period == 0 || !matches!(app.route(), AppArg::Player) || fr.now.wrapping_sub(app.t0) < 7000 {
        return;
    }
    plx_machine::idle::wake();
    if fr.now.wrapping_sub(app.scenarios.submenu_osc.last) < period {
        return;
    }
    app.scenarios.submenu_osc.last = fr.now;
    let probe = crate::app::bridge::player_overlay_mut(&mut app.pages).and_then(|s| s.tracks_probe());
    let Some((tab, depth, has_other)) = probe else {
        if !crate::app::bridge::player_overlay_up(&app.pages) {
            crate::app::bridge::open_player_overlay(&mut app.player.session, app.bridge.metadata_view(), &mut app.pages, plx_screens::player::overlay::OverlayKind::Tracks { tab: 1 });
            pin_headless_hud(app, fr.now, None);
            app.scenarios.submenu_osc.step = 0;
        }
        return;
    };
    if tab == 1 && depth == 0 {
        app.scenarios.submenu_osc.has_other = has_other;
    }
    let step = submenuosc_next(app.scenarios.submenu_osc.step, tab, depth, app.scenarios.submenu_osc.has_other);
    if let Some(row) = step.seat {
        if let Some(surface) = crate::app::bridge::player_overlay_mut(&mut app.pages) {
            if !surface.seat_track_row(row) {
                plx_base::eventlog::log(&format!("submenuosc: no row {row:?} on tab {tab} depth {depth} — skipping the step"));
            }
        }
    }
    plx_base::eventlog::log(&format!("submenuosc: step {} -> {} tab={tab} depth={depth} key={:?}", app.scenarios.submenu_osc.step, step.next, step.key));
    app.scenarios.submenu_osc.step = step.next;
    app.inputs.extend(crate::app::bridge::script_key(step.key, Tick { ms: fr.now, dt_us: 0 }));
}

/// The More oscillator's script, a pure function of the page depth: on the root seat the Quality
/// row and press RIGHT (a push); on the Quality page press LEFT (a pop). Anything deeper climbs out.
fn moreosc_next(depth: usize) -> (bool, Key) {
    if depth == 0 { (true, Key::Right) } else { (false, Key::Left) }
}

/// `/tmp/plxnative-moreosc=<period_ms>` — the device frame-time scene for More's Quality drill-in
/// (panel resize + page slide), the same shape as [`submenuosc_arm`]: with More open
/// (`plxnative-more=1`) one REAL key per period through the dispatcher, alternating push and pop.
/// A More dismissed under it is reopened at the root.
fn moreosc_arm(app: &mut App, fr: &mut Frame) {
    let period = *app.scenarios.more_osc.period.get_or_insert_with(|| {
        plx_base::devtrig::read("moreosc").and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(0)
    });
    if period == 0 || !matches!(app.route(), AppArg::Player) || fr.now.wrapping_sub(app.t0) < 7000 {
        return;
    }
    plx_machine::idle::wake();
    if fr.now.wrapping_sub(app.scenarios.more_osc.last) < period {
        return;
    }
    app.scenarios.more_osc.last = fr.now;
    let Some(depth) = crate::app::bridge::player_overlay_mut(&mut app.pages).and_then(|s| s.more_probe()) else {
        if !crate::app::bridge::player_overlay_up(&app.pages) {
            crate::app::bridge::open_player_overlay(&mut app.player.session, app.bridge.metadata_view(), &mut app.pages, plx_screens::player::overlay::OverlayKind::More { quality: false });
            pin_headless_hud(app, fr.now, None);
        }
        return;
    };
    let (seat, key) = moreosc_next(depth);
    if seat {
        let seated = crate::app::bridge::player_overlay_mut(&mut app.pages).is_some_and(|s| s.seat_more_quality());
        if !seated {
            // Nothing to push: say so, rather than let the scene grade a panel that never moved.
            plx_base::eventlog::log("moreosc: no Quality row");
            return;
        }
    }
    plx_base::eventlog::log(&format!("moreosc: depth={depth} key={key:?}"));
    app.inputs.extend(crate::app::bridge::script_key(key, Tick { ms: fr.now, dt_us: 0 }));
}

#[cfg(test)]
mod moreosc_script_tests {
    use super::moreosc_next;
    use plx_machine::machine::{Key};

    #[test]
    fn the_root_pushes_the_quality_row_and_the_page_pops() {
        assert_eq!(moreosc_next(0), (true, Key::Right));
        assert_eq!(moreosc_next(1), (false, Key::Left));
        assert_eq!(moreosc_next(3), (false, Key::Left), "off script: climb out, never press blind");
    }
}

/// `/tmp/plxnative-subtiming`'s own state across frames — see [`subtiming_arm`].
#[derive(Debug, Default)]
pub(crate) struct Subtiming {
    /// `true` once stage 1 (the subtitle commit) has fired, so a later frame does not commit a
    /// second time.
    tried: bool,
    /// Stage 1's commit, held until `route::cur_sub_sid` agrees — stage 2's own gate — then
    /// cleared once the capsule has been asked to open.
    pending: Option<SubtimingPending>,
}

/// A stage-1 commit stage 2 is waiting on.
#[derive(Debug, Clone, Copy)]
struct SubtimingPending {
    /// The committed stream id.
    sid: i64,
    /// The frame clock at which it was armed — stage 2's deadline: if `route::cur_sub_sid` has not
    /// agreed within ~3 s of this, the arm opens the capsule anyway rather than waiting on a commit
    /// that may never land (see `subtiming_arm`'s own doc for why a commit can silently not
    /// converge).
    armed_at: u32,
}

/// [`subtiming_arm`]'s pure stage-2 decision: given the frame clock, when stage 1 armed (committed
/// or, on the fused fast path, opened directly), the session's live `route::cur_sub_sid`, and the
/// stream id stage 1 wants, what should this frame do?
///
/// [`SubtimingStep::Open`] once the commit has visibly landed; [`SubtimingStep::OpenMismatch`]
/// once 3 s have passed without it landing — a bound, not a guess: a retry can reset track
/// selection underneath this commit, or a transcode's PUT can simply never resolve, and a scene
/// that waits forever for `cur_sub_sid` to agree fails as "the arm logged nothing", with no way to
/// tell stage 1 even fired. Opening late (mismatched) is still useful evidence; never opening is
/// not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubtimingStep {
    Wait,
    Open,
    OpenMismatch,
}

const SUBTIMING_DEADLINE_MS: u32 = 3_000;

fn subtiming_step(now: u32, armed_at: u32, cur_sid: i64, want_sid: i64) -> SubtimingStep {
    if cur_sid == want_sid {
        SubtimingStep::Open
    } else if now.wrapping_sub(armed_at) > SUBTIMING_DEADLINE_MS {
        SubtimingStep::OpenMismatch
    } else {
        SubtimingStep::Wait
    }
}

/// `/tmp/plxnative-subtiming` — the Timing capsule's own headless trigger (plan
/// `subtitle-menu-capsule` §5), self-contained rather than riding `plxnative-menupick`: that one
/// fires at 7000 ms and would stack a Tracks panel on top of the capsule this trigger wants alone.
///
/// **Stage 1**, once the item's PLAN has landed and it is not transcoding: pick the first embedded
/// TEXT subtitle, preferring English (on `movie_h264_ac3_1080p` that is index 2, the same track
/// `subtitle_text_srt` already selects, so the fixture server sees no new selection). The gate is
/// `route::cur_rk` being non-empty (the same "no plan yet" idiom `decision.rs` itself uses), NOT
/// `player::is_playing`: on this item the pipeline does not present its first decoded frame until
/// ~13 s into a 26 s scene, so gating on "playing" opened the capsule too late to clear warmup, or
/// — depending how long that particular run took to reach it — not at all. `route::cur_sub_sid` is
/// already meaningful the moment the plan lands (`decision.rs::apply_plan` sets it from the
/// resolved `/decision` verdict), long before a frame is on screen.
///
/// If the server already selected exactly this track (the `server-selected subtitle:` line —
/// `cur_sub_sid` already agrees), there is nothing to commit: skip straight to opening, with no
/// server write at all. Otherwise commit it exactly as `menupick_arm` commits a picked row, and arm
/// stage 2.
///
/// **Stage 2**, on a later frame: [`subtiming_step`] decides whether the commit has visibly landed
/// (open) or a 3 s deadline has passed without it (open anyway, logged as a mismatch) — see its own
/// doc for why an unbounded wait is not acceptable. No `pin_headless_hud` call in either path,
/// because the production capsule frame hides the HUD rather than pinning it
/// (`OverlayKind::hud_policy`, `PlayerScreen::set_hud_policy`).
fn subtiming_arm(app: &mut App, fr: &mut Frame) {
    use plx_data::metadata::sub_layout::is_image_sub_codec;
    if let Some(SubtimingPending { sid: want_sid, armed_at }) = app.scenarios.subtiming.pending {
        let cur_sid = plx_media::route::cur_sub_sid(&app.player.session);
        match subtiming_step(fr.now, armed_at, cur_sid, want_sid) {
            SubtimingStep::Wait => {}
            step => {
                app.scenarios.subtiming.pending = None;
                if step == SubtimingStep::OpenMismatch {
                    plx_base::eventlog::log(&format!(
                        "subtiming: opened (sid mismatch cur={cur_sid} want={want_sid})"
                    ));
                } else {
                    plx_base::eventlog::log("subtiming: opened");
                }
                open_timing(app);
            }
        }
        return;
    }
    if app.scenarios.subtiming.tried || !plx_base::devtrig::flag("subtiming") {
        return;
    }
    if !matches!(app.route(), AppArg::Player)
        || plx_media::route::cur_rk(&app.player.session).is_empty()
        || plx_media::route::is_transcoding(&app.player.session)
    {
        return;
    }
    let Some(item) = app.bridge.metadata_view().playing() else { return };
    let idx = item
        .subs
        .iter()
        .position(|s| !is_image_sub_codec(&s.codec) && s.lang_code == "eng")
        .or_else(|| item.subs.iter().position(|s| !is_image_sub_codec(&s.codec)));
    let Some(i) = idx else {
        plx_base::eventlog::log("subtiming: no text sub");
        app.scenarios.subtiming.tried = true;
        return;
    };
    let stream_id = item.subs[i].id;
    let render_ordinal = plx_data::metadata::sub_render_ordinal(&item.subs, i);
    app.scenarios.subtiming.tried = true;
    if plx_media::route::cur_sub_sid(&app.player.session) == stream_id {
        plx_base::eventlog::log(&format!("subtiming: already sid={stream_id} — opened without a commit"));
        open_timing(app);
        return;
    }
    plx_base::eventlog::log(&format!("subtiming: committed sid={stream_id}"));
    app.scenarios.subtiming.pending = Some(SubtimingPending { sid: stream_id, armed_at: fr.now });
    crate::app::playback::commit_track(
        &mut app.player.session,
        plx_appkit::track_menu::TrackCommit::Subtitle {
            render_ordinal,
            stream_id,
            sidecar_key: None,
            sidecar_codec: String::new(),
        },
    );
}

/// Present the Timing capsule on the player page — both of [`subtiming_arm`]'s open paths.
fn open_timing(app: &mut App) {
    crate::app::bridge::open_player_overlay(
        &app.player.session,
        app.bridge.metadata_view(),
        &mut app.pages,
        plx_screens::player::overlay::OverlayKind::Timing,
    );
}

#[cfg(test)]
mod submenuosc_script_tests {
    use super::submenuosc_next;
    use plx_machine::machine::{Key};

    /// Follow the script against a model of the menu (RIGHT on a seated Nav row pushes, LEFT pops
    /// or, at the root, goes to Audio, RIGHT on Audio goes back): it must exercise a push, a pop
    /// and both tab switches, and return to where it began.
    fn walk(has_other: bool) -> (Vec<(i32, usize)>, u8) {
        let (mut step, mut tab, mut depth) = (0u8, 1i32, 0usize);
        let mut seen = Vec::new();
        for _ in 0..16 {
            let s = submenuosc_next(step, tab, depth, has_other);
            match (s.key, s.seat.is_some()) {
                (Key::Right, true) => depth += 1,
                (Key::Right, false) => tab = 1,
                (Key::Left, _) if depth > 0 => depth -= 1,
                (Key::Left, _) => tab = 0,
                _ => unreachable!(),
            }
            step = s.next;
            seen.push((tab, depth));
            if step == 0 && tab == 1 && depth == 0 {
                return (seen, step);
            }
        }
        (seen, step)
    }

    #[test]
    fn the_cycle_pushes_pops_switches_tabs_and_comes_back_to_the_root() {
        let (seen, step) = walk(true);
        assert_eq!(step, 0);
        assert!(seen.contains(&(1, 2)), "Style -> Size picker is reached: {seen:?}");
        assert!(seen.contains(&(0, 0)), "the Audio tab is visited: {seen:?}");
        assert_eq!(seen.last(), Some(&(1, 0)));
        let (without, _) = walk(false);
        assert!(without.len() < seen.len(), "no Other languages row, no Other languages leg");
    }

    #[test]
    fn an_off_script_menu_climbs_out_instead_of_pressing_blind() {
        // A page is open where the script wants the root: pop it, keep the step.
        let s = submenuosc_next(0, 1, 2, true);
        assert_eq!((s.key, s.seat, s.next), (Key::Left, None, 0));
        // Lost on the Audio tab at step 3: go back to Subtitles and restart.
        let s = submenuosc_next(3, 0, 0, true);
        assert_eq!((s.key, s.next), (Key::Right, 0));
    }
}

#[cfg(test)]
mod subtiming_step_tests {
    use super::{subtiming_step, SubtimingStep};

    #[test]
    fn opens_the_moment_cur_sub_sid_agrees() {
        assert_eq!(subtiming_step(1_000, 900, 2698, 2698), SubtimingStep::Open);
    }

    #[test]
    fn waits_before_the_deadline_while_it_disagrees() {
        assert_eq!(subtiming_step(1_000, 900, 0, 2698), SubtimingStep::Wait);
        assert_eq!(subtiming_step(3_899, 900, 0, 2698), SubtimingStep::Wait);
    }

    #[test]
    fn opens_mismatched_once_the_deadline_passes_without_agreement() {
        assert_eq!(subtiming_step(3_901, 900, 0, 2698), SubtimingStep::OpenMismatch);
    }

    #[test]
    fn agreement_wins_even_past_the_deadline() {
        assert_eq!(subtiming_step(9_000, 900, 2698, 2698), SubtimingStep::Open);
    }
}

fn marker_arm(app: &mut App, _fr: &mut Frame) {
    if !app.scenarios.marker_tried && matches!(app.route(), AppArg::Player) && plx_media::player::is_playing(&mut app.player.session) {
        match plx_base::devtrig::read("marker") {
            Some(s) => {
                let want = if s.eq_ignore_ascii_case("intro") {
                    plx_data::metadata::MarkerKind::Intro
                } else {
                    plx_data::metadata::MarkerKind::Credits
                };
                let meta = app.bridge.metadata_view();
                let markers = meta.playing_markers();
                if !markers.is_empty() {
                    app.scenarios.marker_tried = true;
                    if let Some(m) = markers.iter().find(|m| m.kind == want) {
                        let t = (m.start_ms - 5_000).max(0) * 1_000_000;
                        plx_base::eventlog::log(&format!("marker trigger: seek to {}s (5s before {:?})", t / 1_000_000_000, want));
                        crate::app::playback::request_seek(t);
                    } else {
                        plx_base::eventlog::log(&format!("marker trigger: item has no {want:?} marker"));
                    }
                }
            }
            None => app.scenarios.marker_tried = true,
        }
    }
}

/// Whether the replay-after-EOS arm should re-arm the next boot iteration. `handed_off_to_up_next`
/// is `finish_playback`'s own return — the only live signal that this EOS was a real exit rather
/// than an Up Next handoff, since the exit's `PopTo` only PARKS the navigation (applied at the
/// next commit) and `app.route()` still reads `Player` for the rest of the frame either way.
fn should_replay_after_eos(handed_off_to_up_next: bool, replay_left: u32, playurl_flag: bool) -> bool {
    !handed_off_to_up_next && replay_left > 0 && playurl_flag
}

/// `/tmp/plxnative-replay[=N]` — REPLAY AFTER COMPLETION (LG App Self Checklist #46). Called from
/// `app::run::playback_tick` right after `finish_playback` has left the player on a real EOS;
/// `handed_off_to_up_next` is that call's own return value, telling an Up Next handoff apart from
/// a real exit (see [`should_replay_after_eos`]). Re-arming `auto_tried` sends the next frame back
/// through the `playurl` entry, which calls `route::clear_url()` and lets `start_bufferfeed` read
/// the trigger again. The trigger is read once at boot (`replay_left`), so this cannot become an
/// endless loop from a file appearing mid-run, and `devtrig::flag` is `false` at COMPILE time in a
/// release build.
pub(crate) fn maybe_replay_after_eos(app: &mut App, handed_off_to_up_next: bool) {
    if should_replay_after_eos(handed_off_to_up_next, app.scenarios.replay_left, plx_base::devtrig::flag("playurl")) {
        app.scenarios.replay_left -= 1;
        app.scenarios.auto_tried = false;
        plx_base::eventlog::log(&format!(
            "replay: starting the finished stream again ({} left)", app.scenarios.replay_left
        ));
    }
}

#[cfg(test)]
mod replay_after_eos_tests {
    use super::should_replay_after_eos;

    #[test]
    fn an_up_next_handoff_never_rearms_the_replay_even_with_budget_and_flag() {
        assert!(!should_replay_after_eos(true, 3, true));
    }

    #[test]
    fn a_real_exit_rearms_only_with_budget_left_and_the_flag_set() {
        assert!(should_replay_after_eos(false, 1, true));
        assert!(!should_replay_after_eos(false, 0, true), "no budget left");
        assert!(!should_replay_after_eos(false, 1, false), "trigger not armed");
    }
}

/// The boot-trigger SCRIPTS (autoplay, seek, quality, pause, menu, menupick, subtiming, marker),
/// called once per iteration from `app::run::run`. Always `true` now: no arm here can refuse a
/// frame (the slot-addressed content arms that could went with the Plex pages).
pub(crate) unsafe fn each_frame(app: &mut App, fr: &mut Frame) -> bool {
    #[cfg(feature = "devtriggers")]
    phcount::tick(fr.now);
    autoplay_arm(app, fr);
    screenshot::clockstop_arm(app, fr);
    autoseek_arm(app, fr);
    qualityswitch_arm(app, fr);
    autopause_arm(app, fr);
    menu_arm(app, fr);
    menupick_arm(app, fr);
    subtiming_arm(app, fr);
    submenuosc_arm(app, fr);
    moreosc_arm(app, fr);
    marker_arm(app, fr);
    if crate::app::bridge::player(&app.pages).is_some() {
        plx_media::player::failure_fixture(&mut app.player.session);
    }
    true
}
