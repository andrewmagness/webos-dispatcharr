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
//! `touch /tmp/plxnative-focus` on the television (or in `PLXNATIVE_RUNTIME_DIR` under the
//! simulator), then read `focus ` lines out of `/tmp/plxnative-events.log`. The trigger is listed
//! in [`crate::dev`]'s `DIAG` set, so arming the observer does not also change which screen the app
//! boots to — the same reasoning `plxnative-noidle` carries: a harness must not change the screen
//! it is watching by watching it.
//! `RELEASE=1` drops the `devtriggers` feature, `devtrig::flag` becomes `false`, and this whole surface
//! goes quiet.

use std::ffi::c_int;
use std::fmt::Write as _;
use std::sync::Mutex;

use plx_appkit::player_hud::ControlSlot;

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
    /// `overlay` is the same word the heartbeat prints after `overlay=`, supplied by `app.rs` from
    /// its own exhaustive match on the private `Overlay` enum.
    Player {
        overlay: &'static str,
    },
}

/// The player HUD's focus cursor, plus whether the transport is on screen at all.
///
/// A copy of the `HudNav` cursor `plex_run` holds as a local. The type is a non-`pub` item of
/// `app`, which `lib.rs` declares as a private `mod`, so it cannot be named from here — the same
/// boundary `Route` sits behind. `visible` is `app.rs`'s `hud_visible(…)` — the same
/// value its own OK and LEFT/RIGHT arms branch on — because the HUD's visibility decides whether a
/// press moves the cursor or merely reveals the bar.
#[derive(Clone, Copy)]
pub(crate) struct Hud {
    pub(crate) focus: c_int,
    pub(crate) btn: c_int,
    pub(crate) tab: c_int,
    pub(crate) visible: bool,
}

plx_base::devtrig::latched_flag!(
    /// Is the probe armed for this boot? Resolved once, from `/tmp/plxnative-focus`.
    ///
    /// Resolved once rather than per frame for two reasons: `tests/run.py` clears `/tmp/plxnative-*`
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
pub(crate) fn sample(ps: &plx_media::route::PlaybackSession, route: &str, screen: Screen, hud: Hud, ctrl: ControlSlot, content: &str, meta: plx_data::metadata::MetadataView<'_>) {
    if !armed() {
        return;
    }
    let line = fingerprint_content(ps, route, screen, hud, ctrl, content, meta);
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
#[cfg(test)]
fn fingerprint(ps: &plx_media::route::PlaybackSession, route: &str, screen: Screen, hud: Hud, ctrl: ControlSlot, meta: plx_data::metadata::MetadataView<'_>) -> String {
    fingerprint_content(ps, route, screen, hud, ctrl, "", meta)
}

fn fingerprint_content(ps: &plx_media::route::PlaybackSession, route: &str, screen: Screen, hud: Hud, ctrl: ControlSlot, content: &str, meta: plx_data::metadata::MetadataView<'_>) -> String {
    let mut s = String::with_capacity(192);
    s.push_str("focus route=");
    s.push_str(route);
    push_fields(ps, &mut s, screen, hud, ctrl, content, meta);
    // The tvOS click, which is route-agnostic: an OK over a card arms a press and the activation
    // commits from the per-frame loop on the spring-back, so "a press is in flight" is a state the
    // ladder put the app into and a state the NEXT key cancels.
    let _ = write!(s, " press={}", b(plx_ui::press::is_active()));
    s
}

/// One screen's own fields.
fn push_fields(ps: &plx_media::route::PlaybackSession, s: &mut String, screen: Screen, hud: Hud, ctrl: ControlSlot, content: &str, meta: plx_data::metadata::MetadataView<'_>) {
    match screen {
        Screen::Setup { elem } => {
            let _ = write!(s, " elem={elem}");
        }
        Screen::Guide { elem } => {
            let _ = write!(s, " elem={elem}");
        }
        // The panels' own fields ride on `content`: since restructure phase 9 each is the state of a
        // MOUNTED INSTANCE
        // (`screens::player::overlay`), and this module cannot reach into the container — the
        // caller (`app::bridge::content_probe`) builds them from the surface that is up.
        Screen::Player { overlay } => {
            push_player(ps, s, overlay, hud, ctrl, meta);
            s.push_str(content);
        }
    }
}


/// The player: the HUD cursor, what the control row currently holds, and each panel's own state.
fn push_player(ps: &plx_media::route::PlaybackSession, s: &mut String, overlay: &str, hud: Hud, ctrl: ControlSlot, meta: plx_data::metadata::MetadataView<'_>) {
    // `upnext` is the PLAYER INSTANCE's countdown since phase 9, so it arrives on `content` with
    // the panels' fields rather than being read off a module global here.
    let slot = match ctrl {
        ControlSlot::Discs => "discs",
        ControlSlot::Skip(_) => "skip",
        ControlSlot::UpNext(_) => "upnext",
    };
    // `hidden` is `player_hud::transport_hidden()`, read here for the reason
    // `PlayerScreen::handle_key` reads it: it is that page's FIRST test, so while it is true every
    // transport arm beneath it is unreachable and only the read-out's two DRAWN escapes — OK to
    // the quality ladder, BACK out — plus EXIT do anything. A characterization run that could not
    // see this field would record those arms as dead code rather than as shadowed ones.
    // (Phase 12, PX-PLAYER: the loop does not read this predicate at all any more.)
    let _ = write!(
        s,
        " ov={} hud={} f={} btn={} tab={} slot={} items={} hidden={}",
        overlay,
        b(hud.visible),
        hud.focus,
        hud.btn,
        hud.tab,
        slot,
        ctrl.items(),
        b(plx_appkit::player_hud::transport_hidden(ps))
    );
    // Whether this item HAS chapters at all — a fact about the item, not about a panel, which is
    // why it stays here while each overlay's own open flag and cursor arrive on `content` from the
    // surface (`app::bridge::content_probe`).
    let _ = write!(
        s,
        " haschap={}",
        b(plx_appkit::chapters_panel::has_chapters(meta))
    );
}

fn b(v: bool) -> u8 {
    v as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        static TEST_METADATA: std::cell::UnsafeCell<plx_data::stores::metadata::MetadataStore> =
            std::cell::UnsafeCell::new(plx_data::stores::metadata::MetadataStore::default());
    }
    fn test_store() -> &'static mut plx_data::stores::metadata::MetadataStore {
        TEST_METADATA.with(|cell| unsafe { &mut *cell.get() })
    }

    fn hud() -> Hud {
        Hud {
            focus: 0,
            btn: 0,
            tab: 0,
            visible: false,
        }
    }

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
            ("player", Screen::Player { overlay: "none" }),
            ("player", Screen::Player { overlay: "info" }),
        ]
    }

    /// **The property the whole instrument rests on**: with nothing moved between two samples, the
    /// two lines are byte-identical. A fingerprint that varied on its own would log every frame and
    /// a harness could not read a key press out of the diff.
    ///
    /// It takes the crate-wide serial lock because it walks every screen's accessors, several of
    /// which read process-global stores that other modules' tests mutate.
    #[test]
    fn a_fingerprint_is_stable_while_nothing_moves() {
        let ps = plx_media::route::PlaybackSession::IDLE;
        let _g = plx_base::testlock::serial();
        for (rn, sc) in every_screen() {
            let a = fingerprint(&ps, rn, sc, hud(), ControlSlot::Discs, test_store().view());
            let b = fingerprint(&ps, rn, sc, hud(), ControlSlot::Discs, test_store().view());
            assert_eq!(a, b, "{rn} fingerprinted differently twice in a row");
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
            let line = fingerprint(&ps, rn, sc, hud(), ControlSlot::Discs, test_store().view());
            assert!(
                !line.contains('\n'),
                "{rn}: a fingerprint is ONE line: {line}"
            );
            let mut fields = line.split(' ');
            assert_eq!(fields.next(), Some("focus"), "{line}");
            for f in fields {
                let (k, v) = f
                    .split_once('=')
                    .unwrap_or_else(|| panic!("{rn}: {f:?} is not key=value in {line}"));
                assert!(
                    !k.is_empty() && k.chars().all(|c| c.is_ascii_lowercase()),
                    "{rn}: odd key {k:?}"
                );
                assert!(
                    v.chars()
                        .all(|c| !c.is_whitespace() && c != '/' && c != '?' && c != '&'),
                    "{rn}: value {v:?} could carry a path, query or title: {line}"
                );
            }
        }
    }

    /// A field's PRESENCE is a function of the screen alone — the property that lets a harness diff
    /// two fingerprints key by key instead of re-parsing a variable schema.
    #[test]
    fn one_screen_always_carries_the_same_keys() {
        let ps = plx_media::route::PlaybackSession::IDLE;
        let _g = plx_base::testlock::serial();
        let keys = |rn, sc, ctrl| {
            fingerprint(&ps, rn, sc, hud(), ctrl, test_store().view())
                .split(' ')
                .skip(1)
                .filter_map(|f| f.split_once('=').map(|(k, _)| k.to_string()))
                .collect::<Vec<_>>()
        };
        // the player's control row swaps occupants under the same cursor — the keys must not swap
        // with it, or every marker segment would look like a schema change
        let credits = plx_data::metadata::Marker {
            kind: plx_data::metadata::MarkerKind::Credits,
            start_ms: 1000,
            end_ms: 2000,
            final_seg: false,
        };
        let discs = keys(
            "player",
            Screen::Player { overlay: "none" },
            ControlSlot::Discs,
        );
        let up = keys(
            "player",
            Screen::Player { overlay: "none" },
            ControlSlot::UpNext(credits),
        );
        assert_eq!(
            discs, up,
            "the control row's occupant changed the SCHEMA, not just a value"
        );
        // …and a HUD cursor move is a value change, never a key change
        let moved = fingerprint(&ps, 
            "player",
            Screen::Player { overlay: "none" },
            Hud {
                focus: 2,
                btn: 1,
                tab: 1,
                visible: true,
            },
            ControlSlot::Discs, test_store().view(),
        );
        let moved_keys: Vec<String> = moved
            .split(' ')
            .skip(1)
            .filter_map(|f| f.split_once('=').map(|(k, _)| k.to_string()))
            .collect();
        assert_eq!(discs, moved_keys);
    }

    /// A moved cursor must actually show up. This is the instrument's whole purpose, and it is the
    /// one assertion that fails if a future edit prints a constant where a getter belongs.
    #[test]
    fn moving_the_hud_cursor_changes_the_line() {
        let ps = plx_media::route::PlaybackSession::IDLE;
        let _g = plx_base::testlock::serial();
        let at = |f, btn, tab| {
            fingerprint(&ps, 
                "player",
                Screen::Player { overlay: "none" },
                Hud {
                    focus: f,
                    btn,
                    tab,
                    visible: true,
                },
                ControlSlot::Discs, test_store().view(),
            )
        };
        assert_ne!(
            at(0, 0, 0),
            at(1, 0, 0),
            "the HUD focus row is not observable"
        );
        assert_ne!(
            at(1, 0, 0),
            at(1, 1, 0),
            "the control-row cursor is not observable"
        );
        assert_ne!(
            at(2, 0, 0),
            at(2, 0, 1),
            "the tab-row cursor is not observable"
        );
        assert_ne!(
            at(0, 0, 0),
            fingerprint(&ps, 
                "player",
                Screen::Player { overlay: "info" },
                Hud {
                    focus: 0,
                    btn: 0,
                    tab: 0,
                    visible: true
                },
                ControlSlot::Discs, test_store().view()
            ),
            "the overlay tag is not observable"
        );
    }

    /// **The stalled-sign-in control appearing must actually show up.** This is the exact gap
    /// `Screen::Login`'s own doc names: `phase=` alone reports nothing for the ~12 s (a stalled
    /// spinner) or ~60 s (an unscanned QR code) the escape/restart control sits on screen with the
    /// phase unchanged, so a (route × key) harness reading `phase=` alone cannot see OK land on the
    /// control at all — only the phase change a beat later, once the press has already been acted
    /// on. This is the assertion that fails if a future edit prints a constant (`ctl=0` always, or
    /// the field dropped) where `has_control` belongs — the same shape as

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
