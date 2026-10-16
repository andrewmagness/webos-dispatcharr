//! **The container requests an owned PAGE raises, performed** — the Guide's Play and the player's
//! requests. Each names an action the screen may not take itself (§2.1); this module turns it into
//! a call the loop owns. Entries own their screens and their return state, so a play carries the
//! `ReturnState` the asking screen froze on ITS press frame rather than one re-read at the commit.

use super::*;
use super::run::Frame;
use plx_machine::machine::{InputOwner, MachineId};
use plx_screens::registry::ContentReq;

pub(super) fn request_play_intent(
    session: &mut plx_media::route::PlaybackSession,
    play: &plx_screens::registry::PlayIntent,
) -> bool {
    match play {
        plx_screens::registry::PlayIntent::Live(tune) => {
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

/// A live engine that is not the player page's: every transport key and the EOS teardown are gated
/// on the player page, so nothing will ever stop it, and the next Load meets it as "a live Engine
/// that belongs to another Load attempt" and fails.
pub(super) fn off_route_orphan(live: bool, player_mounted: bool) -> bool {
    live && !player_mounted
}

/// Hand the engine back before a Play: an engine nothing owns is stopped here, so the Load that
/// follows always finds the slot free. Every Play reachable from a page goes through this one door.
pub(super) fn clear_engine_for_play(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    player_mounted: bool,
) {
    if off_route_orphan(pa.is_live(), player_mounted) {
        log("play: stopping an off-route engine nothing owns before the new Load");
        plx_media::player::stop_bufferfeed(ps, pa);
    }
}

#[cfg(test)]
mod play_clearance_tests {
    use super::off_route_orphan;

    /// A Play from a page must pre-empt an engine nothing owns, not fail against it; the player
    /// page's own engine is not an orphan.
    #[test]
    fn a_play_pre_empts_only_a_live_engine_that_no_page_owns() {
        assert!(off_route_orphan(true, false));
        assert!(!off_route_orphan(true, true), "the player page owns its engine");
        assert!(!off_route_orphan(false, false), "nothing is running");
    }
}

pub(crate) fn content_requests(app: &mut App, _fr: &Frame) {
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
            ContentReq::Play { play } => {
                let mounted = bridge::player(&app.pages).is_some();
                clear_engine_for_play(&mut app.player.session, &mut app.adapters.player, mounted);
                // The PAGE decided what to play; the LOOP performs the request, because starting
                // playback takes the playback session's `&mut` and a screen is only ever shown the
                // frame's publication (§2.2). A refusal leaves the page exactly where it was.
                if !request_play_intent(&mut app.player.session, &play) { continue; }
                // The page's own `ReturnState` rides the push, so BACK out of the playback finds
                // the spot the Play was pressed from.
                start_playback(&mut app.player.session, &mut app.adapters.player,
                    super::playback::Origin::Here,
                    Some(ret), &mut app.pages, &mut app.bridge);
            }
        }
    }
}
