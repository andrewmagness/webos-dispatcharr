//! **The focus fingerprint** — one line naming everything `app.rs`'s key ladder can move, written
//! to the event log only on the frames where it CHANGES.
//!
//! # Why this exists
//!
//! The once/sec heartbeat (`app.rs`, the `loop=` line) publishes `route=`, `overlay=` and `pos=`.
//! Most of the key ladder's arms move focus *within* a screen without touching any of those three:
//! UP on the home grid, LEFT along the detail hero's action row, DOWN through an open popover. A
//! (route × key) characterization harness built on the heartbeat alone would therefore pin the
//! handful of arms that change route and be blind to the rest.
//!
//! This module is the missing instrument. It samples the read-only accessors each screen already
//! exposes, formats them into one ordered line, and logs that line when — and only when — it
//! differs from the last one. What a key press did is then the diff between the fingerprint before
//! it and the fingerprint after.
//!
//! # Rules this module keeps
//!
//! **Read-only.** Every field comes from a getter the screen already exposes for the app's own
//! use. This module adds no accessor to any screen and edits none, and it drains no one-shot
//! mailbox — `detail::take_open_request`, `detail::take_alt_open` and `person::take_request` are
//! deliberately untouched, because reading one would consume a navigation.
//!
//! **It must not touch [`plx_machine::idle`].** It never calls `invalidate()`. Reporting to the frame
//! gate would hold every screen presenting forever, which would destroy the very idle behaviour a
//! harness built on this is meant to be able to observe.
//!
//! **Redaction.** The event log is pasted into public issue threads (`app/diagnostics.rs`'s module
//! doc has the reasoning). So the line carries indices, enum tags and booleans — no title, no path,
//! no URL, no server address, no credential. The enforcement is structural rather than a matter of
//! care: every value written here is either a number, a bool or a `&'static str` tag chosen in this
//! file.
//!
//! **Diffable.** Fixed field order, one line, no timestamps, no addresses, and no float is ever
//! printed. A field's PRESENCE is a function of [`Screen`] alone, so two lines for the same screen
//! always carry the same keys.
//!
//! # Cost
//!
//! [`armed`] resolves its trigger once and answers from a `OnceLock` afterwards, so an unarmed
//! build or an unarmed boot pays one atomic load and a branch per frame. Armed, a frame pays the
//! accessor reads for the CURRENT screen only — the `match` on [`Screen`] never reaches another
//! screen's getters — plus one `String` build of 40–160 bytes.
//!
//! Measured on the dev Mac at `opt-level = 2` (2026-08-15, 50k builds per screen, host accessors
//! over empty stores): **39–223 ns** per frame on every screen except the player, where the line
//! costs **~7.3 µs**. That one number is worth knowing before adding a field: **~6.4 µs of it is a
//! single filesystem `stat`**, inside [`plx_appkit::player_hud::transport_hidden`] →
//! `player_hud::busy` → `devtrig::flag("failtest")`. The rest of the player's seventeen fields together
//! cost ~0.9 µs. It is paid deliberately: `transport_hidden` is the exact predicate
//! `PlayerScreen::handle_key` tests before any transport arm, and the alternative is a second
//! derivation of a rule that module keeps in one place. The player route is also the one route the idle gate excludes, so it draws
//! at full rate and 7 µs is ~0.04% of a 16.7 ms frame.
//!
//! # Arming it
//!
//! `touch /tmp/dispatcharrtv-focus` on the television (or in `DISPATCHARRTV_RUNTIME_DIR` under the
//! simulator), then read `focus ` lines out of `/tmp/dispatcharrtv-events.log`. The trigger is listed
//! in [`crate::dev`]'s `DIAG` set, so arming the observer does not also change which screen the app
//! boots to — the same reasoning `dispatcharrtv-noidle` carries: a harness must not change the screen
//! it is watching by watching it.
//! `RELEASE=1` drops the `devtriggers` feature, `devtrig::flag` becomes `false`, and this whole surface
//! goes quiet.

use std::fmt::Write as _;
use std::sync::Mutex;

/// Which screen the frame ended on, as the probe dispatches on it.
///
/// `app.rs` maps its private `Route` onto this with an EXHAUSTIVE `match`, so a new route is a
/// compile error here rather than a screen that silently fingerprints as nothing. The route WORD
/// printed on the line is not taken from this type — it is `app.rs`'s own `rn`, handed to
/// [`sample`] separately, so the fingerprint's `route=` and the heartbeat's `route=` are the same
/// string built by the same expression and a harness can join the two lines on it.
#[derive(Clone, Copy)]
pub(crate) enum Screen {
    /// Dispatcharr sign-in (`screens::setup`). Its cursor is the engine's, handed in raw: this
    /// module sits below `screens/` and cannot decode the page's element numbers (0–2 the fields,
    /// 3 *Sign in*, 4 *Sign out*).
    Setup {
        /// The focused element, or `-1` with nothing focused (the page still loading).
        elem: i32,
    },
    /// The Dispatcharr guide (`screens::guide`). Its cursor is the engine's, raw, as
    /// [`Screen::Setup`]'s: a cell is `row << 12 | slot`, the header's *Account* `0xFFFE_FFFF`
    /// (just below the tab strip's range), the failed read-out's action `0xFFFE_FFFE`.
    Guide {
        /// The focused element, or `-1` with nothing focused (the page still loading).
        elem: i64,
    },
    /// The live player. It has no cursor of its own outside the failure read-out, so what it
    /// prints is whether that read-out owns the frame.
    Player,
}

plx_base::devtrig::latched_flag!(
    /// Is the probe armed for this boot? Resolved once, from `/tmp/dispatcharrtv-focus`.
    ///
    /// Resolved once rather than per frame for two reasons: `tests/run.py` clears `/tmp/dispatcharrtv-*`
    /// between cases, so a later read could legitimately find the file gone mid-run; and a per-frame
    /// `exists()` is a syscall this is not worth paying. Call sites may check this before building
    /// arguments — [`sample`] checks it again, so the module is correct on its own.
    ///
    /// This body was hand-rolled here first; [`plx_base::devtrig::latched_flag`] is that body, moved to the
    /// module that owns the trigger surface so every per-frame `flag` caller can have it.
    pub(crate) fn armed = "focus";
);

/// Sample the current focus state and log it if it has moved since the last sample.
///
/// Call once per frame, AFTER the frame's input has been handled and the screen drawn, so what is
/// recorded is the state a key press has already moved rather than the state it is about to.
pub(crate) fn sample(ps: &plx_media::route::PlaybackSession, route: &str, screen: Screen) {
    if !armed() {
        return;
    }
    let line = fingerprint(ps, route, screen);
    static LAST: Mutex<Option<String>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.as_deref() == Some(line.as_str()) {
        return;
    }
    plx_base::eventlog::log(&line);
    *last = Some(line);
}

/// Build the line. Split out from [`sample`] so its determinism and its grammar are host-testable
/// without a log file or a change-detection state.
fn fingerprint(ps: &plx_media::route::PlaybackSession, route: &str, screen: Screen) -> String {
    let mut s = String::with_capacity(96);
    s.push_str("focus route=");
    s.push_str(route);
    push_fields(ps, &mut s, screen);
    // The tvOS click, which is route-agnostic: an OK over a card arms a press and the activation
    // commits from the per-frame loop on the spring-back, so "a press is in flight" is a state the
    // ladder put the app into and a state the NEXT key cancels.
    let _ = write!(s, " press={}", b(plx_ui::press::is_active()));
    s
}

/// One screen's own fields.
fn push_fields(ps: &plx_media::route::PlaybackSession, s: &mut String, screen: Screen) {
    match screen {
        Screen::Setup { elem } => {
            let _ = write!(s, " elem={elem}");
        }
        Screen::Guide { elem } => {
            let _ = write!(s, " elem={elem}");
        }
        // `hidden` is `player_hud::transport_hidden()`: while it is true the failure read-out owns
        // the frame and its row is the only thing a key can drive (`PlayerScreen::handle_key`).
        Screen::Player => {
            let _ = write!(s, " hidden={}", b(plx_appkit::player_hud::transport_hidden(ps)));
        }
    }
}

fn b(v: bool) -> u8 {
    v as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every screen this app can end a frame on, so a grammar or determinism assertion covers the
    /// whole instrument rather than the one screen a test happened to pick.
    fn every_screen() -> Vec<(&'static str, Screen)> {
        vec![
            // a cursor and the still-loading page, because the two print different values
            // through one grammar
            ("setup", Screen::Setup { elem: 3 }),
            ("setup", Screen::Setup { elem: -1 }),
            ("guide", Screen::Guide { elem: (3 << 12) | 2 }),
            ("guide", Screen::Guide { elem: -1 }),
            ("player", Screen::Player),
        ]
    }

    /// **The property the whole instrument rests on**: with nothing moved between two samples, the
    /// two lines are byte-identical. A fingerprint that varied on its own would log every frame and
    /// a harness could not read a key press out of the diff.
    #[test]
    fn a_fingerprint_is_stable_while_nothing_moves() {
        let ps = plx_media::route::PlaybackSession::IDLE;
        let _g = plx_base::testlock::serial();
        for (rn, sc) in every_screen() {
            assert_eq!(fingerprint(&ps, rn, sc), fingerprint(&ps, rn, sc), "{rn} fingerprinted differently twice in a row");
        }
    }

    /// The grammar, which is also the redaction enforcement: one line, `focus route=<word>` first,
    /// then `key=value` pairs whose values hold no whitespace, no `/` and no `?`. A title would
    /// bring a space, a URL or path would bring a slash, and either would fail here.
    #[test]
    fn the_line_is_one_ordered_row_of_safe_key_value_pairs() {
        let ps = plx_media::route::PlaybackSession::IDLE;
        let _g = plx_base::testlock::serial();
        for (rn, sc) in every_screen() {
            let line = fingerprint(&ps, rn, sc);
            assert!(!line.contains('\n'), "{rn}: a fingerprint is ONE line: {line}");
            let mut fields = line.split(' ');
            assert_eq!(fields.next(), Some("focus"), "{line}");
            for f in fields {
                let (k, v) = f.split_once('=').unwrap_or_else(|| panic!("{rn}: {f:?} is not key=value in {line}"));
                assert!(!k.is_empty() && k.chars().all(|c| c.is_ascii_lowercase()), "{rn}: odd key {k:?}");
                assert!(
                    v.chars().all(|c| !c.is_whitespace() && c != '/' && c != '?' && c != '&'),
                    "{rn}: value {v:?} could carry a path, query or title: {line}"
                );
            }
        }
    }

    /// A moved cursor must actually show up — the instrument's whole purpose.
    #[test]
    fn moving_the_cursor_changes_the_line() {
        let ps = plx_media::route::PlaybackSession::IDLE;
        let _g = plx_base::testlock::serial();
        assert_ne!(fingerprint(&ps, "guide", Screen::Guide { elem: 1 }), fingerprint(&ps, "guide", Screen::Guide { elem: 2 }));
        assert_ne!(fingerprint(&ps, "setup", Screen::Setup { elem: 0 }), fingerprint(&ps, "setup", Screen::Setup { elem: 3 }));
    }

    /// `armed()` is what every call site checks first, and it must answer the same thing for the
    /// whole boot. Without the latch a run that cleared `/tmp` mid-case would stop fingerprinting
    /// half way through and read as "focus stopped moving".
    #[test]
    fn armed_answers_once_for_the_whole_process() {
        let first = armed();
        for _ in 0..3 {
            assert_eq!(armed(), first);
        }
    }
}
