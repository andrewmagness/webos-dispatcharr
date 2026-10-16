//! Playback orchestration from the app core's side: the scrub/seek/position accessors, the
//! start/exit/finish of a playback, and the performers behind the requests the player page raises
//! ([`player_requests`]). Moved out of `app.rs` verbatim in phase 1a (a pure move; `pub(crate)`
//! widening only).
//!
//! **The HUD's timer, its cursor, the scrub gesture and the repeat gates are no longer here.**
//! Restructure phase 9 moved them into the player's own `Screen` instance
//! (`plx_screens::player`), with the types and the pure predicates in
//! `plx_screens::player::input`; this module re-exports the few names the loop still spells
//! unqualified. What that changed for a reader of the code below: `hud_until()`/`set_hud`/
//! `extend_hud`/`scrub()`/`set_scrub` are gone as free functions, because the values they read and
//! wrote are fields of a screen now (§2.3), and every arm that touches them takes that screen.
//!
//! **And the player's KEY HANDLERS are no longer here either** (phase 12, PX-PLAYER).
//! `key_player_failed`, `key_player_updown`, `key_scrub` and `seed_scrub` are deleted, not moved:
//! `PlayerScreen::step` had been answering the same keys through the dispatcher since the page
//! began reporting `FocusSource::Engine`, and these four went on writing that page's `hud`/`scrub`
//! from the loop beside it. What is left of the player here is the half a screen may not do —
//! [`commit_seek`], [`exit_player`], [`activate_player_row`], the transport toggle — each reached
//! through a `PlayerReq`.

use super::*;
// The list is what the LOOP still spells, and phase 12 (PX-PLAYER) is why it is short: the scrub
// gesture's constants and its two pure policies (`scrub_press`, `failed_key_action`) are read by
// `screens::player` alone now, so re-exporting them here would be this module claiming a
// vocabulary it no longer uses.
// The modal repeat cadence is `screens::registry`'s (phase 10 merge): the item context menu is a
// surface of its own family and needs the same gate, so it is shared vocabulary rather than the
// player's. `App::modal_repeat` still reaches it through this re-export.
pub(crate) use plx_screens::registry::RepeatGate;

#[inline]
pub(crate) fn dur() -> i64 {
    plx_media::player::duration_ns()
}
#[inline]
pub(crate) fn playpos() -> i64 {
    plx_media::player::playpos_ns()
}
#[inline]
pub(crate) fn request_seek(x: i64) {
    plx_media::player::request_seek(x)
}
#[inline]
pub(crate) fn is_started() -> bool {
    plx_media::player::is_started()
}

// ---- the route vocabulary, and the pure questions asked ABOUT a route -------------------------
//
// These are pure functions of a `Route` that read and write no app state, which is what lets
// `route_tests` at the bottom of this file grade them — and grading them is the point, because they
// decide things that have shipped wrong (the teardown rule below, twice), and a `Route` that only
// exists inside the run loop's body is a decision no host test can reach. The loop still owns every
// VALUE — `route` is a local, the trail is a local.

/// **How often a live channel may be reconnected on its own.** A channel's stream ending is not
/// the end of the programme: the connection dropped, or the server restarted its upstream, and the
/// player tunes the same channel again. A channel that keeps ending (its source is down) would
/// loop forever, so at most [`ReconnectBudget::MAX`] reconnects are allowed in any
/// [`ReconnectBudget::WINDOW_MS`]; past that the playback ends as a film does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReconnectBudget {
    /// The ticks of the reconnects still inside the window, oldest first.
    recent: std::collections::VecDeque<u32>,
}

impl ReconnectBudget {
    pub(crate) const MAX: usize = 3;
    pub(crate) const WINDOW_MS: u32 = 60_000;

    /// Spend one reconnect at tick `now`, if the window has one left.
    pub(crate) fn allow(&mut self, now: u32) -> bool {
        while self.recent.front().is_some_and(|&t| now.wrapping_sub(t) >= Self::WINDOW_MS) {
            self.recent.pop_front();
        }
        if self.recent.len() >= Self::MAX {
            return false;
        }
        self.recent.push_back(now);
        true
    }
}

#[cfg(test)]
mod reconnect_budget_tests {
    use super::ReconnectBudget;

    #[test]
    fn a_few_reconnects_a_minute_and_no_more() {
        let mut b = ReconnectBudget::default();
        assert!(b.allow(0) && b.allow(1_000) && b.allow(2_000));
        assert!(!b.allow(3_000), "a fourth inside the minute: the source is down");
        assert!(b.allow(60_000), "the first has aged out");
        assert!(!b.allow(60_500));
    }

    #[test]
    fn the_window_survives_the_tick_wrap() {
        let mut b = ReconnectBudget::default();
        let t = u32::MAX - 500;
        assert!(b.allow(t) && b.allow(t.wrapping_add(100)) && b.allow(t.wrapping_add(200)));
        assert!(!b.allow(t.wrapping_add(1_000)));
        assert!(b.allow(t.wrapping_add(ReconnectBudget::WINDOW_MS)));
    }
}

/// **Change channel while watching** (`PlayerReq::Tune`): stop the stream being watched, which also
/// frees the server's stream slot before the next one is asked for, then tune and start the new
/// channel on the player page that is already up. The page's return target is untouched, so BACK
/// still goes to the Guide.
pub(super) fn tune_channel(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    tune: plx_media::route::LiveTune,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) {
    plx_media::player::stop_bufferfeed(ps, pa);
    plx_media::player::mark_tune();
    let uuid = tune.channel().map(|c| c.uuid.clone()).unwrap_or_default();
    let tune = super::dispatcharr::with_known_codecs(tune);
    if plx_media::route::request_live(ps, tune, plx_dispatcharr::dispatcharr::api::unix_now()) {
        super::dispatcharr::remember_last(&uuid);
        start_playback(ps, pa, Origin::Here, None, pages, bridge);
    } else {
        log("live: the channel change was refused");
    }
}

/// **Where a session that is STARTING returns to** — the whole of what `app::nav::Origin` was,
/// once the origin became an `EntryId` rather than a described page.
///
/// It carried an `Origin::From(Node)` — a whole history entry, re-derived from the live stores at
/// every launch site by `origin_here` — because a `Route` names a KIND of page and BACK has to
/// land on the RIGHT one. The container already holds the right one: it is the entry on top when
/// Play was pressed. A start while the player is already up (a channel change) keeps the page it
/// came from because [`enter_player`] leaves an up player alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Origin {
    /// A fresh launch: Stop/BACK/EOS lands on the page that is on top right now.
    Here,
}

/// **Put the player on the page stack, and record where BACK, Stop and EOS land** (§5.1).
///
/// The origin is the entry that is on top RIGHT NOW — the page the user pressed Play on — seeded
/// for the mount the push performs and read back off the mounted screen at the exit. It replaces
/// `App.play_from: Node` plus `enter_node` plus `Trail::ensure`, which between them said "re-derive
/// a page from a remembered description, then make the history agree".
///
/// **An auto-advance is not a navigation, and this is where that is decided.** `NavOp::Push`
/// always MINTS an entry — it has no reuse rule (`NavStack::apply`) — so re-entering with the
/// player already on top would stack a SECOND player over the first, mount a fresh screen that
/// consumed no seed, and land the exit on Home from the second episode of every chain. The page
/// the user is standing on has not changed; only the media has.
///
/// Its own function so that the page-level half of the ritual is reachable without an engine —
/// `player_return_tests` drives exactly this, over a real container.
pub(crate) fn enter_player(
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
    from: Origin,
    ret: Option<plx_ui::screen::ReturnState<u32, plx_screens::registry::PageMemory>>,
) {
    // …or already on its way: a Play inside the push's own dip-out finds the page it pressed on
    // still the committed top, and re-seeding from it would leave a seed no mount ever spends
    // (the pending push is inert against its twin, `NavStack::is_inert`) for the next player
    // mount to pick up as its origin.
    let already_up = pages
        .nav
        .top_page()
        .map(|e| matches!(e.arg, AppArg::Player))
        .unwrap_or(false)
        || matches!(pages.nav.tabs.stack.pending_dest(), Some(AppArg::Player));
    if already_up {
        return;
    }
    if matches!(from, Origin::Here) {
        if let Some(entry) = pages.nav.top_page() {
            bridge.seed_player_origin(plx_screens::player::Origin {
                entry: entry.id,
                instance: entry.inst.as_ref().map(|i| i.id),
            });
        }
    }
    match ret {
        Some(ret) => super::bridge::nav_push_with_return(pages, AppArg::Player, ret),
        None => super::bridge::nav_push(pages, AppArg::Player),
    }
}

/// The ONE start-playback ritual (detail OK, home episode OK, and the dispatcharrtv-autoplay/
/// -detailplay/-play dev triggers all share it): arm the resume point BEFORE the first
/// Load (direct-play av_seek / transcode &offset restart), start the engine, record the
/// Stop/BACK/EOS return target, reset the HUD focus cursor, and show the HUD. A missed step
/// here used to silently fork behavior between the interactive and headless paths.
pub(crate) fn start_playback(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    from: Origin,
    ret: Option<plx_ui::screen::ReturnState<u32, plx_screens::registry::PageMemory>>,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) -> bool {
    start_playback_with(ps, pa, from, ret, pages, bridge, &mut LivePlaybackResources)
}

/// Resource effects below launch policy. Implementations receive no navigation, origin, or
/// return state: acceptance cannot invent where a session returns to.
pub(super) trait PlaybackResources {
    fn prepare_start(&mut self, ps: &mut plx_media::route::PlaybackSession,
        pa: &mut plx_media::player::adapter::PlayerAdapter) -> bool;
}

pub(super) struct LivePlaybackResources;

impl PlaybackResources for LivePlaybackResources {
    fn prepare_start(&mut self, ps: &mut plx_media::route::PlaybackSession,
        pa: &mut plx_media::player::adapter::PlayerAdapter) -> bool {
        plx_media::player::start_bufferfeed(ps, pa)
    }
}

/// Shared start policy after resource preparation: gate the page push, forward the caller's
/// origin and return state, and reset/seed the HUD.
pub(super) fn start_playback_with<R: PlaybackResources>(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    from: Origin,
    ret: Option<plx_ui::screen::ReturnState<u32, plx_screens::registry::PageMemory>>,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
    resources: &mut R,
) -> bool {
    let entering = resources.prepare_start(ps, pa);
    if entering {
        enter_player(pages, bridge, from, ret);
    }
    set_paused(false);
    entering
}

/// Leaving playback (Stop / BACK / EOS / Info's jump-to-detail): retire every in-player panel.
///
/// **Three of the four things this used to do are gone, and their absence is the phase.** The four
/// panels were module `static mut`s that the route flip merely stopped DRAWING, so each had to be
/// told by hand to forget it was open (the EOS path once forgot the menu); they are entries on the
/// player page's own `ModalStack` now, and unmounting the page unmounts them with it. The Up Next
/// countdown was a pair of statics for the same reason and is a field of the instance. What is
/// left is the one panel that is NOT the player's — the diagnostics read-out (phase 10) — and the
/// stack dismissal, which is here rather than left to the page's unmount because a FAILED playback
/// keeps its `…` popover up over the read-out and BACK must take that panel down first.
pub(crate) fn close_player_overlays(_pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>) {
    crate::app::diagnostics::close(); // a diagnostics panel must not survive into the next session
}

pub(crate) fn player_requests(
    repair: &mut plx_media::player::machine::RepairAttempt,
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    reqs: Vec<plx_screens::registry::PlayerReq>,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) {
    use plx_screens::registry::PlayerReq;
    for req in reqs {
        match req {
            PlayerReq::Tune(tune) => tune_channel(ps, pa, tune, pages, bridge),
            // *Try again* tunes the channel again.
            PlayerReq::RetryPlayback => {
                if let Some(tune) = plx_media::route::live_tune(ps).cloned() {
                    log("live: try again");
                    tune_channel(ps, pa, tune, pages, bridge);
                }
            }
            PlayerReq::RepairSandbox => {
                if ps.jail_load_blocked {
                    pa.repair_sandbox(repair, plx_platform::tv::sandbox::blocks_native_video());
                    ps.repair_status = repair.state();
                    plx_machine::idle::invalidate();
                }
            }
            PlayerReq::Transport(play) => {
                set_transport_paused(pa, super::lifecycle::transport_target(play, super::lifecycle::viewer_paused()));
            }
            // The STOP key, a BACK with nothing else open and the failure read-out's own BACK
            // escape all reach the same ritual.
            PlayerReq::Exit => {
                exit_player(ps, pa, pages);
            }
        }
    }
}

// (`reveal_played_episode` stood here. It answered one question — "is the page this session was
// launched from the SHOW the played episode belongs to?" — and its only use was choosing between
// two ways of writing the same route: `*route = Route::Detail` or `enter_node(play_from)`. Both
// spellings are retired with D1;
// are `NavOp::PopTo(origin.entry)` now, so the question has no consumer: the REVEAL itself was
// always `content::restore_played_entry`'s, which addresses the uncovered entry and delivers
// `AppMsg::DetailRestore`.)

/// Shared return decision after playback ends; resource teardown belongs to `exit_player`: back
/// to the entry the player was launched from, or — with no origin recorded — to the floor of the
/// stack (the Guide), which `nav_pop_to` also falls back to for an origin that is gone.
pub(super) fn return_from_player(
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
) {
    let origin = super::bridge::player(pages).and_then(|p| p.origin);
    let floor = pages.nav.tabs.stack.entries.first().map(|e| e.id);
    if let Some(entry) = origin.map(|o| o.entry).or(floor) {
        super::bridge::nav_pop_to(pages, entry);
    }
}

/// The ONE leave-playback ritual (Stop key, BACK, EOS): close the overlays, stop the
/// engine and put the page the session was LAUNCHED FROM back on screen.
///
/// The return target is [`plx_screens::player::Origin`]'s `entry` — the container entry that was on top when Play was
/// pressed, seeded at the player's own mount by [`enter_player`] and read back off the mounted
/// screen here. Re-entry is `NavOp::PopTo(entry)` (`bridge::nav_pop_to`), the same container op a
/// BACK off a stacking page performs, so a player exit cannot reach a page in a way nothing else
/// does. There is no re-derivation and no second history to reconcile: `App.play_from: Node` plus
/// `enter_node` plus `Trail::ensure` are all deleted (D1).
pub(crate) fn exit_player(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
) {
    plx_media::route::cancel_play(ps); // BACK during a load: supersede, drop the landing
    close_player_overlays(pages);
    plx_media::player::stop_bufferfeed(ps, pa);
    // `stop_bufferfeed` reports/clears a real engine through `report::ended`, but a refusal or a
    // BACK during resolve has no engine for teardown to take. The exit ritual still ends that
    // attempt, so retire its in-memory trace here as the common backstop.
    plx_media::player::report::clear_error_trace();
    // The jail pre-flight refusal (also no Engine to teardown) is already retired above: it
    // lives on `ps.jail_load_blocked`, and `cancel_play` at the top of this function clears it
    // via `clear_play_verdict` the same way it clears a `/decision` refusal — see that function's
    // doc for why a verdict left standing described the item the user walked away from.
    // **The return is a `PopTo` of the ORIGIN ENTRY** (§5.1) — the page that was on top when the
    // push was asked for, captured at the player's own mount and read back off the instance. It
    // replaces `App.play_from: Node` plus `enter_node` plus `Trail::ensure`, which between them
    // said "re-derive a page from a remembered description, then make the history agree". An
    // entry needs no re-deriving and there is no second history to reconcile. A stale entry falls
    // through `nav_pop_to`'s Home floor; a live Content entry with no identity takes the explicit
    // Home floor in `return_from_player`, preserving `return_page`'s old anti-strand contract.
    return_from_player(pages);
}

/// The episode is OVER — drained to EOS, or the user skipped a `final` credits marker.
/// Starts the queued episode when the show has one, else leaves the player exactly as
/// `exit_player` would. There is no interstitial: "always the next episode" — except under the
/// Next episode preference `Off`, which leaves the player here like a film.
///
/// Returns whether playback was handed off to Up Next (`true`) or the player was left (`false`).
/// `exit_player`'s `PopTo` only PARKS the navigation (`Dispatcher::request`, applied at the next
/// commit), so `app.route()` still reads `Player` for the rest of this frame; a caller that needs
/// to tell a real exit from an Up Next handoff — `dev::scenarios::maybe_replay_after_eos` is the
/// one that does — must use this return value rather than the route.
pub(crate) fn finish_playback(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) -> bool {
    let _ = bridge;
    exit_player(ps, pa, pages);
    false
}

// (`key_player_failed` stood here — the arm `app/run.rs` reached for a key arriving while a
// terminal read-out owned the frame: OK opened the quality ladder, BACK closed the `…` popover if
// one was up and otherwise left the playback, everything else was swallowed. Restructure phase 12
// (PX-PLAYER) retired it with the rest of the loop's player input path: `PlayerScreen::handle_key`
// asks `player_hud::transport_hidden` first and dispatches the SAME pure policy
// (`screens::player::input::failed_key_action`) into `PlayerReq::OpenOverlay`/`PlayerReq::Exit`.
// The popover-first half of its BACK is the container's by construction — an open `…` is a surface
// and answers the key before the page beneath it ever sees one.)

/// (`overlay_swallows_key`, `key_track_menu`, `key_more_menu`, `key_info_panel`,
/// `commit_info_panel` and `key_chapters` stood here. Restructure phase 9 made the four panels
/// SURFACES on the player page's own `ModalStack`, so each owns its input outright:
/// `screens::player::overlay::PlayerOverlayScreen::key` is the one ladder they now share, and the
/// predicate's `key` term — a transport press must reach the toggle and leave the panel up — is
/// `OverlayKind::swallows_transport` plus the `PlayerReq::Transport` forward that replaces the
/// fall-through a surface cannot perform. The decisions those arms used to make with the player
/// adapter, the route and the trail in scope arrive back here as `PlayerReq`, drained by
/// [`player_requests`].)

// (`key_player_updown` stood here — UP/DOWN walking the HUD's ring, or spending the press on
// REVEALING a hidden transport. Ported to `PlayerScreen::key_updown` in phase 9 and DELETED with
// its caller in phase 12 (PX-PLAYER): the loop kept calling this on the screen's own `hud`/`scrub`
// while the screen answered the same key through the dispatcher.)

/// PAUSE — the dedicated transport key, which only ever pauses (PLAY is its other half).
pub(crate) fn key_pause(
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    now: u32,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
) {
    if super::bridge::player(pages).is_some() && !paused() {
        if set_transport_paused(pa, true) {
            plx_telemetry::diag::event(plx_telemetry::diag::schema::DiagEvent::FeatureUsed {
                feature: plx_telemetry::diag::schema::Feature::Pause,
            });
        }
    }
    let _ = now;
}

/// PLAY — on the player it un-pauses. Off it there is nothing to start: a channel is tuned from
/// the Guide, and a parked one is tuned again by the foreground.
pub(crate) fn key_play(
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
) {
    if super::bridge::player(pages).is_some() && paused() {
        set_transport_paused(pa, false);
    }
}

// (`key_scrub` and `seed_scrub` stood here — the LEFT/RIGHT ladder for the player route and the
// in-flight-target seed a fresh gesture starts from. Both are `PlayerScreen`'s since phase 12
// (PX-PLAYER): `key_scrub_fresh` runs the same `ScrubPress` dispatch, and the seed is
// `player::intended_pos_ns` read at the same moment for the same reason — while a prior commit's
// seek is still landing, `playpos()` reports the PRE-seek spot, so a quick re-press seeded off it
// would jump back to where the last one started.
//
// The rules those two docs carried are not lost, and they are worth restating where the behaviour
// now lives: a press that finds the HUD HIDDEN is spent RAISING it and moves nothing (the
// transport sits on a 4.5 s timer over full-screen video, so "where am I" and "take me back ten
// seconds" are two intentions the remote cannot tell apart, and reading every LEFT as the second
// cost a viewer their place); and a HOLD is not a tap, so the reveal still arms the gesture and
// the ordinary continuous scrub grows out of it as the repeats arrive. `ScrubPress` and
// `screens::player::input`'s own `a_hidden_hud_spends_the_press_on_itself` are where both are
// pinned.)

/// **Where playback returns to.**
///
/// The origin is an [`Origin`]: the `EntryId` (plus instance) of the page that was on top when
/// Play was pressed, seeded at the player's own mount and read back off the mounted screen. There
/// is nothing to re-derive and no second history to reconcile, so these tests grade the
/// production page push and shared return decision over retained entries. They do not execute
/// native teardown.
#[cfg(test)]
mod player_return_tests {
    use super::super::bridge::{self, AppHost, Bridge};
    use super::{enter_player, return_from_player, Origin};
    use plx_screens::registry::AppArg;
    use plx_ui::dispatch::Dispatcher;
    use plx_ui::fixture::tick;
    use plx_ui::screen::ScreenArg;

    /// The container, driven by the app's own frame — no `App`, no SDL, no engine.
    struct Pages {
        d: Dispatcher<AppHost>,
        rig: Bridge,
        t: u32,
    }

    impl Pages {
        fn new() -> Self {
            Self { d: Dispatcher::<AppHost>::new(), rig: Bridge::for_test(|| 0), t: 0 }
        }

        /// Enough frames for a `PageDip` to reach its floor, apply the op and ramp back up.
        fn settle(&mut self) {
            for _ in 0..24 {
                self.t += 16;
                bridge::frame(&mut self.d, &mut self.rig, tick(self.t), vec![]);
            }
        }

        fn stand_on(&mut self, arg: AppArg) -> &mut Self {
            bridge::show_page(&mut self.d, arg);
            self.settle();
            self
        }

        fn top(&self) -> AppArg {
            self.d.top_arg().cloned().expect("a page is on top")
        }

        fn top_entry(&self) -> plx_machine::machine::EntryId {
            self.d.nav.top_page().expect("a page is on top").id
        }

        /// **Press Play** — production's own page-level ritual, called directly.
        fn play(&mut self, from: Origin) {
            enter_player(&mut self.d, &mut self.rig, from, None);
            self.settle();
        }

        /// **Stop / BACK / EOS** at the shared production return boundary. `exit_player` calls
        /// this same decision after its native teardown.
        fn back_out(&mut self) {
            return_from_player(&mut self.d);
            self.settle();
        }
    }

    /// The rule, over every page playback can be started from: **you come back to the page you
    /// were standing on**, as the same entry rather than a second copy of that kind of page.
    #[test]
    fn a_session_returns_to_the_page_it_was_launched_from() {
        let _serial = plx_base::testlock::serial();
        for path in [vec![AppArg::Guide], vec![AppArg::Guide, AppArg::Server]] {
            let mut p = Pages::new();
            for page in &path {
                p.stand_on(page.clone());
            }
            let launched_from = path.last().unwrap().clone();
            let before = p.top_entry();
            p.play(Origin::Here);
            assert!(matches!(p.top(), AppArg::Player), "the player is the page that was pushed");
            p.back_out();
            assert!(
                p.top().same_instance(&launched_from),
                "a session launched from {:?} came back to {:?}",
                launched_from.id(),
                p.top().id(),
            );
            assert_eq!(p.top_entry(), before, "…and to the SAME entry");
        }
    }

    /// **A channel change must not rewrite the return page.** It starts a NEW stream while the
    /// player is already up: the user chose nothing, and the page on screen is the player itself.
    /// `enter_player` leaves an up player alone, so the chain keeps pointing at the page they came
    /// from; a second `NavOp::Push` would MINT a second player entry and land the exit on the
    /// wrong page.
    #[test]
    fn a_change_inside_the_player_keeps_the_page_the_user_came_from() {
        let _serial = plx_base::testlock::serial();
        let mut p = Pages::new();
        p.stand_on(AppArg::Guide);
        let guide = p.top_entry();
        p.play(Origin::Here);
        let player = p.top_entry();
        for _ in 0..4 {
            p.play(Origin::Here);
            assert_eq!(p.top_entry(), player, "a change inside the player changes the media, not the page");
        }
        p.back_out();
        assert_eq!(p.top_entry(), guide, "four changes later, still the Guide");
    }

    /// **The push is the product's dip, and the player is asked for twice inside it** — once by
    /// the Play press, once by a second Play before the floor. The page the user pressed on is
    /// still the committed top for that whole window, so the second ask must neither prepare a
    /// fresh player that has lost the origin nor re-seed one that no mount will ever spend.
    #[test]
    fn a_second_play_inside_the_push_keeps_the_origin_and_leaves_no_seed() {
        let _serial = plx_base::testlock::serial();
        let mut p = Pages::new();
        p.d.nav.tabs.stack.transition = Box::new(plx_ui::containers::transition::PageDip::new());
        p.stand_on(AppArg::Guide);
        let page = p.top_entry();
        enter_player(&mut p.d, &mut p.rig, Origin::Here, None);
        p.t += 16;
        bridge::frame(&mut p.d, &mut p.rig, tick(p.t), vec![]);
        assert!(
            p.d.nav.tabs.stack.pending_target_mut().is_some_and(|e| e.inst.is_some()),
            "premise: the dip-out has prepared the player",
        );
        p.play(Origin::Here);
        assert!(matches!(p.top(), AppArg::Player));
        assert_eq!(p.d.nav.tabs.stack.depth(), 2, "one player, not two");
        p.back_out();
        assert_eq!(p.top_entry(), page, "the session came back to the Guide");

        bridge::show_page(&mut p.d, AppArg::Player);
        p.settle();
        assert_eq!(
            bridge::player(&p.d).and_then(|player| player.origin).map(|o| o.entry),
            None,
            "an unseeded player mount inherits no origin from the earlier session",
        );
    }

    /// **An origin that is no longer on the stack falls back to the floor of the stack** — the
    /// anti-strand rule `bridge::nav_pop_to` keeps over a real container. Reachable: a sign-out
    /// roots Setup and retires every entry there is, so a session that outlives one holds an
    /// `EntryId` that names nothing.
    #[test]
    fn an_origin_that_is_gone_falls_back_to_the_floor() {
        let _serial = plx_base::testlock::serial();
        let mut p = Pages::new();
        p.stand_on(AppArg::Guide).stand_on(AppArg::Server);
        p.play(Origin::Here);
        let stale = bridge::player(&p.d)
            .and_then(|player| player.origin)
            .expect("the mount consumed the seed");
        assert!(p.d.nav.entry(stale.entry).is_some(), "…and it named a real entry while the page was there");

        bridge::nav_root(&mut p.d, AppArg::Guide);
        p.settle();
        assert!(p.d.nav.entry(stale.entry).is_none(), "the origin entry is gone");

        p.rig.seed_player_origin(stale);
        bridge::nav_push(&mut p.d, AppArg::Player);
        p.settle();
        assert_eq!(
            bridge::player(&p.d).and_then(|player| player.origin).map(|o| o.entry),
            Some(stale.entry),
            "the stale origin really is what the exit will be asked about",
        );

        p.back_out();
        assert!(
            matches!(p.top(), AppArg::Guide),
            "a stranded return lands on the floor rather than on nothing: {:?}",
            p.top().id(),
        );
    }
}
