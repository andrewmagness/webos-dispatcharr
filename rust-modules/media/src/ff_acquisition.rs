//! **One acquisition policy, not two independently-optional parameters, and sealed against the
//! bypass that caused it.** `hls_demux_segment` used to take `(ReserveDeadlineState,
//! Option<StallGuard>)`, and every combination of the two compiled — including "no reserve
//! deadline AND no stall guard", which is exactly what `hls_prefetch_same_encoder`'s same-encoder
//! lookahead built (`ReserveDeadlineState::new(None, false), None`, unconditionally). That is how
//! a rung-20000 link collapse to 500 kbps (`pipe_abr_down_collapse`, segment 11, 5.4 MB) ran
//! 86.8 s and froze the picture for ~84s: the lookahead fetched that very segment and never
//! evaluated a terminal-reserve abort at all, because nothing forced it to.
//!
//! Replacing the pair with one required, role-typed argument was the first fix; it was not
//! enough on its own, because `ff` is one module and `SegmentAcquisition::Active { stall: None,
//! .. }` was just as constructible by hand inside `ff` as the two-parameter call it replaced.
//! This module is the second half: `SegmentAcquisition`'s payload lives in a private inner enum
//! (`Policy`) that `ff` cannot name, so the only way `ff` can produce an `Active` acquisition at
//! all is [`SegmentAcquisition::for_cursor`] — which is also the only place the
//! `arm_active_stall_guard` evaluation runs, from inputs it samples itself rather than inputs a
//! caller hands it.
use super::*;
use plx_base::checkpoint::{Checkpoint, Flow};

/// How often an ARMED acquisition re-asks while a transport or retry wait is blocked, at most.
/// The projected playhead boundary may come sooner; an accepted hold has no projection and is
/// seen within one slice. Unarmed runtimes never ask again at all (`next_check: None`).
pub(super) const CHECK_SLICE: std::time::Duration = std::time::Duration::from_millis(100);

/// Where the fetch is. Only `BodyComplete` changes a decision: a response whose every declared
/// byte arrived (or whose unsized body reached a confirmed end) is credited, never abandoned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Opening,
    RetryWaiting,
    Body,
    BodyComplete,
}

/// Why the runtime answered [`Flow::Stop`]. Latched: every later check stops too, so a
/// subsequent AVIO callback cannot resume a fetch this runtime has already abandoned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    Teardown,
    StallAbort,
}

/// **One active fetch's guard, across every leg that can block.** Built only by
/// [`SegmentAcquisition::begin`], so it inherits the role's sealed arming: a candidate or a
/// non-adaptive fetch carries no guard (and therefore never stops except for teardown), an
/// already-held clock or an unknowable/zero starting reserve arms nothing.
///
/// As the transports' [`Checkpoint`] its answer is, in priority order: teardown (the AU lane
/// aborted) stops; a completed body continues unconditionally; the guard's boundary — an
/// accepted hold, a hold epoch that moved since arming, or a playhead that has spent the
/// starting reserve — stops above the floor after asking the main thread to hold the clock, and
/// at the floor asks once, disarms and continues the SAME operation; otherwise continue, asking
/// again by the projected boundary or [`CHECK_SLICE`], whichever is sooner. Reserve-deadline and
/// transport-liveness classification stay with the caller, which composes them into the
/// transport's own deadline exactly as before.
pub(super) struct AcquisitionRuntime {
    aq: *mut AuQueue,
    stall: Option<StallGuard>,
    phase: Phase,
    stop: Option<StopReason>,
}

impl AcquisitionRuntime {

    /// Move to `phase`. `BodyComplete` is terminal: nothing moves a completed body back.
    pub(super) fn enter(&mut self, phase: Phase) {
        if self.phase != Phase::BodyComplete {
            self.phase = phase;
        }
    }

    /// Record body progress. `received` is the end offset of what the transport has RECEIVED
    /// (`AvioState::note_received`), not what FFmpeg has read. A known length is complete once
    /// every declared byte arrived; an unknown length (`size < 0`) is complete only at a
    /// confirmed end (`eof`).
    pub(super) fn note_body(&mut self, size: i64, received: i64, eof: bool) {
        if eof || (size >= 0 && received >= size) {
            self.enter(Phase::BodyComplete);
        }
    }

    /// The abort latched for the enclosing FFmpeg operation (`classify_hls_avio_operation`).
    pub(super) fn stall_aborted(&self) -> bool {
        self.stop == Some(StopReason::StallAbort)
    }

    /// **Settle a leg that ended without success** — a deadline, a transport failure, a
    /// `NotReady`, a stop. The transport's last answer may be stale: a deadline shorter than
    /// [`CHECK_SLICE`] ends a wait without asking again. So this asks AFRESH, in `check()`'s own
    /// priority (teardown, completed body, guard boundary — at the floor that requests the hold
    /// and continues), and only then lets the leg's own classification stand. Returns whether the
    /// runtime has stopped.
    pub(super) fn settle_stopped(&mut self) -> bool {
        Checkpoint::check(self) == Flow::Stop
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn armed(&self) -> bool {
        self.stall.is_some()
    }
}

impl Checkpoint for AcquisitionRuntime {
    fn check(&mut self) -> Flow {
        let flow = self.evaluate();
        #[cfg(any(test, feature = "test-support"))]
        observe::report(self.phase, flow);
        flow
    }
}

impl AcquisitionRuntime {
    fn evaluate(&mut self) -> Flow {
        if self.stop.is_some() {
            return Flow::Stop;
        }
        if unsafe { crate::aq::aq_is_aborted(self.aq) } {
            self.stop = Some(StopReason::Teardown);
            return Flow::Stop;
        }
        if self.phase == Phase::BodyComplete {
            return Flow::Continue { next_check: None };
        }
        let Some(guard) = self.stall else {
            return Flow::Continue { next_check: None };
        };
        if guard.should_abort(false, SHARED.hls_rebuffering.load(Ordering::Acquire)) {
            request_terminal_hold();
            if guard.aborts_fetch() {
                self.stop = Some(StopReason::StallAbort);
                return Flow::Stop;
            }
            self.stall = None;
            return Flow::Continue { next_check: None };
        }
        let slice = std::time::Instant::now() + CHECK_SLICE;
        Flow::Continue {
            next_check: Some(guard.projected_boundary().min(slice)),
        }
    }
}

/// A host-suite seam: a test registers ONE thread and receives every answer an acquisition
/// runtime on that thread gives, with the phase it gave it in — so it can change the world
/// strictly AFTER a given `Continue` instead of sleeping and hoping.
#[cfg(any(test, feature = "test-support"))]
pub(super) mod observe {
    use super::{Flow, Phase};
    use std::sync::mpsc::SyncSender;
    use std::sync::Mutex;
    use std::thread::ThreadId;

    static WATCHED: Mutex<Option<(ThreadId, SyncSender<(Phase, Flow)>)>> = Mutex::new(None);

    pub(in crate::ff) fn watch_this_thread(tx: SyncSender<(Phase, Flow)>) {
        *WATCHED.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((std::thread::current().id(), tx));
    }

    pub(in crate::ff) fn unwatch() {
        *WATCHED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub(super) fn report(phase: Phase, flow: Flow) {
        let watched = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((thread, tx)) = watched.as_ref() {
            if *thread == std::thread::current().id() {
                let _ = tx.try_send((phase, flow));
            }
        }
    }
}
