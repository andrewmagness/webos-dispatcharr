//! **The player's live-TV controls** (M4): what the remote does while a channel is playing, and
//! the banner's and typed number's timers. Owned by [`super::PlayerScreen`] as one value, and pure:
//! every decision takes the tune and the frame's millisecond, so the host suite grades it.
//!
//! The remote, Magic Remote first (the owner's, 2026-10-14):
//! - CH▲/CH▼ (and UP/DOWN on the D-pad) change channel through the lineup the Guide handed over.
//! - The number keys type a channel number. It is tuned the moment no longer channel number starts
//!   with what is typed (`5` at once; `1` waits while there is an `11`), else [`TYPED_COMMIT_MS`]
//!   after the last digit, or at once on OK. The lineup decides how long a number can be, so no
//!   channel is out of reach. A number no channel carries says so for a moment.
//! - OK shows the banner (channel, now/next, progress), or hides it if it is up. Every tune shows
//!   it for [`BANNER_MS`]. A tap acts on its RELEASE, because the same key held is recall.
//! - Holding OK for [`HOLD_MS`] recalls the channel watched before this one (the owner's choice,
//!   2026-10-14: the Magic Remote has no recall key). Holding it again goes back.
//! - BACK, STOP and EXIT are not live keys: they leave the player for the Guide as they always did.
//! - LEFT/RIGHT and the transport keys do nothing on live TV. There is nothing to seek in, and a
//!   paused live stream would fall behind the broadcast.

use plx_machine::machine::Edge;
use plx_media::route::LiveTune;
use plx_ui::consts::{self, Key};
use std::os::raw::c_uint;

/// How long the banner stays up after a tune or an OK.
pub const BANNER_MS: u32 = 5_000;
/// How long after the last digit a typed number is tuned.
pub const TYPED_COMMIT_MS: u32 = 2_000;
/// How long a number that matched nothing is shown.
pub const MISSING_MS: u32 = 2_500;
/// How long OK must be held to recall the last channel.
pub const HOLD_MS: u32 = 600;
/// An OK still down this long with no repeat and no release lost its key-up: forget it.
const OK_STALE_MS: u32 = 3_000;

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

/// Whether more digits could still make `typed` a tunable channel's number.
fn could_continue(tune: &LiveTune, typed: &str) -> bool {
    tune.lineup
        .channels
        .iter()
        .any(|c| c.tunable() && c.number.len() > typed.len() && c.number.starts_with(typed))
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
    /// The tick OK went down on, while it is down, and whether this hold has already recalled.
    pub ok_down: Option<u32>,
    pub ok_fired: bool,
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
        if self.ok_down.is_some_and(|t0| !before(now, t0.wrapping_add(OK_STALE_MS))) {
            self.ok_down = None;
            self.ok_fired = false;
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
                if !could_continue(tune, &self.typed) {
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

    /// OK's three edges. Down only starts the press; a repeat [`HOLD_MS`] in is the hold, which
    /// recalls once; the release of a press that did not recall is the tap.
    pub fn ok(&mut self, tune: &LiveTune, edge: Edge, now: u32) -> LiveAction {
        match edge {
            Edge::Down => {
                self.ok_down = Some(now);
                self.ok_fired = false;
                LiveAction::None
            }
            Edge::Repeat => match self.ok_down {
                Some(t0) if !self.ok_fired && !before(now, t0.wrapping_add(HOLD_MS)) => {
                    self.ok_fired = true;
                    self.typed.clear();
                    self.recall(tune, now)
                }
                _ => LiveAction::None,
            },
            Edge::Up => {
                let pressed = self.ok_down.take().is_some();
                let fired = std::mem::take(&mut self.ok_fired);
                if pressed && !fired { self.key(tune, LivePress::Ok, now) } else { LiveAction::None }
            }
        }
    }

    /// The channel watched before this one; with none to go back to, the banner says where we are.
    fn recall(&mut self, tune: &LiveTune, now: u32) -> LiveAction {
        match tune.previous.filter(|&p| p != tune.index && tune.lineup.channels.get(p).is_some_and(|c| c.tunable())) {
            Some(p) => LiveAction::Tune(tune.to(p)),
            None => {
                self.banner_until = now.wrapping_add(BANNER_MS);
                LiveAction::None
            }
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
        let channels = ["1", "4", "5", "11"]
            .iter()
            .enumerate()
            .map(|(i, n)| LiveChannel {
                id: i as i64,
                uuid: format!("u-{n}"),
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
    fn a_tap_of_ok_acts_on_release_and_a_hold_recalls_once() {
        let mut c = LiveControls::default();
        let t = LiveTune { previous: Some(2), ..tune(0) };
        c.tick(Some(&t), 0);
        // A tap: nothing on the press, the banner toggles on the release.
        assert_eq!(c.ok(&t, Edge::Down, 6_000), LiveAction::None);
        assert!(!c.banner_visible(6_000), "nothing on the press");
        assert_eq!(c.ok(&t, Edge::Repeat, 6_100), LiveAction::None, "a short hold is still a tap");
        assert_eq!(c.ok(&t, Edge::Up, 6_200), LiveAction::None);
        assert!(c.banner_visible(6_200), "the tap showed the banner");
        // A hold: the repeat past HOLD_MS recalls, once, and the release does nothing more.
        c.ok(&t, Edge::Down, 9_000);
        assert_eq!(tuned_to(c.ok(&t, Edge::Repeat, 9_000 + HOLD_MS)), Some(2), "recall");
        assert_eq!(c.ok(&t, Edge::Repeat, 9_000 + HOLD_MS + 50), LiveAction::None, "once per hold");
        let banner = c.banner_until;
        assert_eq!(c.ok(&t, Edge::Up, 9_900), LiveAction::None);
        assert_eq!(c.banner_until, banner, "the release of a hold is not a tap");
    }

    #[test]
    fn recall_with_nowhere_to_go_back_shows_where_we_are() {
        let mut c = LiveControls::default();
        let t = tune(1);
        c.tick(Some(&t), 0);
        c.ok(&t, Edge::Down, 7_000);
        assert_eq!(c.ok(&t, Edge::Repeat, 7_000 + HOLD_MS), LiveAction::None);
        assert!(c.banner_visible(7_000 + HOLD_MS));
    }

    #[test]
    fn a_lost_ok_release_is_forgotten_rather_than_read_as_a_hold() {
        let mut c = LiveControls::default();
        let t = LiveTune { previous: Some(2), ..tune(0) };
        c.tick(Some(&t), 0);
        c.ok(&t, Edge::Down, 1_000);
        c.tick(Some(&t), 1_000 + OK_STALE_MS);
        assert_eq!(c.ok_down, None);
        assert_eq!(c.ok(&t, Edge::Repeat, 1_000 + OK_STALE_MS + 10), LiveAction::None, "no press, no hold");
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
    fn a_number_only_one_channel_can_be_tunes_at_once() {
        let mut c = LiveControls::default();
        let t = tune(0);
        c.tick(Some(&t), 0);
        assert_eq!(tuned_to(c.key(&t, LivePress::Digit(5), 100)), Some(2), "nothing longer starts with 5");
        c.key(&t, LivePress::Digit(1), 200);
        assert_eq!(tuned_to(c.key(&t, LivePress::Digit(1), 300)), Some(3), "11 is as long as it gets");
    }

    #[test]
    fn a_number_that_could_go_on_waits_for_the_pause_or_ok() {
        let mut c = LiveControls::default();
        let t = tune(1);
        c.tick(Some(&t), 0);
        assert_eq!(c.key(&t, LivePress::Digit(1), 100), LiveAction::None, "1 could still be 11");
        assert_eq!(c.tick(Some(&t), 100 + TYPED_COMMIT_MS - 1), LiveAction::None, "still typing");
        assert_eq!(tuned_to(c.tick(Some(&t), 100 + TYPED_COMMIT_MS)), Some(0), "1 after the pause");
        c.key(&t, LivePress::Digit(1), 5_000);
        assert_eq!(tuned_to(c.key(&t, LivePress::Ok, 5_100)), Some(0), "1 at once on OK");
    }

    #[test]
    fn a_number_as_long_as_the_lineup_allows_is_reachable() {
        let mut c = LiveControls::default();
        let mut t = tune(0);
        let mut lineup = (*t.lineup).clone();
        lineup.channels[3].number = "10001".into();
        t.lineup = Arc::new(lineup);
        c.tick(Some(&t), 0);
        for d in [1, 0, 0, 0] {
            assert_eq!(c.key(&t, LivePress::Digit(d), 10), LiveAction::None);
        }
        assert_eq!(tuned_to(c.key(&t, LivePress::Digit(1), 20)), Some(3), "five digits, no cap");
    }

    #[test]
    fn a_number_no_channel_has_says_so_and_the_one_playing_just_shows_the_banner() {
        let mut c = LiveControls::default();
        let t = tune(1);
        c.tick(Some(&t), 0);
        assert_eq!(c.key(&t, LivePress::Digit(9), 6_000), LiveAction::None);
        assert!(c.typed.is_empty(), "no channel starts with 9: settled at once");
        assert!(c.missing_visible(6_000) && c.missing == "9");
        assert!(!c.missing_visible(6_000 + MISSING_MS));
        assert_eq!(c.key(&t, LivePress::Digit(4), 10_000), LiveAction::None, "4 is already playing");
        assert!(c.banner_visible(10_000));
    }
}
