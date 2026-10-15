//! Application widgets shared by the player: compositions of `ui` components over application
//! types (the player's state, the live lineup, subtitles).
//!
//! **Why this is not `ui/` and not `screens/`.** `ui/` is the widget LIBRARY and names no
//! application type (`ui/CLAUDE.md`'s layer rule), and a widget that reads `player::PlaybackState`
//! is application code. These sit one layer below `screens` and one above `media`, so that `ui`
//! stays free of them (`ci/module-layers.ini`, `[appkit]`; `docs/module-layers.md` step L10).
//!
//! **Direction.** This module may name every layer below it (`ui`, `data`, `media`, ...) and never
//! `screens` or `app`: a value the loop owns is passed in, and a behaviour a higher layer owns is a
//! hook it registers at boot.
// **This blankets the whole `appkit` tree**, as `plx_ui`'s `lib.rs`'s own `allow(dead_code)` did for these
// files while they lived there (that comment records why it is wider than it reads): the moved
// files were never subject to the lint, and moving them must not make a different set of items
// "dead".
#![allow(dead_code)]

pub mod chapters_panel;
pub mod held_key; // the key-hold half of hold-to-scrub, shared by the player HUD and the trailer transport
pub mod live_banner; // the live-TV banner (channel, now/next, progress) and the typed-number read-out
pub mod player_hud;
pub mod skip_pill; // the Skip Intro / Skip Credits pill — a `ControlSlot` occupant of the player HUD's control row
pub mod up_next; // end-of-episode Up Next card + auto-advance countdown

