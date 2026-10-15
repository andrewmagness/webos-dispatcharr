//! Every dev-trigger ARM — the code that reads `/tmp/dispatcharrtv-*` through [`plx_base::devtrig::flag`] /
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



pub(crate) mod screenshot;
#[cfg(feature = "devtriggers")]
pub(crate) mod phcount;
#[cfg(feature = "devtriggers")]
pub(crate) mod toast_probe;

/// The dev triggers read ONCE at boot and consulted by the loop after (each is documented where
/// it is READ, below).
pub(crate) struct DevFlags {
    /// `dispatcharrtv-nobudget`: read at boot, applied to the one `Budget` at boot, and kept here so
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
    pub(crate) pause_tried: bool,
    /// An armed Pause edge: (due at, hold ms, the media position it also waits for).
    pub(crate) pause_script: Option<(u32, Option<u32>, Option<u32>)>,
    pub(crate) pause_resume_at: Option<u32>,
    /// The boot-time trigger flags the loop consults every frame after.
    pub(crate) dev: DevFlags,
    /// The screenshot pipeline's arms (`dispatcharrtv-clockstop`) — see [`screenshot`].
    pub(crate) shots: screenshot::ScreenshotArms,
}

impl Default for DevFlags {
    fn default() -> Self { Self { nobudget: false } }
}

// =================================================================================================
// pre-SDL
// =================================================================================================

/// `/tmp/dispatcharrtv-stats` — force the Stats-for-nerds overlay on, before SDL or a screen exists,
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

/// `/tmp/dispatcharrtv-novsync` — uncap the swap interval so `fps=` reports the true GPU render rate.
pub(crate) fn novsync_armed() -> bool {
    plx_base::devtrig::flag("novsync")
}

plx_base::devtrig::latched_flag! { pub(crate) fn imagecache_stats_armed = "imagecache-stats"; }
plx_base::devtrig::latched_flag! {
    /// `/tmp/dispatcharrtv-imgtrace` — per-image poster timeline and every picture-lost event
    /// (`app/adapters/poster/trace.rs`). Absent in shipping builds.
    pub(crate) fn imgtrace_armed = "imgtrace";
}
plx_base::devtrig::latched_flag! {
    /// RAM-only control leg for cache performance comparisons; absent in shipping builds.
    pub(crate) fn imagecache_bypass_armed = "imagecache-bypass";
}

/// `/tmp/dispatcharrtv-stillclock=<ms>` — see [`screenshot::arm_stillclock`].
pub(crate) fn arm_stillclock() {
    screenshot::arm_stillclock();
}

/// `/tmp/dispatcharrtv-anim` — the animation-diagnostic overlay (off by default).
pub(crate) fn arm_anim() {
    if plx_base::devtrig::flag("anim") {
        plx_ui::anim::set_enabled(true);
    }
}

/// `/tmp/dispatcharrtv-glassload` — the backdrop-glass LOAD DIAL.
pub(crate) fn arm_glassload(glass: &mut plx_ui::frame::glass::GlassPlan) {
    if let Some(v) = plx_base::devtrig::read("glassload") {
        glass.configure_dial(&v);
    }
}

/// `/tmp/dispatcharrtv-navblur` — the blurred-route-transition prototype.
pub(crate) fn arm_navblur(glass: &mut plx_ui::frame::glass::GlassPlan) {
    if let Some(v) = plx_base::devtrig::read("navblur") {
        glass.configure_navblur(&v);
    }
}

/// `/tmp/dispatcharrtv-overdraw` — the CPU-side per-draw-class overdraw ledger.
pub(crate) fn arm_overdraw() {
    if plx_base::devtrig::flag("overdraw") {
        plx_gfx::overdraw::set_ledger(true);
    }
}

/// `/tmp/dispatcharrtv-drawmask=<classes>` — refuse every draw of the named classes.
pub(crate) fn arm_drawmask() {
    if let Some(spec) = plx_base::devtrig::read("drawmask") {
        plx_gfx::overdraw::set_mask(&spec);
    }
}

/// `/tmp/dispatcharrtv-profile` / `/tmp/dispatcharrtv-hwcnt` — the two GPU-time profilers. Both present
/// is refused; either alone arms its mode.
///
/// Gated on `devtriggers` at the item level rather than left to `devtrig::read` folding to `None`:
/// `devtrig::read` already makes this whole function inert in a release build, but the disabled-both
/// diagnostic line below spells out both trigger names in full, and a compiled-but-unreachable
/// function still carries its own string literals into `--no-default-features` bytes. Compiling
/// the function out entirely is what actually keeps `dispatcharrtv-profile`/`dispatcharrtv-hwcnt` out of
/// the binary `ci/check-package.py`'s dev-trigger-catalog check inspects.
#[cfg(feature = "devtriggers")]
pub(crate) fn arm_profile_hwcnt() {
    match (plx_base::devtrig::read("profile"), plx_base::devtrig::read("hwcnt")) {
        (Some(_), Some(_)) => {
            plx_base::eventlog::log("PROFILE disabled: remove either /tmp/dispatcharrtv-profile or /tmp/dispatcharrtv-hwcnt");
        }
        (Some(filter), None) => plx_ui::profile::set_enabled(&filter),
        (None, Some(filter)) => plx_ui::profile::set_hwcnt_enabled(&filter),
        (None, None) => {}
    }
}
#[cfg(not(feature = "devtriggers"))]
pub(crate) fn arm_profile_hwcnt() {}

/// `/tmp/dispatcharrtv-cpuprof` — the render thread's own per-phase CPU clock.
pub(crate) fn arm_cpuprof() {
    if plx_base::devtrig::flag("cpuprof") {
        plx_ui::profile::set_cpu_enabled();
    }
}

/// `/tmp/dispatcharrtv-noidle` — turn the whole-frame present gate off.
pub(crate) fn arm_noidle() {
    if plx_base::devtrig::flag("noidle") {
        plx_machine::idle::set_enabled(false);
        #[cfg(feature = "devtriggers")]
        plx_base::eventlog::log("idle: present gate DISABLED by /tmp/dispatcharrtv-noidle");
    }
}

/// `/tmp/dispatcharrtv-audioenh=off|boost|loudness` — force the PERSISTED Boost Dialog / Normalize
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
        "audioenh: forced boost_dialog={} normalize_loudness={} by /tmp/dispatcharrtv-audioenh",
        a.boost_dialog, a.normalize_loudness,
    ));
}

/// `/tmp/dispatcharrtv-nobudget` — the frame budget's A/B CONTROL LEG (spec §8.1, phase 11).
///
/// Present, admission is what it was before phase 11: the `Poster` quota of three per frame and
/// nothing else — no time ceiling, no solo rule, and a `Residency` upload (a backdrop, a hero
/// logo) spending one of those three exactly as it used to. It exists so a device A/B measures
/// this CHANGE and not the difference between two builds, and it is DIAG for the reason
/// `dispatcharrtv-drawmask` is: an A/B whose two legs boot to different screens has measured the
/// screen.
pub(crate) fn nobudget_armed() -> bool {
    plx_base::devtrig::flag("nobudget")
}

/// `/tmp/dispatcharrtv-framedrop[=<ms>]`.
pub(crate) fn framedrop_value() -> Option<String> {
    plx_base::devtrig::read("framedrop")
}
/// `/tmp/dispatcharrtv-framering[=<ms>]` — the frame-drop detector's context ring
/// (`diag::heartbeat::FrameRing`): write only frames of `<ms>` or more (default 17) and their
/// neighbours, instead of every frame. `None` when unarmed.
pub(crate) fn framering_ms() -> Option<f64> {
    plx_base::devtrig::read("framering").map(|s| s.parse().ok().filter(|v: &f64| *v > 0.0).unwrap_or(17.0))
}
/// `/tmp/dispatcharrtv-replay[=N]`'s raw content, for [`crate::app::boot::replay_budget`].
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


/// `/tmp/dispatcharrtv-playurl` (or `-autoplay`/`-h265`) — start the synthetic playback named by the
/// `dispatcharrtv-playurl` trigger once, two seconds into the run, with the transport pinned for a
/// headless capture. The engine reads the trigger itself when `route::url()` is empty.
fn autoplay_arm(app: &mut App, fr: &mut Frame) {
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

/// `/tmp/dispatcharrtv-replay[=N]` — REPLAY AFTER COMPLETION (LG App Self Checklist #46). Called from
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
    marker_arm(app, fr);
    if crate::app::bridge::player(&app.pages).is_some() {
        plx_media::player::failure_fixture(&mut app.player.session);
    }
    true
}
