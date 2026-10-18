//! **The heartbeat's `route=` and ` overlay=` alphabet** — the words `tests/run.py` selects fps
//! samples by (`LOOP_RE`/`FPS_RE` against `tests/manifest.json`'s `route`/`overlay` fields), and
//! the same words the focus fingerprint, the diag `RouteEntered` event and the lab envelope print.
//!
//! It is its own module since D1's item 8, out of `app/mod.rs` — which is `plx_run`'s ten-line
//! skeleton and the `App` struct, and had grown four test modules that graded four other files'
//! subjects (`ci/check-deps.sh`'s `testmod` gate). Everything the alphabet is made of came with
//! the tests that grade it, because the point of [`heartbeat_word_tests`] is that the two tables
//! are DERIVED rather than transcribed: the routes from [`route_word`] over [`every_route`], the
//! overlays from the real `Mounter` through `bridge::every_surface_word`.
//!
//! A word the app cannot print makes a scene fail on the television as "only 0 post-warmup samples
//! — scene never entered this screen", which is indistinguishable from a real regression. That is
//! the single failure every rule in here exists to prevent.

use super::bridge;
use plx_screens::registry::AppArg;

/// The heartbeat's `route=` WORD for a route — the string `tests/run.py` selects samples by
/// (`LOOP_RE`/`FPS_RE`) against `manifest.json`'s `route` field, and the one the focus
/// fingerprint, the diag `RouteEntered` event and the lab envelope all print. ONE function so the
/// four cannot disagree, and so `heartbeat_word_tests` can grade the table against the manifest:
/// a route renamed here without its scenes following would otherwise fail on the device as
/// "never entered this screen", which reads exactly like a total regression.
pub(crate) fn route_word(route: &AppArg) -> &'static str {
    match route {
        // The Server page is Setup's screen in its other mode, and answers its word.
        AppArg::Setup | AppArg::Server => "setup",
        AppArg::Guide => "guide",
        AppArg::Player => "player",
    }
}
/// **Every PAGE argument, one per heartbeat word** — the domain [`route_word`] is applied over to DERIVE the
/// heartbeat's `route=` alphabet, and the list `app::bridge`'s own argument tests walk.
///
/// The compiler is what keeps it complete: the array's length is written out, so an added variant
/// with no entry here fails the exhaustiveness `match` in `heartbeat_word_tests` rather than
/// quietly missing from the table the fps tier selects on. That is the failure this exists for —
/// a word the app cannot print makes a scene fail on the television as "only 0 post-warmup
/// samples", which is indistinguishable from a real regression.
///
/// `#[cfg(test)]`, because nothing in the SHIPPED app ever wants every route at once: the loop
/// holds one and asks about it. Its two readers are this module's word derivation and
/// `app::bridge`'s argument tests, which used to keep a second copy of the same list.
#[cfg(test)]
pub(crate) fn every_route() -> [AppArg; 4] {
    [AppArg::Setup, AppArg::Guide, AppArg::Server, AppArg::Player]
}

/// The heartbeat's ` overlay=` WORD, or `None` when nothing is over the page.
///
/// **It is the topmost surface's own `Screen::name`, asked FIRST**, and since phase 10's item 4
/// that is the whole of it — `bridge::overlay_word` is a one-line read of the container with no
/// mapping table under it, so the alphabet the fps tier selects on IS the set of names the mounted
/// screens answer. See that function for the two failures the eleven-arm `match` it replaced had
/// already produced.
///
/// [`NO_OVERLAY`] is the one word here that no screen owns, and the reason this function exists at
/// all beside the bridge's: the player with nothing over it prints ` overlay=none`, which is a
/// statement about the ROUTE that the container cannot make. (Five arms stood here, one per
/// `Route::Player { overlay }` value, until phase 9 made the panels surfaces.)
pub(crate) fn overlay_word(pages: &plx_ui::dispatch::Dispatcher<bridge::AppHost>, route: &AppArg) -> Option<&'static str> {
    bridge::overlay_word(pages).or(matches!(route, AppArg::Player).then_some(NO_OVERLAY))
}

/// The bare player, whose ` overlay=` word belongs to no screen — see [`overlay_word`].
pub(crate) const NO_OVERLAY: &str = "none";

/// The heartbeat's ` overlay=<word>` suffix, prefix and all, empty when there is none. The prefix
/// is built HERE rather than baked into every word because the words are the SCREENS' own and a
/// screen has no business knowing what the heartbeat's grammar looks like.
pub(crate) fn overlay_suffix(pages: &plx_ui::dispatch::Dispatcher<bridge::AppHost>, route: &AppArg) -> String {
    overlay_word(pages, route).map_or(String::new(), |w| format!(" overlay={w}"))
}

/// **Every `route=` word [`route_word`] can print, and every ` overlay=` word [`overlay_word`]
/// can — DERIVED, not transcribed** (restructure phase 10, item 4).
///
/// Both were hand-written arrays, and both had already rotted in the one direction nothing fails
/// on: a word in the table that no function prints keeps a manifest scene looking armed while it
/// selects on a string the app never emits, which on the television reads as "only 0 post-warmup
/// samples — scene never entered this screen", i.e. exactly like a total regression. Phase 10
/// moved TWO words between the tables (`account`, `itemmenu`), which is the transition where a
/// transcription is least likely to survive.
///
/// So the routes come from [`route_word`] applied over [`every_route`], and the overlays from the
/// MOUNTER: `bridge::every_surface_word()` mounts one instance of every surface `AppArg` variant
/// through the real `Mounter` and reads its `Screen::name`, so the alphabet is what the screens
/// say it is. Neither list can carry a word its source does not produce, and neither can miss one
/// — the exhaustiveness of `every_route` and of the mounter's own `match` is the compiler's.
#[cfg(test)]
fn route_words() -> Vec<&'static str> {
    every_route().iter().map(route_word).collect()
}

#[cfg(test)]
fn overlay_words() -> Vec<&'static str> {
    let mut words = bridge::every_surface_word();
    words.push(NO_OVERLAY);
    words
}

/// The heartbeat word tables: derived from the routes and the mounted surfaces, and kept apart.
/// (They were also graded against `tests/manifest.json`'s fps scenes; those scenes are the Plex
/// client's and no longer reachable, so that half left with the Plex pages.)
#[cfg(test)]
mod heartbeat_word_tests {
    use super::{every_route, overlay_word, overlay_words, route_words, AppArg, NO_OVERLAY};

    /// **Every caller holds `plx_base::testlock::serial()` for its whole body**, because deriving
    /// this alphabet is not a read: [`overlay_words`] goes through `bridge::every_surface_word`,
    /// which mounts each surface by running real `bridge::frame`s, and the frame trunk asserts the
    /// lock.
    fn overlay_alphabet() -> Vec<&'static str> {
        overlay_words()
    }

    /// **The two tables are DERIVED from the two sources, and this is what says the derivation is
    /// the whole of them** (restructure phase 10 item 4).
    ///
    /// It used to compare two hand-written arrays against two hand-written lists of routes and
    /// screen kinds — four transcriptions of two alphabets, each able to rot in the direction
    /// nothing fails on. What is left to assert is what a derivation cannot state about itself:
    /// that the two alphabets are DISJOINT bar the one word that is deliberately in both, that
    /// every word is a plausible heartbeat token, and that `overlay_word`'s one non-screen answer
    /// is the player's.
    #[test]
    fn the_tables_are_derived_and_the_two_alphabets_stay_apart() {
        let _guard = plx_base::testlock::serial();
        let routes = route_words();
        let overlays = overlay_alphabet();
        assert_eq!(
            routes.len(),
            every_route().len(),
            "one word per route, derived: {routes:?}"
        );
        // Nothing empty, nothing with a space in it: every one of these is a `\w+` token the
        // harness's `LOOP_RE`/`FPS_RE` capture groups have to match, and a word carrying the
        // heartbeat's own ` overlay=` prefix (which is how the mapping table this replaced spelled
        // them) would match nothing at all.
        for w in routes.iter().chain(overlays.iter()) {
            assert!(
                !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "{w:?} is not a heartbeat token"
            );
        }
        // No word is in both alphabets: an overlap is a route and a surface that would be
        // indistinguishable in a `route=` field.
        let both: Vec<&str> = overlays.iter().copied().filter(|w| routes.contains(w)).collect();
        assert!(both.is_empty(), "{both:?} is both a route and a surface word");

        // An EMPTY tree, so `overlay_word`'s second half is what answers: with no surface up the
        // container says nothing and the ROUTE decides, which is exactly the state every BARE
        // playback frame is in. It is the one word no screen owns, which is why it is added by
        // `overlay_words` rather than derived.
        let empty = plx_ui::dispatch::Dispatcher::<super::bridge::AppHost>::new();
        assert_eq!(overlay_word(&empty, &AppArg::Player), Some(NO_OVERLAY));
        assert_eq!(overlay_word(&empty, &AppArg::Guide), None);
        assert_eq!(super::overlay_suffix(&empty, &AppArg::Player), " overlay=none");
        assert_eq!(super::overlay_suffix(&empty, &AppArg::Guide), "");
        assert!(overlays.contains(&NO_OVERLAY));
        assert!(!routes.contains(&NO_OVERLAY));
    }
}
