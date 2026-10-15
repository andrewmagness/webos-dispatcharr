//! **The player's live-TV controls** (M4): what the remote does while a channel is playing, and
//! the banner's and typed number's timers. Owned by [`super::PlayerScreen`] as one value, and pure:
//! every decision takes the tune and the frame's millisecond, so the host suite grades it.
//!
//! The remote, Magic Remote first (the owner's, 2026-10-14):
//! - CH▲/CH▼ (and UP/DOWN on the D-pad) change channel through the lineup the Guide handed over.
//! - The number keys type a channel number. It is tuned [`TYPED_COMMIT_MS`] after the last digit,
//!   at once on OK, or at [`MAX_DIGITS`]. A number no channel carries says so for a moment.
//! - OK shows the banner (channel, now/next, progress), or hides it if it is up. Every tune shows
//!   it for [`BANNER_MS`].
//! - BACK, STOP and EXIT are not live keys: they leave the player for the Guide as they always did.
//! - LEFT/RIGHT and the transport keys do nothing on live TV. There is nothing to seek in, and a
//!   paused live stream would fall behind the broadcast.

use plx_media::route::LiveTune;
use plx_ui::consts::{self, Key};
use std::os::raw::c_uint;

/// How long the banner stays up after a tune or an OK.
pub const BANNER_MS: u32 = 5_000;
/// How long after the last digit a typed number is tuned.
pub const TYPED_COMMIT_MS: u32 = 2_000;
/// How long a number that matched nothing is shown.
pub const MISSING_MS: u32 = 2_500;
/// The longest number typed before it is tuned without waiting.
pub const MAX_DIGITS: usize = 4;

/// A press the live player answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LivePress {
    Digit(u8),
    /// Through the lineup: positive is CH▲.
    Zap(i32),
    Ok,
    /// Pointer motion: show the banner.
    Wake,
    /// A key that does nothing on live TV and must not reach the film transport.
    Swallow,
}

/// What a press or a tick asks of the loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveAction {
    None,
    Tune(LiveTune),
}

/// The live keys among a press, `None` for the keys the player answers as it always has.
pub fn press_of(key: Key, sym: c_uint, wcode: c_uint) -> Option<LivePress> {
    if (b'0' as c_uint..=b'9' as c_uint).contains(&sym) {
        return Some(LivePress::Digit((sym - b'0' as c_uint) as u8));
    }
    // CH▲ pages UP (`-1`) everywhere else; on live TV it goes up the channels.
    if let Some(dir) = consts::page_dir(sym, wcode) {
        return Some(LivePress::Zap(-dir));
    }
    match key {
        Key::Up => Some(LivePress::Zap(1)),
        Key::Down => Some(LivePress::Zap(-1)),
        Key::Ok => Some(LivePress::Ok),
        Key::Left { .. } | Key::Right { .. } | Key::Play | Key::Pause | Key::PlayPause => Some(LivePress::Swallow),
        _ => None,
    }
}

/// `now` is before `until`, across the tick counter's wrap.
fn before(now: u32, until: u32) -> bool {
    (until.wrapping_sub(now) as i32) > 0
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveControls {
    /// The tune last seen, as `(index, at)`, and the tick it was first seen on: a new one shows the
    /// banner, and the tick anchors the wall clock ([`Self::wall_now`]).
    pub seen: Option<(usize, i64)>,
    pub seen_ms: u32,
    pub banner_until: u32,
    /// Digits typed so far, and the tick of the last one.
    pub typed: String,
    pub typed_at: u32,
    /// A typed number that matched no channel, shown until `missing_until`.
    pub missing: String,
    pub missing_until: u32,
}

impl LiveControls {
    pub fn banner_visible(&self, now: u32) -> bool {
        self.seen.is_some() && before(now, self.banner_until)
    }

    pub fn missing_visible(&self, now: u32) -> bool {
        !self.missing.is_empty() && before(now, self.missing_until)
    }

    /// The wall-clock second at frame `now`: the tune's own second plus the frames since.
    pub fn wall_now(&self, tune: &LiveTune, now: u32) -> i64 {
        tune.at + i64::from(now.wrapping_sub(self.seen_ms) / 1000)
    }

    /// Once a frame. Forgets everything when nothing live is playing.
    pub fn tick(&mut self, tune: Option<&LiveTune>, now: u32) -> LiveAction {
        let Some(tune) = tune else {
            *self = LiveControls::default();
            return LiveAction::None;
        };
        let key = (tune.index, tune.at);
        if self.seen != Some(key) {
            self.seen = Some(key);
            self.seen_ms = now;
            self.banner_until = now.wrapping_add(BANNER_MS);
        }
        if !self.typed.is_empty() && !before(now, self.typed_at.wrapping_add(TYPED_COMMIT_MS)) {
            return self.commit(tune, now);
        }
        LiveAction::None
    }

    pub fn key(&mut self, tune: &LiveTune, press: LivePress, now: u32) -> LiveAction {
        match press {
            LivePress::Digit(d) => {
                self.missing.clear();
                self.typed.push(char::from(b'0' + d));
                self.typed_at = now;
                if self.typed.len() >= MAX_DIGITS {
                    return self.commit(tune, now);
                }
                LiveAction::None
            }
            LivePress::Zap(delta) => {
                self.typed.clear();
                match tune.lineup.step(tune.index, delta) {
                    Some(i) => LiveAction::Tune(tune.to(i)),
                    None => LiveAction::None,
                }
            }
            LivePress::Ok if !self.typed.is_empty() => self.commit(tune, now),
            LivePress::Ok => {
                self.banner_until = if self.banner_visible(now) { now } else { now.wrapping_add(BANNER_MS) };
                LiveAction::None
            }
            LivePress::Wake => {
                self.banner_until = now.wrapping_add(BANNER_MS);
                LiveAction::None
            }
            LivePress::Swallow => LiveAction::None,
        }
    }

    fn commit(&mut self, tune: &LiveTune, now: u32) -> LiveAction {
        let typed = std::mem::take(&mut self.typed);
        match tune.lineup.by_number(&typed) {
            Some(i) if i == tune.index => {
                self.banner_until = now.wrapping_add(BANNER_MS);
                LiveAction::None
            }
            Some(i) => LiveAction::Tune(tune.to(i)),
            None => {
                self.missing = typed;
                self.missing_until = now.wrapping_add(MISSING_MS);
                LiveAction::None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plx_media::route::{LiveChannel, LiveLineup};
    use std::sync::Arc;

    fn tune(index: usize) -> LiveTune {
        let channels = ["2", "4", "5", "11"]
            .iter()
            .enumerate()
            .map(|(i, n)| LiveChannel {
                id: i as i64,
                number: (*n).into(),
                name: format!("C{n}"),
                url: format!("u{n}"),
                logo: None,
                programmes: Vec::new(),
            })
            .collect();
        let lineup = LiveLineup { channels, vcodec: "h264".into(), acodec: "aac".into() };
        LiveTune { lineup: Arc::new(lineup), index, previous: None, at: 1_000 }
    }

    fn tuned_to(a: LiveAction) -> Option<usize> {
        match a {
            LiveAction::Tune(t) => Some(t.index),
            LiveAction::None => None,
        }
    }

    #[test]
    fn the_remote_keys_map_to_live_presses() {
        assert_eq!(press_of(Key::Other, b'7' as c_uint, 0), Some(LivePress::Digit(7)));
        assert_eq!(press_of(Key::Other, 0, consts::WCODE_CH_UP_KEY), Some(LivePress::Zap(1)), "CH▲ goes up");
        assert_eq!(press_of(Key::Other, 0, consts::WCODE_CH_DOWN_KEY), Some(LivePress::Zap(-1)));
        assert_eq!(press_of(Key::Up, 0, 0), Some(LivePress::Zap(1)));
        assert_eq!(press_of(Key::Down, 0, 0), Some(LivePress::Zap(-1)));
        assert_eq!(press_of(Key::Ok, 0, 0), Some(LivePress::Ok));
        assert_eq!(press_of(Key::Pause, 0, 0), Some(LivePress::Swallow), "no pausing live TV");
        assert_eq!(press_of(Key::Left { alt: false }, 0, 0), Some(LivePress::Swallow), "nothing to seek in");
        for leave in [Key::Back, Key::Stop, Key::Exit] {
            assert_eq!(press_of(leave, 0, 0), None, "{leave:?} leaves the player as it always has");
        }
    }

    #[test]
    fn a_tune_shows_the_banner_for_a_while_and_anchors_the_clock() {
        let mut c = LiveControls::default();
        let t = tune(0);
        assert_eq!(c.tick(Some(&t), 100), LiveAction::None);
        assert!(c.banner_visible(100) && c.banner_visible(100 + BANNER_MS - 1));
        assert!(!c.banner_visible(100 + BANNER_MS));
        assert_eq!(c.wall_now(&t, 100 + 61_500), 1_061);
        c.tick(Some(&t), 9_000);
        assert!(!c.banner_visible(9_000), "the same tune does not raise it again");
        let next = LiveTune { at: 1_010, ..tune(1) };
        c.tick(Some(&next), 9_100);
        assert!(c.banner_visible(9_100), "a new channel does");
        c.tick(None, 9_200);
        assert_eq!(c, LiveControls::default(), "leaving live TV forgets it all");
    }

    #[test]
    fn ok_toggles_the_banner() {
        let mut c = LiveControls::default();
        let t = tune(0);
        c.tick(Some(&t), 0);
        c.key(&t, LivePress::Ok, 1_000);
        assert!(!c.banner_visible(1_000), "up: OK hides it");
        c.key(&t, LivePress::Ok, 2_000);
        assert!(c.banner_visible(2_000 + BANNER_MS - 1), "down: OK shows it");
    }

    #[test]
    fn up_and_down_change_channel_and_drop_a_half_typed_number() {
        let mut c = LiveControls::default();
        let t = tune(3);
        c.tick(Some(&t), 0);
        c.key(&t, LivePress::Digit(1), 10);
        assert_eq!(tuned_to(c.key(&t, LivePress::Zap(1), 20)), Some(0), "wraps to the first");
        assert!(c.typed.is_empty());
        assert_eq!(tuned_to(c.key(&t, LivePress::Zap(-1), 30)), Some(2));
    }

    #[test]
    fn a_typed_number_tunes_after_the_pause_or_at_once_on_ok() {
        let mut c = LiveControls::default();
        let t = tune(0);
        c.tick(Some(&t), 0);
        c.key(&t, LivePress::Digit(1), 100);
        c.key(&t, LivePress::Digit(1), 900);
        assert_eq!(c.tick(Some(&t), 900 + TYPED_COMMIT_MS - 1), LiveAction::None, "still typing");
        assert_eq!(tuned_to(c.tick(Some(&t), 900 + TYPED_COMMIT_MS)), Some(3), "11 after the pause");
        c.key(&t, LivePress::Digit(5), 5_000);
        assert_eq!(tuned_to(c.key(&t, LivePress::Ok, 5_100)), Some(2), "5 at once on OK");
    }

    #[test]
    fn a_number_no_channel_has_says_so_and_the_one_playing_just_shows_the_banner() {
        let mut c = LiveControls::default();
        let t = tune(0);
        c.tick(Some(&t), 0);
        for d in [9, 9, 9, 9] {
            c.key(&t, LivePress::Digit(d), 6_000);
        }
        assert!(c.typed.is_empty(), "four digits commit without waiting");
        assert!(c.missing_visible(6_000) && c.missing == "9999");
        assert!(!c.missing_visible(6_000 + MISSING_MS));
        c.key(&t, LivePress::Digit(2), 10_000);
        assert_eq!(c.key(&t, LivePress::Ok, 10_100), LiveAction::None, "2 is already playing");
        assert!(c.banner_visible(10_100));
    }
}
