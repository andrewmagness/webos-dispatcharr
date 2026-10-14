//! **The guide: which programme is on which channel**, built from three reads — the channel
//! summary, the EPG rows and the grid — and the rolling window it is fetched for.
//!
//! The grid names a programme only by `tvg_id`; a channel names its EPG row by `epg_data_id`, and
//! the row carries the `tvg_id`. A channel with no EPG row, or one assigned to a DUMMY source, is
//! given generated programmes whose `tvg_id` is the channel's own UUID (Dispatcharr v0.32.0
//! `apps/epg/api_grid.py`, `_iter_dummy_for_channels`). So every channel is indexed under both of
//! its keys, and a programme lands on every channel either key names.
//!
//! The grid returns `tvg_id` and nothing about which EPG source a programme came from, so two
//! sources that share a `tvg_id` string are indistinguishable here: their programmes are merged,
//! and exact duplicates (same start, end and title) are dropped.

use std::collections::HashMap;

use super::models::{Channel, EpgData, Programme};

/// The span of time a guide is fetched for, in Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: i64,
    pub end: i64,
}

impl Window {
    /// How far back the guide reaches: the programme that started before now is still on screen.
    pub const BACK: i64 = 3600;
    /// How far forward the guide reaches.
    pub const FORWARD: i64 = 12 * 3600;
    /// Refetch at least this often, so a schedule change on the server reaches the TV.
    pub const MAX_AGE: i64 = 30 * 60;
    /// Refetch once the remaining forward reach is shorter than this.
    pub const MIN_AHEAD: i64 = 3 * 3600;

    /// The window for a fetch made at `now`, aligned down to the half hour the grid's columns use.
    pub fn around(now: i64) -> Window {
        let anchor = now - now.rem_euclid(1800);
        Window { start: anchor - Self::BACK, end: anchor + Self::FORWARD }
    }

    /// Whether a guide fetched at `fetched` for this window should be fetched again at `now`.
    pub fn stale(&self, fetched: i64, now: i64) -> bool {
        now - fetched >= Self::MAX_AGE || self.end - now < Self::MIN_AHEAD || now < self.start
    }

    pub fn contains(&self, t: i64) -> bool {
        self.start <= t && t < self.end
    }
}

/// Programmes per channel id, each list sorted by start time.
#[derive(Debug, Clone, Default)]
pub struct Guide {
    window: Option<Window>,
    by_channel: HashMap<i64, Vec<Programme>>,
}

impl Guide {
    /// Map `programmes` (each with the `tvg_id` the grid gave it) onto `channels`.
    pub fn build(
        channels: &[Channel],
        epg: &[EpgData],
        programmes: impl IntoIterator<Item = (String, Programme)>,
        window: Window,
    ) -> Guide {
        let tvg_of: HashMap<i64, &str> = epg
            .iter()
            .filter_map(|row| Some((row.id, row.tvg_id.as_deref().filter(|t| !t.is_empty())?)))
            .collect();
        let mut channels_of: HashMap<&str, Vec<i64>> = HashMap::new();
        for c in channels {
            channels_of.entry(c.uuid.as_str()).or_default().push(c.id);
            if let Some(tvg) = c.epg_data_id.and_then(|id| tvg_of.get(&id)) {
                let ids = channels_of.entry(tvg).or_default();
                if !ids.contains(&c.id) {
                    ids.push(c.id);
                }
            }
        }
        let mut by_channel: HashMap<i64, Vec<Programme>> = HashMap::new();
        for (tvg, programme) in programmes {
            if programme.end <= window.start || programme.start >= window.end {
                continue;
            }
            if let Some(ids) = channels_of.get(tvg.as_str()) {
                for id in ids {
                    by_channel.entry(*id).or_default().push(programme.clone());
                }
            }
        }
        for list in by_channel.values_mut() {
            list.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)).then_with(|| a.title.cmp(&b.title)));
            list.dedup_by(|a, b| a.start == b.start && a.end == b.end && a.title == b.title);
        }
        Guide { window: Some(window), by_channel }
    }

    /// The window this guide was built for; `None` for an empty guide that was never fetched.
    pub fn window(&self) -> Option<Window> {
        self.window
    }

    /// A channel's programmes in start order (empty when it has none in the window).
    pub fn programmes(&self, channel_id: i64) -> &[Programme] {
        self.by_channel.get(&channel_id).map_or(&[], Vec::as_slice)
    }

    /// The programme on `channel_id` at `t`.
    pub fn at(&self, channel_id: i64, t: i64) -> Option<&Programme> {
        // The last programme starting at or before `t`; overlapping listings resolve to the one
        // that started latest, which is what is actually on.
        self.index_at(channel_id, t).map(|i| &self.programmes(channel_id)[i])
    }

    /// Now and next on `channel_id` at `t`. "Next" is the first programme starting at or after the
    /// current one ends, or after `t` when nothing is on.
    pub fn now_next(&self, channel_id: i64, t: i64) -> (Option<&Programme>, Option<&Programme>) {
        let now = self.at(channel_id, t);
        let after = now.map_or(t, |p| p.end);
        let next = self.programmes(channel_id).iter().find(|p| p.start >= after);
        (now, next)
    }

    /// How many channels have at least one programme.
    pub fn channels_with_programmes(&self) -> usize {
        self.by_channel.values().filter(|l| !l.is_empty()).count()
    }
}

/// One cell of a channel's lane: a span of the window that is either a listed programme (its index
/// in [`Guide::programmes`]) or a gap the server lists nothing for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub start: i64,
    pub end: i64,
    pub programme: Option<usize>,
}

impl Slot {
    /// Whether instant `t` falls in this slot (start inclusive, end exclusive).
    pub fn contains(&self, t: i64) -> bool {
        self.start <= t && t < self.end
    }
}

impl Guide {
    /// Channel `channel_id`'s lane: the window tiled end to end, with no gaps and no overlaps, so
    /// every instant in it falls in exactly one slot — what a grid's cursor walks. Each slot is
    /// what [`Guide::at`] answers for its instants (an overlap goes to the listing that started
    /// latest, and the earlier one resumes when that ends), merged where neighbours agree; the
    /// stretches the server lists nothing for are `programme: None`. The first and last slots are
    /// clipped to the window. An empty guide (never fetched) has no lane at all.
    pub fn lane(&self, channel_id: i64) -> Vec<Slot> {
        let Some(window) = self.window else { return Vec::new() };
        let list = self.programmes(channel_id);
        let mut edges: Vec<i64> = list
            .iter()
            .flat_map(|p| [p.start, p.end])
            .chain([window.start, window.end])
            .filter(|t| window.start <= *t && *t <= window.end)
            .collect();
        edges.sort_unstable();
        edges.dedup();
        let mut lane: Vec<Slot> = Vec::new();
        for pair in edges.windows(2) {
            let (start, end) = (pair[0], pair[1]);
            let programme = self.index_at(channel_id, start);
            match lane.last_mut() {
                Some(last) if last.programme == programme => last.end = end,
                _ => lane.push(Slot { start, end, programme }),
            }
        }
        lane
    }

    /// [`Guide::at`]'s answer as an index into [`Guide::programmes`].
    fn index_at(&self, channel_id: i64, t: i64) -> Option<usize> {
        let list = self.programmes(channel_id);
        let i = list.partition_point(|p| p.start <= t);
        (0..i).rev().find(|&j| list[j].airs_at(t))
    }
}

/// The slot of `lane` that `t` falls in, clamped to the lane's ends: what a cursor moving up or
/// down a grid lands on.
pub fn slot_at(lane: &[Slot], t: i64) -> Option<usize> {
    if lane.is_empty() {
        return None;
    }
    let i = lane.partition_point(|s| s.end <= t);
    Some(i.min(lane.len() - 1))
}
