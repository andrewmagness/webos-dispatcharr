//! **The container requests an owned PAGE raises, performed** — the Guide's Play and the player
//! overlays' requests. Each names an action the screen may not take itself (§2.1); this module
//! turns it into a call the loop owns. Entries own their screens and their return state, so a play
//! carries the `ReturnState` the asking screen froze on ITS press frame rather than one re-read at
//! the commit.

use super::*;
use super::run::Frame;
use plx_screens::registry::{ContentReq, PageMemory};
use plx_machine::machine::{InputOwner, MachineId};
use plx_ui::screen::ReturnState;

struct HeldFeature {
    play: plx_screens::registry::PlayIntent,
    resume_ns: i64,
    /// The page navigation's own captured spot (BACK returns there).
    ret: Option<ReturnState<u32, PageMemory>>,
}

std::thread_local! {
    static HELD_FEATURE: std::cell::RefCell<Option<HeldFeature>> = const { std::cell::RefCell::new(None) };
}

/// Start abandoning a preview that still holds the engine. It must run before checking
/// [`plx_media::player::preview::occupies`] — see [`hold_feature`]'s doc for why a
/// still-abandoning preview needs a hold rather than a synchronous play.
pub(super) fn halt_preview_now(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
) {
    plx_media::player::preview::halt(ps, pa);
}

/// Queue a play for [`drain_held_feature`] to perform once a still-abandoning preview's Load
/// thread has actually released the engine. `halt_preview`/`halt_preview_now` only STARTS
/// abandonment — the preview may still hold `pa.engine()` installed for a moment after, and
/// starting a second Load against that installed engine hits the double-start conflict guard
/// and refuses instead of queuing. A Play that can reach a still-occupied preview must hold
/// through here rather than call `start_playback` directly.
pub(super) fn hold_feature(
    play: plx_screens::registry::PlayIntent,
    resume_ns: i64,
    ret: Option<ReturnState<u32, PageMemory>>,
) {
    HELD_FEATURE.with(|slot| {
        *slot.borrow_mut() = Some(HeldFeature {
            play,
            resume_ns,
            ret,
        });
    });
}

pub(super) fn request_play_intent(
    session: &mut plx_media::route::PlaybackSession,
    meta: &mut plx_data::stores::metadata::MetadataStore,
    play: &plx_screens::registry::PlayIntent,
) -> bool {
    match play {
        plx_screens::registry::PlayIntent::Live(tune) => {
            // A channel has no library item behind it: nothing describes it to the Info card.
            meta.run(plx_data::stores::metadata::MetadataCmd::SetNowPlaying(None));
            let uuid = tune.channel().map(|c| c.uuid.clone()).unwrap_or_default();
            let tune = super::dispatcharr::with_previous(super::dispatcharr::with_known_codecs(tune.clone()));
            let ok = plx_media::route::request_live(session, tune,
                plx_dispatcharr::dispatcharr::api::unix_now());
            if ok {
                super::dispatcharr::remember_last(&uuid);
            }
            ok
        }
    }
}

/// What a Play must do about the engine the session already holds before asking for its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlayClearance {
    /// Nothing is in the way.
    Clear,
    /// A preview still occupies the engine; queue the play ([`hold_feature`]).
    Hold,
    /// A live engine nothing owns: stop it, then play.
    RetireOrphan,
}

/// A live engine that is neither a preview nor the player page's: every transport key and the
/// EOS teardown are gated on the player page, so nothing will ever stop it, and the next Load
/// meets it as "a live Engine that belongs to another Load attempt" and fails.
pub(super) fn off_route_orphan(live: bool, preview: bool, player_mounted: bool) -> bool {
    live && !preview && !player_mounted
}

/// Asked after [`halt_preview_now`] has already started any preview's abandonment.
pub(super) fn play_clearance(occupies: bool, live: bool, preview: bool, player_mounted: bool) -> PlayClearance {
    if occupies {
        PlayClearance::Hold
    } else if off_route_orphan(live, preview, player_mounted) {
        PlayClearance::RetireOrphan
    } else {
        PlayClearance::Clear
    }
}

/// Hand the engine back before a Play: halt a preview, then answer [`play_clearance`]. `false`
/// means hold the play; an orphan is stopped here, so `true` always means the slot is free.
/// Every Play reachable from a page goes through this one door.
pub(super) fn clear_engine_for_play(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    player_mounted: bool,
) -> bool {
    halt_preview_now(ps, pa);
    match play_clearance(
        plx_media::player::preview::occupies(),
        pa.is_live(),
        plx_media::route::is_preview(ps),
        player_mounted,
    ) {
        PlayClearance::Hold => false,
        PlayClearance::Clear => true,
        PlayClearance::RetireOrphan => {
            log("play: stopping an off-route engine nothing owns before the new Load");
            plx_media::player::stop_bufferfeed(ps, pa);
            true
        }
    }
}

fn drain_held_feature(app: &mut App) {
    if HELD_FEATURE.with(|slot| slot.borrow().is_none()) {
        return;
    }
    let mounted = super::bridge::player(&app.pages).is_some();
    if !clear_engine_for_play(&mut app.player.session, &mut app.adapters.player, mounted) {
        return;
    }
    let held = HELD_FEATURE.with(|slot| slot.borrow_mut().take());
    let Some(held) = held else { return };
    if !request_play_intent(&mut app.player.session, app.bridge.metadata_mut(), &held.play) {
        return;
    }
    start_playback(
        &mut app.player.session,
        &mut app.adapters.player,
        held.resume_ns,
        super::playback::Origin::Here,
        held.ret,
        &mut app.pages,
        &mut app.bridge,
    );
}

#[cfg(test)]
mod play_clearance_tests {
    use super::{off_route_orphan, play_clearance, PlayClearance};

    /// Field report, 0.7.0 prep: a trailer's engine outlived its end on the detail page, and
    /// the film's Play then failed with `start_bufferfeed: live Engine belongs to another Load
    /// attempt`. A Play from a page must pre-empt an engine nothing owns, not fail against it.
    #[test]
    fn a_play_pre_empts_a_live_engine_that_no_page_owns() {
        assert_eq!(play_clearance(false, true, false, false), PlayClearance::RetireOrphan);
        assert!(off_route_orphan(true, false, false));
    }

    #[test]
    fn a_play_holds_for_a_preview_and_leaves_the_player_engine_alone() {
        // A preview still abandoning its Load: hold, never stop it from here.
        assert_eq!(play_clearance(true, true, true, false), PlayClearance::Hold);
        // The player page owns its engine; a page over it is not a reason to stop it.
        assert_eq!(play_clearance(false, true, false, true), PlayClearance::Clear);
        assert!(!off_route_orphan(true, false, true));
        // A live preview is the preview's, whatever the machine says.
        assert!(!off_route_orphan(true, true, false));
        assert_eq!(play_clearance(false, false, false, false), PlayClearance::Clear);
    }
}

pub(crate) fn content_requests(app: &mut App, _fr: &Frame) {
    drain_held_feature(app);
    // What the player decides reaches the loop the same way every other owned screen's decision
    // does — as requests, drained here, after the dispatcher and before the frame's own arms
    // (`playback::player_requests`).
    let player_reqs = app.bridge.take_player_reqs();
    if !player_reqs.is_empty() {
        super::playback::player_requests(&mut app.player.repair, &mut app.player.session,
            &mut app.adapters.player,
            player_reqs,
            &mut app.pages,
            &mut app.bridge,
        );
    }
    for (source, request, ret) in app.bridge.take_content_reqs() {
        let MachineId::Instance(instance) = source else { continue };
        let Some(entry) = app.pages.nav.entry_of_instance(instance) else { continue };
        // A delayed effect cannot act for an instance that has already been covered.
        if app.pages.nav.input_owner() != Some(InputOwner::Entry(entry)) { continue; }
        match request {
            ContentReq::Play { play, resume_ns } => {
                let mounted = bridge::player(&app.pages).is_some();
                if !clear_engine_for_play(&mut app.player.session, &mut app.adapters.player, mounted) {
                    hold_feature(play, resume_ns, Some(ret));
                    continue;
                }
                // The PAGE decided what to play; the LOOP performs the request, because starting
                // playback takes the playback session's `&mut` and a screen is only ever shown the
                // frame's publication (§2.2). A refusal leaves the page exactly where it was.
                if !request_play_intent(&mut app.player.session, app.bridge.metadata_mut(), &play) { continue; }
                // The page's own `ReturnState` rides the push, so BACK out of the playback finds
                // the spot the Play was pressed from.
                start_playback(&mut app.player.session, &mut app.adapters.player, resume_ns,
                    super::playback::Origin::Here,
                    Some(ret), &mut app.pages, &mut app.bridge);
            }
        }
    }
}
