//! player — the buffer-feed video engine (was src/playback.c). THREADING: everything
//! here except sf_on_event/acb_on_event runs on the SDL main thread. Those two are
//! #[no_mangle] and run on the StarfishMediaAPIs library thread; they touch ONLY
//! `SHARED`. Player-engine callback/transport state is synchronized in `shared.rs`; route
//! ownership and route-changing intents have their separate synchronized authority in
//! `route::PLAYER_CONTROL`. The Engine (engine.rs) is main-thread-confined. Design:
//! docs/engine-port-design.md.
//!
//! "Runs on the SDL main thread" is a **compile error to violate** for the two things where it
//! matters — the ACB/Starfish seam and the native session slot. `plex_run` mints ONE
//! [`MainThread`] token; `boot` moves it into [`adapter::PlayerAdapter`], which owns the session
//! that was `engine::ENGINE`. The seam still takes `&MainThread` (reached through the adapter),
//! the slot takes `&mut PlayerAdapter`, and the token is `!Send`, so a closure that captured
//! either cannot be handed to `task::spawn`. The exceptions are the honest ones: the two callbacks
//! above are `extern "C"` entry points *from* the library thread and touch only `SHARED`, and
//! `threads::load_thread` calls `sf_load` off-main by design (see `ffi`).
#![allow(non_upper_case_globals)]
pub mod adapter;
pub mod ass; // pinned libass worker and immutable rendered frames
pub mod ass_script; // a sidecar script fed in windows around the playhead
pub mod ass_source; // bounded embedded scripts and subtitle presentation clock
pub mod engine;
pub mod lifecycle;
pub mod machine;
pub mod playurl; // the `dispatcharrtv-playurl` dev trigger: a stream and its Load declaration, parsed beside the engine that acts on it
// `test` here is THIS crate's own test build only, not `any(test, feature = "test-support")`: the
// application crate's `port.rs` names `StarfishSink` in every build of its library that is not its
// own test build, and `cargo check --lib --tests` builds that library with this crate's
// `test-support` on. The application's test binaries therefore carry this module's code, but port.rs
// installs `NoSink` there, so nothing reaches its externs and the linker drops them.
#[cfg(all(not(feature = "hostsim"), not(test)))]
pub mod ffi;
#[cfg(feature = "hostsim")]
pub mod ffi_host;
mod pump;
pub mod report;
mod shared;
#[cfg(feature = "hostsim")]
pub mod sim_video; // the simulator's decoded picture, for screenshots (see its doc)
pub mod threads;
pub mod video_geometry;
pub mod vp9_hdr; // HDR10 for a VP9 stream: the container's PQ tag, written into the sourceInfo envelope ACB is sent

use plx_base::task::MainThread;
pub use shared::HlsAutomaticTransition;

/// one rect of an image-subtitle display set — the demuxer builds them, the HUD draws them
pub use shared::SubRect;
pub use plx_data::metadata::track_names::TrackNames;

use shared::{
    HlsPauseCompletion, HlsPlayCompletion, HlsUserPause, HlsUserResume, Shared, SubBitmap, SubCue,
    Transport,
};

/// The video sink every seam call goes through. The port installs the television's
/// (`player::ffi::StarfishSink`) or the simulator's (`player::ffi_host::HostSink`) once at boot; a
/// hostsim test binary reads the host sink directly, so it needs no port. Everything in `player/`
/// reaches the seam as `sink().<verb>(mt, ..)`, and nothing branches on which platform it is.
#[cfg(not(all(any(test, feature = "test-support"), feature = "hostsim")))]
fn sink() -> &'static dyn plx_platform::tv::sink::VideoSink {
    plx_platform::tv::sink::installed()
}
#[cfg(all(any(test, feature = "test-support"), feature = "hostsim"))]
fn sink() -> &'static dyn plx_platform::tv::sink::VideoSink {
    &ffi_host::HostSink
}

/// `/tmp/dispatcharrtv-tracknames[=<audio>;<subs>]` — **stand in for the container's own track names**,
/// which nothing off-device can read.
///
/// It exists for the same reason `/tmp/dispatcharrtv-personbio` does, and the shape of the problem is
/// identical: the data comes from a source no automated or host run can reach, so without a seed
/// every headless look at the screen shows the degenerate state. Here the source is the DEMUXER —
/// `ff::track_names` publishes these when it opens a part — and the desktop simulator has no
/// demuxer at all (the bundled FFmpeg is ARM, and `player::ffi_host` has no video path), so
/// the picker there can only ever draw what PMS sent. Which, for the MP4 this exists to fix, is a
/// column of identical language names.
///
/// Pipe-separated within a list, `;` between the two lists, audio first:
/// `Дубляж|Original;Forced|Full`. Either side may be empty (`;Forced|Full` seeds subtitles alone).
/// An EMPTY file seeds the real nine-track sample this was built against, because that is the shape
/// that exercises the case: six same-language rows PMS reports identically, which no shorter list
/// demonstrates.
///
/// **It seeds the real store and stubs nothing else** — the same `SHARED.track_names` the demuxer
/// writes, read back through the same `metadata::track_label::track_name` precedence. So a seeded
/// screenshot verifies the ROW, honestly; what it cannot verify is the FFI read that fills the
/// store on a television. Compiled out of a release build with every other trigger (`devtrig::read` is
/// a compile-time `None`), so a shipped binary cannot be made to show a name that is not the
/// file's.
pub fn seed_dev_track_names() {
    let Some(spec) = plx_base::devtrig::read("tracknames") else {
        return;
    };
    // The real subtitle names of a nine-track MP4 whose PMS record carries none — the file this
    // whole path was written against. Audio is left empty on purpose: one list is enough to
    // demonstrate, and seeding audio too would hide the `audio_descriptor` fallback the real menu
    // still uses for a track the container does not name.
    const SAMPLE: &str = ";Форс. iTunes|Форс. Jaskier песни|Форс. Red Head Sound песни|Полные iTunes|Полные Jaskier|Полные stirloo|Full|Full SDH|Повнi iTunes";
    let spec = spec.trim().to_string();
    let spec = if spec.is_empty() {
        SAMPLE
    } else {
        spec.as_str()
    };
    let list = |s: &str| -> Vec<String> {
        if s.is_empty() {
            Vec::new()
        } else {
            s.split('|').map(|p| p.trim().to_string()).collect()
        }
    };
    // `split_once(';')`, so a name may not contain a `;` and everything after the first one is the
    // subtitle list — a seed is a diagnostic, not a format, and the alternative is quoting rules.
    let (a, sub) = spec.split_once(';').unwrap_or(("", spec));
    let (audio, subs) = (list(a), list(sub));
    #[cfg(feature = "devtriggers")]
    plx_base::eventlog::log(&format!(
        "player: DEV track names seeded (a={} s={}) — /tmp/dispatcharrtv-tracknames",
        audio.len(),
        subs.len()
    ));
    *SHARED.track_names.lock().unwrap() = TrackNames { audio, subs };
}
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_long, c_uint};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering::Relaxed};

pub static SHARED: Shared = Shared::new();
pub static TX: Transport = Transport::new();
static ACB_OK: AtomicBool = AtomicBool::new(false); // was the g_acb availability flag

/// **Is the Stats-for-nerds read-out on screen?** The bit `pump::publish_diag` asks before it
/// samples the queues (`aq_bytes` takes each queue's pthread mutex, so nobody pays for a panel
/// nobody is looking at). It is the player's because the pump reads it and `player` may not name
/// `app`; `app::diagnostics` owns the panel and is the only writer (`toggle`/`open`/`close`).
/// Off at boot, and never persisted.
pub static DIAG_READOUT_ON: AtomicBool = AtomicBool::new(false);

// Kodi in-place seek (flush + reopen + re-anchor the decode position + sendSegmentEvent, NO
// reload/decoder re-init → no HDR-mode popup, no A/V-resync glitch). On webOS<11 (this 4.5)
// setTimeToDecode returns 0, so feed_stream falls back to the content-info path
// (loadSpi_getInfo + setContentInfo(ptsToDecode) — the same path the official app uses).
// Cleared to false if the pipeline can't be reached (sf_send_segment == 0), which drops seeks
// back to the robust reload-per-seek path.
//
// SCOPE: this is a **per-session probe, not a device-capability latch**, and
// `engine::start_bufferfeed` re-arms it to true for every new session. What `sf_send_segment`
// reports is whether `sf_pipeline()` could reach the CustomPipeline behind the CURRENT
// StarfishMediaAPIs object: `SMP_READY()` (dispatch admission set after construction, and cleared
// by destruction or quarantine) plus two non-null shared_ptr hops, `object+0x4c` -> `player+0x04`
// (src/starfish.c). A cleared ready bit does not prove that object storage is unconstructed: the
// safety path retains a quarantined object forever. Every one of these tests is a property of the
// current dispatchable object, not a device capability. `sendSegmentEvent` itself returns
// void, so a 0 here NEVER means "the segment event was rejected", only "there was nothing to
// call it on" — a liveness/timing condition by construction.
// Latching it for the process was therefore a bug with a very long tail: one teardown-window
// race downgraded every later seek of every later item to a ~1 s reload until the app was
// restarted. Re-arming per session is self-healing rather than oscillating, too: the fallback
// the clear selects (`reload_at`) builds a fresh Starfish object, so the exact condition that
// produced the 0 cannot survive into the session that re-arms — and if a fresh session really
// can't reach its pipeline either, it re-clears after one seek and stays on the reload path.
// It remains a static rather than an `Engine` field only because `pump.rs` reads it without an
// Engine borrow at hand; its LIFETIME is the Engine's, since `start_bufferfeed` is the sole
// constructor and the flag is only ever read while a session is live.
pub static INPLACE_SEEK_OK: AtomicBool = AtomicBool::new(true);
static PTYPE: AtomicI32 = AtomicI32::new(10); // g_ptype (PLAYER_TYPE_MSE)

// ---- API app.rs calls (were extern "C" fns in playback.h) ----
//
// `start_bufferfeed`/`start_bufferfeed_tracked` gate on this device's `/dev/rtkmem` jail
// pre-flight (community-tier finding: webosbrew/webos-homebrew-channel PR #202, 2019 Realtek
// k5lp/k3lp sets, default Developer-Mode jailer missing that device node, known to crash native
// A/V apps on this chassis) INSIDE `engine::start_bufferfeed_tracked` itself, latching the
// refusal onto `ps.jail_load_blocked` — a `PlaybackSession` field, not a process-wide static —
// so `state()` derives `PlaybackState::Error` for exactly the attempt that was refused and
// `crate::route::cancel_play`'s `clear_play_verdict` (the same ritual that retires a `/decision`
// refusal on leaving the player, called from `app.rs`'s `exit_player`) retires it precisely the
// same way. See `engine.rs`'s `start_bufferfeed`/`start_bufferfeed_tracked` for the gate itself.
pub use engine::{
    acb_init, start_bufferfeed, start_bufferfeed_tracked, stop_bufferfeed,
    suspend_bufferfeed, BufferfeedStartOutcome, };

pub use pump::pump;
pub use shared::PlaybackState;

/// Is the rebuild now loading the one an audio-track pick asked for? A pick that is still waiting
/// for its reload counts only inside [`shared::AUDIO_PICK_WINDOW_MS`]; once the reload has begun
/// it counts until the session's first frame.
pub fn audio_switching() -> bool {
    match shared::AudioSwitch::from_u8(SHARED.audio_switch.load(Relaxed)) {
        shared::AudioSwitch::Reloading => true,
        shared::AudioSwitch::Picked => {
            i64::from(vclock_ms()) - SHARED.audio_pick_ms.load(Relaxed) <= shared::AUDIO_PICK_WINDOW_MS
        }
        shared::AudioSwitch::None => false,
    }
}

/// [`busy_caption`] with the audio-switch fact given — the pure choice, host-testable.
pub fn busy_caption_with(st: PlaybackState, audio_switch: bool) -> &'static std::ffi::CStr {
    if audio_switch && st == PlaybackState::Buffering {
        plx_platform::i18n::msg::widgets_status_switching_audio_c()
    } else {
        st.caption()
    }
}

/// Test-only: stand the audio-switch state up (`0` none, `1` picked, `2` reloading).
#[cfg(any(test, feature = "test-support"))]
pub fn set_audio_switch_for_test(state: u8) {
    SHARED.audio_pick_ms.store(i64::from(vclock_ms()), Relaxed);
    SHARED.audio_switch.store(state, Relaxed);
}
pub fn pause(pa: &mut adapter::PlayerAdapter) -> bool {
    match SHARED.prepare_hls_user_pause() {
        Some(HlsUserPause::AlreadyHeld) => {
            TX.commit_paused(true);
            acb_mirror_playstate(pa, false);
            return true;
        }
        Some(HlsUserPause::Issue(token)) => {
            let accepted = unsafe { sink().pause(pa.mt()) } != 0;
            match SHARED.complete_hls_user_pause(token, accepted) {
                HlsPauseCompletion::Accepted => {}
                HlsPauseCompletion::Refused => {
                    log("player: Starfish refused Pause");
                    return false;
                }
                HlsPauseCompletion::Stale => {
                    log("player: Pause result lost its clock token");
                    return false;
                }
            }
        }
        None => {
            log("player: Pause deferred by an in-flight clock transition");
            return false;
        }
    }
    // Publish the feed gate at the same accepted actuator boundary. Leaving this to app.rs after
    // the ACB call let a deadline transaction charge accepted Pause time as active playback.
    TX.commit_paused(true);
    acb_mirror_playstate(pa, false);
    true
} // playback_pause
pub fn resume(pa: &mut adapter::PlayerAdapter) -> bool {
    let queued_stream = pa.engine().is_some_and(|eng| eng.uses_stream_queues());
    match SHARED.prepare_hls_user_resume(queued_stream) {
        Some(HlsUserResume::Deferred) => {
            // Feeding may resume, but an initial/seek/recovery certificate still owns the physical
            // clock. Its eventual Play also carries the pending ACB Resume.
            if TX.seek_preroll_active() {
                TX.finish_seek_preroll();
            }
            TX.commit_paused(false);
            log("player: user Resume accepted; physical Play remains fenced");
            true
        }
        Some(HlsUserResume::Prime) => {
            // Keep Starfish and ACB physically Paused. Opening TX first lets the two AU lanes fill
            // together; their ordinary exact prime certificate owns the eventual Play + ACB
            // Resume. Starting the clock here recreates the pause-to-fill A/V drift race.
            let Some(eng) = pa.engine() else {
                log("player: queued Resume lost its Engine before prime could arm");
                return false;
            };
            engine::arm_live_clock_prime(eng);
            if TX.seek_preroll_active() {
                TX.finish_seek_preroll();
            }
            TX.commit_paused(false);
            log("player: queued Resume feeding; physical Play awaits balanced prime");
            true
        }
        Some(HlsUserResume::Issue(token)) => {
            let accepted = unsafe { sink().play(pa.mt()) } != 0;
            match SHARED.complete_hls_prime_play(token, accepted) {
                HlsPlayCompletion::Accepted { resume_acb } => {
                    if TX.seek_preroll_active() {
                        TX.finish_seek_preroll();
                    }
                    TX.commit_paused(false);
                    if resume_acb {
                        acb_mirror_playstate(pa, true);
                    }
                    true
                }
                HlsPlayCompletion::Refused => {
                    log("player: Starfish refused Play");
                    false
                }
                HlsPlayCompletion::Stale => {
                    log("player: Resume result lost its clock token");
                    false
                }
            }
        }
        None if TX.seek_preroll_active() => {
            // The viewer cancelled "stay paused" after the seek had already transferred the
            // physical hold to Initial/Seek (or after its one-frame Play). No native command is
            // needed here; the prime owns Play if it has not happened yet.
            TX.finish_seek_preroll();
            TX.commit_paused(false);
            true
        }
        None => {
            log("player: Resume has no accepted user hold");
            false
        }
    }
} // playback_resume

#[cfg(all(any(test, feature = "test-support"), feature = "hostsim"))]
pub fn force_pause_result_for_test(result: Option<c_int>) {
    ffi_host::force_pause_result_for_test(result);
}

#[cfg(all(any(test, feature = "test-support"), feature = "hostsim"))]
pub fn force_play_result_for_test(result: Option<c_int>) {
    ffi_host::force_play_result_for_test(result);
}

/// Arm (or disarm) the simulator's clock sink for a test, so a Load is accepted and the pump runs
/// past it, without the process-wide `dispatcharrtv-clocksink` trigger.
#[cfg(all(any(test, feature = "test-support"), feature = "hostsim"))]
pub fn force_clocksink_for_test(on: bool) {
    ffi_host::force_clocksink_for_test(on);
}

/// Kodi parity: mirror the ACB PLAYSTATE on transport pause/resume (the pipeline Pause/Play alone
/// leaves the app-owned sink's ACB state stale). Only once the plane is streaming — `Bound` means
/// setMediaId/LOADED has happened but setMediaVideoData/window/PLAYING has not, so mirroring a user
/// Resume there would overtake the rest of the ordered bind transaction.
pub fn acb_mirror_playstate(pa: &mut adapter::PlayerAdapter, playing: bool) {
    let (eng, mt) = pa.split();
    let Some(stage) = eng.map(|e| e.stage) else {
        return;
    };
    acb_mirror_playstate_at(mt, stage, playing);
}

/// [`acb_mirror_playstate`] for a caller that already holds the session's `&mut Engine` and so
/// cannot ask the adapter for it a second time — `engine::try_prime`, inside the feed. Splitting
/// the two is what the owned slot forces: with `ENGINE` a `static mut` handing out `&'static mut`,
/// the prime path re-entered the slot it was already holding, which is the aliasing the token
/// could only assert was safe.
pub fn acb_mirror_playstate_at(mt: &MainThread, stage: shared::Stage, playing: bool) {
    if !ACB_OK.load(Relaxed) {
        return;
    }
    if !acb_playstate_ready(stage) {
        return;
    }
    unsafe {
        if playing {
            sink().plane_resume(mt);
        } else {
            sink().plane_pause(mt);
        }
    }
}

fn acb_playstate_ready(stage: shared::Stage) -> bool {
    stage >= shared::Stage::Streaming
}

/// **Answer the Player machine's video-plane question from the seam** (spec §9), once per frame,
/// at §3.3 step 8 — before the opaque-region call and the present gate read the bit.
///
/// `Stage::Streaming` is the FIRST stage at which the ordered ACB bind transaction has finished:
/// `setMediaId` → LOADED → `setMediaVideoData` → `setDisplayWindow` → PLAYING. It is the same
/// predicate [`acb_mirror_playstate`] uses to decide whether the sink can take a PLAYSTATE at all,
/// deliberately — one definition of "the plane is really ours", not two. No engine means no sink,
/// so the bit clears on teardown without teardown having to remember to say so.
///
/// This is an OBSERVATION of the adapter, not a second owner: it hands the answer to
/// [`machine::Player::set_video_plane_bound`], which is the only writer and the only publisher.
pub fn observe_video_plane(
    pl: &mut machine::Player,
    pa: &mut adapter::PlayerAdapter,
) -> Option<bool> {
    let bound = pa.engine().is_some_and(|e| acb_playstate_ready(e.stage));
    pl.set_video_plane_bound(bound)
}

// ---- transport accessors app.rs / player_hud.rs call ----
pub fn is_started() -> bool {
    TX.started.load(Relaxed)
}
/// Coded video raster, atomically published by the demuxer. Subtitle renderers need this
/// independently of the output canvas for anamorphic glyph/blur scaling.
pub fn video_raster() -> (i32, i32) {
    SHARED.video_raster()
}

/// The picture inside the full-screen video window, including non-square pixels.
pub fn video_viewport(width: i32, height: i32) -> video_geometry::Viewport {
    let (w, h) = SHARED.video_raster();
    video_geometry::Aspect::unpack(SHARED.video_aspect.load(std::sync::atomic::Ordering::Acquire))
        .or_else(|| video_geometry::Aspect::from_raster(w, h))
        .unwrap_or_else(|| video_geometry::Aspect::from_raster(width, height).unwrap())
        .fit(width, height)
}

pub fn playpos_ns() -> i64 {
    SHARED.playpos_ns.load(Relaxed)
}
pub fn frames() -> i32 {
    SHARED.frames.load(Relaxed)
}
/// True once this SESSION has presented at least one frame. Deliberately NOT `frames() > 0`: the
/// pump zeroes `frames` as part of applying a seek (`pump.rs`), so that expression reads "no
/// picture" for the whole of every seek. Cleared only by `reset_session` — i.e. by a real stop or
/// a reload, both of which do blank the video plane. See [`shared::Shared::seen_frame`].
pub fn seen_frame() -> bool {
    SHARED.seen_frame.load(Relaxed)
}
pub fn duration_ns() -> i64 {
    SHARED.duration_ns.load(Relaxed)
}
/// **The codecs a failed live channel should be declared with**, when that is why it failed: the
/// demuxer refused the stream because it is not what the Load declared, and what it found is
/// something a Load can declare. `None` otherwise (still starting, playing, failed for another
/// reason, or a codec no Load can name, which the failure read-out reports).
pub fn live_codec_correction(ps: &crate::route::PlaybackSession) -> Option<(String, String)> {
    if !crate::route::is_live(ps) || !SHARED.demux_failed.load(Relaxed) {
        return None;
    }
    let found = SHARED.stream_codecs.lock().unwrap_or_else(|e| e.into_inner()).clone()?;
    codec_correction(
        (&crate::route::stream_vcodec(ps), &crate::route::stream_acodec(ps)),
        (&found.0, &found.1),
    )
}

/// [`live_codec_correction`]'s decision: the found pair, when it differs from the declared one
/// and a Load can declare it.
fn codec_correction(declared: (&str, &str), found: (&str, &str)) -> Option<(String, String)> {
    let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    if same(declared.0, found.0) && same(declared.1, found.1) {
        return None;
    }
    engine::LoadVideo::of_stream(found.0)?;
    if !found.1.is_empty() {
        engine::audio_payload_codec(found.1)?;
    }
    Some((found.0.to_ascii_lowercase(), found.1.to_ascii_lowercase()))
}

#[cfg(test)]
mod codec_correction_tests {
    use super::codec_correction;

    #[test]
    fn a_failover_stream_in_ac3_is_redeclared_and_an_undeclarable_one_is_not() {
        assert_eq!(codec_correction(("h264", "aac"), ("h264", "ac3")), Some(("h264".into(), "ac3".into())));
        assert_eq!(codec_correction(("h264", "aac"), ("hevc", "eac3")), Some(("hevc".into(), "eac3".into())));
        assert_eq!(codec_correction(("h264", "aac"), ("h264", "aac")), None, "nothing to correct");
        assert_eq!(codec_correction(("h264", "aac"), ("mpeg2video", "aac")), None, "no Load names MPEG-2 video");
        assert_eq!(codec_correction(("h264", "aac"), ("h264", "mp2")), Some(("h264".into(), "mp2".into())),
            "MP2 audio is declared as libpf's MP3");
    }
}

/// true once the pipeline has drained to true end-of-stream (see pump's EOS check). app.rs polls
/// this to tear the player down at the credits.
pub fn ended() -> bool {
    SHARED.ended.load(Relaxed)
}
pub fn request_seek(ns: i64) {
    report::note_seek_for(crate::route::playback_trace_generation());
    crate::route::note_user_seek_intent(ns);
    SHARED.ended.store(false, Relaxed); // seeking back from the end un-ends the stream
    SHARED.seeking.store(true, Relaxed); // HUD: spinner + freeze the playhead until it lands
    SHARED.seek_display_ns.store(ns, Relaxed);
    TX.seek_to_ns.store(ns, Relaxed);
    // Count the request even though the target it carries may be overwritten before the pump
    // ever sees it — that overwrite IS the coalescing, and this is the only place it's countable.
    TX.seek_reqs.fetch_add(1, Relaxed);
}
/// true while a seek is resolving (request → reopen/reload → prime → Play): the HUD shows a
/// spinner and freezes the playhead at `seek_display_ns` instead of wobbling through the reopen.
pub fn loading(ps: &crate::route::PlaybackSession) -> bool {
    state(ps).is_busy()
}
/// true only while the pipeline is actually presenting frames — not resolving, connecting,
/// buffering or seeking. app.rs gates the heartbeat's `pos=` field on this, so a pre-roll 0 is
/// never logged as playback.
pub fn is_playing(ps: &crate::route::PlaybackSession) -> bool {
    matches!(state(ps), shared::PlaybackState::Playing)
}
/// The derived playback state — the ONE thing the HUD renders from. See `PlaybackState`.
pub fn state(ps: &crate::route::PlaybackSession) -> shared::PlaybackState {
    // …and so is the PRE-FLIGHT refusal, for exactly the same reason: `/decision` answers before a
    // byte of video moves, so the plan fails with no URL, no engine is ever built, and the pump
    // that owns `pb_state` never runs. Deriving it in the one reader keeps a single writer — the
    // alternative is poking `Error` into the player's state from the frame loop. It sits BELOW the
    // resolve check because a fresh resolve is the thing that retires the last verdict.
    // A start refused because a timed-out Load is still parked (`ps.native_start_refused`) is the
    // same kind of fact: no Engine was built, so the pump that owns `pb_state` never runs and
    // nothing else could say why the player is empty.
    if ps.jail_load_blocked
        || ps.native_start_refused
        || crate::route::play_refused(ps)
        || crate::route::play_resolution_failed(ps)
    {
        return shared::PlaybackState::Error;
    }
    // A seek in flight is derived HERE too, not only published by the pump's own ladder (which
    // says the same thing in the same order — a seek outranks frames). `request_seek` sets the
    // flag at the press, but `pb_state` is only republished at the end of a pump pass, so a
    // reader between the two still saw Playing: the HUD, which freezes the playhead at the
    // target only while busy, drew one frame of the PRE-seek position between the scrub preview
    // and the frozen target — a visible jump back and forth on every seek.
    if SHARED.seeking.load(Relaxed) {
        return shared::PlaybackState::Seeking;
    }
    shared::PlaybackState::from_u8(SHARED.pb_state.load(Relaxed))
}

/// The one line that makes a phone photograph of the failure read-out a complete report:
/// `Dispatcharr TV 0.6.0-dev · webOS 4.10.2 · 43LM6300PVB · m3r · tv_pipeline`. It exists because
/// issue #63 arrived as a title, a model name and nothing else — no version, no reason — and the
/// only surface carrying those facts (Stats for nerds) is reachable from the player's overflow
/// menu alone and is force-closed on leaving playback, so a failed session had no way to show them.
///
/// Pure. Every part is a product identity or a closed vocabulary: the version every surface shares
/// (`plx_base::identity`), the firmware release, the set (model · board · hw — the rule
/// `app::diagnostics::device_rows` states: shared by every unit LG built, saying nothing about a
/// household) and [`FailureKind::code`], the same string the telemetry channel sends. It never
/// carries `ErrorShape::detail`, the server's free text, which is the one thing here that could
/// name a file.
pub fn support_line(kind: FailureKind) -> String {
    support_line_of(
        plx_platform::tv::device::info(),
        plx_platform::tv::device::device(),
        kind,
    )
}
fn support_line_of(i: &plx_platform::tv::device::Info, hw: &plx_platform::tv::device::Hardware, kind: FailureKind) -> String {
    let set = hw.set_line();
    let set: &str = if set.is_empty() { plx_platform::i18n::msg::settings_login_unknown_device() } else { &set };
    format!(
        "{} {} · {} · {} · {}",
        plx_base::identity::DISPLAY_NAME,
        plx_base::identity::version(),
        plx_platform::i18n::webos_release_line(i),
        set,
        kind.code()
    )
}

/// The `Error` state's wording, shaped by WHY — issue #22's lesson: `ff: no video stream` was
/// technically true and cost the reviewer a full server-side investigation that the sentence
/// "the server sent audio only" would have ended. Pure so every arm is host-testable; the two
/// wrappers below feed it the globals (main thread only — `route::is_transcoding` reads
/// main-thread state).
///
/// `sub` is `plex::serverinfo`'s Plex Pass tristate, an explicit parameter for the same purity.
/// It sharpens the WORDING of the transcode arm and nothing more — and on a KNOWN-free server it
/// appends the subscription as a support FACT, never as the cause. The distinction is the
/// codebase's own audit: docs/plex-pass-audit.md row 1 says h264 encoding is free everywhere,
/// and the profile's target chain ends in h264 precisely so a free server always HAS a usable
/// video target — so reaching this arm on one means something ELSE failed (a source the server's
/// ffmpeg cannot decode, transcoding disabled server-side), and "it cannot encode video without
/// Plex Pass" was asserting a cause the profile had already ruled out, pointing the user at a
/// purchase that would fix nothing. Known-true or unknown keeps the neutral wording alone: a
/// subscription the app cannot prove absent must never even be named. (And wording is as far as
/// subscription state may ever reach into playback — see `serverinfo::subscription`'s doc for
/// why it is not a routing input.)
///
/// The (no-video, not-transcoding) arm — an audio-only DIRECT-PLAYED file — is worded for the
/// file because that is the truth there: route only direct-plays when PMS metadata names an
/// h264/hevc video track, so reaching it means the file disagrees with its own metadata.
/// One error, three surfaces — the HUD caption, the diagnostics panel's verdict line, and the
/// full-screen read-out (`Player Screen.dc.html`, the spec that superseded the retired
/// `Plex Pass Awareness.dc.html` for this screen) — shaped in ONE place so
/// they can never disagree about what happened. `no_pass` is the read-out's cue to draw the
/// filled PLEX PASS capsule: a support FACT beside the reason, never the cause (see the arm
/// comment above).
/// **Why a playback failed, as a closed set.** Drives the wording below AND the telemetry code, so
/// the two are one decision.
///
/// Current variants are outcomes `error_shape` can tell apart. The retired `original_rollback`
/// wire code (the destructive probe transaction was removed) has no cause here any more and lives
/// on only as `telemetry::classes::FailureClass::OriginalRollback`, so old telemetry fixtures and
/// dashboards retain their meaning. Runtime source, interrupted-playback and `Load` failures became
/// distinct only when their worker signals existed. A video-plane bind or stalled feed still
/// reaches [`Unspecified`](FailureKind::Unspecified) until an equally concrete signal exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The stream carries no video track.
    NoVideoTrack,
    /// The producer never opened a usable media stream or produced a video access unit.
    MediaSource,
    /// The media producer stopped after playback had already begun. The signal does not identify
    /// whether that happened in the network, parser, allocator or ABR controller, so neither the
    /// stable code nor the viewer-facing sentence blames a server or connection.
    PlaybackInterrupted,
    /// Starfish refused the Load declaration, so no decoder session could start.
    TvPipeline,
    /// This device's jail is missing `/dev/rtkmem` on a SoC where that is a known cause of
    /// native A/V crashes — the Load was never attempted. Community-tier finding: see
    /// [`plx_platform::tv::sandbox::blocks_native_video`]'s doc.
    JailMissingRtkmem,
    /// Issue #74 D.1.4's `NATIVE_LOAD_BUDGET` fired — either the native `Load` call never
    /// returned, or it returned but `loadCompleted` never arrived. Distinct from
    /// [`TvPipeline`](Self::TvPipeline), which is a firmware refusal the pipeline actually
    /// reported: this is a HANG, and conflating the two made a k5lp-style stall indistinguishable
    /// from an ordinary rejection on the wire.
    LoadTimeout,
    /// Everything else. Honest rather than tidy — see the type's doc.
    Unspecified,
}

impl FailureKind {
    /// The telemetry wire class of this cause. The wire schema is telemetry's
    /// (`telemetry::classes::FailureClass`), so this match is the ONE place the player's own enum
    /// is turned into it, and a new variant cannot compile without choosing its class.
    pub fn class(self) -> plx_telemetry::telemetry::classes::FailureClass {
        use plx_telemetry::telemetry::classes::FailureClass as C;
        match self {
            FailureKind::NoVideoTrack => C::NoVideoTrack,
            FailureKind::MediaSource => C::MediaSource,
            FailureKind::PlaybackInterrupted => C::PlaybackInterrupted,
            FailureKind::TvPipeline => C::TvPipeline,
            FailureKind::JailMissingRtkmem => C::JailMissingRtkmem,
            FailureKind::LoadTimeout => C::LoadTimeout,
            FailureKind::Unspecified => C::Unspecified,
        }
    }

    /// The stable wire code, which is its class's: the read-out's support line and the telemetry
    /// channel quote one string because they read one table.
    pub fn code(self) -> &'static str {
        self.class().code()
    }
}

/// One control on the failure read-out's row. See [`failure_actions`], the ONE table that decides
/// which of these a failure offers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FailureAction {
    /// Tune the failed channel again.
    TryAgain,
    /// Review the sandbox repair (the confirmation `Player.repair` guards).
    Repair,
    /// Leave the player.
    Back,
}

/// What the table needs to know besides the kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FailureContext {
    /// The sandbox repair can still be started (nothing is running or finished).
    pub repair_idle: bool,
}

/// **The failure read-out's row, as ONE table** — every control the read-out draws, in draw order;
/// every key and click resolves through the same list, so a control cannot be drawn and dead, or
/// live and undrawn.
///
/// **The rule: offer an action only if it can change the outcome.** `Back` is always last, because
/// leaving always does something. Whatever stopped a channel (a busy server, a dropped connection,
/// a stream that never opened) may be gone a moment later, so tuning again is offered; a stream
/// with no video track and an unrepairable sandbox get nothing but *Back*.
pub fn failure_actions(kind: FailureKind, cx: FailureContext) -> Vec<FailureAction> {
    use FailureAction as A;
    use FailureKind as K;
    let mut v = Vec::with_capacity(2);
    match kind {
        // A device finding: nothing about the request changes it; only the repair can.
        K::JailMissingRtkmem => {
            if cx.repair_idle {
                v.push(A::Repair);
            }
        }
        // The stream itself has no picture; tuning again puts none in it.
        K::NoVideoTrack => {}
        _ => v.push(A::TryAgain),
    }
    v.push(A::Back);
    v
}

#[cfg(test)]
mod live_failure_tests {
    use super::{failure_actions, FailureAction as A, FailureContext, FailureKind as K};

    #[test]
    fn a_failed_live_channel_offers_try_again() {
        let cx = FailureContext { repair_idle: true };
        for kind in [K::MediaSource, K::PlaybackInterrupted, K::LoadTimeout, K::Unspecified] {
            assert_eq!(failure_actions(kind, cx), vec![A::TryAgain, A::Back], "{kind:?}");
        }
        assert_eq!(failure_actions(K::NoVideoTrack, cx), vec![A::Back], "a stream with no picture stays one");
    }

    #[test]
    fn the_repair_is_offered_only_while_it_can_start() {
        assert_eq!(failure_actions(K::JailMissingRtkmem, FailureContext { repair_idle: true }), vec![A::Repair, A::Back]);
        assert_eq!(failure_actions(K::JailMissingRtkmem, FailureContext { repair_idle: false }), vec![A::Back]);
    }
}

/// The live [`FailureContext`] (main thread).
pub fn failure_context(ps: &crate::route::PlaybackSession) -> FailureContext {
    FailureContext {
        repair_idle: ps.repair_status == plx_platform::tv::sandbox::State::Idle,
    }
}

pub struct ErrorShape {
    /// **The stable, machine-readable reason** — the one field here meant for a wire rather than
    /// for a person. Every other field is prose that will be re-worded, localised or shortened, and
    /// a dashboard keyed on any of them breaks the day somebody improves a sentence.
    ///
    /// It is not a parallel classification either: `panel`, `caption` and `readout` are all derived
    /// FROM it, so the words on screen and the code on the wire cannot come to disagree about what
    /// happened. (`panel` was the obvious thing to key telemetry on, and undercounts: a grep for
    /// `panel: "` finds three arms where there are four outcomes, because one builds its string
    /// through an inner `if`.)
    pub kind: FailureKind,
    pub caption: &'static std::ffi::CStr,
    /// the diagnostics panel's verdict suffix — always present in `Error`
    pub panel: &'static str,
    /// the read-out's reason line — sentence case
    pub readout: &'static str,
    /// Additional reason and recovery instructions for the viewer, authored by the app.
    ///
    /// **It deliberately does NOT reach `panel`.** The diagnostics panel is a PHOTOGRAPH — its
    /// module doc bans URLs, paths and item titles from it; this line is for the viewer's own
    /// screen.
    pub detail: std::borrow::Cow<'static, str>,
}

/// Which runtime boundary ended playback after route resolution succeeded.
///
/// This is a small product vocabulary, not a copy of FFmpeg or Starfish return codes. Every
/// variant is backed by a distinct signal the worker already publishes, so the HUD never parses
/// log strings or guesses from elapsed time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RuntimeFailure {
    /// No worker supplied a cause. Say that honestly rather than leaving the reason slot blank.
    Unknown,
    /// The producer never opened a usable media stream or produced a video access unit.
    MediaSource,
    /// The media producer stopped after it had supplied at least one access unit. The worker's
    /// flag deliberately says nothing narrower about why it stopped.
    PlaybackInterrupted,
    /// Starfish refused the Load declaration, so no decoder session could start.
    TvPipeline,
    /// Issue #74 D.1.4's `NATIVE_LOAD_BUDGET` fired — a hang, not a firmware refusal. See
    /// [`FailureKind::LoadTimeout`].
    LoadTimeout,
}

/// PURE: turn the four terminal worker signals into one cause. More specific downstream evidence
/// wins over the generic producer flag when concurrent teardown makes more than one bit visible.
/// `load_timed_out` is checked FIRST and independently of `load_failed`, even though `pump.rs`
/// always sets both together on a budget expiry: the distinction this function exists to make is
/// "did the pipeline refuse this, or did it never answer at all", and only the timed-out signal
/// can tell the two apart.
fn runtime_failure(
    demux_failed: bool,
    io_failed: bool,
    load_failed: bool,
    load_timed_out: bool,
) -> RuntimeFailure {
    if load_timed_out {
        RuntimeFailure::LoadTimeout
    } else if load_failed {
        RuntimeFailure::TvPipeline
    } else if io_failed {
        RuntimeFailure::PlaybackInterrupted
    } else if demux_failed {
        RuntimeFailure::MediaSource
    } else {
        RuntimeFailure::Unknown
    }
}

/// The read-out for [`FailureKind::JailMissingRtkmem`] — the one `ErrorShape` not produced by
/// [`error_shape`], because it precedes route resolution entirely: it names a device finding, not
/// a decision the server or the runtime made. Phrased as a FINDING throughout — "found... known
/// to..." — never as a certain diagnosis, matching the community-tier evidence it is built on
/// (see [`plx_platform::tv::sandbox::blocks_native_video`]'s doc). Caption and readout are kept short for
/// legibility from a phone photograph, same bar as every other arm here; the remedy's detail goes
/// in `detail`. `Player.repair` (see `tv::sandbox`) can actually attempt the Homebrew
/// Channel service call that patches the jail profile, so the remedy text points at that confirmed
/// in-app repair rather than at a bare reinstall.
/// A live channel whose video no Load can declare: the codec, and the server-side fix.
fn unsupported_video_shape(codec: &str) -> ErrorShape {
    let name = match codec {
        "mpeg2video" => "MPEG-2",
        "mpeg1video" => "MPEG-1",
        other => other,
    };
    ErrorShape {
        kind: FailureKind::MediaSource,
        caption: plx_platform::i18n::msg::widgets_failure_open_c(),
        panel: plx_platform::i18n::msg::dispatcharr_live_unsupported_video(),
        readout: plx_platform::i18n::msg::dispatcharr_live_unsupported_video(),
        detail: std::borrow::Cow::Owned(plx_platform::i18n::msg::dispatcharr_live_unsupported_help(name)),
    }
}

#[cfg(test)]
mod unsupported_video_tests {
    #[test]
    fn an_mpeg2_channel_names_its_format_and_the_fix() {
        let e = super::unsupported_video_shape("mpeg2video");
        assert_eq!(e.readout, plx_platform::i18n::msg::dispatcharr_live_unsupported_video());
        assert!(e.detail.contains("MPEG-2") && e.detail.contains("H.264"), "{}", e.detail);
    }
}

fn jail_error_shape() -> ErrorShape {
    ErrorShape {
        kind: FailureKind::JailMissingRtkmem,
        caption: plx_platform::i18n::msg::widgets_failure_jail_c(),
        panel: plx_platform::i18n::msg::widgets_panel_jail(),
        readout: plx_platform::i18n::msg::widgets_reason_jail(),
        detail: std::borrow::Cow::Borrowed(
            plx_platform::i18n::msg::widgets_reason_jail_help(),
        ),
    }
}

fn error_shape(no_video: bool, runtime: RuntimeFailure) -> ErrorShape {
    if no_video {
        return ErrorShape {
            kind: FailureKind::NoVideoTrack,
            caption: plx_platform::i18n::msg::widgets_failure_no_video_c(),
            panel: plx_platform::i18n::msg::widgets_panel_no_video(),
            readout: plx_platform::i18n::msg::widgets_reason_no_video(),
            detail: std::borrow::Cow::Borrowed(""),
        };
    }
    match runtime {
        RuntimeFailure::MediaSource => ErrorShape {
            kind: FailureKind::MediaSource,
            caption: plx_platform::i18n::msg::widgets_failure_open_c(),
            panel: plx_platform::i18n::msg::widgets_panel_open(),
            readout: plx_platform::i18n::msg::widgets_reason_open(),
            detail: std::borrow::Cow::Borrowed(""),
        },
        RuntimeFailure::PlaybackInterrupted => ErrorShape {
            kind: FailureKind::PlaybackInterrupted,
            caption: plx_platform::i18n::msg::widgets_failure_stopped_c(),
            panel: plx_platform::i18n::msg::widgets_panel_stopped(),
            readout: plx_platform::i18n::msg::widgets_reason_stopped(),
            detail: std::borrow::Cow::Borrowed(""),
        },
        RuntimeFailure::TvPipeline => ErrorShape {
            kind: FailureKind::TvPipeline,
            caption: plx_platform::i18n::msg::widgets_failure_tv_rejected_c(),
            panel: plx_platform::i18n::msg::widgets_panel_tv_rejected(),
            readout: plx_platform::i18n::msg::widgets_reason_tv_rejected(),
            detail: std::borrow::Cow::Borrowed(""),
        },
        // A DIFFERENT `kind` from `TvPipeline` (issue #74 D.1.4's budget is a hang, not a firmware
        // refusal — see `RuntimeFailure::LoadTimeout`'s doc), and now its own reader-facing wording
        // too: the pipeline never actually answered, so "rejected" would claim a firmware verdict
        // that was never given.
        RuntimeFailure::LoadTimeout => ErrorShape {
            kind: FailureKind::LoadTimeout,
            caption: plx_platform::i18n::msg::widgets_failure_load_timeout_c(),
            panel: plx_platform::i18n::msg::widgets_panel_load_timeout(),
            readout: plx_platform::i18n::msg::widgets_reason_load_timeout(),
            detail: std::borrow::Cow::Borrowed(""),
        },
        RuntimeFailure::Unknown => ErrorShape {
            kind: FailureKind::Unspecified,
            caption: plx_platform::i18n::msg::widgets_status_failed_c(),
            panel: plx_platform::i18n::msg::widgets_panel_unknown(),
            readout: plx_platform::i18n::msg::widgets_reason_unknown(),
            detail: std::borrow::Cow::Borrowed(""),
        },
    }
}

/// The live [`ErrorShape`] for `PlaybackState::Error` (main thread).
pub fn error_now(ps: &crate::route::PlaybackSession) -> ErrorShape {
    if let Some(arm) = failtest_arm() {
        return arm;
    }
    // Ahead of every other cause: on an affected, unfixed set every Load this boot has been (and
    // every later one will be) refused before it reached the native Engine at all, so no other
    // signal below can be the real explanation for a playback failure.
    if ps.jail_load_blocked {
        return jail_error_shape();
    }
    // A live channel in a video format the TV's pipeline does not take: say which, and the fix.
    if crate::route::is_live(ps) {
        if let Some(codec) = SHARED.unsupported_video.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return unsupported_video_shape(&codec);
        }
    }
    let demux_failed = SHARED
        .demux_failed
        .load(std::sync::atomic::Ordering::Acquire);
    let demux_io_failed = SHARED
        .demux_io_failed
        .load(std::sync::atomic::Ordering::Acquire);
    error_shape(
        SHARED.demux_no_video.load(Relaxed),
        runtime_failure(
            demux_failed,
            demux_io_failed,
            SHARED.load_failed.load(Relaxed),
            // The refused start's cause: an abandoned Load exists only after the budget fired.
            SHARED.load_timed_out.load(Relaxed) || ps.native_start_refused,
        ),
    )
}

/// dev: `/tmp/dispatcharrtv-failtest=<arm>` — force one variant of the failure read-out.
///
/// The read-out is the one screen in the app that **cannot be reached on purpose**: it needs a
/// server that refuses, which is exactly the state a working setup does not have. It is also the
/// screen most designed to be looked at — `Player Screen.dc.html` shapes it to survive a phone
/// photograph, because a maintainer triaging a report from someone else's television is its whole
/// audience. So the arms are selectable, and there is no other way to grade them on a panel.
///
/// Arms: `novideo` (an audio-only stream), `stream` (no usable media), `connection` (an interrupted
/// transfer), `tv` (the native pipeline refused Load), `jail` (this device's jail is missing
/// `/dev/rtkmem` — forces [`jail_error_shape`] regardless of the real device probe, since most
/// dev machines are not an affected SoC), and `none` or any other word (no cause was reported). It feeds
/// [`error_shape`] rather than short-circuiting it, so what is photographed is the real resolver.
///
/// `player_hud::busy` has the other half — the state itself — for the same reason.
///
fn failtest_arm() -> Option<ErrorShape> {
    let arm = plx_base::devtrig::read("failtest")?;
    Some(match arm.trim() {
        "novideo" => error_shape(true, RuntimeFailure::Unknown),
        "stream" => error_shape(false, RuntimeFailure::MediaSource),
        "connection" => error_shape(false, RuntimeFailure::PlaybackInterrupted),
        "tv" => error_shape(false, RuntimeFailure::TvPipeline),
        "load_timeout" => error_shape(false, RuntimeFailure::LoadTimeout),
        "jail" => jail_error_shape(),
        _ => error_shape(false, RuntimeFailure::Unknown),
    })
}

/// Publish the jail read-out fixture on the same session fact the owned confirmation reads.
/// Only ordinary development scenarios call this; controlled replay never reads this trigger.
/// It lives beside [`failtest_arm`], the other reader of `/tmp/dispatcharrtv-failtest`, because it
/// only reads that trigger and writes a `PlaybackSession` field — nothing of the app's.
pub fn failure_fixture(session: &mut crate::route::PlaybackSession) {
    if plx_base::devtrig::read("failtest").is_some_and(|arm| arm.trim() == "jail") {
        session.jail_load_blocked = true;
    }
}

/// HUD caption for `PlaybackState::Error` (main thread).
pub fn error_caption(ps: &crate::route::PlaybackSession) -> &'static std::ffi::CStr {
    error_now(ps).caption
}
/// The same non-empty answer for the diagnostics panel's verdict line.
pub fn error_reason(ps: &crate::route::PlaybackSession) -> &'static str {
    error_now(ps).panel
}
/// Test-only: drive the derived playback state, returning the previous raw value to restore.
///
/// `pb_state` is the pump's field and `shared` is a private module, so a host test that needs the
/// app in a given state — `app.rs`'s HUD-visibility pair, which pins the bug that made the `…` disc
/// unreachable while stalled — sets it through here rather than widening the module for a test.
/// Callers must hold `plx_base::testlock::serial()`: this is a crate global.
#[cfg(any(test, feature = "test-support"))]
pub fn swap_state_for_test(s: shared::PlaybackState) -> u8 {
    let prev = SHARED.pb_state.load(Relaxed);
    SHARED.pb_state.store(s as u8, Relaxed);
    prev
}
#[cfg(any(test, feature = "test-support"))]
pub fn restore_state_for_test(raw: u8) {
    SHARED.pb_state.store(raw, Relaxed);
}

// ---- diagnostics --------------------------------------------------------------------------

pub use engine::aq_caps;
pub use plx_platform::tv::sink::{VP_ACB, VP_EXPORTED, VP_NONE};
#[cfg(feature = "hostsim")]
/// The simulator's clock-sink stop; the television's pipeline has no such control.
pub use ffi_host::stop_clock_at as stop_sim_clock_at;

/// The video codec a `dg_load_v` code names (video 1 = H264, 2 = H265, 3 = AV1, 4 = VP9; 0 = not built).
/// The codes are written from `engine::LoadVideo::diag_code`, the names from its `payload_name`.
pub fn load_v_name(code: u8) -> &'static str {
    match code {
        1 => "H264",
        2 => "H265",
        3 => "AV1",
        4 => "VP9",
        _ => "—",
    }
}

/// One consistent read of everything the on-screen diagnostics overlay shows (`app::diagnostics`).
///
/// A struct rather than twenty accessors for one reason: the panel must not tell a story that
/// never happened. Sampled field-by-field across a frame it could report "no frames" beside a
/// callback count taken 16 ms later, and the whole point is that a maintainer trusts the
/// photograph. One call, one instant, main thread.
///
/// Everything here is a MIRROR — see `Shared`'s diagnostics block. Nothing in the playback state
/// machine may read a `Diag` back.
///
/// `Default` is the never-started session — all zero, no window, nothing fed — which is both the
/// honest pre-playback reading and what the host tests build, since the real [`diag`] reaches
/// `starfish.c` symbols that do not exist on the dev Mac.
#[derive(Default)]
pub struct Diag {
    pub vp_mode: c_int,
    pub window_id: String,
    pub acb_ok: bool,
    pub place_rv: i32,
    pub placed_w: i32,
    pub placed_h: i32,
    pub stage: u8,
    pub load_completed: bool,
    pub load_failed: bool,
    pub cb_count: u32,
    pub pushed_any: bool,
    pub fed_v: i64,
    pub fed_a: i64,
    pub frames: i32,
    pub seen_frame: bool,
    pub aq_video: i64,
    pub aq_audio: i64,
    pub fed_v_pts: i64,
    pub fed_a_pts: i64,
    pub load_v: u8,
    pub load_a: u8,
    pub feed_state: u8,
    pub cb_err: i32,
    pub cb_err_at: u32,
    pub http_status: i32,
    pub net_rx: i64,
    /// Playable content-time reserve derived from the elementary-stream tails and the displayed
    /// movie position. Unlike `abr_buffer_ms`, this is transport/controller independent and is
    /// therefore present for Manual Original and fixed qualities too. `None` means one required
    /// lane has not published a post-open/post-seek timestamp yet.
    pub playable_buffer_ms: Option<i64>,
    pub load_at: u32,
    pub frame_at: u32,
    pub video_w: i32,
    pub video_h: i32,
    pub video_fps_milli: i64,
    pub pos_ns: i64,
    pub dur_ns: i64,
    /// Whole-file transport requirement from the route resolve. Unlike the ABR fields this also
    /// exists for a manual Original session, so the diagnostics sweep can keep the same demand
    /// lane in every delivery mode.
    pub source_kbps: i64,
}

/// One physical reserve definition for every delivery mode. Direct-file tails already use movie
/// time and have `display_base_ns == 0`; segmented HLS and offset progressive transcodes publish a
/// zero-based tail and carry the movie offset in `display_base_ns`. Adding that base universally
/// makes all three shapes land in the same timeline without a route-specific heuristic.
fn playable_buffer_ms(
    video_tail_ns: i64,
    audio_tail_ns: i64,
    audio_expected: bool,
    display_base_ns: i64,
    playpos_ns: i64,
) -> Option<i64> {
    if video_tail_ns < 0 {
        return None;
    }
    let tail_ns = if audio_expected {
        if audio_tail_ns < 0 {
            return None;
        }
        video_tail_ns.min(audio_tail_ns)
    } else {
        video_tail_ns
    };
    Some(
        tail_ns
            .saturating_add(display_base_ns.max(0))
            .saturating_sub(playpos_ns.max(0))
            .max(0)
            / 1_000_000,
    )
}

impl Diag {
    /// The lab snapshot's wire spelling; the on-screen read-out words its own labels.
    #[cfg_attr(not(feature = "lab-diagnostics"), allow(dead_code))]
    pub fn vp_mode_str(&self) -> &'static str {
        match self.vp_mode {
            VP_EXPORTED => "exported window (webOS 5+)",
            VP_ACB => "ACB (webOS 4)",
            _ => "NONE — no video path",
        }
    }
    /// What the Load payload named as the video codec, or `—` before one was built.
    pub fn load_v_str(&self) -> &'static str {
        load_v_name(self.load_v)
    }
    /// …and the audio codec. `needAudio:false` is its own answer, not an absence.
    pub fn load_a_str(&self) -> &'static str {
        match self.load_a {
            1 => "AC3",
            2 => "AC3 PLUS",
            3 => "AAC",
            4 => "DTS",
            5 => "MP3",
            _ => "NONE (needAudio:false)",
        }
    }
    /// Why the VIDEO feeder is where it is. The video lane specifically: the picture is what a
    /// user complains about, and a two-lane string overflows the value column.
    ///
    /// `queue empty` vs `BufferFull` is the row's whole point — a dead PRODUCER and a dead SINK
    /// look identical from every other field on the panel and want opposite fixes.
    ///
    /// The throttle state is worded as what it IS, not as what it is waiting for. It was
    /// "waiting for a frame", which is the literal truth and reads as a stall — and it is the
    /// state a healthy playback sits in most of the time, because the feeder deliberately stays
    /// within `MAX_FEED_AHEAD_NS` of the presented position. The first person to see the panel in
    /// the wild asked why playback was stuck; it was not.
    #[cfg_attr(not(feature = "lab-diagnostics"), allow(dead_code))]
    pub fn feed_state_str(&self) -> &'static str {
        match self.feed_state {
            1 => "accepting",
            2 => "BufferFull (sink is full)",
            3 => "REFUSED",
            4 => "holding ~1.6 s ahead",
            5 => "queue empty (no data)",
            _ => "— nothing fed yet",
        }
    }
    /// Only an outright refusal is a fault. BufferFull is the steady state under the feed-ahead
    /// throttle, and the throttle and an empty queue are ordinary moments in a healthy stream.
    pub fn feed_is_fault(&self) -> bool {
        self.feed_state == 3
    }
}

pub fn diag(ps: &crate::route::PlaybackSession) -> Diag {
    let (fed_v, fed_a) = engine::fed_totals();
    // The sink hands back the seam's own static buffer — never NULL, "" when no window was
    // created — so this is a copy of a bounded char[64], not a borrow with a lifetime to reason about.
    let window_id = sink().window_id().to_string_lossy().into_owned();
    let load_a = SHARED.dg_load_a.load(Relaxed);
    let playable_buffer_ms = playable_buffer_ms(
        SHARED.hls_video_tail_ns.load(Relaxed),
        SHARED.hls_audio_tail_ns.load(Relaxed),
        load_a != 0,
        SHARED.disp_base.load(Relaxed),
        SHARED.playpos_ns.load(Relaxed),
    );
    let (video_w, video_h) = SHARED.video_raster();
    Diag {
        vp_mode: sink().window_mode(),
        window_id,
        acb_ok: ACB_OK.load(Relaxed),
        place_rv: SHARED.dg_place_rv.load(Relaxed),
        placed_w: SHARED.dg_placed_w.load(Relaxed),
        placed_h: SHARED.dg_placed_h.load(Relaxed),
        stage: SHARED.dg_stage.load(Relaxed),
        load_completed: SHARED.load_completed.load(Relaxed),
        load_failed: SHARED.load_failed.load(Relaxed),
        cb_count: SHARED.dg_cb_count.load(Relaxed),
        pushed_any: crate::ff::pushed_any(),
        fed_v,
        fed_a,
        frames: SHARED.frames.load(Relaxed),
        seen_frame: SHARED.seen_frame.load(Relaxed),
        aq_video: SHARED.dg_aq_video.load(Relaxed),
        aq_audio: SHARED.dg_aq_audio.load(Relaxed),
        fed_v_pts: SHARED.dg_fed_v_pts.load(Relaxed),
        fed_a_pts: SHARED.dg_fed_a_pts.load(Relaxed),
        load_v: SHARED.dg_load_v.load(Relaxed),
        load_a,
        feed_state: SHARED.dg_feed_state.load(Relaxed),
        cb_err: SHARED.dg_cb_err.load(Relaxed),
        cb_err_at: SHARED.dg_cb_err_at.load(Relaxed),
        http_status: SHARED.dg_http_status.load(Relaxed),
        net_rx: SHARED.dg_net_rx.load(Relaxed),
        playable_buffer_ms,
        load_at: SHARED.dg_load_at.load(Relaxed),
        frame_at: SHARED.dg_frame_at.load(Relaxed),
        video_w,
        video_h,
        video_fps_milli: SHARED.video_fps_milli.load(Relaxed),
        pos_ns: SHARED.playpos_ns.load(Relaxed),
        dur_ns: SHARED.duration_ns.load(Relaxed),
        source_kbps: crate::route::transport_kbps(ps),
    }
}

/// Whether a route change has scheduled an encoder rebuild. Test-visible so route policy can be
/// graded independently of the pump's frame timing.
#[cfg(any(test, feature = "test-support"))]
pub fn pending_transcode_refresh() -> bool {
    crate::route::pending_user_route_intent(crate::route::UserRouteIntent::Retranscode)
}

// Dev-only: used only by `route::decision`'s `#[cfg(feature = "devtriggers")]` tests (a store
// build's `CredentialPolicy::HttpsOnly` refuses those tests' plaintext loopback PMS fixtures; see
// the comment on that module's first gated test).
#[cfg(all(any(test, feature = "test-support"), feature = "devtriggers"))]
pub fn pending_adaptive_reload() -> bool {
    crate::route::pending_user_route_intent(crate::route::UserRouteIntent::AdaptiveReload)
}

/// Route-policy tests share the process-wide player mailbox even though no Engine pumps it.
/// Empty it between cases so one test's requested handoff cannot become the next test's input.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_route_requests_for_test(ps: &crate::route::PlaybackSession) {
    crate::route::reset_player_control_for_test(ps);
}

// ---- client-rendered subtitles (direct-play only; a transcode carries no subs) ----
/// selected subtitle track index (-1 = off); the demuxer reads this per block.
pub fn desired_sub_idx() -> i32 {
    SHARED.desired_sub_idx.load(Relaxed)
}
/// The tone client-rendered subtitles are drawn in. A [`crate::route::LiveCell`] rather than a field
/// of [`SHARED`] because it OUTLIVES a playback — it is a preference, not session state
/// (the same reasoning as the quality cell in `route::prefs`) — and `reset_session` must not put a viewer back on white
/// between two episodes. Restored by `route::restore_all`; written by [`set_subtitle_tone`] through
/// the one write door (`route::choose`).
///
/// Seeded to white, which is what every build before the preference drew.
pub(crate) static SUBTITLE_TONE: crate::route::LiveCell<plx_plex::plex::session::SubtitleTone> =
    crate::route::LiveCell::new("subtitle tone preference");

/// The selected tone. Read once a frame by the two subtitle draws (`appkit::player_hud`) and by the
/// track menu for its checkmark.
pub fn subtitle_tone() -> plx_plex::plex::session::SubtitleTone {
    SUBTITLE_TONE.get()
}

/// The viewer's subtitle timing offset in MILLISECONDS — positive draws every client-rendered cue
/// later, negative earlier. **It belongs to ONE playback of ONE subtitle track**, unlike
/// [`SUBTITLE_TONE`]: a timing error is a property of a track against a media file, so the next
/// film, or another track of this one, must never inherit it. It is remembered per item (never per
/// track) by [`persist_subtitle_offset`], through the memory door, and recalled at that item's next
/// play; [`reset_subtitle`]
/// (a new item) and `route::commit_subtitle_selection` (a different track) put it back to 0. A
/// retry of the same item with the same subtitle carries it ([`restore_subtitle_offset`]).
/// `AtomicI32` rather than `I64`: the range fits trivially, and the 32-bit target gets a plain
/// word load on the per-frame lookups.
static SUBTITLE_OFFSET_MS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// The offset in milliseconds (the unit the menu speaks).
pub fn subtitle_offset_ms() -> i64 {
    i64::from(SUBTITLE_OFFSET_MS.load(Relaxed))
}

/// The offset in nanoseconds (the unit every cue store speaks).
pub(crate) fn subtitle_offset_ns() -> i64 {
    subtitle_offset_ms() * 1_000_000
}

/// **The content time whose cue is on screen at playhead `now_ns`**: the playhead minus the
/// offset, so a +2 s offset shows at 12 s the cue authored for 10 s. Every lookup — embedded text,
/// image sets, the sidecar — goes through this one subtraction; saturating, so no offset can wrap
/// a timestamp at either end of `i64`.
///
/// The sidecar holds its whole file, so any offset in its range is exact there. An EMBEDDED track
/// only has the cues the demuxer has read, which is why [`subtitle_offset_range_ms`] gives it no
/// advance at all. A delay is served from retained cues, but a seek can leave embedded text and
/// image stores without the required history: the demuxer restarts at the target, not before it.
/// Embedded ASS retains known events across an in-place seek, so buffered delayed cues remain
/// available. A full pipeline reload, including a native audio-track switch
/// (`engine::switch_audio_native` → `reload_at`), discards embedded history and can leave a delayed
/// track empty until enough history has been read again.
pub fn subtitle_clock_ns(now_ns: i64) -> i64 {
    now_ns.saturating_sub(subtitle_offset_ns())
}

/// The oldest cue end a store must still hold: the LATEST delay ([`SUBTITLE_OFFSET_LATEST_MS`])
/// plus two seconds behind the playhead, whatever the offset is NOW. A positive offset makes the
/// subtitle clock trail the playhead, and the demuxer never republishes a cue it has read, so a
/// floor that followed the current offset (2 s of history at offset 0) left nothing for a delay
/// raised mid-playback to show until the window had refilled. A negative offset (sidecar only)
/// reads ahead of the playhead, which this floor never prunes. What bounds memory is each store's
/// cap and its eviction order ([`push_subtitle_text`], [`SUB_BITMAP_BUDGET`]), not this floor.
fn subtitle_floor_ns() -> i64 {
    subtitle_floor_for(
        SHARED.playpos_ns.load(Relaxed),
        SHARED.seeking.load(Relaxed),
        SHARED.seek_display_ns.load(Relaxed),
    )
}

fn subtitle_floor_for(position_ns: i64, seeking: bool, target_ns: i64) -> i64 {
    // The demuxer can already be at a backward seek's target while the native clock still
    // reports the old picture. Keep those newly read cues through the rebase. min also makes
    // independently sampled atomics conservative if a new request races this read.
    let anchor = if seeking && target_ns >= 0 { position_ns.min(target_ns) } else { position_ns };
    anchor.saturating_sub((SUBTITLE_OFFSET_LATEST_MS + 2_000) * 1_000_000)
}

/// The latest the offset goes, for every kind of track: a DELAY is served from cues already in the
/// store, which retains this much history at every offset, so a delay raised mid-playback is
/// served at once (`subtitle_floor_ns`, and the image store's eviction order in
/// [`push_subtitle_bitmap`]).
pub const SUBTITLE_OFFSET_LATEST_MS: i64 = 60_000;

/// The viewer's Plex Pass audio-DSP preference (issue #266: dialog boost, loudness normalization),
/// as two bits — a PREFERENCE, like [`SUBTITLE_TONE`], so it outlives a playback. Only the
/// resolve's `ResolveEnv::snapshot` reads it, on the main thread, and it reaches the wire only
/// through `route::desired_audio`: with no Plex Pass (or an unknown one) nothing reads it at all.
/// Restored by `route::restore_all`; written through the one write door (`route::choose`).
pub(crate) static AUDIO_ENHANCEMENTS: crate::route::LiveCell<plx_plex::plex::AudioEnhancements> =
    crate::route::LiveCell::new("audio enhancements preference");

/// The current preference (see [`AUDIO_ENHANCEMENTS`]).
pub fn audio_enhancements() -> plx_plex::plex::AudioEnhancements {
    AUDIO_ENHANCEMENTS.get()
}

/// The text store's hard cap, a runaway guard that an ordinary file never reaches. The store
/// holds every text track (the demux pushes them all) from `subtitle_floor_ns` — 32 s behind the
/// playhead at EVERY offset — to the demuxer's read position. The read-ahead is bounded by the
/// A/V queues (`engine::AQ_VIDEO_BYTES` 10 MiB, `AQ_AUDIO_BYTES` 1 MiB); even a 2 Mbit/s encode
/// with 192 kbit/s audio fills them in about 42 s, so the window is at most 32 + 42 = 74 s. A
/// dense dialogue track runs about one cue per second at its peak: 3 text tracks x 74 s x 1
/// cue/s = 222 cues, under half the cap. Eviction is oldest first, i.e. history no delay is
/// reading at an ordinary offset.
const SUB_TEXT_CAP: usize = 512;

/// push a ready (already-clean) subtitle cue into the shared store, tagged with its 0-based
/// track index (the demux pushes for every text track).
/// Bounded by TIME rather than a fixed count: since every track is pushed regardless of
/// selection, drop cues more than the latest delay behind the playhead (`subtitle_floor_ns`)
/// and keep the demuxer's forward window. [`SUB_TEXT_CAP`] guards against a runaway.
pub fn push_subtitle_text(track: i32, start_ns: i64, end_ns: i64, text: String) {
    if text.is_empty() {
        return;
    }
    let mut cues = SHARED.sub_cues.lock().unwrap();
    let floor = subtitle_floor_ns();
    cues.retain(|c| c.end_ns >= floor);
    if cues.len() >= SUB_TEXT_CAP {
        cues.remove(0);
    }
    cues.push(SubCue {
        track,
        start_ns,
        end_ns,
        text,
    });
}
/// demux (D-thread) pushes a subtitle cue (content-time ns) for track `track`. Called for
/// EVERY plain-text track so a mid-play switch is instant; only the selected track's cues are logged.
pub fn push_subtitle_cue(
    track: i32,
    start_ns: i64,
    end_ns: i64,
    payload: &[u8],
) {
    let text = sub_text(payload);
    if text.is_empty() {
        return;
    }
    if track == SHARED.desired_sub_idx.load(Relaxed) {
        // LENGTH, never the dialogue. This line used to carry 34 characters of what the viewer
        // was watching — the most sensitive thing the event log has ever held, in a file that gets
        // photographed into public issue threads. `len=` answers every question the text answered
        // for triage (did a cue arrive, at what time, was it empty, is the track the right one)
        // without being viewing content.
        log(&format!(
            "sub cue [{}..{}ms] len={}",
            start_ns / 1_000_000,
            end_ns / 1_000_000,
            text.chars().count()
        ));
    }
    push_subtitle_text(track, start_ns, end_ns, text);
}

/// [`active_subtitle`] with the cue's `(start_ns, end_ns)` in the subtitle clock (the clock
/// [`subtitle_clock_ns`] converts the playhead to).
pub fn active_subtitle_span(now_ns: i64) -> Option<(String, i64, i64)> {
    let sel = SHARED.desired_sub_idx.load(Relaxed);
    if sel < 0 {
        return None;
    }
    let cues = SHARED.sub_cues.lock().unwrap();
    let lookup_ns = subtitle_clock_ns(now_ns);
    cues.iter()
        .rev()
        .find(|c| c.track == sel && lookup_ns >= c.start_ns && lookup_ns < c.end_ns)
        .map(|c| (c.text.clone(), c.start_ns, c.end_ns))
}

/// **The IDENTITY of the text cue active at `now_ns`** — its start, or 0 for none.
///
/// [`active_subtitle`] clones the cue's `String`, which is right for the draw and wrong for the
/// per-frame motion report that has to run whether or not the frame draws (spec §8.3): a subtitle
/// appearing or vanishing is a whole-screen content change with no spring behind it, and before
/// phase 9 nothing reported it because the player route presented unconditionally. `start_ns` is a
/// sufficient identity — two cues of one track cannot share a start.
pub fn subtitle_cue_id(now_ns: i64) -> i64 {
    let sel = SHARED.desired_sub_idx.load(Relaxed);
    if sel < 0 {
        return 0;
    }
    let cues = SHARED.sub_cues.lock().unwrap();
    let lookup_ns = subtitle_clock_ns(now_ns);
    cues.iter()
        .rev()
        .find(|c| c.track == sel && lookup_ns >= c.start_ns && lookup_ns < c.end_ns)
        .map_or(0, |c| c.start_ns)
}

/// **The image-subtitle store's byte ceiling**, which must hold the window a delayed caption
/// needs: from the history floor (`subtitle_floor_ns`: 60 s of the latest delay,
/// `SUBTITLE_OFFSET_LATEST_MS`, + 2 s behind the playhead, retained at EVERY offset so a raised
/// delay finds its sets) to the demuxer's read position.
///
/// - Read-ahead: the demuxer is bounded by the 10 MiB video queue (`engine::AQ_VIDEO_BYTES`) —
///   about 2 s of a 40 Mbit/s remux, 10.5 s of an 8 Mbit/s 1080p encode. Take 12 s: the window
///   is at most 60 + 12 + 2 = 74 s.
/// - Sets in it: a dense dialogue scene publishes about one display set with pixels per 2 s
///   (the CLEAR between two is `close_subtitle_bitmap`, which stores nothing), so 37 sets.
/// - One set: a PGS object is its text's bounding box. Two lines of large text are ~1400x150 px
///   on a 1920x1080 canvas and ~2800x300 px on a 3840x2160 one.
/// - Held as RGBA (4 B/px, how this store kept them first): 37 x 0.84 MB = 31.1 MB at 1080p,
///   and 37 x 3.36 MB = 124.3 MB for a 4K canvas — well over this budget.
/// - Held indexed ([`SubRect`], 1 B/px + a 1 KiB palette): 37 x 0.21 MB = 7.8 MB at 1080p and
///   37 x 0.84 MB = 31.1 MB for a 4K canvas, inside 40 MiB with room for other tracks' sets.
///
/// 40 MiB is 25% of the 160 MB `requiredMemory` the app declares — raised from the 24 MiB this
/// store held at the old 30 s ceiling, which no longer covers a 4K PGS track's indexed worst case
/// above now that the sync-offset ceiling itself is 60 s (`SUBTITLE_OFFSET_LATEST_MS`).
///
/// The floor keeps the window at offset 0 too, where the 60 s of history is only insurance for a
/// delay the viewer has not asked for yet. Under pressure the eviction order drops a set the
/// subtitle clock has passed FIRST ([`push_subtitle_bitmap`]), so at offset 0 that history is
/// best-effort and never costs a set still to be shown; at +60 s nothing has passed the clock, and
/// the arithmetic above is what holds the window.
pub const SUB_BITMAP_BUDGET: usize = 40 * 1024 * 1024;

/// Image-subtitle store (PGS/VobSub). The demux (D) thread decodes EVERY image track while
/// subtitles are on (so a switch between image tracks is instant — see ff.rs) and pushes each
/// display set here; the renderer (M) reads the selected track's active one for the playpos. A
/// new display-set supersedes any still-open cue on the same track (PGS signals the end via a
/// later CLEAR or a superseding set, both handled here). Bounded by time like the text store, and
/// by [`SUB_BITMAP_BUDGET`].
///
/// `cw`/`ch` are the stream's authoring canvas (0 = the decoder never declared one) and every
/// rect's coords are relative to it — the renderer scales the whole set into the video rect, so
/// a 720×480 VobSub and a 1920×1080 PGS land the same size on screen.
pub fn push_subtitle_bitmap(
    track: i32,
    start_ns: i64,
    cw: i32,
    ch: i32,
    rects: Vec<SubRect>,
) {
    if rects.is_empty() {
        return;
    }
    let mut v = SHARED.sub_bitmaps.lock().unwrap();
    for c in v.iter_mut() {
        if c.track == track && c.end_ns == i64::MAX {
            c.end_ns = start_ns; // this set replaces the one still showing
        }
    }
    let floor = subtitle_floor_ns();
    v.retain(|c| c.end_ns >= floor);
    v.push(SubBitmap {
        track,
        start_ns,
        end_ns: i64::MAX,
        cw,
        ch,
        rects,
    });
    // Hard RAM ceiling, by total bytes rather than cue count: several image tracks are buffered
    // at once, and a multi-rect display set counts as the sum of its rects.
    //
    // `v` is in demux (increasing-pts) order and the time-retain above has already dropped
    // everything older than the latest delay's window (`subtitle_floor_ns`). What goes, in order:
    //   1. a set that ENDED before the earlier of the playhead and the subtitle clock, oldest
    //      first — on any track; only a delay raised later could show it again;
    //   2. another track's set, farthest ahead first (a switch to it would at least still find
    //      the cue at the subtitle clock);
    //   3. only then the selected track's farthest set.
    // The selected track's sets ahead of the subtitle clock go last because the demuxer never
    // republishes a set it has read: with a +30 s delay the "far end of the read-ahead" is up to
    // 30 s of captions the viewer has not seen yet, and evicting it (what this did) blanked them.
    // `SUB_BITMAP_BUDGET` is sized so step 3 does not happen over the supported window.
    let mut total: usize = v.iter().map(|c| c.bytes()).sum();
    let now = SHARED.playpos_ns.load(Relaxed);
    let passed = now.min(subtitle_clock_ns(now));
    let sel = SHARED.desired_sub_idx.load(Relaxed);
    while total > SUB_BITMAP_BUDGET && v.len() > 1 {
        let i = v
            .iter()
            .position(|c| c.end_ns <= passed)
            .or_else(|| v.iter().rposition(|c| c.track != sel))
            .unwrap_or(v.len() - 1);
        total -= v[i].bytes();
        v.remove(i);
    }
}
/// A CLEAR display-set (num_rects==0): close the currently-open cue on this track at `end_ns`.
pub fn close_subtitle_bitmap(track: i32, end_ns: i64) {
    let mut v = SHARED.sub_bitmaps.lock().unwrap();
    for c in v.iter_mut() {
        if c.track == track && c.end_ns == i64::MAX {
            c.end_ns = end_ns;
        }
    }
}
/// Cheap per-frame lookup: the `start_ns` key of the selected track's image cue active at
/// `now_ns`, or None. The renderer only re-uploads its GL texture when this key changes.
pub fn active_bitmap_key(now_ns: i64) -> Option<i64> {
    let sel = SHARED.desired_sub_idx.load(Relaxed);
    if sel < 0 {
        return None;
    }
    let v = SHARED.sub_bitmaps.lock().unwrap();
    let lookup_ns = subtitle_clock_ns(now_ns);
    v.iter()
        .rev()
        .find(|c| c.track == sel && lookup_ns >= c.start_ns && lookup_ns < c.end_ns)
        .map(|c| c.start_ns)
}
/// Fetch (canvas_w, canvas_h, rects) for the selected track's display set with this `start_ns`
/// key. Clones the bitmaps once (only when the renderer sees a new key), so the per-frame path
/// stays cheap.
pub fn bitmap_by_key(key: i64) -> Option<(i32, i32, Vec<SubRect>)> {
    let sel = SHARED.desired_sub_idx.load(Relaxed);
    let v = SHARED.sub_bitmaps.lock().unwrap();
    v.iter()
        .rev()
        .find(|c| c.track == sel && c.start_ns == key)
        .map(|c| (c.cw, c.ch, c.rects.clone()))
}
/// Extract plain caption text, stripping markup and normalizing line breaks.
/// ASS/SSA never enters this path: the native renderer receives its complete script/events.
fn sub_text(payload: &[u8]) -> String {
    let s = String::from_utf8_lossy(payload);
    let mut out = String::with_capacity(s.len());
    let mut ch = s.chars().peekable();
    while let Some(c) = ch.next() {
        match c {
            '<' => {
                while let Some(x) = ch.next() {
                    if x == '>' {
                        break;
                    }
                }
            } // <i></i>
            '{' => {
                while let Some(x) = ch.next() {
                    if x == '}' {
                        break;
                    }
                }
            } // {\an8}
            '\\' => match ch.peek() {
                Some('N') | Some('n') => {
                    ch.next();
                    out.push('\n');
                }
                Some('h') => {
                    ch.next();
                    out.push(' ');
                }
                _ => out.push('\\'),
            },
            '\r' => {}
            _ => out.push(c),
        }
    }
    out.trim().to_string()
}

pub use plx_base::eventlog::log; // event-log sink (crate-wide single copy in lib.rs)

fn find(h: &[u8], n: &[u8]) -> bool {
    !n.is_empty() && h.windows(n.len()).any(|w| w == n)
}
/// bytes between `prefix` and the next `term`, or None if `prefix` absent.
fn between(h: &[u8], prefix: &[u8], term: u8) -> Option<Vec<u8>> {
    let start = h.windows(prefix.len()).position(|w| w == prefix)? + prefix.len();
    let rest = &h[start..];
    let end = rest.iter().position(|&b| b == term).unwrap_or(rest.len());
    Some(rest[..end].to_vec())
}

/// Parse one JSON number into thousandths without allocating a JSON tree on the pipeline callback
/// thread. SourceInfo is firmware-owned and may contain integer (`24`) or fractional (`23.976`)
/// frame rates; malformed, zero and non-finite values remain "not reported".
fn source_fps_milli(h: &[u8]) -> Option<i64> {
    let prefix = b"\"frameRate\":";
    let start = h.windows(prefix.len()).position(|w| w == prefix)? + prefix.len();
    let rest = &h[start..];
    let first = rest.iter().position(|b| !b.is_ascii_whitespace())?;
    let number = &rest[first..];
    let end = number
        .iter()
        .position(|b| !b.is_ascii_digit() && !matches!(*b, b'.' | b'-' | b'+'))
        .unwrap_or(number.len());
    let value = std::str::from_utf8(&number[..end])
        .ok()?
        .parse::<f64>()
        .ok()?;
    let milli = value * 1_000.0;
    (value.is_finite() && value > 0.0 && milli <= i64::MAX as f64).then(|| milli.round() as i64)
}

/// Monotonic milliseconds, from an origin fixed at the first call.
///
/// Not SDL ticks: this is read on the pipeline's own callback thread, and the value is only ever
/// differenced, so a private origin is enough and owes SDL nothing.
///
/// **Private to `player::` again.** It was widened to `pub` earlier in phase 9 so
/// `route::decision`'s `auto_last_switch` aging could stamp from it; that stamp now comes from the
/// Player machine's frame tick (`machine::Player::set_now`, spec §4.1), which is the loop's own
/// `fr.now` and therefore cannot disagree with the frame it belongs to. The remaining readers are
/// the pump's silence watchdog and the host clock sink, both inside this module.
fn vclock_ms() -> u32 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_millis() as u32
}

/// **The pipeline's `FRAMEREADY` cadence since the last call**: ticks received, and the worst gap
/// between two consecutive ones in milliseconds. Draining, like
/// [`plx_machine::idle::take_presents`] — the heartbeat is the one caller, once a second.
///
/// **This is a liveness signal, not a frame rate.** See [`sf_on_event`]: the healthy reading on
/// every codec, resolution and container measured so far is `5` and `201`, because the tick is
/// ~5 Hz regardless of the stream's frame rate. What it is good for is the opposite question —
/// whether the pipeline still thinks it is running. A steady `5 / 201` through a picture the
/// viewer says is stuttering is a real and useful finding: it rules the fault OUT of everything
/// this process can see, and sends the search to the display side.
/// **Start timing a channel change**: called as the old stream has been stopped, before the new
/// one is asked for. The first picture tick after it ends the measurement ([`tune_take`]).
pub fn mark_tune() {
    SHARED.tune_mark.store(vclock_ms().wrapping_add(1).max(1), Relaxed);
    SHARED.tune_ms.store(0, Relaxed);
}

/// **How long the last channel change took to put up a picture**, in milliseconds, once: from
/// [`mark_tune`] to the first `FRAMEREADY` after it. That tick is ~5 Hz (see [`sf_on_event`]), so
/// the figure is good to about 200 ms. Draining; the heartbeat is the caller.
pub fn tune_take() -> Option<u32> {
    match SHARED.tune_ms.swap(0, Relaxed) {
        0 => None,
        ms => Some(ms - 1),
    }
}

pub fn vplane_take() -> (u32, u32) {
    (
        SHARED.dg_vpres_ct.swap(0, Relaxed),
        SHARED.dg_vpres_gap.swap(0, Relaxed),
    )
}

/// pipeline event on the LIBRARY thread. type 0 = `PF_EVENT_TYPE_FRAMEREADY` (num = fed pts).
///
/// **`FRAMEREADY` is NOT one callback per decoded frame on this firmware, and reading it that way
/// is how a stutter investigation gets the wrong answer.** Kodi's Starfish path treats it as one
/// picture per event, which is where the old "frame presented" gloss here came from. Measured on
/// webOS 4.10.2 (2026-08-21), it is a **~5 Hz position tick**: a 1080p H264 direct play, a 4K HEVC
/// direct play and a visibly stuttering Dolby Vision direct play all deliver it 5 times a second,
/// 201 ms apart, to the millisecond. So [`frames`](crate::player::shared::Shared::frames) counts
/// TICKS, not frames — which is what `pump`'s `frames >= 2` gate really means (≈400 ms of
/// playback, not two pictures) and what `app::diagnostics` really shows.
///
/// The consequence for diagnosis: this callback can say the pipeline still believes it is
/// presenting, and cannot say the picture is smooth. The video plane's real cadence is not
/// observable from this process at all — the evidence for that lives in the TV's own kernel log
/// (`kad-hdr`).
/// Panic-guarded (unwinding into C is UB); touches only SHARED.
#[no_mangle]
pub extern "C" fn sf_on_event(epoch: c_uint, ty: c_int, num: i64, s: *const c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // The mutex is deliberately held through the complete callback. A bare equality check
        // would let teardown retire A, reset the process-long SHARED storage for B, and then let
        // a callback which had already validated A publish into B. Firmware supplies `epoch`
        // through the device-proven callback-context overload of StarfishMediaAPIs::Load.
        let class = match ty {
            0 => shared::NativeEventClass::Presentation,
            23 => shared::NativeEventClass::UnloadCompleted,
            _ => shared::NativeEventClass::Other,
        };
        SHARED.with_native_session(epoch, class, num, || sf_on_event_inner(ty, num, s));
    }));
}
/// The two libpf frame counters a callback type can name, once the webOS 4 vs 5+ numbering
/// shift is taken out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SinkCounter {
    Dropped,
    Displayed,
}

/// PURE: which sink counter a raw callback type is, on a firmware of this major. Decompiled from
/// libpf on webOS 4.10 (`CustomPipeline::updateDroppedFrame` → 0x2e = 46,
/// `updateDisplayedFrame` → 0x2f = 47); every type above 0x1c is two higher on webOS 5+
/// (Kodi's `if (webOSVersion < 5 && type > ENDOFSTREAM) type += 2`), so 48/49 there. An unknown
/// major (0, os_info unreadable) is read as the numbering this project has actually measured.
fn sink_counter_kind(ty: c_int, major: u32) -> Option<SinkCounter> {
    let normalised = if major == 0 || major < 5 { ty + 2 } else { ty };
    match normalised {
        48 => Some(SinkCounter::Dropped),
        49 => Some(SinkCounter::Displayed),
        _ => None,
    }
}

fn sf_on_event_inner(ty: c_int, num: i64, s: *const c_char) {
    if ty != 0 {
        // The diagnostics census, beside the log line that already records every event. A COUNT is
        // what the read-out needs: "Load completed and then nothing ever called us" is the sharpest
        // symptom the stuck-buffering reports could carry, and it is invisible in a log the user
        // cannot reach. Kept here rather than in the `ty` dispatch below so an event we do not
        // handle still counts — an unhandled callback is still the pipeline talking.
        let n = SHARED.dg_cb_count.fetch_add(1, Relaxed) + 1;
        // Latch the FIRST error, with the callback index it arrived at. Sticky, because a later
        // healthy callback must not erase the one event that explains the session — and the index
        // separates "refused immediately" from "died after a long healthy run". Only `ty == 18`:
        // it is the one value this project has ever acted on, and it sits below the 0x1c point
        // where the numbering shifts between webOS 4 and 5+, so it means the same on both. Naming
        // any higher type would be a confident lie on the firmware we cannot test.
        //
        // The firmware's own error NUMBER rides in the same latch (`601` = resource allocation),
        // clamped to the `i32` range: it is the one integer a handled `tv_pipeline` report carries
        // about the refusal. The callback's STRING (`s`, logged below) is never kept: it is free
        // text and can hold a URI or a path.
        if ty == 18 && SHARED.dg_cb_err.load(Relaxed) == 0 {
            SHARED.dg_cb_err.store(ty, Relaxed);
            SHARED.dg_cb_err_at.store(n, Relaxed);
            SHARED
                .dg_cb_err_num
                .store(num.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32, Relaxed);
        }
        let preview = if s.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(s) }
                .to_string_lossy()
                .chars()
                .take(1400)
                .collect()
        };
        log(&format!("smp_cb type={ty} num={num} str={preview}"));
        // The video sink's own frame counters, forwarded by libpf every 200 ms (see the payload
        // note in engine.rs: `streamQualityInfo` / `streamQualityInfoNonFlushable`). Logged under
        // a firmware-independent name because these two sit ABOVE the 0x1c point where the
        // callback numbering shifts by two between webOS 4 and 5+ (`docs/webos5-port.md` §5):
        // 46/47 on this set are 48/49 on a webOS 5+ set. The harness reads THIS line, never
        // the raw type.
        match sink_counter_kind(ty, plx_platform::tv::device::info().major) {
            Some(SinkCounter::Displayed) => log(&format!("sink: displayed={num} (type={ty})")),
            Some(SinkCounter::Dropped) => log(&format!("sink: dropped={num} (type={ty})")),
            None => {}
        }
        // **A refusal before the first picture is a VERDICT, not a statistic.** Until 2026-09-03
        // the latch above was all this arm did, and `docs/webos10-lab-report.md` §3.5 records the
        // result on a set that refused the Load asynchronously (`Load()` returned ok=1, then
        // `type=18 num=601 Resource Allocation Error`): `load_failed` stayed false, the pump waited
        // in Connecting for a `loadCompleted` that could never come, the demuxer kept downloading,
        // the adaptive controller stepped down against an estimator that would never see a frame,
        // and the failure read-out — the one screen built to survive a phone photograph — never
        // ran, for ~70 s, on the failure it most exists for. Publishing the same flag the
        // synchronous refusal publishes (`threads::load_thread` on `sf_load == 0`) hands it to the
        // pump's existing `load_failed` arm: HLS rollback if one is pending, else `Error` with
        // `FailureKind::TvPipeline`.
        //
        // Scoped to `!seen_frame` — SESSION-scoped, never `frames == 0`, which a seek zeroes (see
        // player/CLAUDE.md, "`frames` is SEEK-scoped"). A `loadCompleted` already received is NOT
        // an exemption: a completed Load is the pipeline accepting a declaration, and a refusal
        // that lands after it but before any picture is still a session that never started. A
        // type=18 after a picture stays diagnostic-only here: whether that shape exists, and what
        // it means mid-play, is unmeasured, and the lab could not say whether 18 is the only
        // refusal type either — so the `num` is logged rather than filtered on.
        if ty == 18 && !SHARED.seen_frame.load(Relaxed) {
            log(&format!(
                "smp: Load refused by the pipeline before any picture (type=18 num={num}) — failing the source"
            ));
            SHARED.load_failed.store(true, std::sync::atomic::Ordering::Release);
        }
    }
    if ty == 0 {
        // a POSITION UPDATE — map fed pts -> real content position.
        //
        // **It is not one per presented frame, and `vtick`/`vgap` cannot see a dropped frame.**
        // Measured 2026-08-21 across every Profile 5 run: `vtick=5 vgap=201ms`, unvarying, on
        // clean and visibly stuttering playback alike. The pipeline emits this at 5 Hz — it is a
        // position report, not a vsync. This comment used to say "a frame was PRESENTED" and the
        // `dg_vpres_*` block below still describes a cadence probe; both were written from the
        // callback's NAME rather than from its rate, and an instrument that reads a flat 201 ms
        // through the fault it exists to catch is worse than no instrument, because it is quoted.
        //
        // The real per-frame cadence is only observable from LG's own tracing — `GST_DEBUG=
        // dualsequencer:6` via `/tmp/dispatcharrtv-gstlog`, whose `push_dual` and `lxvideosink`
        // timestamps give one line per frame. That was long avoided as perturbing; it is not, at
        // level 6: the same scene measured 123 LUT misses uninstrumented and 122 with the trace
        // running. Level 9 IS perturbing and is what that reputation came from.
        //
        // `pres_fed` below is still sound — the feed-ahead throttle wants a position, and a 200 ms
        // granularity against a 1.6 s budget is ample. Only the TIMING half was wrong.
        let t = vclock_ms();
        let prev = SHARED.dg_vpres_at.swap(t, Relaxed);
        SHARED.dg_vpres_ct.fetch_add(1, Relaxed);
        let mark = SHARED.tune_mark.swap(0, Relaxed);
        if mark != 0 {
            SHARED.tune_ms.store(t.wrapping_sub(mark - 1).saturating_add(1), Relaxed);
        }
        if prev != 0 {
            let gap = t.saturating_sub(prev);
            SHARED.dg_vpres_gap.fetch_max(gap, Relaxed);
        }
        SHARED.frames.fetch_add(1, Relaxed);
        SHARED.seen_frame.store(true, Relaxed); // session-scoped: unlike `frames`, a seek won't clear it
        // the rebuild an audio pick asked for has put its picture up: the switch is over
        let _ = SHARED.audio_switch.compare_exchange(
            shared::AudioSwitch::Reloading as u8,
            shared::AudioSwitch::None as u8,
            Relaxed,
            Relaxed,
        );
        SHARED.pres_fed.store(num, Relaxed); // raw fed pts, for the feed-ahead throttle
        SHARED.playpos_ns.store(
            num - SHARED.pts_shift.load(Relaxed) + SHARED.disp_base.load(Relaxed),
            Relaxed,
        );
    }
    if s.is_null() {
        return;
    }
    let b = unsafe { CStr::from_ptr(s) }.to_bytes();

    if let Some(fps_milli) = source_fps_milli(b) {
        SHARED.video_fps_milli.store(fps_milli, Relaxed);
    }
    if find(b, b"\"video\"") {
        if let Some(aspect) = video_geometry::source_aspect(b) {
            SHARED.video_aspect.store(aspect.pack(), std::sync::atomic::Ordering::Release);
        }
    }

    {
        let mut mid = SHARED.media_id.lock().unwrap();
        if mid.is_none() {
            if let Some(id) =
                between(b, b"\"context\":\"", b'"').or_else(|| between(b, b"\"mediaId\":\"", b'"'))
            {
                if let Ok(c) = std::ffi::CString::new(id.clone()) {
                    log(&format!(
                        "SMP context/mediaId={}",
                        String::from_utf8_lossy(&id)
                    ));
                    *mid = Some(c);
                }
            }
        }
    }

    if !SHARED.load_completed.load(Relaxed) && (find(b, b"loadCompleted") || find(b, b"\"loaded\""))
    {
        SHARED.load_completed.store(true, Relaxed);
        log("SMP loadCompleted");
    }

    {
        // capture the WHOLE sourceInfo envelope VERBATIM (byte-for-byte + NUL), never re-encoded
        let mut si = SHARED.source_info.lock().unwrap();
        if si.is_none() && find(b, b"\"video\":") && find(b, b"\"context\":") {
            let mut v = Vec::with_capacity(b.len() + 1);
            v.extend_from_slice(b);
            v.push(0);
            log(&format!("SMP sourceInfoRaw captured ({} bytes)", b.len()));
            *si = Some(v);
        }
    }
}

#[no_mangle]
pub extern "C" fn acb_on_event(ev: c_long, reply: *const c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let r = if reply.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(reply) }
                .to_string_lossy()
                .into_owned()
        };
        log(&format!("acb_cb ev={ev} reply={r}"));
    }));
}

/// A channel that stopped mid-stream, for tests outside this module that need a read-out.
#[cfg(any(test, feature = "test-support"))]
pub fn interrupted_shape_for_test() -> ErrorShape {
    error_shape(false, RuntimeFailure::PlaybackInterrupted)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The jail/rtkmem refusal (no Engine ever built, `state()` deriving `Error`, and the
    // refusal retiring on route exit) is exercised by
    // `native_failure_regressions::jail_refusal_enters_error_without_engine_and_retires_on_exit`
    // below, against the real `ps.jail_load_blocked`-based gate in `engine.rs` — see that test.

    /// **Each cause converts to its own telemetry class, under the code it always had.** The class
    /// enum is telemetry's and the match that fills it is this layer's, so a copy-paste that sent
    /// two causes to one class would merge two dashboard series without any compile error. (The
    /// retired `original_rollback` code has no cause here; its class is exercised by telemetry.)
    #[test]
    fn every_failure_kind_converts_to_its_own_stable_wire_class() {
        use FailureKind as K;
        let table = [
            (K::NoVideoTrack, "no_video_track"),
            (K::MediaSource, "media_source"),
            (K::PlaybackInterrupted, "playback_interrupted"),
            (K::TvPipeline, "tv_pipeline"),
            (K::JailMissingRtkmem, "jail_missing_rtkmem"),
            (K::LoadTimeout, "load_timeout"),
            (K::Unspecified, "unspecified"),
        ];
        for (kind, code) in table {
            assert_eq!(kind.code(), code, "{kind:?}");
            assert_eq!(kind.class().code(), code, "{kind:?}");
        }
        let mut classes: Vec<_> = table.iter().map(|(kind, _)| kind.class().code()).collect();
        classes.sort_unstable();
        classes.dedup();
        assert_eq!(classes.len(), table.len(), "two causes share one wire class");
    }

    #[test]
    fn acb_pause_resume_cannot_overtake_the_bind_transaction() {
        assert!(!acb_playstate_ready(shared::Stage::Playing));
        assert!(!acb_playstate_ready(shared::Stage::Bound));
        assert!(acb_playstate_ready(shared::Stage::Streaming));
    }

    #[test]
    fn playable_buffer_uses_one_movie_timeline_for_every_transport() {
        assert_eq!(
            playable_buffer_ms(70_000_000_000, 69_500_000_000, true, 0, 60_000_000_000,),
            Some(9_500),
            "a direct file publishes absolute movie timestamps",
        );
        assert_eq!(
            playable_buffer_ms(
                4_000_000_000,
                3_500_000_000,
                true,
                120_000_000_000,
                122_000_000_000,
            ),
            Some(1_500),
            "an HLS or offset-transcode tail is translated by its display base",
        );
        assert_eq!(
            playable_buffer_ms(4_000_000_000, -1, true, 0, 1_000_000_000),
            None,
            "an A/V stream cannot claim reserve before its audio lane arrives",
        );
        assert_eq!(
            playable_buffer_ms(4_000_000_000, -1, false, 0, 1_000_000_000),
            Some(3_000),
            "a declared video-only stream uses its video tail",
        );
    }

    #[test]
    fn source_info_reports_the_stream_fps_not_the_position_tick_rate() {
        assert_eq!(
            source_fps_milli(br#"{"video":{"frameRate":24,"width":3840}}"#),
            Some(24_000),
        );
        assert_eq!(
            source_fps_milli(br#"{"video":{"frameRate": 23.976,"width":1920}}"#),
            Some(23_976),
        );
        assert_eq!(source_fps_milli(br#"{"video":{"frameRate":0}}"#), None);
        assert_eq!(source_fps_milli(br#"{"video":{"width":1920}}"#), None);
    }

    #[test]
    fn callback_after_native_session_retirement_cannot_mutate_the_idle_session() {
        let _guard = plx_base::testlock::serial();
        SHARED.reset_session();

        sf_on_event(1, 0, 7_000_000_000, std::ptr::null());
        let mutated = SHARED.seen_frame.load(std::sync::atomic::Ordering::Acquire)
            || SHARED.frames.load(std::sync::atomic::Ordering::Acquire) != 0
            || SHARED.pres_fed.load(std::sync::atomic::Ordering::Acquire) != 0
            || SHARED.playpos_ns.load(std::sync::atomic::Ordering::Acquire) != 0;

        SHARED.reset_session();
        assert!(
            !mutated,
            "a callback with no live native-session owner must be discarded"
        );
    }

    /// The four shapes a `type=18` can arrive in, graded on the ONE bit that decides them:
    /// `seen_frame`. Before any picture it is a refusal whether or not `loadCompleted` came first;
    /// after a picture it is diagnostic only — including after a seek, which zeroes `frames` but
    /// never `seen_frame` (the trap player/CLAUDE.md names).
    #[test]
    fn a_type_18_before_any_picture_publishes_load_failed_and_after_one_does_not() {
        let _guard = plx_base::testlock::serial();
        let refusal = c"Resource Allocation Error".as_ptr();
        let refuse = |epoch: u32| sf_on_event(epoch, 18, 601, refusal);
        let failed = || SHARED.load_failed.load(std::sync::atomic::Ordering::Acquire);

        // 1. before loadCompleted
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("session");
        refuse(epoch);
        assert!(failed(), "a refusal before loadCompleted is a verdict");
        SHARED.retire_native_session(epoch);

        // 2. after loadCompleted, still no picture
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("session");
        sf_on_event(epoch, 2, 0, c"{\"loadCompleted\":true}".as_ptr());
        assert!(SHARED.load_completed.load(std::sync::atomic::Ordering::Acquire));
        refuse(epoch);
        assert!(
            failed(),
            "a completed Load is a declaration accepted, not a picture; the refusal still counts"
        );
        SHARED.retire_native_session(epoch);

        // 3. after a picture: diagnostic only
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("session");
        sf_on_event(epoch, 0, 7_000_000_000, std::ptr::null());
        assert!(SHARED.seen_frame.load(std::sync::atomic::Ordering::Acquire));
        refuse(epoch);
        assert!(!failed(), "a type=18 on an established session is not a Load refusal");
        assert_eq!(
            SHARED.dg_cb_err.load(std::sync::atomic::Ordering::Acquire),
            18,
            "…but the diagnostics latch still records it"
        );

        // 4. after a seek on that session: `frames` is zeroed, `seen_frame` is not
        SHARED.frames.store(0, std::sync::atomic::Ordering::Relaxed);
        refuse(epoch);
        assert!(
            !failed(),
            "a seek zeroes `frames`; the refusal predicate must read `seen_frame`, not that"
        );
        SHARED.retire_native_session(epoch);
        SHARED.reset_session();
    }

    /// **The firmware's error number is latched beside the type, once.** `type=18 num=601
    /// Resource Allocation Error` is the shape the lab recorded; the `601` is what a handled
    /// `tv_pipeline` report carries, so it must survive a later healthy callback and a later,
    /// different error, and must go with the rest of the session at `reset_session`.
    #[test]
    fn the_first_type_18_latches_its_number_and_nothing_later_erases_or_replaces_it() {
        let _guard = plx_base::testlock::serial();
        let num = || SHARED.dg_cb_err_num.load(std::sync::atomic::Ordering::Acquire);
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("session");
        assert_eq!(num(), 0, "nothing recorded yet");
        sf_on_event(epoch, 2, 0, c"{\"loadCompleted\":true}".as_ptr());
        assert_eq!(num(), 0, "a healthy callback is not an error");
        sf_on_event(epoch, 18, 601, c"Resource Allocation Error".as_ptr());
        assert_eq!(num(), 601);
        sf_on_event(epoch, 2, 0, c"{}".as_ptr());
        sf_on_event(epoch, 0, 7_000_000_000, std::ptr::null());
        assert_eq!(num(), 601, "a later healthy callback does not erase the latch");
        sf_on_event(epoch, 18, 99, std::ptr::null());
        assert_eq!(num(), 601, "the FIRST error is the one that explains the session");
        assert_eq!(SHARED.dg_cb_err.load(std::sync::atomic::Ordering::Acquire), 18);
        SHARED.retire_native_session(epoch);
        SHARED.reset_session();
        assert_eq!(num(), 0, "cleared where `dg_cb_err` is cleared");
    }

    /// The number is bounded to the `i32` range on the way in: the callback hands over an `i64`,
    /// and the report carries only a value a fixed-width integer field can hold.
    #[test]
    fn a_type_18_number_outside_the_i32_range_is_clamped_not_wrapped() {
        let _guard = plx_base::testlock::serial();
        let num = || SHARED.dg_cb_err_num.load(std::sync::atomic::Ordering::Acquire);
        for (given, want) in [
            (i64::MAX, i32::MAX),
            (i64::from(i32::MAX) + 1, i32::MAX),
            (i64::MIN, i32::MIN),
            (-5, -5),
        ] {
            SHARED.reset_session();
            let epoch = SHARED.begin_native_session().expect("session");
            sf_on_event(epoch, 18, given, std::ptr::null());
            assert_eq!(num(), want, "num {given}");
            SHARED.retire_native_session(epoch);
        }
        SHARED.reset_session();
    }

    /// A SYNCHRONOUS Load refusal (`sf_load` returning 0) has no firmware number: Starfish's `Load`
    /// answers with a bare success flag, and the other refusals inside `sf_load` are the seam's own
    /// lifecycle gates. So it publishes `load_failed` and leaves the latch at 0 — pinned so a
    /// number is never invented for it.
    #[test]
    fn a_synchronous_load_refusal_publishes_no_firmware_number() {
        let _guard = plx_base::testlock::serial();
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("session");
        assert!(SHARED.publish_native_load_failure(epoch));
        assert!(SHARED.load_failed.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(SHARED.dg_cb_err_num.load(std::sync::atomic::Ordering::Acquire), 0);
        SHARED.retire_native_session(epoch);
        SHARED.reset_session();
    }

    #[test]
    fn the_support_line_names_version_firmware_set_and_code_and_nothing_free_text() {
        let i = plx_platform::tv::device::Info {
            release: "4.10.2".into(),
            major: 4,
            ..Default::default()
        };
        let hw = plx_platform::tv::device::Hardware {
            model: "43LM6300PVB".into(),
            board: "m3r".into(),
            hw_revision: String::new(),
        };
        let line = support_line_of(&i, &hw, FailureKind::TvPipeline);
        assert_eq!(
            line,
            format!(
                "Dispatcharr TV {} · webOS 4.10.2 · 43LM6300PVB · m3r · tv_pipeline",
                plx_base::identity::version()
            )
        );
        let bare = support_line_of(
            &plx_platform::tv::device::Info::default(),
            &plx_platform::tv::device::Hardware::default(),
            FailureKind::Unspecified,
        );
        assert!(bare.contains(&format!(
            "{} · {} · unspecified",
            plx_platform::i18n::msg::browse_diagnostics_unknown_os(),
            plx_platform::i18n::msg::settings_login_unknown_device(),
        )), "{bare}");
    }

    #[test]
    fn the_sink_counters_are_read_through_the_numbering_shift() {
        use SinkCounter::{Displayed, Dropped};
        assert_eq!(sink_counter_kind(46, 4), Some(Dropped));
        assert_eq!(sink_counter_kind(47, 4), Some(Displayed));
        assert_eq!(sink_counter_kind(47, 0), Some(Displayed), "unknown major reads as measured");
        assert_eq!(sink_counter_kind(48, 5), Some(Dropped));
        assert_eq!(sink_counter_kind(49, 10), Some(Displayed));
        assert_eq!(sink_counter_kind(47, 10), None, "47 is something else on webOS 5+");
        assert_eq!(sink_counter_kind(49, 4), None);
        assert_eq!(sink_counter_kind(18, 4), None);
    }

    #[test]
    fn late_native_callback_cannot_cross_into_the_next_session() {
        let _guard = plx_base::testlock::serial();
        SHARED.reset_session();
        let retired = SHARED.begin_native_session().expect("session A");
        assert!(SHARED.retire_native_session(retired));
        let current = SHARED.begin_native_session().expect("session B");

        sf_on_event(retired, 0, 7_000_000_000, std::ptr::null());
        let stale_mutated = SHARED.seen_frame.load(std::sync::atomic::Ordering::Acquire)
            || SHARED.frames.load(std::sync::atomic::Ordering::Acquire) != 0
            || SHARED.pres_fed.load(std::sync::atomic::Ordering::Acquire) != 0
            || SHARED.playpos_ns.load(std::sync::atomic::Ordering::Acquire) != 0;
        sf_on_event(current, 0, 8_000_000_000, std::ptr::null());
        let current_landed = SHARED.seen_frame.load(std::sync::atomic::Ordering::Acquire)
            && SHARED.pres_fed.load(std::sync::atomic::Ordering::Acquire) == 8_000_000_000;

        SHARED.retire_native_session(current);
        SHARED.reset_session();
        assert!(!stale_mutated, "session A must not mutate session B");
        assert!(current_landed, "session B's own callback must still land");
    }

    #[test]
    fn native_epoch_retirement_drains_a_callback_already_inside_the_reducer() {
        let _guard = plx_base::testlock::serial();
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("native session");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let callback = std::thread::spawn(move || {
            SHARED.with_native_session(epoch, shared::NativeEventClass::Other, 0, || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("callback entered the native-session reducer");

        let (retired_tx, retired_rx) = std::sync::mpsc::channel();
        let retire = std::thread::spawn(move || {
            retired_tx
                .send(SHARED.retire_native_session(epoch))
                .unwrap();
        });
        assert!(
            retired_rx
                .recv_timeout(std::time::Duration::from_millis(20))
                .is_err(),
            "retirement crossed a callback which had already been admitted",
        );
        release_tx.send(()).unwrap();
        assert!(retired_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("retirement completes after the callback leaves"),);
        callback.join().unwrap();
        retire.join().unwrap();
        SHARED.reset_session();
    }

    #[test]
    fn unload_completed_is_an_explicit_terminal_native_session_transition() {
        let _guard = plx_base::testlock::serial();
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("native session");

        sf_on_event(epoch, 23, 0, std::ptr::null());
        assert!(SHARED.native_unload_completed(epoch));
        sf_on_event(epoch, 0, 8_000_000_000, std::ptr::null());
        assert_eq!(
            SHARED.frames.load(std::sync::atomic::Ordering::Acquire),
            0,
            "an event after unload-completed entered the terminal Rust epoch",
        );
        assert!(SHARED.retire_native_session(epoch));
        let next_epoch = SHARED
            .begin_native_session()
            .expect("the next Load can mint an epoch after native gate+Rust retirement");
        assert_ne!(next_epoch, epoch);
        assert!(
            SHARED.begin_native_session().is_none(),
            "overlapping native Load was admitted",
        );
        assert!(SHARED.retire_native_session(next_epoch));
        SHARED.reset_session();
    }

    #[test]
    fn pre_seek_presentation_cannot_certify_the_post_seek_timeline() {
        let _guard = plx_base::testlock::serial();
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("native session");
        assert!(SHARED.begin_native_media_discontinuity(epoch));

        sf_on_event(epoch, 0, 7_000_000_000, std::ptr::null());
        assert_eq!(SHARED.frames.load(std::sync::atomic::Ordering::Acquire), 0);
        assert_eq!(
            SHARED.playpos_ns.load(std::sync::atomic::Ordering::Acquire),
            0
        );
        assert!(
            !SHARED.seen_frame.load(std::sync::atomic::Ordering::Acquire),
            "an old type-0 callback cannot prove the new seek presented"
        );

        assert!(SHARED.arm_native_presentations(epoch));
        sf_on_event(epoch, 0, 8_000_000_000, std::ptr::null());
        assert_eq!(SHARED.frames.load(std::sync::atomic::Ordering::Acquire), 1);
        assert_eq!(
            SHARED.pres_fed.load(std::sync::atomic::Ordering::Acquire),
            8_000_000_000,
        );

        SHARED.retire_native_session(epoch);
        SHARED.reset_session();
    }

    #[test]
    fn post_seek_feed_commits_or_discards_callbacks_that_race_its_reply() {
        let _guard = plx_base::testlock::serial();
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("native session");
        assert!(SHARED.begin_native_media_discontinuity(epoch));

        // A BufferFull/error Feed may race a position callback, but the AU was not accepted. The
        // callback is latched during the call and discarded with its failed transaction.
        assert!(SHARED.begin_native_presentation_probe(epoch));
        sf_on_event(epoch, 0, 7_000_000_000, std::ptr::null());
        assert_eq!(SHARED.frames.load(std::sync::atomic::Ordering::Acquire), 0);
        assert!(SHARED.reject_native_presentation_probe(epoch));
        sf_on_event(epoch, 0, 7_500_000_000, std::ptr::null());
        assert_eq!(SHARED.frames.load(std::sync::atomic::Ordering::Acquire), 0);

        // On the retained AU's accepted retry, a callback which arrives before Feed returns is
        // replayed exactly once at commit and later callbacks flow normally.
        assert!(SHARED.begin_native_presentation_probe(epoch));
        sf_on_event(epoch, 0, 8_000_000_000, std::ptr::null());
        assert_eq!(SHARED.frames.load(std::sync::atomic::Ordering::Acquire), 0);
        assert!(SHARED.commit_native_presentation_probe(epoch, |num| {
            sf_on_event_inner(0, num, std::ptr::null())
        }));
        assert_eq!(SHARED.frames.load(std::sync::atomic::Ordering::Acquire), 1);
        assert_eq!(
            SHARED.pres_fed.load(std::sync::atomic::Ordering::Acquire),
            8_000_000_000
        );
        sf_on_event(epoch, 0, 8_200_000_000, std::ptr::null());
        assert_eq!(SHARED.frames.load(std::sync::atomic::Ordering::Acquire), 2);

        assert!(SHARED.retire_native_session(epoch));
        SHARED.reset_session();
    }

    /// User Play releases the transport pause, but an internal runway hold owns the media clock.
    /// Calling the ordinary seam here would bypass the only place that checks fresh A/V media and
    /// recreate the short burst/freeze cycle after a manual Pause/Play during rebuffering.
    #[cfg(feature = "hostsim")]
    #[test]
    fn user_resume_cannot_bypass_an_internal_hls_rebuffer_hold() {
        let _guard = plx_base::testlock::serial();
        let old_paused = TX.paused.load(std::sync::atomic::Ordering::Acquire);
        SHARED.reset_hls_clock_for_test();
        let pause = SHARED
            .prepare_hls_rebuffer_pause()
            .expect("reserve internal Pause");
        assert_eq!(
            SHARED.complete_hls_rebuffer_pause(pause, true),
            HlsPauseCompletion::Accepted
        );
        assert_eq!(
            SHARED.prepare_hls_user_pause(),
            Some(HlsUserPause::AlreadyHeld)
        );
        TX.commit_paused(true);
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                SHARED.reset_hls_clock_for_test();
                TX.commit_paused(self.0);
            }
        }
        let _restore = Restore(old_paused);
        let before = ffi_host::play_calls_for_test();
        let mut pa = adapter::PlayerAdapter::new(unsafe { plx_base::task::MainThread::assume() });

        assert!(resume(&mut pa));

        assert_eq!(
            ffi_host::play_calls_for_test(),
            before,
            "ordinary Resume called Starfish while the measured-runway gate still owned the clock",
        );
        assert!(SHARED
            .hls_rebuffering
            .load(std::sync::atomic::Ordering::Acquire));
        assert!(!TX.paused.load(std::sync::atomic::Ordering::Acquire));
    }

    /// A terminal runtime failure already knows which subsystem stopped: the media source,
    /// the live transfer, or the television pipeline. The player read-out reserves a reason
    /// slot for that answer, so falling through to an empty string turns a diagnosed failure
    /// back into the unhelpful bare "Playback failed" screen.
    #[test]
    fn runtime_failures_fill_the_existing_readout_reason_slot() {
        let cases = [
            (
                (true, false, false, false),
                RuntimeFailure::MediaSource,
                FailureKind::MediaSource,
                "media_source",
                "media stream",
            ),
            (
                (false, true, false, false),
                RuntimeFailure::PlaybackInterrupted,
                FailureKind::PlaybackInterrupted,
                "playback_interrupted",
                "stopped after it had started",
            ),
            (
                (false, false, true, false),
                RuntimeFailure::TvPipeline,
                FailureKind::TvPipeline,
                "tv_pipeline",
                "TV",
            ),
            (
                // The budget always sets `load_failed` alongside `load_timed_out` — see
                // `Shared::load_timed_out`'s doc — but the KIND must still differ from the
                // ordinary refusal case above, which is what this row pins.
                (false, false, true, true),
                RuntimeFailure::LoadTimeout,
                FailureKind::LoadTimeout,
                "load_timeout",
                "TV",
            ),
            (
                (false, false, false, false),
                RuntimeFailure::Unknown,
                FailureKind::Unspecified,
                "unspecified",
                "identify the problem",
            ),
        ];
        for ((demux, io, load, timed_out), want, kind, code, words) in cases {
            let cause = runtime_failure(demux, io, load, timed_out);
            assert_eq!(
                cause, want,
                "the flags must resolve to the subsystem that stopped"
            );
            let e = error_shape(false, cause);
            assert_eq!(e.kind, kind);
            assert_eq!(e.kind.code(), code, "the Sentry/usage wire code is stable");
            assert!(
                !e.readout.is_empty(),
                "a terminal runtime failure must explain what stopped"
            );
            assert!(
                e.readout.contains(words),
                "{} did not name {words:?}",
                e.readout
            );
            assert!(
                !e.panel.is_empty(),
                "diagnostics and the viewer read-out share the answer"
            );
        }
        assert_eq!(
            runtime_failure(true, true, true, false),
            RuntimeFailure::TvPipeline,
            "the most specific downstream signal must win if teardown exposes all three",
        );
        assert_eq!(
            runtime_failure(true, true, true, true),
            RuntimeFailure::LoadTimeout,
            "a timed-out Load must win over every other concurrently-set signal",
        );
    }

    /// A firmware REFUSAL (`TvPipeline`) and a HANG (`LoadTimeout`, issue #74 D.1.4's
    /// `NATIVE_LOAD_BUDGET`) are different events on the telemetry wire — `runtime_failures_fill_
    /// the_existing_readout_reason_slot` above already pins the distinct `kind`/`code` — but until
    /// now every viewer-facing string (`caption`, `panel`, `readout`) was byte-identical between
    /// them, so a maintainer reading a photographed read-out or the diagnostics panel could not
    /// tell "the TV said no" from "the TV never answered" apart. Pin that they now differ, and that
    /// the timeout's own wording says something a hang actually describes (never finished /
    /// answered), not the refusal's "rejected".
    #[test]
    fn load_timeout_has_its_own_wording_distinct_from_an_ordinary_tv_refusal() {
        let refused = error_shape(false, RuntimeFailure::TvPipeline);
        let timed_out = error_shape(false, RuntimeFailure::LoadTimeout);
        assert_ne!(
            refused.caption, timed_out.caption,
            "a refusal and a hang must not share a caption"
        );
        assert_ne!(
            refused.panel, timed_out.panel,
            "a refusal and a hang must not share a diagnostics panel line"
        );
        assert_ne!(
            refused.readout, timed_out.readout,
            "a refusal and a hang must not share a read-out reason"
        );
        let timed_out_caption = timed_out.caption.to_str().unwrap();
        assert!(
            timed_out_caption.contains("did not finish") || timed_out_caption.contains("never finished"),
            "{timed_out_caption:?} should say the Load never finished, not that the TV rejected anything"
        );
        assert!(
            !timed_out_caption.to_lowercase().contains("rejected"),
            "{timed_out_caption:?} borrows the refusal's wording; a hang was never rejected"
        );
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> SubRect {
        SubRect {
            x,
            y,
            w,
            h,
            index: vec![0u8; (w * h) as usize],
            palette: Box::new([[0u8; 4]; 256]),
        }
    }

    /// A rect whose share of the store's byte budget is about `bytes`, whatever a rect's pixels
    /// cost in the store's representation (measured, not assumed).
    fn rect_of(bytes: usize) -> SubRect {
        let set = |h| SubBitmap { track: 0, start_ns: 0, end_ns: 0, cw: 0, ch: 0, rects: vec![rect(0, 0, 1024, h)] };
        let per_row = set(2).bytes() - set(1).bytes();
        rect(0, 0, 1024, (bytes / per_row) as i32)
    }

    /// **Held bytes are what is really allocated.** A rect whose index buffer has spare capacity
    /// (a `Vec` that grew by doubling) holds that capacity, so the budget counts it.
    #[test]
    fn a_rects_bytes_are_its_allocation_not_its_length() {
        let tight = rect(0, 0, 100, 10);
        let mut loose = rect(0, 0, 100, 10);
        loose.index.reserve_exact(1 << 20);
        assert_eq!(tight.bytes(), 1000 + 1024);
        assert!(loose.bytes() >= tight.bytes() + (1 << 20), "spare capacity is held, so it is counted");
    }

    /// The renderer uploads what [`SubRect::to_rgba`] expands, so the expansion is the pixels:
    /// each index becomes its palette entry's straight-alpha RGBA, in row order.
    #[test]
    fn an_indexed_rect_expands_through_its_palette() {
        let mut palette = Box::new([[0u8; 4]; 256]);
        palette[1] = [10, 20, 30, 255];
        palette[255] = [1, 2, 3, 128];
        let r = SubRect { x: 0, y: 0, w: 3, h: 1, index: vec![0, 1, 255], palette };
        assert_eq!(r.to_rgba(), [0, 0, 0, 0, 10, 20, 30, 255, 1, 2, 3, 128]);
        assert_eq!(r.bytes(), 3 + 1024, "the store counts the indices and the palette");
    }

    /// The image-subtitle store, exercised as a display SET rather than a single bitmap. Three
    /// invariants moved when multi-rect landed and none of them is observable on the host except
    /// here: every rect of a set survives the round trip under ONE key (so a two-line PGS cue is
    /// not silently halved); a later set still closes the one still showing; and the RAM ceiling
    /// counts a set's rects together, so a multi-rect cue cannot smuggle bytes past the budget.
    ///
    /// Takes the crate-wide `testlock` — `SHARED` is a process-global the whole player shares.
    #[test]
    fn an_image_display_set_round_trips_whole_and_is_superseded_as_a_unit() {
        let _g = plx_base::testlock::serial();
        SHARED.sub_bitmaps.lock().unwrap().clear();
        SHARED.playpos_ns.store(0, Relaxed);
        SHARED.desired_sub_idx.store(0, Relaxed);

        // a two-rect set (dialogue plus a sign), authored on a DVD canvas
        push_subtitle_bitmap(
            0,
            1_000,
            720,
            480,
            vec![rect(60, 400, 600, 60), rect(100, 20, 200, 40)],
        );
        let key = active_bitmap_key(1_500).expect("the set should be active at its start");
        let (cw, ch, rects) = bitmap_by_key(key).expect("the active key must resolve");
        assert_eq!(
            (cw, ch),
            (720, 480),
            "the authoring canvas travels with the set"
        );
        assert_eq!(
            rects.len(),
            2,
            "BOTH rects must survive — rect 0 only was the bug"
        );
        assert_eq!((rects[1].x, rects[1].y), (100, 20));

        // the next set closes the open one AT ITS OWN START — a display set stays up until the
        // one that replaces it begins, so the handover is seamless and never double-shows
        push_subtitle_bitmap(0, 5_000, 720, 480, vec![rect(60, 400, 600, 60)]);
        assert_eq!(
            active_bitmap_key(4_999),
            Some(1_000),
            "the first set holds right up to the handover"
        );
        assert_eq!(
            active_bitmap_key(5_000),
            Some(5_000),
            "and the second takes over on that exact ns"
        );

        // an empty set is not a cue: it must not land and must not close what is showing
        push_subtitle_bitmap(0, 6_000, 720, 480, Vec::new());
        assert_eq!(active_bitmap_key(5_500), Some(5_000));

        // The byte budget charges a set for ALL its rects (two of a sixth of the budget each
        // here), and — the part that only matters once a set can be big — it must not evict the
        // cue the viewer is READING. The playhead sits inside the 5_000 cue, so that one has to
        // survive four sets of a third of the budget arriving from the demuxer's read-ahead; what
        // goes is the far end of that read-ahead.
        SHARED.playpos_ns.store(5_500, Relaxed);
        for i in 0..4 {
            push_subtitle_bitmap(
                0,
                10_000 + i,
                720,
                480,
                vec![rect_of(SUB_BITMAP_BUDGET / 6), rect_of(SUB_BITMAP_BUDGET / 6)],
            );
        }
        let v = SHARED.sub_bitmaps.lock().unwrap();
        let total: usize = v.iter().map(|c| c.bytes()).sum();
        assert!(
            total <= SUB_BITMAP_BUDGET,
            "the store stayed inside its ceiling ({total} bytes)"
        );
        assert!(
            v.iter().any(|c| c.start_ns == 5_000),
            "the cue under the playhead was not evicted"
        );
        drop(v);
        assert_eq!(
            active_bitmap_key(5_500),
            Some(5_000),
            "and it is still the one on screen"
        );

        // leave the globals as they were found — `desired_sub_idx` deliberately survives a reset
        // (shared.rs), so a test that leaves it selected changes what the NEXT one sees
        SHARED.sub_bitmaps.lock().unwrap().clear();
        SHARED.desired_sub_idx.store(-1, Relaxed);
    }
}

#[cfg(all(test, feature = "hostsim"))]
mod native_failure_regressions {
    use super::*;
    struct JailGuard;
    impl Drop for JailGuard {
        fn drop(&mut self) { plx_platform::tv::sandbox::FORCE_BLOCKED.store(false, Relaxed); }
    }
    #[test]
    fn jail_refusal_enters_error_without_engine_and_retires_on_exit() {
        let _serial = plx_base::testlock::serial();
        let _guard = JailGuard;
        let mut ps = crate::route::PlaybackSession::IDLE;
        crate::route::reset_player_control_for_test(&ps);
        SHARED.reset_session();
        let mut pa = adapter::PlayerAdapter::new(unsafe { plx_base::task::MainThread::assume() });
        plx_platform::tv::sandbox::FORCE_BLOCKED.store(true, Relaxed);
        assert!(start_bufferfeed(&mut ps, &mut pa));
        assert!(!pa.is_live());
        assert_eq!(state(&ps), PlaybackState::Error);
        assert_eq!(error_now(&ps).kind, FailureKind::JailMissingRtkmem);
        crate::route::cancel_play(&mut ps);
        assert!(!ps.jail_load_blocked);
        assert_ne!(state(&ps), PlaybackState::Error);
        assert!(matches!(start_bufferfeed_tracked(&mut ps, &mut pa), BufferfeedStartOutcome::Failed));
        assert!(ps.jail_load_blocked);
        assert!(!pa.is_live());
        crate::route::cancel_play(&mut ps);
    }
    #[test]
    fn timeout_is_distinct_from_pipeline_refusal_and_resets_per_session() {
        let _serial = plx_base::testlock::serial();
        assert_eq!(runtime_failure(true, true, true, true), RuntimeFailure::LoadTimeout);
        assert_eq!(runtime_failure(false, false, true, false), RuntimeFailure::TvPipeline);
        SHARED.load_timed_out.store(true, Relaxed);
        SHARED.reset_session();
        assert!(!SHARED.load_timed_out.load(Relaxed));
    }
}

/// The two halves of "the playbar jumps on a seek", reported on an LG C3 (webOS 23), where
/// in-place seeking is disabled and every seek is a reload.
#[cfg(test)]
mod seek_hud_regressions {
    use super::*;

    /// **A reload keeps the file's duration; a real stop does not.** `teardown` zeroed it on
    /// every reload, so for the few hundred ms until the demuxer reopened the file the HUD drew
    /// the playhead at position ÷ 0 — the far left — while the clock beside it, which is the
    /// position alone, read correctly. Differential: before the fix the reload path called
    /// `reset_session` and the first assertion reads 0.
    #[test]
    fn a_reload_keeps_the_files_duration_and_a_stop_does_not() {
        let _serial = plx_base::testlock::serial();
        SHARED.reset_session();
        SHARED.duration_ns.store(5_400_000_000_000, Relaxed);
        SHARED.playpos_ns.store(1_200_000_000_000, Relaxed);
        SHARED.reset_session_for_reload();
        assert_eq!(duration_ns(), 5_400_000_000_000, "the same file is about to be reopened");
        assert_eq!(playpos_ns(), 0, "…and everything that IS the session's was still cleared");
        SHARED.reset_session();
        assert_eq!(duration_ns(), 0, "a real stop: the next item is a new file");
    }

}

/// **Which rebuild is "Switching audio…"?** Only the one an audio-track pick asked for: it begins
/// at the pick, survives the reload, and ends at that session's first presented frame; a pick the
/// server never rebuilt for expires; anything else (a seek's reload, no pick at all) is Buffering.
#[cfg(test)]
mod audio_switch_tests {
    use super::*;

    #[test]
    fn the_caption_is_chosen_by_the_pick_and_only_for_buffering() {
        use PlaybackState as S;
        assert_eq!(busy_caption_with(S::Buffering, true), plx_platform::i18n::msg::widgets_status_switching_audio_c());
        assert_eq!(busy_caption_with(S::Buffering, false), S::Buffering.caption());
        // a switch never renames the other waits
        assert_eq!(busy_caption_with(S::Seeking, true), S::Seeking.caption());
        assert_eq!(busy_caption_with(S::Connecting, true), S::Connecting.caption());
        assert_eq!(busy_caption_with(S::Resolving, true), S::Resolving.caption());
    }

}
