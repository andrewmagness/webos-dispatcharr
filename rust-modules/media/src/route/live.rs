//! **Live TV's lineup, as the player sees it.** A tune from the Guide hands the session the whole
//! lineup the Guide was showing (shared, read-only) and the index being watched, so the player can
//! change channel, take a typed number, recall the last channel and draw now/next without asking
//! the Guide or the server for anything.
//!
//! Plain data and functions of their arguments, like [`super::plan`]: no session, no clock. The
//! wall-clock second is always passed in. Built by the Guide from the Dispatcharr data; this crate
//! names no Dispatcharr type.

use std::sync::Arc;

/// One programme on a channel, in Unix seconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveProgramme {
    pub start: i64,
    pub end: i64,
    pub title: String,
}

/// One channel of the lineup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveChannel {
    /// The server's channel id: what the Guide keys its rows by.
    pub id: i64,
    /// The channel's UUID: what the viewer's choices (favourites, the last channel) are stored by,
    /// since a server keeps it when it renumbers or renames the channel.
    pub uuid: String,
    /// The number as a viewer reads and types it (`"2"`, `"2.1"`); empty when it has none.
    pub number: String,
    pub name: String,
    /// The live stream URL; empty for a channel the server gave no stream for.
    pub url: String,
    pub logo: Option<String>,
    /// Sorted by start, non-overlapping.
    pub programmes: Vec<LiveProgramme>,
}

impl LiveChannel {
    /// The programme airing at `t`.
    pub fn on_at(&self, t: i64) -> Option<&LiveProgramme> {
        self.programmes.iter().find(|p| p.start <= t && t < p.end)
    }

    /// The first programme that starts after `t`.
    pub fn next_after(&self, t: i64) -> Option<&LiveProgramme> {
        self.programmes.iter().find(|p| p.start > t)
    }

    pub fn tunable(&self) -> bool {
        !self.url.is_empty()
    }
}

/// The lineup in Guide order, and the Load declaration its streams are played with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveLineup {
    pub channels: Vec<LiveChannel>,
    pub vcodec: String,
    pub acodec: String,
}

impl LiveLineup {
    /// The tunable channel `delta` steps from `from` (positive = up the list, toward higher
    /// numbers), wrapping at either end. `None` when no other channel can be tuned.
    pub fn step(&self, from: usize, delta: i32) -> Option<usize> {
        let n = self.channels.len();
        if n == 0 || delta == 0 {
            return None;
        }
        let dir: isize = if delta > 0 { 1 } else { -1 };
        let mut at = from.min(n - 1) as isize;
        let mut left = delta.unsigned_abs() as usize;
        for _ in 0..n * left.max(1) {
            at = (at + dir).rem_euclid(n as isize);
            if at as usize == from {
                return None;
            }
            if self.channels[at as usize].tunable() {
                left -= 1;
                if left == 0 {
                    return Some(at as usize);
                }
            }
        }
        None
    }

    /// The tunable channel whose number is exactly `typed`. A typed `"7"` also finds `"7.0"`, the
    /// way a server that numbers in floats prints a whole channel.
    pub fn by_number(&self, typed: &str) -> Option<usize> {
        let typed = typed.trim();
        if typed.is_empty() {
            return None;
        }
        self.channels.iter().position(|c| {
            c.tunable() && (c.number == typed || c.number.strip_suffix(".0") == Some(typed))
        })
    }
}

/// What the session is tuned to: the lineup, the channel being watched, and the one before it
/// (what recall returns to).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveTune {
    pub lineup: Arc<LiveLineup>,
    pub index: usize,
    pub previous: Option<usize>,
    /// The wall-clock second the tune was made (0 until [`super::request_live`] stamps it). The
    /// player reads the time of day from this and its own frame clock, so a recorded session
    /// replays without asking the system clock.
    pub at: i64,
}

impl LiveTune {
    pub fn channel(&self) -> Option<&LiveChannel> {
        self.lineup.channels.get(self.index)
    }

    /// The same lineup tuned to `index`, remembering this channel as the previous one.
    pub fn to(&self, index: usize) -> LiveTune {
        LiveTune { lineup: Arc::clone(&self.lineup), index, previous: Some(self.index), at: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(id: i64, number: &str, url: &str) -> LiveChannel {
        LiveChannel { id, uuid: format!("u-{id}"), number: number.into(), name: format!("c{id}"), url: url.into(), logo: None,
            programmes: Vec::new() }
    }

    fn lineup(chs: Vec<LiveChannel>) -> LiveLineup {
        LiveLineup { channels: chs, vcodec: "h264".into(), acodec: "aac".into() }
    }

    #[test]
    fn stepping_wraps_and_skips_channels_with_no_stream() {
        let l = lineup(vec![ch(1, "1", "u1"), ch(2, "2", ""), ch(3, "3", "u3"), ch(4, "4", "u4")]);
        assert_eq!(l.step(0, 1), Some(2), "channel 2 has no stream");
        assert_eq!(l.step(3, 1), Some(0), "wraps past the end");
        assert_eq!(l.step(0, -1), Some(3), "wraps past the start");
        assert_eq!(l.step(0, 2), Some(3));
        assert_eq!(l.step(0, 0), None);
        let alone = lineup(vec![ch(1, "1", "u1"), ch(2, "2", "")]);
        assert_eq!(alone.step(0, 1), None, "nothing else to tune");
    }

    #[test]
    fn a_typed_number_finds_its_channel() {
        let l = lineup(vec![ch(1, "2", "u"), ch(2, "7.0", "u"), ch(3, "7.1", "u"), ch(4, "9", "")]);
        assert_eq!(l.by_number("2"), Some(0));
        assert_eq!(l.by_number("7"), Some(1), "a float-numbered whole channel");
        assert_eq!(l.by_number("7.1"), Some(2));
        assert_eq!(l.by_number("9"), None, "no stream to tune");
        assert_eq!(l.by_number("5"), None);
        assert_eq!(l.by_number(""), None);
    }

    #[test]
    fn now_and_next_come_from_the_programmes() {
        let mut c = ch(1, "1", "u");
        c.programmes = vec![
            LiveProgramme { start: 0, end: 1800, title: "A".into() },
            LiveProgramme { start: 1800, end: 3600, title: "B".into() },
        ];
        assert_eq!(c.on_at(100).map(|p| p.title.as_str()), Some("A"));
        assert_eq!(c.next_after(100).map(|p| p.title.as_str()), Some("B"));
        assert_eq!(c.on_at(1800).map(|p| p.title.as_str()), Some("B"), "end is exclusive");
        assert!(c.on_at(3600).is_none() && c.next_after(3600).is_none());
    }

    #[test]
    fn a_tune_remembers_where_it_came_from() {
        let t = LiveTune { lineup: Arc::new(lineup(vec![ch(1, "1", "u"), ch(2, "2", "u")])), index: 0, previous: None, at: 7 };
        let u = t.to(1);
        assert_eq!((u.index, u.previous, u.at), (1, Some(0), 0), "a new tune is stamped when it is made");
        assert!(Arc::ptr_eq(&t.lineup, &u.lineup), "the lineup is shared, not copied");
    }
}
