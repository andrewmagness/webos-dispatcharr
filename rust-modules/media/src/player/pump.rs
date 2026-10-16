//! player::pump — the main-thread pump (was bufferfeed_pump). Runs each frame from
//! plex_run: the pending-seek handler, the ACB-bind state machine (Stage), and the
//! feed dispatch. All ACB/Starfish control calls happen here on the main thread.
use super::engine::{
    arm_live_clock_prime, drain_aq, feed_both_lanes, feed_sample, Engine, Source,
};
use super::shared::{HlsPauseCompletion, HlsPrimeKind, HlsSeekPause, Stage};
use super::{ACB_OK, SHARED, TX};
use plx_base::task::MainThread;
use std::os::raw::c_char;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::time::Duration;

/// issue #74 D.1.4: the longest the `loadCompleted` arm will wait for `sf_load` to actually
/// return before giving up and publishing `load_failed`. Kodi's own precedent
/// (`MediaPipelineWebOS.cpp:795-801`) is a 1s `wait_for` then `PLAYER_ABORT`; this budget is
/// deliberately far larger because a real `Load` on the dev set measures in the HUNDREDS OF
/// MILLISECONDS for a cold pipeline (investigation.md §A.0's own citation), and the point of this
/// timeout is to catch a genuine hang — the k5lp `DirectVoInit` block investigation.md §B
/// describes — not to interrupt an ordinary slow one. It exists so a Load that never returns
/// becomes a failure read-out instead of a permanent Connecting spinner.
pub const NATIVE_LOAD_BUDGET: Duration = Duration::from_secs(20);

/// The issued→return gate shared by the constructor wait and the loadCompleted arm.
/// Ready objects retain the existing demux-failure precedence before this gate is evaluated.
fn native_load_gate_ready(epoch: u32) -> bool {
    if SHARED.native_load_returned(epoch) { return true; }
    // Log the deferral exactly once per session (a fast Load may never hit this arm at all,
    // in which case nothing here fires and the ordinary loadCompleted path below runs
    // immediately once the gate is already open).
    if SHARED
        .native_load_deferred_logged_epoch
        .swap(epoch, Relaxed)
        != epoch
    {
        super::log("native: loadCompleted while Load in flight — deferring");
    }
    // D.1.4: bound the wait. A Load that never returns (the k5lp DirectVoInit hang,
    // investigation.md §B) must not leave the player in Connecting forever — past the
    // budget, publish the same signal an outright Load refusal sets, so the EXISTING
    // failure/rollback arm above (this function, near the top) raises the failure read-out
    // on the next tick instead of a permanent spinner.
    match SHARED.native_load_elapsed(epoch) {
        Some(elapsed) if elapsed >= NATIVE_LOAD_BUDGET => {
            super::log(&format!(
                "native: Load did not return within {}s — publishing load_failed",
                NATIVE_LOAD_BUDGET.as_secs()
            ));
            SHARED.load_failed.store(true, Release);
            SHARED.load_timed_out.store(true, Release);
            super::report::note_load_gate_for(
                crate::route::playback_trace_generation(),
                super::report::LoadElapsedClass::from_ms(elapsed.as_millis() as i64),
            );
        }
        Some(_) => {}
        // `native_load_elapsed` shares its precondition with `native_load_returned` — both
        // require the epoch to still own `NativeSessionPhase::Active`. A synchronous
        // UnloadCompleted callback (firmware type=23) for this epoch flips the phase to
        // `Unloaded` *without* this arm ever having seen `Returned`, which would otherwise
        // make the budget above unreachable at exactly the moment the gate can never open
        // again — the permanent Connecting spinner D.1.4 exists to rule out. Treat losing
        // `Active` while still deferred as a fired timeout rather than silently disabling it.
        None => {
            super::log(
                "native: Load epoch left Active while loadCompleted was still deferred — \
                 publishing load_failed",
            );
            SHARED.load_failed.store(true, Release);
            SHARED.load_timed_out.store(true, Release);
            // The epoch left Active before this arm ever saw an elapsed reading, so the exact
            // in-flight time is unknown — bucket it as the budget's own ceiling, which is the
            // honest floor for "still deferred when Active was lost".
            super::report::note_load_gate_for(
                crate::route::playback_trace_generation(),
                super::report::LoadElapsedClass::Over20s,
            );
        }
    }
    false
}

/// Publish the one value the HUD renders from. Pure derivation off signals the workers already
/// maintain — no new cross-thread plumbing, and it runs on every path out of `pump` (including
/// the early returns) so the state can never go stale behind a spinner that never resolves.
fn set_state(s: super::shared::PlaybackState) {
    SHARED.pb_state.store(s as u8, Relaxed);
}

/// How many seek requests this pump seek MERGED — i.e. requests that arrived while an earlier
/// seek was still resolving and so never got a seek of their own. Reset as it's read, so each
/// logged seek reports only the requests it consumed.
///
/// The count is what makes coalescing observable. `seek_to_ns` only ever holds the newest target,
/// so a burst of six taps and a single tap leave identical state behind; and the burst is applied
/// by whichever of two paths wins a race with the stuck-watchdog, which log different lines.
/// Counting either line therefore measures PMS latency, not merging — `coalesced=` measures
/// merging directly, and both paths report it.
fn take_coalesced() -> u32 {
    TX.seek_reqs.swap(0, Relaxed).saturating_sub(1)
}

fn prime_before_play(rebase_pending: bool, segmented_hls: bool) -> bool {
    rebase_pending || segmented_hls
}

/// Playable HLS reserve in the same content-time coordinates as the demux controller. The main
/// thread needs this read for the exact depleted-buffer fallback and for rebuffer diagnostics;
/// ordinary ABR decisions remain on the worker.
fn hls_buffered_ms() -> Option<i64> {
    let video = SHARED.hls_video_tail_ns.load(Acquire);
    if video < 0 {
        return None;
    }
    let audio = SHARED.hls_audio_tail_ns.load(Acquire);
    let tail = if audio >= 0 { video.min(audio) } else { video };
    let display_base = SHARED.disp_base.load(Relaxed).max(0);
    Some(
        tail.saturating_add(display_base)
            .saturating_sub(SHARED.playpos_ns.load(Relaxed).max(0))
            .max(0)
            / 1_000_000,
    )
}

/// The measured period of LG's native position callback, in milliseconds.
///
/// **Measured, not chosen.** The pipeline emits `PF_EVENT_TYPE_FRAMEREADY` at 5 Hz and the
/// device heartbeat reads `vtick=5 vgap=201ms` — unvarying across codec, raster and container,
/// on clean and on visibly stuttering playback alike (`player::sf_on_event`, measured
/// 2026-08-21). 201 is that measurement rather than the nominal 200.
const NATIVE_POSITION_PERIOD_MS: u32 = 201;

/// How many consecutive silent periods make the presentation clock physically STOPPED rather
/// than merely late.
///
/// Five, so the shortest silence this reports is 1005 ms. That is far outside anything
/// scheduling jitter can produce against a cadence measured to the millisecond, and it costs the
/// viewer about a second before a stalled stream can be acted on — against the 82 s a completing
/// fetch cost on the device (`pipe_abr_down_collapse`, 2026-09-02).
const NATIVE_CLOCK_SILENT_PERIODS: u32 = 5;

/// The silence that means the native clock has stopped: [`NATIVE_CLOCK_SILENT_PERIODS`] periods
/// of [`NATIVE_POSITION_PERIOD_MS`].
const NATIVE_CLOCK_STOPPED_MS: u32 = NATIVE_POSITION_PERIOD_MS * NATIVE_CLOCK_SILENT_PERIODS;

/// When the silence observation was last restarted, in [`super::vclock_ms`] milliseconds.
///
/// Every state that legitimately stops the native clock — user Pause, an internal rebuffer hold,
/// a stage below `Playing`, a route that is not segmented HLS — restarts it, because otherwise a
/// viewer who paused for a minute would resume straight into an internal hold. Main-thread only;
/// an atomic merely to avoid a `static mut`.
static CLOCK_WATCH_SINCE_MS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Everything that stops the clock on purpose restarts the silence observation with it.
fn restart_native_clock_watch() {
    CLOCK_WATCH_SINCE_MS.store(super::vclock_ms(), Relaxed);
}

/// Milliseconds since the native position callback last fired, or `None` if this seek/session
/// epoch has not seen one at all.
///
/// `dg_vpres_at` is zeroed wherever `frames` is — session reset and the post-seek re-count in
/// this file — so a seek starts with no observation rather than with a gap the size of the seek.
/// The [`CLOCK_WATCH_SINCE_MS`] floor does the same for the deliberate holds, which do not clear
/// the stamp.
fn native_clock_silence_ms() -> Option<u32> {
    let last_tick = SHARED.dg_vpres_at.load(Relaxed);
    if last_tick == 0 {
        return None;
    }
    let since = last_tick.max(CLOCK_WATCH_SINCE_MS.load(Relaxed));
    Some(super::vclock_ms().saturating_sub(since))
}

/// **The terminal reserve boundary as a PHYSICAL fact rather than an arithmetic one.**
///
/// `hls_buffered_ms` is content-time bookkeeping: the demuxed tail minus the playhead. It reaches
/// exactly zero only if the playhead reaches the tail, and on this pipeline it does not. The last
/// fed access units cannot be presented until the ones behind them arrive, so when acquisition
/// stops the position callback stops with the playhead parked a few hundred milliseconds short,
/// and that remnant then never moves again. Device-measured 2026-09-02
/// (`pipe_abr_down_collapse`): the callback went silent at `pos=29s` with the demuxed tail at
/// ~29.96 s and stayed silent for 77 s, so neither `buffered_ms == Some(0)` here nor the worker's
/// `spent >= reserve_ms_at_start` in [`crate::ff`]'s `StallGuard` could ever be reached, and a
/// 20000 kbps object ran to completion on a 500 kbps link before any rung could be decided.
///
/// The clock's own silence is the observation both of those were standing in for. If the clock
/// had been running through it, it would have presented `silence_ms` of media and said so five
/// times a second; a reserve no larger than the silence has therefore already been outlasted, and
/// `B = 0` is true within the only resolution the arithmetic can offer.
///
/// **The inequality is one-sided on purpose.** A large surviving reserve is not a starvation
/// boundary, so a stream holding 5.6 s whose callback is 1.2 s late keeps playing and only a
/// reserve the stopped clock has already outlasted counts. An unknowable reserve (`None`, the
/// audio lane silent after an open or a seek) holds nothing, exactly as it arms no `StallGuard`.
/// User Pause never reaches here — [`maybe_begin_hls_rebuffer`] returns before this on
/// `TX.paused` and restarts the observation — and an internal hold cannot re-arm, because the
/// hold sets `prime_play`, which returns at the same place.
fn native_clock_stopped(silence_ms: Option<u32>, buffered_ms: Option<i64>) -> bool {
    let (Some(silence_ms), Some(buffered_ms)) = (silence_ms, buffered_ms) else {
        return false;
    };
    silence_ms >= NATIVE_CLOCK_STOPPED_MS && buffered_ms <= i64::from(silence_ms)
}

fn should_begin_hls_rebuffer(
    requested: bool,
    _trial_reserve_ms: i64,
    _runtime_runway_ms: i64,
    buffered_ms: Option<i64>,
    native_clock_stopped: bool,
) -> bool {
    // `runtime_runway_ms` is a boundary requirement: it says how much reserve must be present
    // when a clock starts, not how much of the same acquisition remains at every later pump tick.
    // Comparing a decreasing mid-fetch B with the original full R makes a sustainable stream
    // pause shortly before virtually every segment lands. The reserve retained by a bounded
    // candidate is a boundary balance too: `exploration_budget_ms` gives the transaction only
    // B-max(R,D), and its deadline is the actuator that enforces that spend. Treating R as a live
    // main-thread stop arm duplicates the deadline and, on the device, latched seconds of stopped
    // playback after 136 ms of harmless measurement/clock skew (B=5167, R=5303).
    //
    // Only the in-flight response, which can see its current byte progress, may request an early
    // hold. Actual B=0 remains the coefficient-free terminal condition when no response byte has
    // arrived. Keep the boundary arguments in this pure function so the regression exercises the
    // exact values the pump samples and so the diagnostic line below can continue publishing them.
    // `native_clock_stopped` is the same coefficient-free `B = 0` observation, taken from the
    // pipeline's own cadence instead of from content-time arithmetic that cannot reach zero. See
    // its doc comment for why the arithmetic form is not merely imprecise here but unreachable.
    requested || buffered_ms == Some(0) || native_clock_stopped
}

fn rebuffer_request_is_current(
    requested: bool,
    requested_tail_ns: i64,
    current_tail_ns: i64,
) -> bool {
    requested && (requested_tail_ns < 0 || current_tail_ns <= requested_tail_ns)
}

/// Stop the INTERNAL A/V clock at a physical reserve boundary while leaving demux and feeding
/// alive. This is intentionally not `TX.paused`: the viewer did not pause, and the whole point is
/// to keep acquiring until `try_prime` sees enough measured runway to resume smoothly.
fn maybe_begin_hls_rebuffer(ps: &crate::route::PlaybackSession, mt: &MainThread, eng: &mut Engine) {
    if !crate::route::is_segmented_hls(ps)
        || eng.stage < Stage::Playing
        || eng.prime_play
        || TX.paused.load(Relaxed)
    {
        // Each of these legitimately stops the native clock, so none of them may be read later as
        // a stalled one: a viewer paused for a minute must resume into playback, not into an
        // instant internal hold, and the tick after this hold releases must start counting again.
        restart_native_clock_watch();
        return;
    }
    // Consume atomically: a worker request racing this tick must either be the value returned here
    // or remain set for the next tick, never be erased by a later unconditional store.
    let requested = SHARED.hls_rebuffer_requested.swap(false, Acquire);
    let requested_tail_ns = SHARED.hls_rebuffer_request_tail_ns.load(Acquire);
    let current_tail_ns = SHARED.hls_video_tail_ns.load(Acquire);
    let requested = rebuffer_request_is_current(requested, requested_tail_ns, current_tail_ns);
    let trial_reserve_ms = SHARED.hls_trial_reserve_ms.load(Acquire);
    let runtime_runway_ms = SHARED.hls_prime_runway_ms.load(Acquire);
    let buffered_ms = hls_buffered_ms();
    let silence_ms = native_clock_silence_ms();
    let clock_stopped = native_clock_stopped(silence_ms, buffered_ms);
    // A silent clock that does NOT become a hold is the case this gate was blind to for 82 s on
    // the device; say why, once per silent second, so a device log can answer it.
    if silence_ms.is_some_and(|ms| ms >= NATIVE_CLOCK_STOPPED_MS) && !clock_stopped {
        static LAST_SILENT_LINE_MS: std::sync::atomic::AtomicU32 =
            std::sync::atomic::AtomicU32::new(0);
        let now = super::vclock_ms();
        if now.wrapping_sub(LAST_SILENT_LINE_MS.load(Relaxed)) >= 1_000 {
            LAST_SILENT_LINE_MS.store(now, Relaxed);
            super::log(&format!(
                "hls: clock silent={}ms buf={}ms vtail={} atail={} disp_base={} pos={} \
                 requested={} stage_playing=1",
                silence_ms.unwrap_or(0),
                buffered_ms.map_or(-1, |ms| ms),
                SHARED.hls_video_tail_ns.load(Acquire),
                SHARED.hls_audio_tail_ns.load(Acquire),
                SHARED.disp_base.load(Relaxed),
                SHARED.playpos_ns.load(Relaxed),
                requested,
            ));
        }
    }
    if !should_begin_hls_rebuffer(
        requested,
        trial_reserve_ms,
        runtime_runway_ms,
        buffered_ms,
        clock_stopped,
    ) {
        return;
    }
    let Some(pause_token) = SHARED.prepare_hls_rebuffer_pause() else {
        if requested {
            SHARED.hls_rebuffer_requested.store(true, Release);
        }
        super::log("hls: auto-rebuffer pause deferred by clock transition; will retry");
        return;
    };
    let paused = unsafe { super::sink().pause(mt) };
    match SHARED.complete_hls_rebuffer_pause(pause_token, paused != 0) {
        HlsPauseCompletion::Accepted => {}
        HlsPauseCompletion::Refused => {
            if requested {
                SHARED.hls_rebuffer_requested.store(true, Release);
            }
            super::log("hls: auto-rebuffer pause refused by Starfish; will retry");
            return;
        }
        HlsPauseCompletion::Stale => {
            if requested {
                SHARED.hls_rebuffer_requested.store(true, Release);
            }
            super::log("hls: auto-rebuffer pause result was stale; will retry");
            return;
        }
    }
    arm_live_clock_prime(eng);
    // The pause-local proof was armed before sf_pause, so a completed segment in the native-call
    // window is already part of this hold instead of being silently discarded.
    // `silent=` is the only field that separates the two ways this hold is reached: an exact
    // arithmetic zero, or a presentation clock that stopped with a remnant it will never spend.
    super::log(&format!(
        "hls: auto-rebuffer pause buf={}ms trial_reserve={}ms runway={}ms silent={}ms",
        buffered_ms.unwrap_or(-1),
        trial_reserve_ms,
        runtime_runway_ms,
        silence_ms.map_or(-1, i64::from),
    ));
}

/// Place the webOS 5+ exported window: source frame -> on-screen rect.
///
/// No-op on the ACB path. `src` is the frame being FED and `dst` where it lands, and the pair is
/// what expresses scaling — so a 4K direct play must not be described as 1080p, which passing the
/// authoring canvas for both would do. The coded size comes from the demuxer, the only thing that
/// knows it for certain; before it has published, the canvas is the least-wrong fallback and the
/// caller re-places once the real one arrives.
fn place_exported(mt: &MainThread, eng: &mut super::engine::Engine) {
    if super::sink().window_mode() != super::VP_EXPORTED {
        return;
    }
    let (w, h) = SHARED.video_raster();
    let src = if w > 0 && h > 0 { (w, h) } else { (1920, 1080) };
    let rv = unsafe { super::sink().place_window(mt, src.0, src.1, 0, 0, 1920, 1080) };
    eng.placed_src = src;
    // RECORD it, do not merely log it. These three fields had no writer anywhere in the tree, so
    // `dg_place_rv` sat at its `i32::MIN` "never called" sentinel for the life of the process and
    // the diagnostics read-out rendered `Placed: not placed` in DANGER bold on every webOS 5+ set
    // — a fabricated fault, on the one firmware family nobody here can test, pointing every reader
    // at the video plane. The dev TV takes the ACB path, so no amount of looking at it would have
    // caught this.
    SHARED.dg_place_rv.store(rv, Relaxed);
    SHARED.dg_placed_w.store(src.0, Relaxed);
    SHARED.dg_placed_h.store(src.1, Relaxed);
    super::log(&format!(
        "vplane: exported window placed src={}x{} rv={rv}",
        src.0, src.1
    ));
}

/// Mirror Engine-confined observations into `Shared` for diagnostics and handled-error context.
///
/// The read-out cannot reach the `Engine` itself: the draw runs inside a frame where the pump
/// holds the adapter's `&mut`, and it holds no `&mut PlayerAdapter` of its own (before phase 9 the
/// slot was a `static mut` handing out `&'static mut`, so the same call was possible and was
/// instant UB). This is the only bridge, it is one-way, and nothing in the playback state machine
/// may read these fields back.
///
/// The stage is a cheap scalar and is always published so an opted-in terminal report does not
/// depend on whether Stats for Nerds happened to be open. `aq_bytes` takes each queue's pthread
/// mutex, which is why the rest is sampled HERE, once per visible diagnostics tick, and never from
/// a draw.
/// Last `frames` we saw, so a CHANGE can be stamped. A decrease counts: `frames` is seek-scoped
/// and the pump zeroes it applying a seek, which is motion, not a freeze.
static LAST_FRAMES: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

fn publish_diag(eng: &Engine, now: u32) {
    SHARED.dg_stage.store(eng.stage as u8, Relaxed);
    // Nobody is looking: skip it. `aq_bytes` takes each queue's pthread mutex, and the read-out
    // samples at 2 Hz, so publishing at 60 Hz is 30x more often than anything can observe. Costs
    // no freshness — the loop order is pump → `Diagnostics::update` → `Diagnostics::draw`
    // (`app/run.rs`), so the frame the panel
    // is switched on has already republished.
    if !super::DIAG_READOUT_ON.load(Relaxed) {
        return;
    }
    let qv = eng.aq_video.as_ref().map_or(0, |q| {
        crate::aq::aq_bytes(&**q as *const _ as *mut _) as i64
    });
    let qa = eng.aq_audio.as_ref().map_or(0, |q| {
        crate::aq::aq_bytes(&**q as *const _ as *mut _) as i64
    });
    SHARED.dg_aq_video.store(qv, Relaxed);
    SHARED.dg_aq_audio.store(qa, Relaxed);
    SHARED.dg_fed_v_pts.store(eng.max_fed_video_pts, Relaxed);
    SHARED.dg_fed_a_pts.store(eng.max_fed_audio_pts, Relaxed);
    // Stamp when the frame count MOVES. The panel needs "how long has it been stuck" and a
    // photograph has no time axis; stamping here rather than in `app::diagnostics` is what makes the
    // clock measure the STALL rather than how long the panel has been open.
    let f = SHARED.frames.load(Relaxed);
    if LAST_FRAMES.swap(f, Relaxed) != f {
        SHARED.dg_frame_at.store(now, Relaxed);
    }
}

/// The three source-failure branches' shared answer: either a recovery is flying (the frame is
/// done, the spinner stays up) or the Engine fails and the viewer gets the read-out. Always the
/// frame's last act.
/// A failed open or a stream that died: publish the failure, which the read-out and the live
/// reconnect both key on.
fn fail_engine() {
    crate::route::fail_current_engine();
    set_state(super::shared::PlaybackState::Error);
}

/// Settle a synchronous native reload effect.  A failed `start_bufferfeed` used to be only a log:
/// callers had already advanced the route reducer and then returned, leaving no Engine to publish
/// the failure.
pub(super) fn settle_reload(outcome:super::engine::ReloadOutcome, operation: &str) -> bool {
    if outcome == super::engine::ReloadOutcome::Started {
        return true;
    }
    super::log(&format!(
        "route transition: {operation} failed synchronously ({outcome:?})",
    ));
    // The reload's teardown parked a Load that had already passed its budget and cleared the
    // session flags with the rest of that session's state, then the start was refused for the
    // outstanding object. Without the cause the report says `unspecified`; the cause is the
    // timeout, so publish it again beside the Error.
    if outcome == super::engine::ReloadOutcome::StartFailed && SHARED.native_load_abandoned() {
        SHARED.load_failed.store(true, Release);
        SHARED.load_timed_out.store(true, Release);
    }
    set_state(super::shared::PlaybackState::Error);
    false
}

pub fn pump(ps: &mut crate::route::PlaybackSession, pa: &mut super::adapter::PlayerAdapter, now: u32) {
    use super::shared::PlaybackState;
    // ONE `&mut PlayerAdapter`, split into the live session and the seam token. Everything below
    // that replaces the session (`reload_*`) takes `pa` back, which the borrow checker only allows
    // where neither half is read afterwards — the "`eng` dangles after it, return immediately"
    // rule this function used to state in three comments and enforce by hand.
    let (eng, mt) = pa.split();
    let eng = match eng {
        Some(e) => e,
        None => {
            set_state(PlaybackState::Idle);
            return;
        }
    };
    // Republish the Engine-confined values the diagnostics read-out needs, BEFORE the early
    // returns below. Placed here, immediately after the borrow, for two reasons: it is the one
    // spot that cannot be forgotten as the pump grows, and it therefore keeps reporting through
    // the `sf_ready` and producer-died bail-outs — which is precisely when a stalled user is
    // looking at the panel. See `Shared`'s diagnostics-mirror block for the one-way rule.
    publish_diag(eng, now);
    // The media thread may have returned from sf_load immediately, but it is not allowed to make
    // the route Stable before this Engine exists in the main-thread slot. Drain its exact-token
    // result here, after installation and before any worker publication can be accepted.
    crate::route::drain_route_start_results(ps);
    // `sf_load == 0` may leave no callable object, so this must precede the sf_ready wait below;
    // otherwise the pump returns Connecting forever and never consumes the explicit failure.
    if SHARED.load_failed.load(Acquire) {
        fail_engine();
        return;
    }
    // wait for the media-thread ctor
    if unsafe { super::sink().ready(mt) } == 0 {
        if eng.stage == Stage::Loading {
            native_load_gate_ready(eng.native_epoch);
        }
        set_state(PlaybackState::Connecting);
        return;
    }
    if SHARED.frames.load(Relaxed) > 0 {
        crate::route::confirm_resume_presented(ps);
    }

    // The producer died before publishing a duration: the EOS path is gated on `duration_ns > 0`
    // so it can NEVER fire, and the player used to sit on a black screen forever with no error
    // and no exit. Surface it instead — BACK is already the escape.
    //
    // **Unless there is a way back**, which is the whole of the deferral above. Device,
    // 2026-08-29: the viewer asked for Original by hand on a film that was playing, the source URL
    // failed the way the same server's Original probe had failed forty seconds earlier, and this
    // line raised the failure read-out on a stream that had been working. A failed recovery is
    // worth a second reload; it is not worth the playback.
    // A worker may publish a more specific terminal code immediately before this generic Release
    // flag. Acquire is the matching hand-off; `error_now` can then report the transaction cause
    // instead of racing it into the generic producer bucket.
    if SHARED.demux_io_failed.load(Acquire) {
        // A live channel's transport failing mid-read is its stream ending: the server or its
        // upstream dropped. `ended` hands it to the app's reconnect, which tunes the channel
        // again and gives up after a few; the Error read-out would leave it dead until the viewer
        // pressed Try Again (the 2026-10-16 TV run: the stream went silent during the probe, the
        // next read failed, and nothing followed). The open's own failures raise `demux_failed`
        // too and keep the read-out, which says why the channel cannot be opened.
        if live_transport_ended(crate::route::is_live(ps), SHARED.demux_failed.load(Acquire)) {
            if !SHARED.ended.swap(true, Relaxed) {
                super::log("live: the transport failed → ended");
            }
            return;
        }
        fail_engine();
        return;
    }
    if SHARED.demux_failed.load(Acquire) && SHARED.frames.load(Relaxed) == 0 {
        fail_engine();
        return;
    }
    let stream = matches!(eng.source, Source::Stream);

    // ---------- an in-place seek is still resolving (flushed, not yet re-anchored): COALESCE.
    // Firing another now would race the demux reopen (rapid tap-seeks) and the rebase guard would
    // then eat everything → stuck. Hold the latest requested target (it stays in TX.seek_to_ns) and
    // process it once this seek anchors. If it hasn't anchored within SEEK_STUCK_MS the reopen was
    // lost — first RETRY the reopen cheaply (re-interrupt the demuxer + re-arm), and only fall back
    // to a full (slow) reload if the retries also fail.
    // SEEK_STUCK_MS must comfortably exceed a real reopen+av_seek+first-keyframe on 4K HEVC over
    // PMS (~700ms): the retry closes the demux socket, so a premature watchdog KILLS an open that
    // was about to succeed and restarts it — at 500ms a rapid tap-burst self-DoS'd into the
    // reload fallback every time (caught by the seek_rapid harness cases).
    const SEEK_STUCK_MS: u32 = 1200;
    if eng.flushed && eng.rebase_pending
        && now.wrapping_sub(eng.seek_armed_at) > SEEK_STUCK_MS
    {
        // Adopt the NEWEST coalesced target if later taps landed while this seek was resolving
        // (TX.seek_to_ns holds the latest request): retrying/reloading at the original armed
        // target would land where the user already tapped away from, then seek AGAIN.
        let pending = TX.seek_to_ns.swap(-1, Relaxed);
        let tgt = if pending >= 0 {
            pending
        } else {
            SHARED.seek_target_ns.load(Relaxed)
        }
        .max(0);
        if pending >= 0 {
            // The coalesced tap is a distinct desired timeline. It used to replace the demux
            // target below without crossing the reducer boundary, leaving `pending_seek_ns`
            // permanently Busy and old ABR evidence authorized against the new bytes.
            crate::route::commit_user_seek();
        }
        if eng.seek_retries < 2 {
            eng.seek_retries += 1;
            SHARED.seek_to_ns.store(tgt, Release); // re-publish; the demux thread seeks on it
            SHARED.seek_target_ns.store(tgt, Relaxed); // rebase guard keys on the retried target
            eng.seek_armed_at = now;
            super::log(&format!(
                "seek: in-place stuck → retry reopen at {}s (#{}) coalesced={}",
                tgt / 1_000_000_000,
                eng.seek_retries,
                take_coalesced()
            ));
        } else {
            super::log(&format!(
                "seek: in-place stuck → reload at {}s",
                tgt / 1_000_000_000
            ));
            settle_reload(
                super::engine::reload_at(ps, pa, tgt),
                "stuck in-place seek reload",
            ); // REPLACES the engine — eng dangles, return
            return;
        }
    }
    // ---------- pending seek: flush, drop queued AUs, re-point the demux ----------
    let t = TX.seek_to_ns.load(Relaxed);
    // Only a sized file (the dev URL playback) seeks, in place via av_seek; a live channel has no
    // Content-Length and nothing to seek to.
    if stream
        && t >= 0
        && !(eng.flushed && eng.rebase_pending) // coalesce: don't stack in-place seeks
        && eng.stage >= Stage::Playing
        && SHARED.duration_ns.load(Relaxed) > 0
        && SHARED.file_size.load(Relaxed) > 0
    {
        TX.seek_to_ns.store(-1, Relaxed);
        let coalesced = take_coalesced(); // read here so BOTH branches consume this seek's requests
        let t = t.max(0);
        // DIRECT-PLAY seek: Kodi IN-PLACE seek — flush + av_seek the demuxer, and
        // (in feed_stream, on the first post-flush keyframe) setTimeToDecode + sendSegmentEvent
        // to re-anchor the GStreamer segment WITHOUT a reload/decoder re-init (no A/V-resync
        // glitch). A plain flush()+refeed with NO fresh segment left a STALE GStreamer segment
        // → the HW sink stops draining ~48 s later → permanent BufferFull + "Playing error"
        // (root-caused by decompiling libpipeline/lxvideosink; see engine::reload_at). Falls
        // back to reload-per-seek if the pipeline isn't reachable (INPLACE_SEEK_OK cleared by
        // feed_stream when sendSegmentEvent can't find it). reload_at REPLACES the ENGINE, so
        // `eng` dangles after it: return immediately.
        if !super::INPLACE_SEEK_OK.load(Relaxed) {
            crate::route::commit_user_seek();
            settle_reload(super::engine::reload_at(ps, pa, t), "direct seek reload");
            return;
        }
        // Pause, user-held seek reuse and eventual prime all share one actuator state. In
        // particular a seek from Paused must not issue a redundant native Pause, and a result from
        // an older transition must not authorize flushing this session's queues.
        match SHARED.prepare_seek_pause() {
            Some(HlsSeekPause::AlreadyHeld) => {}
            Some(HlsSeekPause::Issue(token)) => {
                let accepted = unsafe { super::sink().pause(mt) } != 0;
                match SHARED.complete_seek_pause(token, accepted) {
                    HlsPauseCompletion::Accepted => {}
                    HlsPauseCompletion::Refused => {
                        super::log("seek(in-place): Starfish refused Pause → reload fallback");
                        crate::route::commit_user_seek();
                        settle_reload(
                            super::engine::reload_at(ps, pa, t),
                            "seek reload after native Pause refusal",
                        );
                        return;
                    }
                    HlsPauseCompletion::Stale => {
                        super::log("seek(in-place): stale Pause result → reload fallback");
                        crate::route::commit_user_seek();
                        settle_reload(
                            super::engine::reload_at(ps, pa, t),
                            "seek reload after stale Pause",
                        );
                        return;
                    }
                }
            }
            None => {
                // The main-thread transition in flight will settle before the next pump tick. Keep
                // the newest seek mailbox intact rather than consuming it into no operation.
                TX.seek_to_ns.store(t, Release);
                TX.seek_reqs.store(1, Release);
                super::log("seek(in-place): clock transition busy; retrying");
                return;
            }
        }
        if !SHARED.begin_native_media_discontinuity(eng.native_epoch) {
            crate::route::commit_user_seek();
            super::log("seek(in-place): native callback epoch retired → reload fallback");
            settle_reload(
                super::engine::reload_at(ps, pa, t),
                "seek reload after retired native epoch",
            );
            return;
        }
        // This is the applied media boundary: everything after it belongs to the new playhead.
        // Advancing earlier would strand the live worker when PMS/native refused; advancing later
        // would let pre-seek ABR evidence publish while queues are already being reset.
        crate::route::commit_user_seek();
        unsafe {
            super::sink().flush(mt); // drop decoded/queued frames only after the clock is really held
        }
        drain_aq(eng);
        // in-place direct-play seek: publish the target and let the DEMUX THREAD av_seek_frame
        // between two reads (ff.rs's inner loop). Nothing here interrupts the socket: the pump
        // used to shutdown(2) it to break the read so the outer loop would reopen+seek, but our
        // AVIO is seekable, so libavformat just healed the broken read through `seek_cb` and read
        // on — no reopen, no seek, and every direct-play seek escalated to a reload. See ff.rs.
        // eng.flushed → feed_stream fires setTimeToDecode+sendSegmentEvent on the first post-seek
        // keyframe. disp_base=0 (rebase to the landed keyframe).
        eng.flushed = true;
        eng.presentation_rearm_pending = false;
        SHARED.seek_to_ns.store(t, Release); // the demux thread's av_seek target
        SHARED.seek_target_ns.store(t, Relaxed); // rebase guard: reject stale drifted keyframes
        SHARED.disp_base.store(0, Relaxed);
        super::log(&format!(
            "seek(in-place): av_seek t={t} coalesced={coalesced}"
        ));
        // zero-base the fed timeline on the first post-seek keyframe (feed_stream), so it
        // presents against the flush-reset clock immediately — no catch-up freeze
        eng.rebase_pending = true;
        eng.rebase_drops = 0;
        eng.seek_armed_at = now; // stuck-watchdog start (see the coalesce/escape above)
        eng.seek_retries = 0;
        eng.max_fed_video_pts = 0;
        eng.max_fed_audio_pts = 0;
        eng.audio_start_fp = None;
        // Drop any AU held from BEFORE the seek (the per-lane BufferFull retry). drain_aq cleared the
        // QUEUES but not these pending AUs; feeding a stale pre-seek AU after the seek re-bases it to
        // the NEW pts_shift → a big A/V desync — the stale audio pts advances max_fed_audio_pts, then
        // the STALE_BACKJUMP guard drops the fresh post-seek audio as "too far back" → no sound. The
        // reopened stream supplies fresh post-seek AUs.
        eng.pending_video = None;
        eng.pending_audio = None;
        eng.prime_play = true; // paused above; Play once PRIME_NS is buffered (no fast-forward)
                               // "no post-seek frame presented yet" — the feed-ahead throttle feeds freely until the first
                               // real presented pts lands, instead of comparing the new fed pts against the STALE pre-seek
                               // presented position (which would wrongly break feeding on a forward in-place seek).
        SHARED.pres_fed.store(super::engine::PRES_NONE, Relaxed);
        eng.feed_pace = super::engine::FeedPace::default(); // the run-on belongs to the pre-seek report
        SHARED.frames.store(0, Relaxed); // count only POST-seek frames (rebind + resume re-pause gate)
                                         // …and forget the last presentation stamp with it. The plane goes dark across a seek by
                                         // design, so the first frame after one would otherwise post a gap the size of the seek and
                                         // read as a stutter (`Shared`'s `dg_vpres_*` block).
        SHARED.dg_vpres_at.store(0, Relaxed);
        SHARED.playpos_ns.store(t, Relaxed); // displayed position jumps; wall clock takes over
    }

    // ---------- load -> Play (decode fed frames as soon as loaded) ----------
    // issue #74 D.1: the gate below requires `SHARED.native_load_returned(eng.native_epoch)`
    // BEFORE this arm may even poll `sf_is_load_completed` — that poll is itself a concurrent
    // Starfish call, and on v0.6.0 it was the FIRST thing this arm did while `sf_load` was still
    // executing (synchronously) on the load thread. See `src/starfish.c`'s `g_load_returned` for
    // the matching seam-side half; `threads::load_thread` flips the Rust side right after the
    // real, blocking `sf_load` call returns. The helper is where the deferral logging and D.1.4
    // budget/telemetry live (see its own doc).
    if eng.stage == Stage::Loading
        && native_load_gate_ready(eng.native_epoch)
        && (SHARED.load_completed.load(Relaxed) || unsafe { super::sink().is_load_completed(mt) } != 0)
    {
        // "native: Load returned after Nms" is logged unconditionally at the gate transition
        // itself (`threads::load_thread`, right where `mark_native_load_returned` flips it), not
        // here — see `load-returned-log-is-conditional-on-loadcompleted-and-unbounded-after`. It
        // used to live in this arm, which meant it only ever appeared paired with the deferral
        // line above, and never at all on the path this fix is most likely to be read on: Load
        // eventually returns but `loadCompleted` goes quiet, so this arm never runs.
        SHARED.load_completed.store(true, Relaxed);
        // when it completed, so the panel can say how long we have been waiting for a frame since
        SHARED.dg_load_at.store(now, Relaxed);
        super::log("SMP loadCompleted");
        eng.stage = Stage::Playing;
        // webOS 5+: place the exported window HERE, the instant Load reports success — which is
        // exactly where ss4s does it (smp_player.c calls StarfishResourcePostLoad synchronously
        // on the line after StarfishMediaAPIs_load returns true).
        //
        // It used to wait for `frames >= 2`, copied from the ACB block below, and that is a
        // DEADLOCK on this path: the pipeline does not emit frames until its sink is bound, and
        // the sink is not placed until frames arrive. The symptom is a player stuck in buffering
        // forever, which is what webOS 6 and 10 reported. The gate is right for ACB and only for
        // ACB — `setMediaVideoData` there needs the sourceInfo envelope, which cannot exist until
        // the pipeline has produced something. The exported window has no such dependency: the
        // BINDING already happened via `option.windowId` in the Load payload, and this call is
        // pure geometry.
        place_exported(mt, eng);
        if super::sink().window_mode() == super::VP_EXPORTED {
            // Playing -> Streaming directly: there is no bind sequence to sit in the middle of.
            // The transition is load-bearing beyond bookkeeping — `pushEOS` is gated on
            // `>= Streaming`, so without it the last frames never drain and Up Next never fires.
            eng.stage = Stage::Streaming;
        }
        // A fresh Load for a seek/resume primes before Play so the clock does not free-run through
        // the demux reopen gap. Segmented HLS needs the same gate even at offset zero: its video
        // and AAC arrive through independent queues, and starting Starfish's audio-master clock
        // before AAC is present produced silent initial playback that recovered only after a seek
        // (the seek path already primed both lanes). Progressive initial play keeps its proven
        // immediate-start behavior.
        if prime_before_play(eng.rebase_pending, crate::route::is_segmented_hls(ps)) {
            eng.prime_play = true;
            super::log("SMP loadCompleted (priming before Play)");
        } else {
            let generation = SHARED.hls_candidate_generation.load(Acquire);
            let recovery = SHARED.hls_recovery();
            if let Some((token, _)) =
                SHARED.reserve_hls_prime_play(HlsPrimeKind::Fresh, generation, recovery)
            {
                let accepted = unsafe { super::sink().play(mt) } != 0;
                match SHARED.complete_hls_prime_play(token, accepted) {
                    super::shared::HlsPlayCompletion::Accepted { resume_acb } => {
                        if resume_acb {
                            super::acb_mirror_playstate_at(mt, eng.stage, true);
                        }
                        super::log("SMP Play");
                    }
                    super::shared::HlsPlayCompletion::Refused => {
                        eng.prime_play = true;
                        super::log("SMP Play refused; priming for retry");
                    }
                    super::shared::HlsPlayCompletion::Stale => {
                        eng.prime_play = true;
                        super::log("SMP Play result stale; priming for retry");
                    }
                }
            } else {
                eng.prime_play = true;
                super::log("SMP Play fenced; priming before retry");
            }
        }
    } else if eng.stage == Stage::Loading
        && SHARED.native_load_returned(eng.native_epoch)
    {
        // Load HAS returned here (the first arm's `native_load_gate_ready` failed) and
        // `loadCompleted` has NOT arrived (the previous arm's condition also failed) — the other
        // half of issue #74 D.1's budget, and the one `load-returned-log-is-conditional-on-
        // loadcompleted-and-unbounded-after` found unbounded: on v0.6.0 and unchanged until now,
        // a Load that returns ok=1 and then never signals `loadCompleted` left the player parked
        // in Connecting forever, with no counterpart to the D.1.4 timeout above. Budget it from
        // the moment Load RETURNED, not from when it was ISSUED — `native_load_elapsed` (used
        // above) never resets, so a Load that used most of its own budget getting here would
        // otherwise leave this second wait almost none of its own.
        match SHARED.native_load_returned_elapsed(eng.native_epoch) {
            Some(elapsed) if elapsed >= NATIVE_LOAD_BUDGET => {
                super::log(&format!(
                    "native: loadCompleted did not arrive within {}s after Load returned — \
                     publishing load_failed",
                    NATIVE_LOAD_BUDGET.as_secs()
                ));
                SHARED.load_failed.store(true, Release);
                SHARED.load_timed_out.store(true, Release);
                super::report::note_load_gate_for(
                    crate::route::playback_trace_generation(),
                    super::report::LoadElapsedClass::from_ms(elapsed.as_millis() as i64),
                );
            }
            Some(_) => {}
            // Same rationale as the `None` arm above: losing `Active` (a synchronous
            // UnloadCompleted for this epoch) while still waiting on `loadCompleted` must fire
            // the timeout rather than silently going quiet forever.
            None => {
                super::log(
                    "native: Load epoch left Active while still waiting on loadCompleted after \
                     Load returned — publishing load_failed",
                );
                SHARED.load_failed.store(true, Release);
                SHARED.load_timed_out.store(true, Release);
                super::report::note_load_gate_for(
                    crate::route::playback_trace_generation(),
                    super::report::LoadElapsedClass::Over20s,
                );
            }
        }
    }

    // ---------- ACB bind, Kodi/ss4s order: setSinkType(MAIN)+setMediaId+setState(LOADED) ----------
    if eng.stage == Stage::Playing && ACB_OK.load(Relaxed) {
        let id = SHARED.media_id.lock().unwrap().clone();
        if let Some(id) = id {
            unsafe { super::sink().plane_bind(mt, id.as_ptr()) };
            super::log(&format!("SMP ACB bound id={}", id.to_string_lossy()));
            // **Dolby Atmos, to ACB, exactly here** — this is where LG's own client fires it
            // (`libcbe` 0x1b98d78, on LOADCOMPLETED): after setMediaId, and BEFORE the
            // frame-gated setMediaVideoData below. It needs no decoded frame and reads no
            // state; `context` is the `id` we have in hand, which is the whole reason the call
            // exists rather than a forward of the pipeline's own AUDIO_INFO callback (that
            // string carries `track` and `dualMono` and no context). See `acb_send_atmos`.
            //
            // **This looks like it breaks the "never feed audio to ACB" rule and it does not** —
            // the rule is about the audio ELEMENTARY STREAM, which the pipeline owns and which we
            // still never hand ACB. This is a two-key metadata descriptor, and the distinction is
            // now readable rather than remembered (see `acb_send_atmos` in `src/starfish.c` for
            // the addresses). The rule's stated consequence, `SOUND_ERROR_019`, is a literal that
            // exists in NO library on this device.
            //
            // Ran behind `/tmp/dispatcharrtv-atmosacb` first, then measured, then made the default:
            // `rv=1` accepted, 1600 audio AUs fed with `reply=O` and no error of any kind, and the
            // television's own read-out — "Dolby Vision / Dolby Atmos", both lines — photographed
            // in a DISPLAY capture at 11 s. `/tmp/dispatcharrtv-noatmosacb` is the way back out.
            if crate::route::stream_immersive(ps) && !plx_base::devtrig::flag("noatmosacb") {
                let rv = unsafe { super::sink().plane_send_atmos(mt, id.as_ptr()) };
                super::log(&format!("atmos: acb setMediaAudioData rv={rv}"));
            }
            eng.stage = Stage::Bound;
        }
    }

    // ---------- send the WHOLE sourceInfo envelope once frames flow (verbatim, except a
    // container-PQ VP9 stream, see below), then
    // window + PLAYING (setMediaVideoData -> setDisplayWindow -> setState PLAYING) ----------
    if eng.stage == Stage::Bound && !eng.video_info_sent && SHARED.frames.load(Relaxed) >= 2 {
        let bytes = SHARED.source_info.lock().unwrap().clone();
        if let Some(mut bytes) = bytes {
            // A VP9 stream's PQ lives in the container, which the pipeline never sees, so its own
            // envelope says "hdrType":"none". Say HDR10 here when the demuxer found PQ (see
            // `vp9_hdr`). `/tmp/dispatcharrtv-novp9hdr` sends the envelope as the pipeline made it.
            let container_hdr = *SHARED.container_hdr.lock().unwrap();
            if let Some(h) = container_hdr {
                if !plx_base::devtrig::flag("novp9hdr") {
                    if let Some(rewritten) = super::vp9_hdr::rewrite_source_info(&bytes, &h) {
                        super::log(&super::vp9_hdr::log_line(&h));
                        bytes = rewritten;
                    }
                }
            }
            let rv = unsafe { super::sink().plane_send_video_data(mt, bytes.as_ptr() as *const c_char) };
            super::log(&format!(
                "setMediaVideoData rv={rv} frames={}",
                SHARED.frames.load(Relaxed)
            ));
            if rv != -1 {
                // -1 = client-side isJsonError reject; else accepted
                eng.video_info_sent = true;
                unsafe { super::sink().plane_start(mt, 0, 0, 1920, 1080) };
                eng.stage = Stage::Streaming;
                super::log("setMediaVideoData sent → window+PLAYING");
            }
        }
    }

    // ---------- webOS 5+ (VP_EXPORTED): correct the placement if the real frame size only
    // became known after Load. The demuxer publishes the coded size when it opens the stream,
    // which races the load thread — so the placement above may have used the fallback. Re-placing
    // costs one call, and Kodi re-places on every render-area change anyway.
    // ----------
    if eng.stage >= Stage::Playing && eng.placed_src != (0, 0) {
        let now = SHARED.video_raster();
        if now.0 > 0 && now.1 > 0 && now != eng.placed_src {
            place_exported(mt, eng);
        }
    }

    // ---------- feed AUs after the initial Load has reached Playing. The native clock may itself
    // be Running or Paused: runtime rebuffer and ResumePrime deliberately fill both lanes before
    // issuing Play. NOT while a seek is armed: on a resume the seek is armed before PLAYING, so
    // feeding first would present the file start for a frame before the seek repositions — a
    // visible jump. ----------
    maybe_begin_hls_rebuffer(ps, mt, eng);
    if eng.stage >= Stage::Playing && TX.feed_allowed() && TX.seek_to_ns.load(Relaxed) < 0 {
        if stream {
            // Two-lane feed, then the prime attempt — the ordering and the reason both live in
            // `feed_both_lanes`, so this call site cannot drift from them.
            plx_base::diag::spans::span("feed", || feed_both_lanes(mt, eng));
        } else {
            plx_base::diag::spans::span("feed", || feed_sample(mt, eng));
        }
    }

    // ---------- end-of-stream: the producer hit file EOF (eos_pushed → all AUs to the end were
    // fed) and the pipeline has now played out to within EOS_TAIL of the duration. Mark ended so
    // app.rs tears the player down at the credits instead of freezing on the last frame. Paused
    // at the end stays paused (playpos won't climb) — correct; it fires when resumed. Any seek
    // clears the flag (request_seek), so seeking back from the end doesn't re-trigger. ----------
    // ---------- publish the derived UI state (see PlaybackState's doc) ----------
    // Order matters: a seek in flight outranks "we have frames", because the frames on the panel
    // are the PRE-seek ones and the HUD must show the target, not them.
    set_state(if SHARED.seeking.load(Relaxed) {
        PlaybackState::Seeking
    } else if eng.stage == Stage::Loading {
        PlaybackState::Connecting
    } else if SHARED.hls_rebuffering.load(Relaxed) || SHARED.frames.load(Relaxed) == 0 {
        PlaybackState::Buffering
    } else {
        PlaybackState::Playing
    });

    const EOS_TAIL_NS: i64 = 1_000_000_000;
    if eng.eos_pushed && !SHARED.ended.load(Relaxed) {
        let dur = SHARED.duration_ns.load(Relaxed);
        let pos = SHARED.playpos_ns.load(Relaxed);
        if dur > 0 && pos >= dur - EOS_TAIL_NS {
            SHARED.ended.store(true, Relaxed);
            super::log(&format!(
                "EOS reached: playpos={}s/{}s → ended",
                pos / 1_000_000_000,
                dur / 1_000_000_000
            ));
        } else if drained_without_duration(dur, SHARED.pres_fed.load(Relaxed), eng.max_fed_video_pts, EOS_TAIL_NS) {
            // A stream with no duration — a live channel whose connection or upstream dropped —
            // ends when what was fed has been shown. Without this it froze on its last frame
            // forever, because the test above needs a duration to reach.
            SHARED.ended.store(true, Relaxed);
            super::log(&format!("EOS reached: no duration, presented to {}s → ended", pos / 1_000_000_000));
        }
    }

    // ---------- a live channel whose picture stopped: the producer went quiet without reaching
    // EOF (measured in the 2026-10-15 soak: after 1 h 46 min the feed stopped, no read error, no
    // EOF, and the panel held its last frame for the remaining 14 minutes). Nothing above can see
    // that, because nothing ended. A playhead that stands still while the viewer is not paused
    // ends the stream, and the app's reconnect tunes the channel again. ----------
    let watching = crate::route::is_live(ps)
        && eng.stage >= Stage::Playing
        && SHARED.seen_frame.load(Relaxed)
        && !SHARED.ended.load(Relaxed)
        && !SHARED.seeking.load(Relaxed)
        && !super::lifecycle::paused();
    let rx = SHARED.dg_net_rx.load(Relaxed);
    if let Some(stalled) = eng.live_stall.observe(watching, SHARED.playpos_ns.load(Relaxed), rx, now) {
        SHARED.ended.store(true, Relaxed);
        super::log(&format!(
            "live: stalled {}ms at pos={}s rx+{}B fed_v={}ms → ended",
            stalled.ms,
            SHARED.playpos_ns.load(Relaxed) / 1_000_000_000,
            stalled.rx_bytes,
            eng.max_fed_video_pts / 1_000_000
        ));
    }
}

/// A live picture that has not moved for this long is a stalled stream, not a slow one: the
/// position ticks every 200 ms while anything is shown.
pub(crate) const LIVE_STALL_MS: u32 = 10_000;

/// What [`LiveStall::observe`] reports when it fires: how long the playhead stood, and how many
/// bytes the transport received meanwhile (zero: the server stopped sending; more: the bytes did
/// not become pictures).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stalled {
    pub ms: u32,
    pub rx_bytes: i64,
}

/// Watches a live playhead for [`LIVE_STALL_MS`] without a second's movement.
#[derive(Clone, Copy, Debug, Default)]
pub struct LiveStall {
    armed: bool,
    pos: i64,
    since: u32,
    rx: i64,
}

impl LiveStall {
    /// One frame's look. `watching` is false whenever a still playhead is expected (not live,
    /// not yet playing, paused, seeking, ended); it disarms. Fires once, then disarms.
    pub fn observe(&mut self, watching: bool, pos: i64, rx: i64, now: u32) -> Option<Stalled> {
        if !watching {
            self.armed = false;
            return None;
        }
        // Within a second of where it stood counts as standing: a stalled pipeline's position
        // may still creep by a tick, a playing one moves a second a second.
        if !self.armed || (pos - self.pos).abs() >= 1_000_000_000 {
            *self = Self { armed: true, pos, since: now, rx };
            return None;
        }
        let ms = now.wrapping_sub(self.since);
        if ms < LIVE_STALL_MS {
            return None;
        }
        self.armed = false;
        Some(Stalled { ms, rx_bytes: rx - self.rx })
    }
}

#[cfg(test)]
mod live_stall_tests {
    use super::{LiveStall, Stalled, LIVE_STALL_MS};
    const S: i64 = 1_000_000_000;

    #[test]
    fn a_live_picture_that_stops_moving_ends_the_stream() {
        let mut w = LiveStall::default();
        assert_eq!(w.observe(true, 6347 * S, 100, 1_000), None);
        assert_eq!(w.observe(true, 6347 * S + S / 5, 100, 1_000 + LIVE_STALL_MS - 1), None, "a creeping tick");
        assert_eq!(
            w.observe(true, 6347 * S, 100, 1_000 + LIVE_STALL_MS),
            Some(Stalled { ms: LIVE_STALL_MS, rx_bytes: 0 })
        );
        assert_eq!(w.observe(true, 6347 * S, 100, 1_000 + 2 * LIVE_STALL_MS), None, "fires once, then re-arms");
    }

    #[test]
    fn a_moving_or_paused_picture_is_not_a_stall() {
        let mut w = LiveStall::default();
        for i in 0..100u32 {
            assert_eq!(w.observe(true, i64::from(i) * S / 5, 0, i * 200), None);
        }
        let mut p = LiveStall::default();
        assert_eq!(p.observe(true, S, 0, 0), None);
        assert_eq!(p.observe(false, S, 0, LIVE_STALL_MS / 2), None, "paused");
        assert_eq!(p.observe(true, S, 0, LIVE_STALL_MS + 1), None, "resuming re-arms the clock");
        assert!(p.observe(true, S, 7, 2 * LIVE_STALL_MS + 1).is_some_and(|s| s.rx_bytes == 7));
    }

    #[test]
    fn the_clock_survives_the_tick_wrap() {
        let mut w = LiveStall::default();
        let t = u32::MAX - 100;
        assert_eq!(w.observe(true, S, 0, t), None);
        assert!(w.observe(true, S, 0, t.wrapping_add(LIVE_STALL_MS)).is_some());
    }
}

/// Whether a stream with no duration (`dur <= 0`), its producer at EOF, has presented
/// (`pres_fed`) to within `tail` of the last video it was fed (`max_fed`).
/// Whether a failed transport ends the stream (and the app reconnects) rather than failing the
/// playback: only on a live channel, and only once the stream was open — a failure the open itself
/// raised (`demux_failed`) is the read-out's to explain.
fn live_transport_ended(live: bool, open_failed: bool) -> bool {
    live && !open_failed
}

#[cfg(test)]
mod live_transport_tests {
    use super::live_transport_ended;

    #[test]
    fn only_an_open_live_stream_ends_on_a_transport_failure() {
        assert!(live_transport_ended(true, false), "a live stream that dropped: reconnect");
        assert!(!live_transport_ended(true, true), "a channel that could not be opened: say why");
        assert!(!live_transport_ended(false, false), "anything that is not live keeps its Error");
    }
}

fn drained_without_duration(dur: i64, pres_fed: i64, max_fed: i64, tail: i64) -> bool {
    dur <= 0 && pres_fed != super::engine::PRES_NONE && max_fed > 0 && pres_fed >= max_fed - tail
}

#[cfg(test)]
mod drained_tests {
    use super::drained_without_duration;
    const S: i64 = 1_000_000_000;

    #[test]
    fn a_live_stream_ends_once_what_was_fed_has_been_shown() {
        assert!(!drained_without_duration(0, 3 * S, 6 * S, S), "two seconds still buffered");
        assert!(drained_without_duration(0, 5 * S + 1, 6 * S, S));
        assert!(!drained_without_duration(0, super::super::engine::PRES_NONE, 6 * S, S), "nothing shown yet");
        assert!(!drained_without_duration(0, 0, 0, S), "nothing fed");
        assert!(!drained_without_duration(90 * S, 89 * S, 89 * S, S), "a file ends by its duration");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        
        hls_buffered_ms, native_clock_silence_ms, native_clock_stopped, prime_before_play, rebuffer_request_is_current, should_begin_hls_rebuffer,
        CLOCK_WATCH_SINCE_MS, NATIVE_CLOCK_STOPPED_MS, SHARED,
    };
    use std::sync::atomic::Ordering::{Relaxed, Release};

    /// The device's measured position-callback period, so the replay below advances in the same
    /// steps the television reports in (`vtick=5 vgap=201ms`; `player::sf_on_event`).
    const POSITION_PERIOD_MS: u32 = 201;

    /// Put the three atomics `hls_buffered_ms` samples back where a fresh session leaves them, so
    /// a replay cannot leak a frozen playhead into another test holding the same serial lock.
    fn clear_hls_reserve_sample() {
        SHARED.hls_video_tail_ns.store(-1, Release);
        SHARED.hls_audio_tail_ns.store(-1, Release);
        SHARED.disp_base.store(0, Relaxed);
        SHARED.playpos_ns.store(0, Relaxed);
    }

    /// **Regression: `pipe_abr_down_collapse` on the television, 2026-09-02.** A 40000 kbps link
    /// collapsed to 500 kbps at t=25 s while a 20000 kbps rung was active. The controller's
    /// descent was correct once it could run, but it could not run for 82 s, and the case failed
    /// its `pos_climb` gate with 38 s of progress in a 120 s run.
    ///
    /// What stopped it is the terminal boundary being defined arithmetically. Segment 9 was
    /// demuxed to a video tail of ~29.96 s; the pipeline then stopped reporting a position
    /// altogether (`play=0pm vtick=0 vgap=0ms`, log line 248, holding `pos=29s` for the next
    /// 77 s) with the playhead parked SHORT of that tail, because the last fed access units
    /// cannot be presented until the ones behind them arrive. So `hls_buffered_ms` stuck at a
    /// small positive remnant forever, `should_begin_hls_rebuffer`'s `buffered_ms == Some(0)`
    /// never became true, no `hls: auto-rebuffer pause` was ever written, and the worker's
    /// `StallGuard` — which reads that same hold as `terminal_hold_started` — never abandoned the
    /// fetch. The 5.17 MB object therefore ran to completion on a 500 kbps link
    /// (`open_probe_ms=82431 total_ms=82682`, log line 323) before any rung decision existed.
    ///
    /// The physical fact the arithmetic cannot see is that the clock has STOPPED. This replays
    /// the numbers the log carries and asserts the hold begins about a second after it stops,
    /// not when the fetch completes.
    #[test]
    fn a_stopped_native_clock_reaches_the_terminal_boundary_the_arithmetic_never_will() {
        let _serial = plx_base::testlock::serial();
        // Segment 9 demuxed: 48 video AUs at 24 fps and 94 AAC frames, ending the tenth 2 s
        // segment of the stream (`hls: segment=9 ... v=48 a=94`, log line 235).
        SHARED.hls_video_tail_ns.store(29_958_000_000, Release);
        SHARED.hls_audio_tail_ns.store(29_980_000_000, Release);
        SHARED.disp_base.store(0, Relaxed);
        // The playhead then froze. The log prints whole seconds, so the exact remnant is not
        // recoverable from it; what the log does prove is that it never reached zero, because the
        // auto-rebuffer line that an exact zero writes was never written in those 77 s.
        SHARED.playpos_ns.store(29_700_000_000, Relaxed);
        let buffered_ms = hls_buffered_ms();
        // Clear the shared sample BEFORE the first assertion: `testlock::serial()` recovers a
        // poisoned lock, so a panic past this point would otherwise leak a frozen playhead into
        // every later serial test (see the test-suite pollution note in `lib.rs`).
        clear_hls_reserve_sample();
        assert_eq!(
            buffered_ms,
            Some(258),
            "the arithmetic reserve the main thread samples is positive and stays there"
        );

        // Walk the silence forward one position-callback period at a time and find the first tick
        // at which the main thread holds the clock. That tick is also what the worker's
        // `StallGuard` reads as `terminal_hold_started`, so it is when the oversized fetch is
        // abandoned and a cheaper rung becomes decidable.
        let mut held_at_ms: Option<u32> = None;
        let mut silence_ms = 0u32;
        while silence_ms <= 82_682 {
            let stopped = native_clock_stopped(Some(silence_ms), buffered_ms);
            if should_begin_hls_rebuffer(false, -1, 5_668, buffered_ms, stopped) {
                held_at_ms = Some(silence_ms);
                break;
            }
            silence_ms += POSITION_PERIOD_MS;
        }
        assert!(
            held_at_ms.is_some_and(|ms| ms <= 1_500),
            "a presentation clock that has physically stopped is the terminal reserve boundary:              expected the hold within 1500 ms of the last position callback, got {held_at_ms:?}              (the device waited 82682 ms for the fetch to complete instead)"
        );
    }

    #[test]
    fn segmented_hls_primes_both_lanes_even_without_a_seek() {
        assert!(prime_before_play(false, true));
        assert!(prime_before_play(true, true));
        assert!(prime_before_play(true, false));
        assert!(!prime_before_play(false, false));
    }

    #[test]
    fn boundary_reserves_are_resume_gates_not_mid_acquisition_pause_triggers() {
        assert!(
            !should_begin_hls_rebuffer(false, -1, 3_000, Some(2_999), false),
            "a full acquisition runway cannot be compared to a partially spent buffer mid-fetch",
        );
        assert!(
            should_begin_hls_rebuffer(true, -1, 3_000, Some(2_999), false),
            "the in-flight fetch may still request a controlled hold from its live progress",
        );
        assert!(
            !should_begin_hls_rebuffer(false, 5_303, 5_303, Some(5_167), false),
            "a bounded candidate that reaches its deadline must finish instead of latching a \
             multi-second playback hold at the retrospective replay balance",
        );
        assert!(
            should_begin_hls_rebuffer(false, -1, 3_000, Some(0), false),
            "physical depletion must hold the clock even when a fetch has no measurable prefix",
        );
        assert!(
            should_begin_hls_rebuffer(false, -1, 3_000, Some(258), true),
            "a presentation clock that has stopped is that same physical depletion, observed \
             where the content-time arithmetic cannot reach zero",
        );
    }

    /// The stopped-clock observation is one-sided: it must fire on the device's remnant and must
    /// not fire on a healthy reserve whose callback is merely late.
    #[test]
    fn only_a_reserve_the_stopped_clock_has_outlasted_is_a_terminal_boundary() {
        assert_eq!(NATIVE_CLOCK_STOPPED_MS, 1_005, "five 201 ms position periods");
        assert!(
            !native_clock_stopped(Some(NATIVE_CLOCK_STOPPED_MS - 1), Some(258)),
            "four and a bit periods of silence is jitter, not a stopped clock",
        );
        assert!(
            native_clock_stopped(Some(NATIVE_CLOCK_STOPPED_MS), Some(258)),
            "the device's remnant is smaller than the silence that has already outlasted it",
        );
        assert!(
            !native_clock_stopped(Some(1_200), Some(5_668)),
            "a stream holding 5.6 s of reserve is not starving because one callback was late",
        );
        assert!(
            native_clock_stopped(Some(5_668), Some(5_668)),
            "silence long enough to have presented the whole reserve is the same boundary",
        );
        assert!(
            !native_clock_stopped(None, Some(0)),
            "an epoch with no position callback yet has nothing to call stopped",
        );
        assert!(
            !native_clock_stopped(Some(82_682), None),
            "an unknowable reserve holds nothing, exactly as it arms no StallGuard",
        );
    }

    /// A seek and a session reset both zero `dg_vpres_at`, so the silence must read as absent
    /// rather than as a gap the size of the seek.
    #[test]
    fn a_seek_leaves_no_silence_observation_behind() {
        let _serial = plx_base::testlock::serial();
        let restore = SHARED.dg_vpres_at.swap(0, Relaxed);
        assert_eq!(
            native_clock_silence_ms(),
            None,
            "a cleared presentation stamp is no observation, not an infinite one",
        );
        // And a deliberate hold floors the observation at the moment it was restarted, so a long
        // user Pause cannot be read as a stall on the first tick after Resume.
        SHARED.dg_vpres_at.store(1, Relaxed);
        CLOCK_WATCH_SINCE_MS.store(super::super::vclock_ms(), Relaxed);
        assert!(
            native_clock_silence_ms().is_some_and(|ms| ms < NATIVE_CLOCK_STOPPED_MS),
            "a just-restarted watch cannot already be a stopped clock",
        );
        CLOCK_WATCH_SINCE_MS.store(0, Relaxed);
        SHARED.dg_vpres_at.store(restore, Relaxed);
    }

    #[test]
    fn media_completed_after_a_hold_request_makes_that_request_stale() {
        assert!(rebuffer_request_is_current(true, 10_000, 10_000));
        assert!(
            !rebuffer_request_is_current(true, 10_000, 12_000),
            "a completed and feedable segment removed the underflow the old request described",
        );
        assert!(!rebuffer_request_is_current(false, 10_000, 10_000));
    }

}

