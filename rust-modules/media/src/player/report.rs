//! **Playback lifecycle events joined by one attempt.**
//!
//! `requested -> started -> failed|ended|abandoned`, or
//! `requested -> failed|cancelled|abandoned`. The interesting number is the gap between requested
//! and a terminal outcome: `started / requested` is the startup success rate without leaving a
//! silent unresolved bucket for "it just sat there".
//!
//! The error-consent channel also keeps a separate sparse, typed trace in memory across reloads of
//! this same attempt. It queues exactly one handled Sentry event only if the derived state reaches
//! `Error`; ordinary low quality, buffering and rejected ABR candidates remain breadcrumbs at most,
//! never errors of their own.
//!
//! # A failure says WHY the attempt it ended was made
//!
//! A report names the attempt that failed beside the playback it failed in: the route being opened
//! and whether that open ever showed a picture (`attempted_delivery`, `attempt_started`), why it
//! was made (`attempt_origin`: a Play, a Retry, a quality pick, an automatic handoff, a recovery, a
//! foreground resume) and which open of the session it was (`attempt_ordinal`).
//! Origin and ordinal are decided where the open is, never inferred from the failure: [`requested`]
//! for a request (a Retry continues the count, a Play restarts it) and
//! [`note_delivery_requested_for`] / [`note_attempt`] for a reload of the same playback. They live
//! in [`AttemptLedger`], not in the trace, because a Retry stops the failed playback first and that
//! clears the trace.
//!
//! # These are TRANSITIONS, not log lines
//!
//! The first design put `playback.started` on the engine's `load:` line. That line is the wrong
//! seam and the name would have been a lie: it is emitted BEFORE the source is opened and before
//! anything plays, so a television that never produced a frame would report a start. It is
//! `requested` now, and `started` fires on the first transition into `Playing` — the same value the
//! HUD renders, so the event says what the viewer saw.
//!
//! # Observed at the DERIVED state, not at `pump::set_state`
//!
//! `set_state` looks like the choke point and is not: [`super::state`] derives two of its answers
//! outside `pb_state` entirely — `Resolving` while a plan is in flight, and `Error` for a
//! `/decision` refusal, which happens before an engine exists and so before the pump has ever run.
//! Hooking the setter would have silently missed the earliest and most certain failure there is.
//! So this observes the value the HUD reads, once a frame.
//!
//! # Once each, and only for a REAL end
//!
//! `Playing` is re-entered after every seek and every reload, and `Error` can be republished on
//! consecutive frames; the latch is what makes each event mean "this attempt", not "this frame".
//! And `ended` fires on a genuine teardown only — a seek, an ABR rung change and an app-switch
//! suspend all end an ENGINE without ending a playback, and counting those as endings would make
//! the completion rate a measure of how often people scrub.

use plx_telemetry::diag::schema::DiagEvent;
// The classification vocabulary is telemetry's wire schema (`telemetry::classes`); this module
// classifies a live playback INTO it. Re-exported here so the player and the route layer keep
// naming the closed domains where they report from.
pub use plx_telemetry::telemetry::classes::{
    AttemptOrdinal, AttemptOrigin, AudioCodecClass, BufferClass, HttpClass, LoadElapsedClass,
    PipelineClass, PlaybackErrorContext, RasterClass, RateClass, TraceAge, TraceEvent, TraceStep,
    TransferAgeClass, TransportClass, VideoCodecClass,
};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU8, Ordering::Relaxed};

/// The current attempt's opaque id — random, per attempt, never stored. See `DiagEvent`'s playback
/// block: it joins one attempt's lifecycle events and cannot link two playbacks, let alone two sets.
static ATTEMPT: AtomicI64 = AtomicI64::new(0);
/// Process-local trace generation. Unlike `ATTEMPT`, this is never sent; it only prevents an
/// outgoing demux worker from writing its late transitions into the next Play's reset trace.
static NEXT_TRACE_GENERATION: AtomicU32 = AtomicU32::new(0);
/// The last state this module reported a transition FROM.
static LAST: AtomicU8 = AtomicU8::new(0);
/// One `started`/`failed`/`ended` per attempt.
static SAW_START: AtomicBool = AtomicBool::new(false);
static SAW_FAIL: AtomicBool = AtomicBool::new(false);
static SAW_END: AtomicBool = AtomicBool::new(false);
/// At most one bounded rebuffer summary, emitted when a started attempt reaches an observable
/// terminal or replacement path. The app deliberately does not report dropped frames: LG's
/// position callback is a fixed 5 Hz clock and looks identical on smooth and visibly stuttering
/// playback, so treating it as frame cadence would fabricate data.
static SAW_QUALITY: AtomicBool = AtomicBool::new(false);
static REBUFFER_COUNT: AtomicU8 = AtomicU8::new(0);
static REBUFFER_AT_MS: AtomicI64 = AtomicI64::new(0);
static REBUFFER_TOTAL_MS: AtomicI64 = AtomicI64::new(0);
/// When this attempt was requested, in `SDL_GetTicks` milliseconds — the same monotonic clock every
/// other timestamp in this app uses, because pmlog's wall clock on this television runs ~3h off.
static REQUESTED_MS: AtomicI64 = AtomicI64::new(0);

/// At most this many sparse state changes survive until a terminal playback error. Segment fetches
/// are deliberately absent: one entry per segment would both drown the causal transitions and turn
/// a long film into a larger report than a short one.
pub const ERROR_TRACE_MAX: usize = 32;

/// **Why the engine open now in flight was made, and which open of the session it is.** Two small
/// closed values and the generation they belong to; no history and nothing the viewer typed or
/// chose beyond the cause. Kept apart from [`PlaybackTrace`] on purpose: a Retry tears the failed
/// playback down first, and that teardown clears the trace (`ended` -> [`clear_error_trace`]), but
/// the Retry is the SECOND open of the same session, which only a counter that outlives the trace
/// can say. A fresh Play restarts it; [`note_attempt_for`] and [`requested`] advance it.
struct AttemptLedger {
    generation: u32,
    origin: AttemptOrigin,
    ordinal: AttemptOrdinal,
}

impl AttemptLedger {
    /// A new request begins. A fresh Play starts the session over; a Retry (or a quality pick on
    /// the failure screen) is the next open of the session it repeats.
    fn begin(&mut self, generation: u32, origin: AttemptOrigin) {
        self.ordinal = match origin {
            AttemptOrigin::Fresh => AttemptOrdinal::FIRST,
            _ => self.ordinal.next(),
        };
        self.generation = generation;
        self.origin = origin;
    }

}

fn ledger() -> std::sync::MutexGuard<'static, AttemptLedger> {
    LEDGER.lock().unwrap_or_else(|e| e.into_inner())
}

static LEDGER: std::sync::Mutex<AttemptLedger> = std::sync::Mutex::new(AttemptLedger {
    generation: 0,
    origin: AttemptOrigin::Fresh,
    ordinal: AttemptOrdinal::FIRST,
});

/// The current play attempt's in-memory, privacy-bounded history. It lives in [`super::SHARED`]
/// because both the demux worker (ABR/probe) and main thread (request/seek/error) append to it.
/// Engine reloads do not clear it; a new play attempt does.
pub struct PlaybackTrace {
    generation: u32,
    started_ms: i64,
    sealed: bool,
    /// Whether the engine open now in flight (or last run) has presented a picture. Cleared when
    /// an open begins, set by the presented step.
    attempt_started: bool,
    steps: Vec<TraceStep>,
}

impl PlaybackTrace {
    pub const fn new() -> Self {
        Self {
            generation: 0,
            started_ms: -1,
            sealed: false,
            attempt_started: false,
            steps: Vec::new(),
        }
    }

    fn reset(&mut self, generation: u32, at_ms: i64) {
        self.generation = generation;
        self.started_ms = at_ms;
        self.sealed = false;
        self.forget_attempt();
        self.steps.clear();
        self.push(at_ms, TraceEvent::Requested);
    }

    /// Install an attempt boundary without collecting a breadcrumb. This keeps a later opt-in
    /// scoped to the current Play while preserving the rule that nothing is collected before it.
    fn arm(&mut self, generation: u32) {
        self.generation = generation;
        self.started_ms = -1;
        self.sealed = false;
        self.forget_attempt();
        self.steps.clear();
    }

    fn push_for(&mut self, generation: u32, at_ms: i64, event: TraceEvent) -> bool {
        if generation == 0 || self.generation != generation || self.sealed {
            return false;
        }
        self.push(at_ms, event);
        true
    }

    fn push(&mut self, at_ms: i64, event: TraceEvent) {
        if self.steps.len() == ERROR_TRACE_MAX {
            // Keep the attempt boundary when it exists and retire the oldest interior transition.
            let drop_at = usize::from(matches!(
                self.steps.first().map(|s| s.event),
                Some(TraceEvent::Requested { .. })
            ));
            self.steps.remove(drop_at.min(self.steps.len() - 1));
        }
        if self.started_ms < 0 {
            self.started_ms = at_ms;
        }
        match event {
            // A picture is on the panel: the attempt has started.
            TraceEvent::Presented { .. } => self.attempt_started = true,
            _ => {}
        }
        self.steps.push(TraceStep {
            age: TraceAge::from_ms(at_ms.saturating_sub(self.started_ms)),
            event,
        });
    }

    fn forget_attempt(&mut self) {
        self.attempt_started = false;
    }

    /// Whether the engine open in flight has put a picture on the panel.
    fn attempt_started(&self) -> bool {
        self.attempt_started
    }

    /// An engine open begins (a Load after Resolving or Connecting), so nothing has been shown by
    /// THIS attempt yet. Not a breadcrumb: the open itself is not an event worth a step.
    fn engine_opening(&mut self) {
        self.attempt_started = false;
    }

    fn clear(&mut self) {
        self.generation = 0;
        self.started_ms = -1;
        self.sealed = false;
        self.forget_attempt();
        self.steps.clear();
    }

    fn snapshot(&self) -> Vec<TraceStep> {
        self.steps.clone()
    }

    fn finish(&mut self, at_ms: i64, event: TraceEvent) -> Vec<TraceStep> {
        if !self.sealed {
            self.push(at_ms, event);
            self.sealed = true;
        }
        self.snapshot()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn seed_for_reset_test(&mut self) {
        self.reset(42, 0);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn step_count_for_test(&self) -> usize {
        self.steps.len()
    }
}

fn push_trace_for(generation: u32, event: TraceEvent) {
    let mut trace = super::SHARED
        .playback_trace
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Check consent while holding the same trace lock used by withdrawal's clear. Either this
    // append finishes before the clear (and is erased) or it observes the new decision and does
    // nothing; no breadcrumb can appear after withdrawal and survive it.
    if !plx_telemetry::telemetry::consent::allows_errors() {
        return;
    }
    trace.push_for(generation, now_ms(), event);
}

/// An engine open begins: the attempt that follows has shown nothing yet. Same consent and
/// generation discipline as the breadcrumbs, but no step is added.
fn note_engine_opening() {
    let mut trace = super::SHARED
        .playback_trace
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !plx_telemetry::telemetry::consent::allows_errors() || trace.generation == 0 || trace.sealed {
        return;
    }
    trace.engine_opening();
}

fn push_trace(event: TraceEvent) {
    let mut trace = super::SHARED
        .playback_trace
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !plx_telemetry::telemetry::consent::allows_errors() || trace.generation == 0 {
        return;
    }
    let generation = trace.generation;
    trace.push_for(generation, now_ms(), event);
}

/// Append the terminal step, seal the attempt against late worker writes, and take the report
/// snapshot under one lock. A split append/snapshot lets an ABR worker put an event *after*
/// `playback failed`, producing a causal sequence that never happened.
fn finish_trace(event: TraceEvent) -> Vec<TraceStep> {
    let mut trace = super::SHARED
        .playback_trace
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !plx_telemetry::telemetry::consent::allows_errors() {
        trace.sealed = true;
        return Vec::new();
    }
    trace.finish(now_ms(), event)
}

/// Forget the in-memory error trace immediately when error reporting is withdrawn. Telemetry
/// reaches this through the hook [`install_trace_eraser`] registers
/// (`telemetry::playback::clear_error_trace`); the app's own call sites name it directly.
pub fn clear_error_trace() {
    super::SHARED
        .playback_trace
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// **Hand telemetry this trace's eraser.** Withdrawal and sign-out (`telemetry::transition`) and
/// the boot load (`telemetry::activate_initial`) erase the trace through
/// `telemetry::playback::clear_error_trace`, which cannot name the player and does nothing until
/// this runs. `app::enter_application` runs it
/// on every boot, before telemetry loads and before anything can play.
pub fn install_trace_eraser() {
    plx_telemetry::telemetry::playback::install_error_trace_clear(clear_error_trace);
}

pub fn note_seek_for(generation: u32) {
    push_trace_for(generation, TraceEvent::SeekRequested);
}

/// Why the open now in force was made and which open of the session it is.
pub(crate) fn attempt_snapshot() -> (AttemptOrigin, AttemptOrdinal) {
    let ledger = ledger();
    (ledger.origin, ledger.ordinal)
}

/// Record the native Load-returned gate opening, or issue #74 D.1.4's budget firing first — see
/// [`LoadElapsedClass`]. Callable off the main thread (the load thread reports the ordinary gate
/// transition), like every other `_for` breadcrumb here.
pub fn note_load_gate_for(generation: u32, elapsed: LoadElapsedClass) {
    push_trace_for(generation, TraceEvent::LoadGateOpened { elapsed });
}

/// The closed classes of what the demuxer published about a dying transfer (issue #504): the
/// libcurl code, the transfer's age in ms and whether the reopen after it was refused. A refused
/// reopen is the class on its own — the first death's code is in the event log — and a code of
/// zero with no refusal is "no transfer failure recorded".
fn transport_classes(
    rc: i32,
    age_ms: i64,
    reopen_failed: bool,
) -> (TransportClass, TransferAgeClass) {
    let class = if reopen_failed {
        TransportClass::ReopenFailed
    } else {
        TransportClass::from_curl_rc(rc)
    };
    (class, TransferAgeClass::from_ms(age_ms))
}

fn error_context() -> PlaybackErrorContext {
    let (transport, transfer_age) = transport_classes(
        super::SHARED.dg_transport_rc.load(Relaxed),
        super::SHARED.dg_transport_age_ms.load(Relaxed),
        super::SHARED.dg_transport_reopen_failed.load(Relaxed),
    );
    let attempt_started = super::SHARED
        .playback_trace
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .attempt_started();
    let http_status = super::SHARED.dg_http_status.load(Relaxed);
    let (attempt_origin, attempt_ordinal) = attempt_snapshot();
    let context = PlaybackErrorContext {
        declared_rate: RateClass::from_kbps(-1),
        media_rate: RateClass::from_kbps(-1),
        raster: RasterClass::from_height(super::SHARED.video_raster().1),
        pipeline: PipelineClass::from_stage(super::SHARED.dg_stage.load(Relaxed)),
        http: HttpClass::from_status(http_status),
        buffer: BufferClass::from_ms(-1),
        transport,
        transfer_age,
        started: SAW_START.load(Relaxed),
        attempt_started,
        attempt_origin,
        attempt_ordinal,
        http_status: HttpClass::exact_status(http_status),
        pipeline_error: super::SHARED.dg_cb_err_num.load(Relaxed),
    };
    context
}

/// Put the per-attempt latches back the way an idle player has them, after a route test that
/// asked for a trace.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_latches_for_test() {
    super::SHARED.pb_state.store(super::shared::PlaybackState::Idle as u8, Relaxed);
    LAST.store(super::shared::PlaybackState::Idle as u8, Relaxed);
    SAW_START.store(false, Relaxed);
    SAW_FAIL.store(false, Relaxed);
    clear_error_trace();
}

fn presented_event() -> TraceEvent {
    TraceEvent::Presented {
        declared_rate: RateClass::from_kbps(-1),
        raster: RasterClass::from_height(super::SHARED.video_raster().1),
    }
}

/// **A new attempt.** Called where the app commits to a plan, before anything opens a socket.
///
/// Mints the id and clears every latch, so a second Play on the same item is a second attempt with
/// its own funnel rather than a silent no-op against the first one's latches.
///
/// `origin` is why the viewer or the app is making it ([`AttemptOrigin`]): `Fresh` for a tune the
/// viewer asked for, which also restarts the session's open count; `UserRetry`, `Recovery` or
/// `Resume` for a re-tune of the same channel, which continues it.
pub fn requested(origin: AttemptOrigin) -> u32 {
    resolve_replaced_attempt();
    let id = new_attempt_id();
    let at = now_ms();
    // **`fetch_update`'s own read-modify-write loop, written out.** That method was deprecated on
    // nightly in favour of `try_update`, and `make lint` denies warnings, so CI goes red on a
    // toolchain this repo deliberately does not pin — while renaming would stop every checkout on
    // an older nightly compiling at all. This is exactly the loop the method runs internally, it
    // predates both spellings, and the closure here never declined an update anyway.
    let previous = {
        let mut current = NEXT_TRACE_GENERATION.load(Relaxed);
        loop {
            let next = if current == u32::MAX { 1 } else { current + 1 };
            match NEXT_TRACE_GENERATION.compare_exchange_weak(current, next, Relaxed, Relaxed) {
                Ok(previous) => break previous,
                Err(actual) => current = actual,
            }
        }
    };
    let generation = if previous == u32::MAX {
        1
    } else {
        previous + 1
    };
    ATTEMPT.store(id, Relaxed);
    ledger().begin(generation, origin);
    SAW_START.store(false, Relaxed);
    SAW_FAIL.store(false, Relaxed);
    SAW_END.store(false, Relaxed);
    SAW_QUALITY.store(false, Relaxed);
    REBUFFER_COUNT.store(0, Relaxed);
    REBUFFER_AT_MS.store(0, Relaxed);
    REBUFFER_TOTAL_MS.store(0, Relaxed);
    REQUESTED_MS.store(at, Relaxed);
    LAST.store(super::shared::PlaybackState::Resolving as u8, Relaxed);
    let mut trace = super::SHARED
        .playback_trace
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if plx_telemetry::telemetry::consent::allows_errors() {
        trace.reset(generation, at);
    } else {
        trace.arm(generation);
    }
    drop(trace);
    emit(DiagEvent::PlaybackRequested { playback_id: id });
    generation
}

fn emit(event: DiagEvent) {
    plx_telemetry::diag::event(event);
}

/// Resolve an attempt before a newer Play overwrites its join key. Before first frame this is an
/// explicit cancellation; after first frame it is an abandoned viewing session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Replacement {
    None,
    Cancelled,
    Abandoned,
}

fn replacement(saw_start: bool, saw_fail: bool, saw_end: bool) -> Replacement {
    if saw_fail || saw_end {
        Replacement::None
    } else if saw_start {
        Replacement::Abandoned
    } else {
        Replacement::Cancelled
    }
}

fn resolve_replaced_attempt() {
    let id = ATTEMPT.swap(0, Relaxed);
    if id == 0 {
        return;
    }
    match replacement(
        SAW_START.load(Relaxed),
        SAW_FAIL.load(Relaxed),
        SAW_END.load(Relaxed),
    ) {
        Replacement::None => {}
        Replacement::Cancelled => {
            emit(DiagEvent::PlaybackCancelled {
                playback_id: id,
            });
        }
        Replacement::Abandoned => {
            report_quality(id);
            emit(DiagEvent::PlaybackAbandoned {
                playback_id: id,
            });
        }
    }
}

/// The process is leaving an unresolved attempt. Unlike a newer Play, this was not a replacement
/// choice, so classify it as abandonment; if playback had started, close its quality summary too.
pub fn abandon_pending() {
    let id = ATTEMPT.load(Relaxed);
    if id == 0 || SAW_FAIL.load(Relaxed) || SAW_END.swap(true, Relaxed) {
        return;
    }
    report_quality(id);
    emit(DiagEvent::PlaybackAbandoned {
        playback_id: id,
    });
}

/// What a frame's state change is worth reporting, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum What {
    Started,
    Failed,
}

/// **The rule, pure — because this is the part that decides whether a number is right or double.**
///
/// Everything a dashboard says about playback rests on each of these firing exactly once per
/// attempt, and the two ways to get it wrong are invisible from the dashboard itself: a `started`
/// that re-fires makes the success rate exceed 100% quietly, and one that fires on a rebuffer makes
/// heavy scrubbers look like heavy watchers. Neither is observable without a test, so the decision
/// is separated from the globals and graded on the host.
fn transition(
    prev: super::shared::PlaybackState,
    now: super::shared::PlaybackState,
    saw_start: bool,
    saw_fail: bool,
) -> Option<What> {
    use super::shared::PlaybackState as S;
    if prev == now {
        return None;
    }
    match now {
        // Re-entered after every seek, every ABR rung change and every rebuffer — the latch is what
        // makes this event mean "this attempt", not "this frame".
        S::Playing if !saw_start => Some(What::Started),
        // Republished on consecutive frames while the read-out is up, and reachable twice if a
        // transient state passes through between. Same latch, same reason.
        S::Error if !saw_fail => Some(What::Failed),
        _ => None,
    }
}

/// **Observe the state the HUD is rendering.** Called once a frame from the main loop.
pub fn tick(ps: &crate::route::PlaybackSession) {

    let now = super::state(ps);
    let prev = super::shared::PlaybackState::from_u8(LAST.swap(now as u8, Relaxed));
    note_rebuffer(prev, now, SAW_START.load(Relaxed));
    // `Resolving` and `Connecting` are the states of an engine being opened, whether the first or
    // a reload after a quality switch; a rebuffer or a seek is neither, so an open that never
    // reaches a picture is told apart from a session that had one.
    if prev != now
        && matches!(
            now,
            super::shared::PlaybackState::Resolving | super::shared::PlaybackState::Connecting
        )
    {
        note_engine_opening();
    }
    if prev != now && now == super::shared::PlaybackState::Playing {
        // Unlike the usage funnel's once-per-attempt `Started`, every return to actual presented
        // video is useful causal evidence after a seek or delivery reload.
        push_trace(presented_event());
    }
    match transition(prev, now, SAW_START.load(Relaxed), SAW_FAIL.load(Relaxed)) {
        Some(What::Started) => {
            SAW_START.store(true, Relaxed);
            emit(DiagEvent::PlaybackStarted {
                playback_id: ATTEMPT.load(Relaxed),
                raster: raster_class(super::SHARED.video_raster().1),
                fps: fps_rung(crate::route::stream_fps(ps)),
                video: video_codec_class(&crate::route::stream_vcodec(ps)),
                audio: audio_codec_class(&crate::route::stream_acodec(ps)),
                startup: startup_class(now_ms() - REQUESTED_MS.load(Relaxed)),
            });
        }
        Some(What::Failed) => {
            SAW_FAIL.store(true, Relaxed);
            report_quality(ATTEMPT.load(Relaxed));
            let shape = super::error_now(ps);
            let kind = shape.kind.class();
            let trace = finish_trace(TraceEvent::Failed { kind });
            let context = error_context();
            plx_telemetry::telemetry::playback::report_error(kind, context, &trace);
            emit(DiagEvent::PlaybackFailed {
                playback_id: ATTEMPT.load(Relaxed),
                kind: shape.kind.code(),
            });
        }
        None => {}
    }
}

/// **A real teardown.** Called from the one place playback actually ends — never from a seek, a
/// rung change or a suspend, each of which destroys an engine and keeps the playback.
pub fn ended() {
    if SAW_END.swap(true, Relaxed) || SAW_FAIL.load(Relaxed) {
        clear_error_trace();
        return; // already terminal
    }
    let id = ATTEMPT.load(Relaxed);
    if id == 0 {
        clear_error_trace();
        return;
    }
    if !SAW_START.load(Relaxed) {
        emit(DiagEvent::PlaybackAbandoned {
            playback_id: id,
        });
        clear_error_trace();
        return;
    }
    report_quality(id);
    emit(DiagEvent::PlaybackEnded { playback_id: id });
    clear_error_trace();
}

fn note_rebuffer(
    prev: super::shared::PlaybackState,
    now: super::shared::PlaybackState,
    saw_start: bool,
) {
    use super::shared::PlaybackState as S;
    if starts_rebuffer(prev, now, saw_start) {
        // A saturating increment, as a loop; see `requested` for why not `fetch_update`.
        let mut current = REBUFFER_COUNT.load(Relaxed);
        while let Err(actual) =
            REBUFFER_COUNT.compare_exchange_weak(current, current.saturating_add(1), Relaxed, Relaxed)
        {
            current = actual;
        }
        REBUFFER_AT_MS.store(now_ms().max(1), Relaxed);
    } else if now != S::Buffering {
        finish_rebuffer_window();
    }
}

fn starts_rebuffer(
    prev: super::shared::PlaybackState,
    now: super::shared::PlaybackState,
    saw_start: bool,
) -> bool {
    use super::shared::PlaybackState as S;
    saw_start && prev == S::Playing && now == S::Buffering
}

fn finish_rebuffer_window() {
    let at = REBUFFER_AT_MS.swap(0, Relaxed);
    if at > 0 {
        REBUFFER_TOTAL_MS.fetch_add((now_ms() - at).max(0), Relaxed);
    }
}

fn report_quality(playback_id: i64) {
    if playback_id == 0 || !SAW_START.load(Relaxed) || SAW_QUALITY.swap(true, Relaxed) {
        return;
    }
    finish_rebuffer_window();
    emit(DiagEvent::PlaybackQuality {
        playback_id,
        rebuffers: rebuffer_count_class(REBUFFER_COUNT.load(Relaxed)),
        buffering: rebuffer_time_class(REBUFFER_TOTAL_MS.load(Relaxed)),
    });
}

fn rebuffer_count_class(n: u8) -> &'static str {
    match n {
        0 => "0",
        1 => "1",
        2..=3 => "2-3",
        _ => "4+",
    }
}

fn rebuffer_time_class(ms: i64) -> &'static str {
    match ms {
        ..=0 => "none",
        1..=1_999 => "<2s",
        2_000..=9_999 => "2-10s",
        _ => "10s+",
    }
}

/// Milliseconds since this process started, monotonic.
///
/// **`std::time::Instant`, not `SDL_GetTicks`**, and the reason is the host suite rather than
/// taste: `cargo test --lib` links no SDL, so a `SDL_GetTicks` here does not skip a test — it stops
/// the whole suite LINKING, which is the boundary `ui/CLAUDE.md` records for `TTF_SizeUTF8`. Only
/// the DIFFERENCE of two readings is ever used, so any monotonic origin will do.
///
/// Never a wall clock either way: pmlog's on this television runs about three hours off, which is
/// why `docs/agent-reference.md` says to correlate a crash by monotonic time and not by time of day.
fn now_ms() -> i64 {
    static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    ORIGIN
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as i64
}

/// A random attempt id. `/dev/urandom` like every other random value in this crate — never a clock
/// or a counter, both of which would say something about the television across attempts.
fn new_attempt_id() -> i64 {
    use std::io::Read;
    let mut b = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_err()
    {
        return 0; // no randomness: the funnel loses its join and nothing else
    }
    (i64::from_le_bytes(b) & i64::MAX) as i64
}

pub fn video_codec_class(name: &str) -> &'static str {
    VideoCodecClass::from_name(name).code()
}

pub fn audio_codec_class(name: &str) -> &'static str {
    AudioCodecClass::from_name(name).code()
}

// ---- the buckets, which are the privacy decision --------------------------------------------
//
// Exact duration + exact raster + exact frame rate + codec identifies a specific file in a specific
// library. As classes they answer every question this channel exists to answer — does 4K HEVC fail
// more than 1080p h264, does startup get worse on big files — and identify nothing. All pure, so
// every boundary is graded on the host.

/// The four rungs the whole project already reasons in — the pipeline tier's resolution matrix, the
/// PMS decision's own classes, LG's checklist. Named rather than measured for the reason above.
pub fn raster_class(height: i32) -> &'static str {
    RasterClass::from_height(height).code()
}

/// A fixed rung, so 23.976 and 24.000 are one bucket rather than two — the distinction is a
/// fingerprint of a particular encode and answers nothing. Anything off the ladder is `other`
/// rather than the nearest rung: a genuinely odd rate is a fact worth being able to see.
pub fn fps_rung(fps: f64) -> &'static str {
    const RUNGS: [(f64, &str); 6] = [
        (24.0, "24"),
        (25.0, "25"),
        (30.0, "30"),
        (50.0, "50"),
        (60.0, "60"),
        (100.0, "100"),
    ];
    if !(fps > 0.0) {
        return "unknown";
    }
    // 1.5% either side, which separates every rung above and still catches both spellings of each
    // (23.976/24, 29.97/30, 59.94/60 — the 1001-denominator forms this project's own fixtures use).
    RUNGS
        .iter()
        .find(|(r, _)| (fps - r).abs() / r <= 0.015)
        .map(|(_, n)| *n)
        .unwrap_or("other")
}

/// How long the viewer waited for a picture. The boundaries are where the EXPERIENCE changes, not
/// round numbers: under a second reads as instant, three seconds is where a person starts to wonder,
/// ten is where they press something.
pub fn startup_class(ms: i64) -> &'static str {
    match ms {
        m if m < 0 => "unknown",
        m if m < 1_000 => "<1s",
        m if m < 3_000 => "1-3s",
        m if m < 10_000 => "3-10s",
        _ => "10s+",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_an_attempt_always_gives_the_old_one_a_terminal_outcome() {
        assert_eq!(replacement(false, false, false), Replacement::Cancelled);
        assert_eq!(replacement(true, false, false), Replacement::Abandoned);
        assert_eq!(replacement(false, true, false), Replacement::None);
        assert_eq!(replacement(true, false, true), Replacement::None);
    }

    #[test]
    fn quality_is_bounded_and_seek_priming_is_not_a_rebuffer() {
        assert_eq!(rebuffer_count_class(0), "0");
        assert_eq!(rebuffer_count_class(1), "1");
        assert_eq!(rebuffer_count_class(3), "2-3");
        assert_eq!(rebuffer_count_class(u8::MAX), "4+");
        assert_eq!(rebuffer_time_class(0), "none");
        assert_eq!(rebuffer_time_class(1_999), "<2s");
        assert_eq!(rebuffer_time_class(2_000), "2-10s");
        assert_eq!(rebuffer_time_class(10_000), "10s+");
        assert!(starts_rebuffer(S::Playing, S::Buffering, true));
        assert!(!starts_rebuffer(S::Seeking, S::Buffering, true));
        assert!(!starts_rebuffer(S::Playing, S::Buffering, false));
    }

    /// Source codecs reach the report through the closed tables only: a name the table does not
    /// know is `other` and is never echoed, which is what keeps a file's own tag out of Sentry.
    #[test]
    fn source_codecs_are_closed_classes_and_unknown_names_are_never_echoed() {
        assert_eq!(VideoCodecClass::from_name("HEVC").code(), "hevc");
        assert_eq!(VideoCodecClass::from_name("vc1").code(), "other");
        assert_eq!(VideoCodecClass::from_name("").code(), "unknown");
        assert_eq!(AudioCodecClass::from_name("dca").code(), "dts");
        assert_eq!(AudioCodecClass::from_name("x-private-tag").code(), "other");
        // The usage funnel's string helpers are the same table.
        assert_eq!(video_codec_class("h265"), VideoCodecClass::Hevc.code());
        assert_eq!(audio_codec_class("ec-3"), AudioCodecClass::Eac3.code());
    }

    /// The firmware's error number reaches the report context from the callback's latch: a
    /// simulated `type=18 num=601` then a failure reports 601, and a session whose pipeline never
    /// spoke an error reports 0.
    #[test]
    fn a_type_18_callback_number_reaches_the_error_context() {
        let _g = plx_base::testlock::serial();
        super::super::SHARED.reset_session();
        assert_eq!(error_context().pipeline_error, 0, "no callback, nothing to report");
        let epoch = super::super::SHARED.begin_native_session().expect("session");
        super::super::sf_on_event(epoch, 18, 601, c"Resource Allocation Error".as_ptr());
        assert_eq!(error_context().pipeline_error, 601);
        super::super::SHARED.retire_native_session(epoch);
        super::super::SHARED.reset_session();
        assert_eq!(error_context().pipeline_error, 0, "the latch does not outlive its session");
    }

    /// The sealed trace the shared slot now holds.
    fn trace_steps() -> Vec<TraceStep> {
        super::super::SHARED
            .playback_trace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot()
    }

    /// The handled event a failure in `ps` would queue, as the telemetry serializer builds it.
    fn event_for(ps: &crate::route::PlaybackSession) -> serde_json::Value {
        let kind = crate::player::error_now(ps).kind.class();
        serde_json::from_slice(&plx_telemetry::telemetry::playback::event_body(
            &"a".repeat(32),
            "0123456789abcdef",
            Some(&"e".repeat(32)),
            kind,
            error_context(),
            &trace_steps(),
        ))
        .expect("handled event JSON")
    }

    /// Run `body` with error reporting consented, then put every latch back.
    fn consented<T>(body: impl FnOnce() -> T) -> T {
        use plx_telemetry::telemetry::consent::{self, Consent};
        let previous = consent::current();
        let mut enabled = Consent::default();
        enabled.errors = true;
        enabled.asked_version = consent::POLICY_VERSION;
        consent::install(enabled);
        let out = body();
        crate::player::SHARED.pb_state.store(S::Idle as u8, Relaxed);
        LAST.store(S::Idle as u8, Relaxed);
        SAW_START.store(false, Relaxed);
        SAW_FAIL.store(false, Relaxed);
        clear_error_trace();
        consent::install(previous.unwrap_or_default());
        out
    }

    /// A film's failure reports its own origin and the plain playback fingerprint.
    #[test]
    fn a_film_failure_keeps_its_origin_and_the_playback_fingerprint() {
        let _g = plx_base::testlock::serial();
        consented(|| {
            let mut ps = crate::route::PlaybackSession::default();
            crate::route::refuse_for_test(&mut ps, crate::route::PlayVerdict::DirectPlayDisabled);
            requested(AttemptOrigin::Fresh);
            observe(&ps, S::Resolving);
            observe(&ps, S::Error);
            assert_eq!(error_context().attempt_origin, AttemptOrigin::Fresh);
            let v = event_for(&ps);
            let kind = v["tags"]["playback.kind"].as_str().expect("kind tag").to_owned();
            assert_eq!(v["fingerprint"], serde_json::json!(["playback-error", kind]));
            assert_eq!(v["tags"]["playback.attempt_origin"], "fresh");
        });
    }

    /// Present `state` to the per-frame observer, as the main loop does once a frame.
    fn observe(ps: &crate::route::PlaybackSession, state: S) {
        crate::player::SHARED.pb_state.store(state as u8, Relaxed);
        tick(ps);
    }

    /// **A Retry press is the SECOND open of the session, and the report says why.** The failed
    /// playback is torn down first (`ended` clears the trace) and the retry mints a new attempt, so
    /// the count has to outlive both. A fresh Play afterwards starts again at the first.
    #[test]
    fn a_retry_press_reports_user_retry_as_the_second_open_and_a_fresh_play_starts_over() {
        let _g = plx_base::testlock::serial();
        consented(|| {
            let ps = crate::route::PlaybackSession::default();
            requested(AttemptOrigin::Fresh);
            observe(&ps, S::Connecting);
            observe(&ps, S::Error);
            let first = error_context();
            assert_eq!((first.attempt_origin.code(), first.attempt_ordinal.code()), ("fresh", "1"));

            // The Retry: the failed playback is stopped (which clears the trace), then re-requested.
            clear_error_trace();
            requested(AttemptOrigin::UserRetry);
            observe(&ps, S::Resolving);
            observe(&ps, S::Error);
            let retried = error_context();
            assert_eq!(retried.attempt_origin.code(), "user_retry");
            assert_eq!(retried.attempt_ordinal.code(), "2");

            // The app's own reconnect continues the count, with its own cause.
            clear_error_trace();
            requested(AttemptOrigin::Recovery);
            let reconnected = error_context();
            assert_eq!((reconnected.attempt_origin.code(), reconnected.attempt_ordinal.code()), ("recovery", "3"));

            // Tuning a channel starts a new session.
            clear_error_trace();
            requested(AttemptOrigin::Fresh);
            let fresh = error_context();
            assert_eq!((fresh.attempt_origin.code(), fresh.attempt_ordinal.code()), ("fresh", "1"));
        });
    }

    /// The ordinal is bounded: a session that keeps failing reads `9+`, never a bigger number.
    #[test]
    fn the_attempt_ordinal_saturates_at_nine_plus() {
        let _g = plx_base::testlock::serial();
        consented(|| {
            requested(AttemptOrigin::Fresh);
            let mut seen = vec![error_context().attempt_ordinal.code()];
            for _ in 0..12 {
                requested(AttemptOrigin::UserRetry);
                seen.push(error_context().attempt_ordinal.code());
            }
            assert_eq!(
                seen,
                ["1", "2", "3", "4", "5", "6", "7", "8", "9", "9+", "9+", "9+", "9+"],
            );
        });
    }

    /// The attempt state on the pure trace: a fresh Play has shown nothing, a picture on screen
    /// starts the attempt, a new engine open starts another, and a late note from a previous Play
    /// (stale generation) changes nothing.
    #[test]
    fn the_attempt_state_follows_the_trace_and_ignores_a_stale_generation() {
        let mut trace = PlaybackTrace::new();
        trace.reset(7, 0);
        assert!(!trace.attempt_started(), "a fresh Play has shown nothing yet");

        let presented = TraceEvent::Presented {
            declared_rate: RateClass::Unknown,
            raster: RasterClass::Unknown,
        };
        assert!(!trace.push_for(6, 1, presented), "a stale generation is refused");
        assert!(!trace.attempt_started());

        trace.push(3, presented);
        assert!(trace.attempt_started(), "a picture shown");

        trace.engine_opening();
        assert!(!trace.attempt_started(), "a new engine open starts a new attempt");

        trace.reset(8, 0);
        assert!(!trace.attempt_started());
    }

    /// **The exact status beside the class.** The class says "4xx"; the number says whether the
    /// source refused (403), lost the part (404) or the server asked for a retry (429). Only a real
    /// HTTP status travels: anything outside 100-599 (an unset latch, a transport code that landed
    /// in the field) is reported as 0 rather than as itself.
    #[test]
    fn the_exact_http_status_rides_beside_the_class_and_only_when_it_is_one() {
        let _g = plx_base::testlock::serial();
        let status = &crate::player::SHARED.dg_http_status;
        for (raw, want) in [
            (404, 404u16),
            (403, 403),
            (503, 503),
            (100, 100),
            (599, 599),
            (0, 0),
            (99, 0),
            (600, 0),
            (999, 0),
            (-1, 0),
            (i32::MIN, 0),
        ] {
            status.store(raw, Relaxed);
            let ctx = error_context();
            assert_eq!(ctx.http_status, want, "raw {raw}");
            assert_eq!(ctx.http, HttpClass::from_status(raw), "the class is unchanged for raw {raw}");
        }
        status.store(404, Relaxed);
        assert_eq!(error_context().http.code(), "4xx");
        status.store(0, Relaxed);
    }

    use super::super::shared::PlaybackState as S;

    /// Drive a sequence of states through the rule the way a frame loop would, latches and all, and
    /// report what it emitted.
    fn drive(states: &[S]) -> Vec<What> {
        let (mut saw_start, mut saw_fail) = (false, false);
        let mut prev = S::Idle;
        let mut out = Vec::new();
        for &s in states {
            if let Some(w) = transition(prev, s, saw_start, saw_fail) {
                match w {
                    What::Started => saw_start = true,
                    What::Failed => saw_fail = true,
                }
                out.push(w);
            }
            prev = s;
        }
        out
    }

    /// The ordinary success: one `started`, whatever the pre-roll did on the way there.
    #[test]
    fn a_normal_start_reports_once() {
        assert_eq!(
            drive(&[S::Resolving, S::Connecting, S::Buffering, S::Playing]),
            [What::Started]
        );
    }

    /// **A seek is not a second start.** `Playing` is re-entered after every seek, every ABR rung
    /// change and every rebuffer; counting those would push the success rate over 100% and make a
    /// heavy scrubber look like several viewers — both silently, since neither is visible from the
    /// dashboard the number appears on.
    #[test]
    fn seeking_and_rebuffering_do_not_start_a_second_playback() {
        let scrubbed = drive(&[
            S::Playing,
            S::Seeking,
            S::Playing,
            S::Seeking,
            S::Playing,
            S::Buffering, // a rebuffer on a bad link
            S::Playing,
        ]);
        assert_eq!(
            scrubbed,
            [What::Started],
            "a seek or a rebuffer reported a second start"
        );
    }

    /// A failure republished on consecutive frames — which is what the read-out being up looks
    /// like — is one failure.
    #[test]
    fn a_failure_held_on_screen_reports_once() {
        assert_eq!(
            drive(&[S::Resolving, S::Error, S::Error, S::Error]),
            [What::Failed]
        );
        // …and it stays one even if a transient state passes through and comes back.
        assert_eq!(drive(&[S::Error, S::Buffering, S::Error]), [What::Failed]);
    }

    /// A playback that started and then failed reports both, in that order: they are different
    /// questions ("did it ever play" and "did it break"), and a stream that dies mid-film answers
    /// yes to each.
    #[test]
    fn a_playback_that_starts_and_then_dies_reports_both() {
        assert_eq!(
            drive(&[S::Playing, S::Error]),
            [What::Started, What::Failed]
        );
    }

    /// The pre-flight refusal — `/decision` said no, so no engine ever existed and the pump never
    /// ran. It is the earliest and most certain failure there is, and it is why this observes the
    /// DERIVED state rather than `pump::set_state`.
    #[test]
    fn a_refusal_before_any_engine_still_reports_a_failure() {
        assert_eq!(drive(&[S::Resolving, S::Error]), [What::Failed]);
    }

    /// A long, eventful playback stays one bounded Sentry event. Preserve the attempt boundary and
    /// the newest transitions; retiring the terminal end to keep an old middle step would erase
    /// the cause this trace exists to carry.
    #[test]
    fn handled_error_trace_is_bounded_and_keeps_both_ends() {
        let mut trace = PlaybackTrace::new();
        trace.reset(7, 0);
        for at in 1..=ERROR_TRACE_MAX + 8 {
            trace.push(at as i64 * 1_000, TraceEvent::SeekRequested);
        }
        let snapshot = trace.finish(
            (ERROR_TRACE_MAX as i64 + 9) * 1_000,
            TraceEvent::Failed {
                kind: plx_telemetry::telemetry::classes::FailureClass::PlaybackInterrupted,
            },
        );
        assert_eq!(trace.steps.len(), ERROR_TRACE_MAX);
        assert!(matches!(
            trace.steps.first().map(|s| s.event),
            Some(TraceEvent::Requested { .. })
        ));
        assert!(matches!(
            snapshot.last().map(|s| s.event),
            Some(TraceEvent::Failed {
                kind: plx_telemetry::telemetry::classes::FailureClass::PlaybackInterrupted,
            })
        ));
        assert!(
            !trace.push_for(7, 99_000, TraceEvent::SeekRequested),
            "nothing may be appended after the terminal event",
        );
        trace.clear();
        assert!(
            trace.steps.is_empty(),
            "withdrawing consent must forget the trace in memory"
        );
    }

    #[test]
    fn a_late_worker_cannot_write_into_the_next_attempt() {
        let mut trace = PlaybackTrace::new();
        trace.reset(11, 0);
        trace.reset(12, 1_000);
        assert!(
            !trace.push_for(11, 2_000, TraceEvent::SeekRequested),
            "the outgoing worker's generation is stale"
        );
        assert!(trace.push_for(12, 2_000, TraceEvent::SeekRequested));
        assert_eq!(
            trace.steps.len(),
            2,
            "new request plus its own transition only"
        );
    }

    /// **A codec name the table does not know becomes `other`, never itself.** The wire vocabulary
    /// is closed by construction — `diag::schema` has no arm that can carry a runtime string — and
    /// this is the mapping that keeps it that way for a field whose source IS one.
    #[test]
    fn an_unknown_codec_name_cannot_travel_as_itself() {
        assert_eq!(video_codec_class("h264"), "h264");
        assert_eq!(
            video_codec_class("HEVC"),
            "hevc",
            "the server's casing varies"
        );
        assert_eq!(
            audio_codec_class("AC3 PLUS"),
            "eac3",
            "the Load payload's own spelling"
        );
        for odd in ["cinepak", "../../etc/passwd", "Dune.mkv", "h264 (Main)"] {
            assert_eq!(video_codec_class(odd), "other", "{odd} travelled as itself");
        }
        assert_eq!(video_codec_class(""), "unknown");
        assert_eq!(audio_codec_class(""), "unknown");
    }

    /// The rungs, and both spellings of each. `pipe_h264_1080p5994` is the project's own fixture
    /// that reaches `fps_rational`'s 1001-denominator branch and measures `60000/1001`; a bucket
    /// that put it beside 60 in one build and beside `other` in the next would make a year of
    /// comparisons meaningless.
    #[test]
    fn both_spellings_of_a_frame_rate_land_on_one_rung() {
        for (fps, want) in [
            (24.0, "24"),
            (24000.0 / 1001.0, "24"),
            (25.0, "25"),
            (30.0, "30"),
            (30000.0 / 1001.0, "30"),
            (50.0, "50"),
            (60.0, "60"),
            (60000.0 / 1001.0, "60"),
        ] {
            assert_eq!(fps_rung(fps), want, "{fps}");
        }
        // Off the ladder stays off it — a genuinely odd rate is worth being able to see.
        assert_eq!(fps_rung(48.0), "other");
        assert_eq!(fps_rung(23.0), "other");
        assert_eq!(fps_rung(0.0), "unknown");
        assert_eq!(fps_rung(f64::NAN), "unknown");
    }

    /// The raster classes are the ones the rest of the project already reasons in, and the
    /// boundaries are inclusive at the top of each: 1080 is FHD, 1081 is not.
    #[test]
    fn the_raster_classes_are_the_projects_own_rungs() {
        for (h, want) in [
            (480, "sd"),
            (576, "sd"),
            (720, "hd"),
            (1080, "fhd"),
            (1081, "uhd"),
            (2160, "uhd"),
        ] {
            assert_eq!(raster_class(h), want, "{h}");
        }
        assert_eq!(raster_class(0), "unknown");
        assert_eq!(raster_class(-1), "unknown");
    }

    /// **No bucket ever reports an exact value**, which is the whole reason they exist: raster plus
    /// frame rate plus duration plus codec identifies a specific file in a specific library.
    #[test]
    fn a_bucket_never_carries_the_number_it_was_built_from() {
        for h in [479, 481, 719, 1079, 2160, 4320] {
            assert!(
                !raster_class(h).contains(char::is_numeric),
                "{h} leaked its height"
            );
        }
    }

    /// The startup boundaries are where the EXPERIENCE changes rather than round numbers, and a
    /// negative interval — a clock that went backwards, or an `ended` with no `requested` — is
    /// `unknown` rather than the fastest bucket, which would silently improve the metric.
    #[test]
    fn a_backwards_clock_is_unknown_and_not_the_fastest_bucket() {
        assert_eq!(startup_class(0), "<1s");
        assert_eq!(startup_class(999), "<1s");
        assert_eq!(startup_class(1_000), "1-3s");
        assert_eq!(startup_class(9_999), "3-10s");
        assert_eq!(startup_class(10_000), "10s+");
        assert_eq!(startup_class(-1), "unknown");
    }

    /// **No `fetch_update` may come back, and only a source grep can say so here.**
    ///
    /// Nightly deprecated that method in favour of `try_update`; `make lint` denies warnings, so
    /// three call sites turned into `error: use of deprecated method` and CI went red on
    /// `67b61515`, `446f64c7` and `69489be7` alike. The repo pins no nightly, so whether a given
    /// checkout SEES the deprecation is a property of when its toolchain was installed — this Mac
    /// was on a June nightly and compiled all three cleanly, which is exactly why the breakage
    /// first appeared on a runner. A reintroduction is therefore invisible to `make check` on the
    /// machine that writes it, and visible only after a push. This grep is that missing signal.
    ///
    /// The needle is assembled at run time so this test does not match its own source.
    #[test]
    fn no_atomic_read_modify_write_uses_the_deprecated_fetch_update() {
        let needle = concat!(".", "fetch_update", "(");
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offences: Vec<String> = Vec::new();
        let mut files = 0usize;
        // Every crate of the workspace: this crate (`src`), the application (`../src`) and each
        // layer crate split out of it. The list read `src` and `ui/src` of the application crate
        // until the media split moved this test, and the layers that had left before then were
        // not in it, so moving a directory had already moved it out of this scan.
        let layers = ["base", "machine", "platform", "gfx", "net", "ui", "plex", "dispatcharr", "telemetry", "data", "appkit", "screens"];
        let roots = [manifest.join("src"), manifest.join("../src")]
            .into_iter()
            .chain(layers.iter().map(|layer| manifest.join("..").join(layer).join("src")));
        for src in roots {
            walk(&src, &mut |path: &std::path::Path, text: &str| {
                files += 1;
                for (n, line) in text.lines().enumerate() {
                    if line.contains(needle) {
                        offences.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
                    }
                }
            });
        }
        assert!(
            files > 300,
            "the walk found only {files} source files — it is not reading the tree"
        );
        assert!(
            offences.is_empty(),
            "`fetch_update` is deprecated on current nightlies and `make lint` denies warnings; \
             write the compare-exchange loop instead (see `requested`):\n{}",
            offences.join("\n")
        );
    }

    fn walk(dir: &std::path::Path, f: &mut impl FnMut(&std::path::Path, &str)) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, f);
            } else if p.extension().is_some_and(|x| x == "rs") {
                if let Ok(t) = std::fs::read_to_string(&p) {
                    f(&p, &t);
                }
            }
        }
    }
}
