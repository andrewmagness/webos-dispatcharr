//! The app's side of the foreground/background lifecycle: the loop's adapter for the player's
//! resume machine ([`PlayerForegroundActuator`]) and the names the loop spells for it.
//!
//! The machine itself (`ForegroundLifecycle`, already a tested `State + Event -> Effects`
//! machine), the foreground driver and the transport-pause contract (`paused`,
//! `viewer_paused`, `set_transport_paused`, ...) describe the PLAYER's transport, so they live in
//! [`plx_media::player::lifecycle`] (module-layer step L12: `player` may not name `app`) and are
//! re-exported here, which keeps every `app` caller spelling the names it always did. Moved out of
//! `app.rs` verbatim in phase 1a (a pure move; `pub(crate)` widening only).

use super::*;

pub(crate) use plx_media::player::lifecycle::{
    clock_for_suspend_now, drive_foreground, park_pending_foreground_flight, paused,
    poll_foreground_flight, poll_foreground_load, set_paused,
    set_transport_paused, transport_target, viewer_paused, FlightLanding, ForegroundActivation,
    ForegroundActuator, ForegroundClock, ForegroundInput, ForegroundLifecycle, ForegroundLoadStart,
    ForegroundLoadStatus,
};

pub(crate) struct PlayerForegroundActuator<'a> {
    /// The native session slot — phase 9's replacement for the `&MainThread` this held, and the
    /// same value every other player call in the loop is threaded (`player::adapter`).
    pub(crate) pa: &'a mut plx_media::player::adapter::PlayerAdapter,
    pub(crate) repause_at: &'a mut i64,
}

/// What the foreground's `Load` does with the Original rollback's answer: the rebase is a flight
/// (`Pending`), or — once it landed and reloaded — the exact attempt to follow.
fn foreground_load_start(
    recovery: plx_media::player::ForegroundOriginalRecovery,
) -> ForegroundLoadStart<plx_media::route::RouteStartAttempt> {
    use plx_media::player::ForegroundOriginalRecovery as Recovery;
    match recovery {
        Recovery::NotOriginal | Recovery::RetryPrepared => ForegroundLoadStart::Failed,
        Recovery::Pending => ForegroundLoadStart::Pending,
        Recovery::Tracking(attempt) => ForegroundLoadStart::Launched(attempt),
        Recovery::Terminal => ForegroundLoadStart::Terminal,
    }
}

impl ForegroundActuator for PlayerForegroundActuator<'_> {
    type Attempt = plx_media::route::RouteStartAttempt;

    fn prepare_resume(&mut self, ps: &mut plx_media::route::PlaybackSession, resume_ns: i64) -> plx_media::player::ResumeStart {
        plx_media::player::begin_resume(ps, resume_ns)
    }

    fn land_prepare(&mut self, ps: &mut plx_media::route::PlaybackSession) -> FlightLanding<plx_media::player::ResumeOutcome> {
        match plx_media::player::land_resume(ps) {
            Some(outcome) => FlightLanding::Landed(outcome),
            // `land_resume` reaped a stale landing first: nothing outstanding means the flight
            // ended (a Back, a newer request) and its ender owns what comes next.
            None if plx_media::route::resume_flight_outstanding() => FlightLanding::Waiting,
            None => FlightLanding::Stale,
        }
    }

    fn land_recovery(&mut self, ps: &mut plx_media::route::PlaybackSession) -> FlightLanding<ForegroundLoadStart<Self::Attempt>> {
        use plx_media::player::RollbackLanding;
        match plx_media::player::land_engineless_rollback(ps, self.pa) {
            RollbackLanding::Waiting => FlightLanding::Waiting,
            RollbackLanding::Stale => FlightLanding::Stale,
            RollbackLanding::Landed(recovery) => FlightLanding::Landed(foreground_load_start(recovery)),
        }
    }

    fn before_load(&mut self, resume_ns: i64, clock: ForegroundClock) {
        if matches!(clock, ForegroundClock::Paused) {
            *self.repause_at = resume_ns;
            set_resume_pend(true);
            plx_media::player::TX.begin_paused_seek();
        }
    }

    fn start_load(&mut self, ps: &mut plx_media::route::PlaybackSession) -> ForegroundLoadStart<Self::Attempt> {
        let first = plx_media::player::start_bufferfeed_tracked(ps, self.pa);
        if matches!(first, plx_media::player::BufferfeedStartOutcome::Failed) {
            // A synchronous failure inside an Original trial has no Engine for player::pump to
            // recover. Take the same explicit rollback edge here and follow its exact HLS Load.
            return foreground_load_start(plx_media::player::recover_failed_foreground_original(ps));
        }
        match first {
            plx_media::player::BufferfeedStartOutcome::AlreadyRunning => {
                ForegroundLoadStart::AlreadyRunning
            }
            plx_media::player::BufferfeedStartOutcome::Launched(attempt) => {
                ForegroundLoadStart::Launched(attempt)
            }
            plx_media::player::BufferfeedStartOutcome::Failed => unreachable!("handled above"),
        }
    }

    fn load_status(&mut self, attempt: Self::Attempt) -> ForegroundLoadStatus<Self::Attempt> {
        match plx_media::route::route_start_status(attempt) {
            plx_media::route::RouteStartStatus::Pending => ForegroundLoadStatus::Pending,
            plx_media::route::RouteStartStatus::Started => ForegroundLoadStatus::Started,
            plx_media::route::RouteStartStatus::Failed => ForegroundLoadStatus::Failed,
            plx_media::route::RouteStartStatus::Superseded(replacement) => {
                ForegroundLoadStatus::Superseded(replacement)
            }
            plx_media::route::RouteStartStatus::Stale => ForegroundLoadStatus::Stale,
        }
    }

    fn after_load(
        &mut self,
        ps: &mut plx_media::route::PlaybackSession,
        attempt: Option<Self::Attempt>,
        clock: ForegroundClock,
        started: bool,
    ) {
        if matches!(clock, ForegroundClock::Paused) {
            if started {
                set_resume_pend(true);
            } else {
                plx_media::player::TX.finish_seek_preroll();
            }
        }
        if !started && attempt.is_some() {
            // `sf_load == 0` publishes Error but intentionally leaves the Engine available for
            // diagnostics. Retire it only if it still owns the observed token: player::pump may
            // already have launched a healthy replacement before foreground polls this result.
            let _ = plx_media::player::suspend_bufferfeed_if_attempt(ps, self.pa, attempt.unwrap());
        }
    }

    fn play_clock(&mut self) -> bool {
        // start_bufferfeed has installed the Initial native-clock hold from TX.paused. Publish
        // Play through the synchronized reducer so that hold and the feed gate reopen together.
        !paused() || set_transport_paused(self.pa, false)
    }
}

#[cfg(test)]
mod root_back_tests {
    //! **BACK at a ROOT hands the screen back to the television, and the app keeps running.**
    //!
    //! [`back_at_root`] is driven for real — it is the app's whole answer to "there is nowhere
    //! further back to go", and the regression to catch is a future edit putting `running = false`,
    //! or a modal question, back where the platform call now goes.
    //!
    //! What NO host test can say is that the television actually shows its launcher and that the
    //! process survives it. That is the port's device half of `tv::home::go_home` — `gohome: SAM accepted`, a
    //! capture of the launcher (on webOS 4 a RIBBON over the still-running app, so no lifecycle
    //! event at all) and `fuser` reporting one pid throughout — and it is why this file's
    //! `home_requests` counter grades the DECISION and never the outcome.
    use super::*;

    /// **The one that matters (issue #16).** The root press asks the platform for its Home screen.
    #[test]
    fn back_at_the_root_shows_the_platform_home() {
        let _g = plx_base::testlock::serial();
        plx_platform::tv::home::release_root_press();
        let before = plx_platform::tv::home::home_requests();
        back_at_root();
        assert_eq!(
            plx_platform::tv::home::home_requests(),
            before + 1,
            "BACK at the root must ask webOS for its Home screen"
        );
        plx_platform::tv::home::release_root_press();
    }

    fn back_input(ms: u32) -> plx_machine::machine::InputEvent<u32> {
        plx_machine::machine::InputEvent {
            at: plx_machine::machine::Tick { ms, dt_us: 16_000 },
            source: plx_machine::machine::Source::Script,
            kind: plx_machine::machine::InputKind::Key {
                key: plx_machine::machine::Key::Back, sym: 0, wcode: 0,
                edge: plx_machine::machine::Edge::Down, at_edge: false,
            },
        }
    }

    /// A real Guide instance owns its root decision and emits exactly the request whose
    /// production performer is `back_at_root`. EntryId/InstanceId prove the root was neither
    /// replaced nor torn down, and the container's root fallback is not entered a second time.
    #[test]
    fn owned_guide_root_back_reaches_platform_home_without_moving_the_root() {
        let _guard = plx_base::testlock::serial();
        plx_platform::tv::home::release_root_press();
        let before = plx_platform::tv::home::home_requests();
        let mut d = plx_ui::dispatch::Dispatcher::<crate::app::bridge::AppHost>::new();
        let mut rig = crate::app::bridge::Bridge::for_test(|| 0);
        crate::app::bridge::nav_root(&mut d, plx_screens::registry::AppArg::Guide);
        crate::app::bridge::frame(&mut d, &mut rig, plx_machine::machine::Tick::default(), vec![]);
        let entry = d.nav.top_page().expect("Guide root").id;
        let instance = d.nav.instance_of(entry).expect("owned Guide body");
        assert_eq!(d.top_screen().unwrap().name(), "guide");

        let (_, report) = crate::app::bridge::frame(&mut d, &mut rig,
            plx_machine::machine::Tick { ms: 16, dt_us: 16_000 }, vec![back_input(16)]);
        assert!(!report.back_at_root, "the Guide answered its own root BACK exactly once");
        let requests = rig.take_reqs();
        assert!(matches!(requests.as_slice(),
            [plx_screens::registry::LoopReq::BackAtRoot]));
        for request in requests {
            crate::app::run::reduce_navigation_request(request, &mut d);
        }
        assert_eq!(plx_platform::tv::home::home_requests(), before + 1);
        assert_eq!(d.nav.top_page().map(|page| page.id), Some(entry));
        assert_eq!(d.nav.instance_of(entry), Some(instance));
        plx_platform::tv::home::release_root_press();
    }
}
