//! **The Guide — what is on, channel by channel.** The app's home once a Dispatcharr account is
//! stored: a header (the page's name, the time, the group and favourites switches, *Account*), the
//! focused programme's details, and the grid — every channel a row, its programmes laid against a
//! two-hour time axis ([`plx_ui::time_grid`]), with a marker at the current time.
//!
//! **The lattice.** Each channel's row is its lane ([`Guide::lane`]): the fetched window tiled end
//! to end, so every instant falls in exactly one cell and the stretches the server lists nothing
//! for are cells too ("No information"). An element is `row << SLOT_BITS | slot`, so the focus
//! engine walks rows and cells without this page keeping a cursor of its own.
//!
//! **Walking.** LEFT and RIGHT step a row's cells. UP and DOWN keep a TIME, not a column: the
//! anchor is where the last sideways step left the cursor (the focused cell's start, or the view's
//! left edge when the cell began before it), and the row above or below lands on whatever airs at
//! that instant, so a walk down the channels at 21:00 stays at 21:00 however the programmes are cut.
//! The view's left edge rests on half-hour boundaries and moves only when the cursor's cell starts
//! outside it; the first row moves the least that keeps the cursor's row whole. Both glide.
//!
//! **Time.** The page reads no clock of its own: each answer carries the wall-clock second it was
//! fetched at, and the page counts on from there with its logical ticks, so a replay's ticks give
//! the same "now". The guide is fetched again when its window goes stale ([`Window::stale`]): the
//! old one stays on screen, and the cursor keeps its channel and its time across the landing.
//!
//! **Nothing here blocks**: the account file and the three reads are the application's to run
//! ([`DispatcharrCmd::Guide`], on a worker), answered on the channel this page holds.

use std::borrow::Cow;
use std::ffi::CString;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;

use plx_dispatcharr::dispatcharr::guide::{slot_at, Guide, Slot, Window};
use plx_dispatcharr::dispatcharr::models::{Channel, Group, Programme};
use plx_dispatcharr::dispatcharr::Error;
use plx_media::route::{LiveChannel, LiveLineup, LiveProgramme, LiveTune};
use plx_machine::machine::{
    Canon, Cx, Delivery, Edge, Effects, EntryId, FocusKey, Fx, GroupId, Handled, InputEvent,
    InputKind, InstanceId, Key, LogicalState, Machine, MachineId, Tick,
};
use plx_machine::present::Provenance;
use plx_platform::i18n::{self, msg};
use plx_ui::frame::Budget;
use plx_ui::label::Label;
use plx_ui::route_screen::RouteGround;
use plx_ui::screen::{
    Activate, At, AxisMask, Dir, DrawFrame, EdgeRule, ElemKind, Enter, FocusSource, FocusTarget,
    Focusable, GroupKind, GroupSpec, HitSource, Hover, Placed, RenderStrategy, Screen, ScreenEvent,
    Seat, Step, Stop,
};
use plx_ui::text_view::TextView;
use plx_ui::time_grid::{self, CellLook, View as GridView};
use plx_ui::widgets::{Button, StatusKind, StatusOverlay};
use plx_ui::{consts, theme, Env, Painter, Rect, Spring, View};

use super::registry::{word, AppFx, AppLike, ContentReq, DispatcharrCmd, GuideOutcome, LoopReq, PlayIntent, PlayerLike};

/// The header's one control. Below `STRIP_BASE`: every element from there up is the shared tab
/// strip's (`plx_ui::dispatch::PageWithStrip`), and the page is never asked about one.
const ACCOUNT: u32 = plx_ui::dispatch::STRIP_BASE - 1;
/// A failed read-out's action (*Try again*, or *Sign in again* for a refused key).
const RETRY: u32 = plx_ui::dispatch::STRIP_BASE - 2;
/// The header's favourites switch: every channel, or only the favourites.
const FAVOURITES: u32 = plx_ui::dispatch::STRIP_BASE - 3;
/// The header's group switch: every group, or one channel group; OK steps to the next. The lowest
/// reserved element: everything below it is a cell.
const GROUP: u32 = plx_ui::dispatch::STRIP_BASE - 4;
/// A cell's element: its row above these bits, its slot in them. A lane is capped at
/// `1 << SLOT_BITS` cells — a 13-hour window of one-minute programmes is 780.
const SLOT_BITS: u32 = 12;
const MAX_SLOTS: usize = 1 << SLOT_BITS;
/// The widest the group switch grows: a long group name is cut, not the header's other controls.
const GROUP_MAX_W: f32 = 420.0;
/// Rows that fit beside the reserved elements above.
const MAX_ROWS: usize = ((GROUP >> SLOT_BITS) - 1) as usize;

/// The grid. `GroupId(0)` is the container's own fresh-mount target (`stack.rs::fresh`).
const GRID: GroupId = GroupId(0);
const HEADER: GroupId = GroupId(1);
const STATUS: GroupId = GroupId(2);

fn cell(row: usize, slot: usize) -> u32 {
    ((row as u32) << SLOT_BITS) | slot as u32
}

/// A cell element's `(row, slot)`; `None` for the reserved elements.
fn split(elem: u32) -> Option<(usize, usize)> {
    (elem < GROUP).then(|| ((elem >> SLOT_BITS) as usize, (elem & (MAX_SLOTS as u32 - 1)) as usize))
}

/// The recorded shape of [`GuideScreen`]'s logical state (`registry::SCREEN_SHAPES`). Instants are
/// written as their `u64` bit patterns.
pub const SHAPE: &str = "GuideScreen{phase:u8,rows:u64,window:Option<(u64,u64)>,anchor:u64,view_start:u64,\
    view_top:f32,pending:bool,failure:Option<u8>,favourites:u64,only_favourites:bool,group:Option<u64>}";

mod layout {
    use plx_ui::consts::{MARGIN_X, MARGIN_Y, SCR_H, SCR_W};
    use plx_ui::time_grid::TimeGrid;
    use plx_ui::{theme, Rect};

    pub const HEADER_H: f32 = 60.0;
    pub const DETAIL_TOP: f32 = MARGIN_Y + HEADER_H + theme::space::MD;
    pub const DETAIL_H: f32 = 228.0;
    pub const GRID_TOP: f32 = DETAIL_TOP + DETAIL_H + theme::space::MD;

    pub fn header() -> Rect {
        Rect::new(MARGIN_X, MARGIN_Y, SCR_W - 2.0 * MARGIN_X, HEADER_H)
    }

    pub fn detail() -> Rect {
        Rect::new(MARGIN_X, DETAIL_TOP, (SCR_W - 2.0 * MARGIN_X) * 0.62, DETAIL_H)
    }

    /// The corner preview: 16:9 at the detail block's height, flush with the grid's right edge,
    /// in the band the detail block leaves empty beside it.
    pub fn preview() -> Rect {
        let w = DETAIL_H * 16.0 / 9.0;
        Rect::new(SCR_W - MARGIN_X - w, DETAIL_TOP, w, DETAIL_H)
    }

    pub fn grid() -> TimeGrid {
        TimeGrid {
            frame: Rect::new(MARGIN_X, GRID_TOP, SCR_W - 2.0 * MARGIN_X, SCR_H - MARGIN_Y - GRID_TOP),
            label_w: 440.0,
            axis_h: 52.0,
            row_h: 84.0,
            row_gap: 8.0,
            span: 2 * 3600,
            slot: 1800,
        }
    }
}

/// **Where the channel being watched plays while the Guide is up** (`PlaybackSession::preview`), on
/// the 1920x1080 canvas: the app places the video window here and the Guide leaves it transparent.
pub fn preview_rect() -> Rect {
    layout::preview()
}

/// Where the page is.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Loading,
    Ready,
    /// Signed in, and the account can watch nothing.
    Empty,
    Failed(Error),
}

impl Phase {
    fn disc(&self) -> u8 {
        match self {
            Phase::Loading => 0,
            Phase::Ready => 1,
            Phase::Empty => 2,
            Phase::Failed(_) => 3,
        }
    }
}

/// Everything the last fetch brought, whatever the page is showing of it: the favourites and group
/// switches filter this into a [`Listing`] without asking the server again.
struct Source {
    channels: Vec<Channel>,
    /// The channel groups that hold at least one of `channels`, in the server's order: what the
    /// group switch steps through.
    groups: Vec<Group>,
    logos: Vec<Option<String>>,
    streams: Vec<String>,
    guide: Guide,
    fetched_at: i64,
}

/// Where a [`GuideScreen::relist`] puts focus.
#[derive(Clone, Copy, Debug)]
enum Refocus {
    /// Onto a cell: the carried `(channel id, time)` when that channel is still listed, else row
    /// `row` (or the last row) at that time, or at now.
    Cell { carry: Option<(i64, i64)>, row: usize },
    /// Leave it where it is (the header switch keeps it).
    Keep,
}

/// What the page shows: the channels in guide order, their programmes, and each row's lane.
struct Listing {
    channels: Vec<Channel>,
    /// Each channel's logo URL (`None` for one without), row for row.
    logos: Vec<Option<String>>,
    /// What OK hands the player: every row as a live channel, built once per landing.
    lineup: Arc<LiveLineup>,
    /// Which rows are favourites.
    stars: Vec<bool>,
    guide: Guide,
    lanes: Vec<Vec<Slot>>,
    /// Each channel's number as a viewer reads it, kept so a frame does not format sixty of them.
    numbers: Vec<String>,
    window: Window,
    fetched_at: i64,
}

impl Listing {
    fn new(mut channels: Vec<Channel>, mut logos: Vec<Option<String>>, mut streams: Vec<String>,
        favourites: &[String], guide: Guide, fetched_at: i64) -> Option<Self> {
        let window = guide.window()?;
        channels.truncate(MAX_ROWS);
        // Row for row with the channels whatever the answer held: a short list means no logos.
        logos.resize(channels.len(), None);
        streams.resize(channels.len(), String::new());
        let lineup = Arc::new(LiveLineup {
            channels: channels
                .iter()
                .zip(streams)
                .zip(&logos)
                .map(|((c, url), logo)| LiveChannel {
                    id: c.id,
                    uuid: c.uuid.clone(),
                    number: c.number_label(),
                    name: c.name.clone(),
                    url,
                    logo: logo.clone(),
                    programmes: guide
                        .programmes(c.id)
                        .iter()
                        .map(|p| LiveProgramme { start: p.start, end: p.end, title: p.title.clone() })
                        .collect(),
                })
                .collect(),
            // Every channel sampled on a real server (2026-10-14) was H.264 + AAC. The Load must
            // be declared before the stream is opened; reading it from the stream is M5's work.
            vcodec: "h264".to_owned(),
            acodec: "aac".to_owned(),
        });
        let lanes = channels
            .iter()
            .map(|c| {
                let mut lane = guide.lane(c.id);
                lane.truncate(MAX_SLOTS);
                lane
            })
            .collect();
        let numbers = channels.iter().map(Channel::number_label).collect();
        let stars = channels.iter().map(|c| favourites.contains(&c.uuid)).collect();
        Some(Listing { channels, logos, lineup, stars, guide, lanes, numbers, window, fetched_at })
    }

    fn rows(&self) -> usize {
        self.channels.len()
    }

    fn slot(&self, row: usize, slot: usize) -> Option<&Slot> {
        self.lanes.get(row)?.get(slot)
    }

    fn programme(&self, row: usize, slot: &Slot) -> Option<&Programme> {
        let channel = self.channels.get(row)?;
        slot.programme.and_then(|i| self.guide.programmes(channel.id).get(i))
    }

    /// The element airing at `t` on `row`.
    fn at(&self, row: usize, t: i64) -> Option<u32> {
        let slot = slot_at(self.lanes.get(row)?, t)?;
        Some(cell(row, slot))
    }
}

/// The wall clock as this page knows it: one second the application read, and the logical tick it
/// arrived on.
#[derive(Clone, Copy, Debug)]
struct WallClock {
    unix: i64,
    ms: u32,
}

impl WallClock {
    fn now(&self, ms: u32) -> i64 {
        self.unix + (ms.wrapping_sub(self.ms) / 1000) as i64
    }
}

/// The logical state this page records (`LogicalState`), refreshed after every change.
struct GuideState {
    phase: u8,
    rows: usize,
    window: Option<(i64, i64)>,
    anchor: i64,
    view_start: f64,
    view_top: f32,
    pending: bool,
    failure: Option<u8>,
    favourites: usize,
    only_favourites: bool,
    group: Option<i64>,
}

impl LogicalState for GuideState {
    fn write(&self, c: &mut Canon) {
        c.u8(self.phase).u64(self.rows as u64);
        c.option(self.window, |c, (a, b)| {
            c.u64(a as u64).u64(b as u64);
        });
        c.u64(self.anchor as u64).u64(self.view_start.to_bits()).f32(self.view_top).bool(self.pending);
        c.option(self.failure, |c, d| {
            c.u8(d);
        });
        c.u64(self.favourites as u64).bool(self.only_favourites);
        c.option(self.group, |c, g| {
            c.u64(g as u64);
        });
    }
    fn probe(&self, out: &mut String) {
        out.push_str(&format!("guide phase={} rows={} pending={}", self.phase, self.rows, self.pending));
    }
}

pub struct GuideScreen {
    entry: EntryId,
    instance: InstanceId,
    phase: Phase,
    source: Option<Source>,
    listing: Option<Listing>,
    /// The favourite channels' UUIDs, in the order they were added (what is saved).
    favourites: Vec<String>,
    /// The page shows only the favourites.
    only_favourites: bool,
    /// The page shows only this channel group's channels.
    group: Option<i64>,
    pending: Option<Receiver<GuideOutcome>>,
    clock: Option<WallClock>,
    /// The latest tick's wall-clock second (0 until the first answer).
    now: i64,
    /// The `now` the last drawn frame put the marker at: the marker moves a pixel every few
    /// seconds, so the page asks for a frame only when it would move.
    drawn_now: i64,
    /// Where the view is going; the lags below are how far behind it the drawn view still is, in
    /// pixels, each gliding to zero.
    view: GridView,
    start_lag: Spring,
    top_lag: Spring,
    /// The instant UP and DOWN keep.
    anchor: i64,
    /// The cursor's channel and time, carried across a refetch so the landing does not move it.
    carry: Option<(i64, i64)>,
    spin_ms: f32,
    /// The focused row's label crossfade, per row id.
    hot_row: Option<(usize, Spring)>,
    cell_hot: Spring,
    ground: RouteGround,
    state: GuideState,
}

impl GuideScreen {
    pub fn new(entry: EntryId, instance: InstanceId) -> Self {
        let mut screen = Self {
            entry,
            instance,
            phase: Phase::Loading,
            source: None,
            listing: None,
            favourites: Vec::new(),
            only_favourites: false,
            group: None,
            pending: None,
            clock: None,
            now: 0,
            drawn_now: 0,
            view: GridView { start: 0.0, top: 0.0 },
            start_lag: Spring::at(0.0),
            top_lag: Spring::at(0.0),
            anchor: 0,
            carry: None,
            spin_ms: 0.0,
            hot_row: None,
            cell_hot: Spring::at(0.0),
            ground: RouteGround::new(),
            state: GuideState {
                phase: 0, rows: 0, window: None, anchor: 0, view_start: 0.0, view_top: 0.0, pending: false,
                failure: None, favourites: 0, only_favourites: false, group: None,
            },
        };
        screen.sync_state();
        screen
    }

    fn key(&self, elem: u32) -> FocusKey<u32> {
        FocusKey { entry: self.entry, elem }
    }

    fn sync_state(&mut self) {
        self.state = GuideState {
            phase: self.phase.disc(),
            rows: self.listing.as_ref().map_or(0, Listing::rows),
            window: self.listing.as_ref().map(|l| (l.window.start, l.window.end)),
            anchor: self.anchor,
            view_start: self.view.start,
            view_top: self.view.top,
            pending: self.pending.is_some(),
            failure: match &self.phase {
                Phase::Failed(e) => Some(failure_disc(e)),
                _ => None,
            },
            favourites: self.favourites.len(),
            only_favourites: self.only_favourites,
            group: self.group,
        };
    }

    /// The view as drawn this frame: the target, less the lags still gliding.
    fn drawn_view(&self) -> GridView {
        let grid = layout::grid();
        GridView {
            start: self.view.start + self.start_lag.pos as f64 / grid.px_per_sec(),
            top: self.view.top + self.top_lag.pos / grid.pitch(),
        }
    }

    fn view_for(&self, at: At) -> GridView {
        match at {
            At::SpringTarget => self.view,
            At::Drawn => self.drawn_view(),
        }
    }

    /// Move the view's target, keeping what is drawn where it is (the lag absorbs the jump).
    fn retarget(&mut self, view: GridView) {
        let grid = layout::grid();
        self.start_lag.pos += ((self.view.start - view.start) * grid.px_per_sec()) as f32;
        self.top_lag.pos += (self.view.top - view.top) * grid.pitch();
        self.view = view;
    }

    fn request<H: AppLike>(&mut self, fx: &mut Effects<'_, H>) {
        let (reply, rx) = mpsc::channel();
        self.pending = Some(rx);
        fx.push(Fx::App(AppFx::Dispatcharr(DispatcharrCmd::Guide { reply })));
    }

    fn reseat<H: AppLike>(&self, elem: u32, fx: &mut Effects<'_, H>) {
        fx.push(Fx::Deliver(
            MachineId::Instance(self.instance),
            Delivery::Screen(ScreenEvent::Enter(Enter::Fresh { focus: FocusTarget::Elem(self.key(elem)) })),
        ));
    }

    /// Read whatever the application has answered since the last tick.
    fn poll<H: AppLike>(&mut self, ms: u32, fx: &mut Effects<'_, H>) {
        let Some(rx) = &self.pending else { return };
        let outcome = match rx.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.pending = None;
                // A refused worker: a page still loading says so; one with a guide keeps it.
                if self.listing.is_none() {
                    self.phase = Phase::Failed(Error::Unreachable);
                    self.reseat(RETRY, fx);
                }
                fx.invalidate(Provenance::Input);
                return;
            }
        };
        self.pending = None;
        self.land(outcome, ms, fx);
        fx.invalidate(Provenance::Input);
    }

    fn land<H: AppLike>(&mut self, outcome: GuideOutcome, ms: u32, fx: &mut Effects<'_, H>) {
        match outcome {
            GuideOutcome::SignedOut => fx.push(Fx::App(AppFx::Loop(LoopReq::DispatcharrSetup))),
            GuideOutcome::Failed(e) => {
                // A guide already on screen stays: the next refresh will try again.
                if self.listing.is_none() || e == Error::Unauthorized {
                    self.listing = None;
                    self.phase = Phase::Failed(e);
                    self.reseat(RETRY, fx);
                }
            }
            GuideOutcome::Loaded { channels, groups, logos, streams, favourites, resume, guide, fetched_at } => {
                self.clock = Some(WallClock { unix: fetched_at, ms });
                self.now = fetched_at;
                self.drawn_now = fetched_at;
                self.favourites = favourites;
                // The channel to start on (the last one watched, on the run's first guide).
                let resume = resume.and_then(|uuid| channels.iter().find(|c| c.uuid == uuid).map(|c| c.id));
                let groups: Vec<Group> =
                    groups.into_iter().filter(|g| channels.iter().any(|c| c.channel_group_id == Some(g.id))).collect();
                // A group the server no longer lists (or that has lost its channels) shows everything.
                if self.group.is_some_and(|id| !groups.iter().any(|g| g.id == id)) {
                    self.group = None;
                }
                self.source = Some(Source { channels, groups, logos, streams, guide, fetched_at });
                // The channel and the time under the cursor, before the old listing goes; or the
                // channel being resumed, so BACK out of it lands on its row.
                let carry = resume.map(|id| (id, fetched_at)).or(self.carry.take());
                self.relist(Refocus::Cell { carry, row: 0 }, fx);
                let row = resume.and_then(|id| self.listing.as_ref()?.channels.iter().position(|c| c.id == id));
                if let Some(play) = row.and_then(|row| self.tune(cell(row, 0))) {
                    fx.push(Fx::App(AppFx::Content(ContentReq::Play { play })));
                }
            }
        }
    }

    /// Rebuild what the page shows from the last fetch, the favourites and the switches, and say
    /// where focus goes: back onto a cell (the carried channel, else row `row` or the last one),
    /// or nowhere new.
    fn relist<H: AppLike>(&mut self, refocus: Refocus, fx: &mut Effects<'_, H>) {
        let Some(src) = &self.source else { return };
        let keep: Vec<usize> = (0..src.channels.len())
            .filter(|&i| !self.only_favourites || self.favourites.contains(&src.channels[i].uuid))
            .filter(|&i| self.group.is_none() || src.channels[i].channel_group_id == self.group)
            .collect();
        let listing = Listing::new(
            keep.iter().map(|&i| src.channels[i].clone()).collect(),
            keep.iter().map(|&i| src.logos.get(i).cloned().flatten()).collect(),
            keep.iter().map(|&i| src.streams.get(i).cloned().unwrap_or_default()).collect(),
            &self.favourites,
            src.guide.clone(),
            src.fetched_at,
        );
        let fetched_at = src.fetched_at;
        let Some(listing) = listing else {
            self.phase = Phase::Failed(Error::BadResponse("guide window".into()));
            self.reseat(RETRY, fx);
            return;
        };
        if listing.rows() == 0 {
            self.listing = None;
            self.phase = Phase::Empty;
            if !matches!(refocus, Refocus::Keep) {
                let switch = if self.only_favourites {
                    FAVOURITES
                } else if self.group.is_some() {
                    GROUP
                } else {
                    ACCOUNT
                };
                self.reseat(switch, fx);
            }
            return;
        }
        let first = self.listing.is_none();
        self.listing = Some(listing);
        self.phase = Phase::Ready;
        let listing = self.listing.as_ref().expect("just stored");
        if first {
            // The view opens on the half hour now falls in, the first rows in place.
            let start = layout::grid().slot_start(fetched_at, listing.window.start);
            self.view = GridView { start, top: 0.0 };
            self.start_lag = Spring::at(0.0);
            self.top_lag = Spring::at(0.0);
            self.anchor = fetched_at;
        }
        let Refocus::Cell { carry, row } = refocus else { return };
        let (row, t) = match carry {
            Some((channel, t)) => (listing.channels.iter().position(|c| c.id == channel).unwrap_or(row), t),
            None => (row, fetched_at),
        };
        let elem = listing.at(row.min(listing.rows() - 1), t).unwrap_or(cell(0, 0));
        self.reseat(elem, fx);
    }

    /// Hold OK on a cell: its channel becomes a favourite, or stops being one. Saved at once; with
    /// only favourites showing, a channel taken off leaves the list and focus stays on its row.
    fn toggle_favourite<H: AppLike>(&mut self, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) {
        let Some((row, slot)) = self.cursor(cx) else { return };
        let Some(listing) = &self.listing else { return };
        let Some(channel) = listing.channels.get(row) else { return };
        let (id, uuid) = (channel.id, channel.uuid.clone());
        let t = listing.slot(row, slot).map_or(self.anchor, |s| s.start.max(self.anchor));
        match self.favourites.iter().position(|f| *f == uuid) {
            Some(i) => {
                self.favourites.remove(i);
            }
            None => self.favourites.push(uuid),
        }
        fx.push(Fx::App(AppFx::Dispatcharr(DispatcharrCmd::SaveFavourites { favourites: self.favourites.clone() })));
        self.relist(Refocus::Cell { carry: Some((id, t)), row }, fx);
        fx.invalidate(Provenance::Input);
    }

    /// The header switch: only favourites, or every channel. Focus stays on the switch.
    fn toggle_only_favourites<H: AppLike>(&mut self, fx: &mut Effects<'_, H>) {
        self.only_favourites = !self.only_favourites;
        // A different list under the grid: its view starts over at the top.
        self.listing = None;
        self.relist(Refocus::Keep, fx);
    }

    /// The group switch: every group, then each group in the server's order, then every group
    /// again. Focus stays on the switch.
    fn next_group<H: AppLike>(&mut self, fx: &mut Effects<'_, H>) {
        let Some(src) = &self.source else { return };
        self.group = next_group(&src.groups, self.group);
        self.listing = None;
        self.relist(Refocus::Keep, fx);
    }

    fn tick<H: AppLike>(&mut self, t: Tick, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) {
        self.poll(t.ms, fx);
        if let Some(clock) = self.clock {
            self.now = clock.now(t.ms);
            let grid = layout::grid();
            // Redraw when the marker would move a whole pixel.
            if ((self.now - self.drawn_now) as f64 * grid.px_per_sec()).abs() >= 1.0 {
                self.drawn_now = self.now;
                fx.invalidate(Provenance::Input);
            }
            if let Some(listing) = &self.listing {
                if self.pending.is_none() && listing.window.stale(listing.fetched_at, self.now) {
                    self.carry = self.cursor(cx).map(|(row, slot)| {
                        let channel = listing.channels[row].id;
                        (channel, listing.slot(row, slot).map_or(self.anchor, |s| s.start.max(self.anchor)))
                    });
                    self.request(fx);
                }
            }
        }
        if matches!(self.phase, Phase::Loading) || (self.pending.is_some() && self.listing.is_none()) {
            self.spin_ms += t.dt_us as f32 / 1000.0;
        }
        let mut present = fx.present();
        plx_machine::motion::spring(&mut self.start_lag.pos, &mut self.start_lag.vel, 0.0, consts::K_SCROLL, t,
            &mut present);
        plx_machine::motion::spring(&mut self.top_lag.pos, &mut self.top_lag.vel, 0.0, consts::K_SCROLL, t,
            &mut present);
        let focus_row = self.cursor(cx).map(|(row, _)| row);
        match (&mut self.hot_row, focus_row) {
            (Some((row, hot)), Some(now)) if *row == now => {
                plx_machine::motion::spring(&mut hot.pos, &mut hot.vel, 1.0, consts::K_SCALE, t, &mut present);
            }
            (_, Some(now)) => self.hot_row = Some((now, Spring::at(0.0))),
            (_, None) => self.hot_row = None,
        }
        let cell_target = if focus_row.is_some() { 1.0 } else { 0.0 };
        plx_machine::motion::spring(&mut self.cell_hot.pos, &mut self.cell_hot.vel, cell_target, consts::K_SCALE, t,
            &mut present);
    }

    /// The focused cell's `(row, slot)`, when focus is on one of this page's cells.
    fn cursor<H: AppLike>(&self, cx: &Cx<'_, H>) -> Option<(usize, usize)> {
        cx.focus.current.filter(|k| k.entry == self.entry).and_then(|k| split(k.elem))
    }

    fn focus_moved(&mut self, from: Option<FocusKey<u32>>, to: FocusKey<u32>) {
        let Some((row, slot)) = split(to.elem) else { return };
        let Some(listing) = &self.listing else { return };
        let Some(s) = listing.slot(row, slot).copied() else { return };
        let rows = listing.rows();
        let floor = listing.window.start;
        let grid = layout::grid();
        let start = grid.reveal_start(self.view.start, s.start, s.end, floor);
        let top = grid.reveal_top(self.view.top, row, rows);
        self.retarget(GridView { start, top });
        // A sideways step (or a fresh seat) sets the time UP and DOWN keep; a step between rows
        // keeps the one it had, which is the whole point of the anchor.
        let same_row = from.and_then(|f| split(f.elem)).is_some_and(|(r, _)| r == row);
        let entered = from.is_none_or(|f| split(f.elem).is_none());
        if same_row || (entered && !s.contains(self.anchor)) {
            self.anchor = s.start.max(start as i64);
        }
    }

    fn activate<H: AppLike>(&mut self, elem: u32, fx: &mut Effects<'_, H>) {
        match elem {
            ACCOUNT => fx.push(Fx::App(AppFx::Loop(LoopReq::DispatcharrAccount))),
            FAVOURITES => self.toggle_only_favourites(fx),
            GROUP => self.next_group(fx),
            RETRY => match &self.phase {
                Phase::Failed(Error::Unauthorized) => fx.push(Fx::App(AppFx::Loop(LoopReq::DispatcharrSetup))),
                Phase::Failed(_) => {
                    self.phase = Phase::Loading;
                    self.spin_ms = 0.0;
                    self.request(fx);
                }
                _ => {}
            },
            // OK on any programme tunes its channel.
            elem => {
                if let Some(play) = self.tune(elem) {
                    fx.push(Fx::App(AppFx::Content(ContentReq::Play { play })));
                }
            }
        }
        fx.invalidate(Provenance::Input);
    }

    /// What OK on `elem` plays: its row's channel, live, whichever programme the cursor was on
    /// (live TV can only play the present).
    fn tune(&self, elem: u32) -> Option<PlayIntent> {
        let (row, _) = split(elem)?;
        let listing = self.listing.as_ref()?;
        listing.lineup.channels.get(row).filter(|c| c.tunable())?;
        Some(PlayIntent::Live(LiveTune { lineup: Arc::clone(&listing.lineup), index: row, previous: None, at: 0, codecs: None }))
    }

    // ── drawing ──────────────────────────────────────────────────────────────────────────────

    fn account_rect(measure: &dyn plx_machine::machine::Measure) -> Rect {
        let h = layout::header();
        let w = Button::pill_w_measured(msg::dispatcharr_guide_account_c(), theme::size::BODY, false, false, measure);
        Rect::new(h.x + h.w - w, h.y, w, layout::HEADER_H)
    }

    fn favourites_label(&self) -> &'static std::ffi::CStr {
        if self.only_favourites { msg::dispatcharr_guide_all_channels_c() } else { msg::dispatcharr_guide_favourites_c() }
    }

    /// The favourites switch, left of *Account*. It wears the star while it offers the favourites.
    fn favourites_rect(&self, measure: &dyn plx_machine::machine::Measure) -> Rect {
        let account = Self::account_rect(measure);
        let w = Button::pill_w_measured(self.favourites_label(), theme::size::BODY, !self.only_favourites, false, measure);
        Rect::new(account.x - theme::space::MD - w, account.y, w, layout::HEADER_H)
    }

    /// Whether the header shows the group switch: there is more than one group to choose from.
    fn has_groups(&self) -> bool {
        self.source.as_ref().is_some_and(|s| s.groups.len() > 1 || (s.groups.len() == 1 && self.group.is_some()))
    }

    /// The group switch's words: the group shown, or that every group is.
    fn group_label(&self) -> CString {
        let name = self.group.and_then(|id| self.source.as_ref()?.groups.iter().find(|g| g.id == id));
        match name {
            Some(g) => CString::new(g.name.replace('\0', "")).unwrap_or_default(),
            None => msg::dispatcharr_guide_all_groups_c().to_owned(),
        }
    }

    /// The group switch, left of the favourites switch.
    fn group_rect(&self, measure: &dyn plx_machine::machine::Measure) -> Rect {
        let favourites = self.favourites_rect(measure);
        let label = self.group_label();
        let w = Button::pill_w_measured(&label, theme::size::BODY, false, false, measure).min(GROUP_MAX_W);
        Rect::new(favourites.x - theme::space::MD - w, favourites.y, w, layout::HEADER_H)
    }

    fn retry_label(&self) -> &'static std::ffi::CStr {
        match self.phase {
            Phase::Failed(Error::Unauthorized) => msg::dispatcharr_guide_sign_in_again_c(),
            _ => msg::dispatcharr_guide_retry_c(),
        }
    }

    fn status_text(&self) -> Option<(CString, Option<CString>, StatusKind)> {
        let (caption, reason, kind): (Cow<'static, str>, Option<Cow<'static, str>>, StatusKind) = match &self.phase {
            // placeholder-exempt: the caption only; `draw` counts the spinner it stands beside.
            Phase::Loading => (msg::dispatcharr_guide_loading().into(), None, StatusKind::Working),
            Phase::Empty if self.only_favourites => (
                msg::dispatcharr_guide_no_favourites().into(),
                Some(msg::dispatcharr_guide_no_favourites_reason().into()),
                StatusKind::Empty,
            ),
            Phase::Empty => {
                (msg::dispatcharr_guide_empty().into(), Some(msg::dispatcharr_guide_empty_reason().into()), StatusKind::Empty)
            }
            Phase::Failed(e) => (msg::dispatcharr_guide_failed().into(), Some(failure_reason(e)), StatusKind::Failed),
            Phase::Ready => return None,
        };
        Some((
            CString::new(caption.as_ref()).unwrap_or_default(),
            reason.map(|r| CString::new(r.as_ref()).unwrap_or_default()),
            kind,
        ))
    }

    fn status_overlay<'a>(&self, caption: &'a std::ffi::CStr, reason: Option<&'a std::ffi::CStr>, kind: StatusKind,
        focused: bool) -> StatusOverlay<'a> {
        let frame = Rect::new(0.0, layout::DETAIL_TOP, consts::SCR_W, consts::SCR_H - layout::DETAIL_TOP);
        let mut overlay = StatusOverlay::new(frame, caption, kind)
            .phase(self.spin_ms as u32)
            .focused(focused);
        if let Some(reason) = reason {
            overlay = overlay.reason(reason);
        }
        if kind == StatusKind::Failed {
            overlay = overlay.page(plx_ui::icons::Icon::ServerBadgeMinus).glyph_ceiling(layout::DETAIL_TOP)
                .action(self.retry_label());
        }
        overlay
    }

    fn retry_rect(&self, measure: &dyn plx_machine::machine::Measure) -> Option<Rect> {
        if !matches!(self.phase, Phase::Failed(_)) {
            return None;
        }
        let (caption, reason, kind) = self.status_text()?;
        self.status_overlay(&caption, reason.as_deref(), kind, false).action_frame_measured(measure)
    }

    fn draw_header<H: AppLike>(&self, f: &mut DrawFrame<'_, '_, H>, p: Painter, focus: Option<u32>) {
        let h = layout::header();
        Label::new(msg::dispatcharr_guide_title_c().as_ptr(), theme::size::HEADLINE, theme::TEXT_HEADING)
            .bold()
            .draw(p, Rect::new(h.x, h.y, h.w * 0.5, h.h));
        let account = Self::account_rect(f.measure);
        let switch = self.favourites_rect(f.measure);
        let group = self.has_groups().then(|| self.group_rect(f.measure));
        if self.clock.is_some() {
            let time = CString::new(i18n::wall_time(self.now)).unwrap_or_default();
            let w = f.measure.width(&time, theme::size::BODY, false);
            let left = group.unwrap_or(switch).x;
            Label::new(time.as_ptr(), theme::size::BODY, theme::TEXT_SECONDARY)
                .draw(p, Rect::new(left - theme::space::LG - w, h.y, w, h.h));
        }
        if let Some(rect) = group {
            // A server's group name can outrun the pill's cap: it is cut to fit with an ellipsis.
            let label = self.group_label();
            let air = Button::pill_w_measured(c"", theme::size::BODY, false, false, f.measure);
            let fitted = f.measure.fit_line(label.to_str().unwrap_or_default(), rect.w - air, theme::size::BODY, true);
            Button::new(fitted.as_ptr(), theme::size::BODY, rect).focused(focus == Some(GROUP)).draw(&Env::inert(), p);
            self.stop(f, p, GROUP, rect);
        }
        if self.source.is_some() {
            let mut button = Button::new(self.favourites_label().as_ptr(), theme::size::BODY, switch)
                .focused(focus == Some(FAVOURITES));
            if !self.only_favourites {
                button = button.icon(plx_ui::icons::Icon::Star);
            }
            button.draw(&Env::inert(), p);
            self.stop(f, p, FAVOURITES, switch);
        }
        Button::new(msg::dispatcharr_guide_account_c().as_ptr(), theme::size::BODY, account)
            .focused(focus == Some(ACCOUNT))
            .draw(&Env::inert(), p);
        self.stop(f, p, ACCOUNT, account);
    }

    /// The focused programme: its channel, title, times and the start of its description.
    fn draw_detail<H: AppLike>(&self, f: &mut DrawFrame<'_, '_, H>, p: Painter, row: usize, slot: usize) {
        let Some(listing) = &self.listing else { return };
        let (Some(channel), Some(s)) = (listing.channels.get(row), listing.slot(row, slot)) else { return };
        let programme = listing.programme(row, s);
        let d = layout::detail();
        let mut y = d.y;
        let channel_line = match listing.numbers[row].as_str() {
            "" => channel.name.clone(),
            n => format!("{n}  {}", channel.name),
        };
        let caption_h = f.measure.line_h(theme::size::CAPTION);
        let line = f.measure.fit_line(&channel_line, d.w, theme::size::CAPTION, false);
        Label::new(line.as_ptr(), theme::size::CAPTION, theme::TEXT_SECONDARY).draw(p, Rect::new(d.x, y, d.w, caption_h));
        y += caption_h + theme::space::XS;
        let title = programme.map_or(msg::dispatcharr_guide_unlisted(), |p| p.title.as_str());
        let title_h = f.measure.line_h(theme::size::TITLE);
        let line = f.measure.fit_line(title, d.w, theme::size::TITLE, true);
        Label::new(line.as_ptr(), theme::size::TITLE, theme::TEXT_PRIMARY).bold().draw(p, Rect::new(d.x, y, d.w, title_h));
        y += title_h + theme::space::XS;
        let meta = meta_line(s, programme);
        let body_h = f.measure.line_h(theme::size::BODY);
        let line = f.measure.fit_line(&meta, d.w, theme::size::BODY, false);
        Label::new(line.as_ptr(), theme::size::BODY, theme::TEXT_SECONDARY).draw(p, Rect::new(d.x, y, d.w, body_h));
        y += body_h + theme::space::SM;
        if let Some(text) = programme.and_then(|p| p.description.as_deref()) {
            TextView::new(text, theme::size::BODY, theme::TEXT_READING)
                .max_lines(2)
                .draw(p, Rect::new(d.x, y, d.w, (d.y + d.h - y).max(0.0)));
        }
    }

    fn draw_grid<H: AppLike>(&self, f: &mut DrawFrame<'_, '_, H>, p: Painter, cursor: Option<(usize, usize)>) {
        let Some(listing) = &self.listing else { return };
        let grid = layout::grid();
        let view = self.drawn_view();
        let lanes = grid.lanes();
        time_grid::draw_axis(p, &grid, view, f.measure, i18n::wall_time);
        // Under the cells, so a programme's title is drawn over the marker rather than struck
        // through by it; the cells are translucent, so it still reads down the whole grid.
        time_grid::draw_now(p, &grid, view, self.now);
        let rows = grid.visible_rows(view, listing.rows());
        // Rows gliding past the top or bottom are cut at the lanes' edges, labels with them.
        let body = Rect::new(grid.frame.x, lanes.y, grid.frame.w, lanes.h);
        let _clip = f.clip(p, body);
        for row in rows {
            let channel = &listing.channels[row];
            let hot = match self.hot_row {
                Some((r, hot)) if r == row => hot.pos,
                _ => 0.0,
            };
            // Resolved on every draw it is drawn on: that is the cache's "on screen" stamp, and a
            // miss is what starts the fetch.
            let logo = listing.logos[row].as_deref().map(|url| {
                plx_ui::tex::resolve_wh_on(plx_ui::tex::DIRECT, url, time_grid::LOGO_W as i32, time_grid::LOGO_H as i32,
                    true)
            });
            time_grid::draw_row_label(p, grid.label(row, view), &listing.numbers[row], logo, &channel.name,
                listing.stars.get(row).copied().unwrap_or(false), hot,
                f.measure);
            for (i, s) in listing.lanes[row].iter().enumerate() {
                let Some(rect) = grid.cell(row, s.start, s.end, view) else { continue };
                let programme = listing.programme(row, s);
                let focused = cursor == Some((row, i));
                let look = CellLook {
                    focus: if focused { self.cell_hot.pos } else { 0.0 },
                    airing: s.start <= self.now && self.now < s.end,
                    unlisted: programme.is_none(),
                };
                let title = programme.map_or(msg::dispatcharr_guide_unlisted(), |p| p.title.as_str());
                let second = programme.map(|_| time_range(s.start, s.end));
                time_grid::draw_cell(p, rect, title, second.as_deref(), look, f.measure);
            }
        }
        drop(_clip);
        if let Some((row, slot)) = cursor {
            self.stop(f, p, cell(row, slot), self.cell_rect(row, slot, At::Drawn).unwrap_or(lanes));
        }
    }

    fn cell_rect(&self, row: usize, slot: usize, at: At) -> Option<Rect> {
        let listing = self.listing.as_ref()?;
        let s = listing.slot(row, slot)?;
        layout::grid().cell(row, s.start, s.end, self.view_for(at))
    }

    fn stop<H: AppLike>(&self, f: &mut DrawFrame<'_, '_, H>, p: Painter, elem: u32, rect: Rect) {
        if !f.records_stops() {
            return;
        }
        f.stop(p, Stop { key: self.key(elem), rect, rest_rect: rect, clip: Rect::FULL, hover: Hover::Focus,
            activate: Activate::Direct });
    }
}

/// "21:00 – 22:30", in the viewer's clock. (The generated accessors take their arguments in
/// alphabetical order — `end` before `start` — so each is bound by name first.)
fn time_range(start: i64, end: i64) -> String {
    let (start, end) = (i18n::wall_time(start), i18n::wall_time(end));
    msg::dispatcharr_guide_time_range(&end, &start)
}

/// The detail band's second line: the times, then the episode and its own title when known.
fn meta_line(slot: &Slot, programme: Option<&Programme>) -> String {
    let mut parts = vec![time_range(slot.start, slot.end)];
    if let Some(p) = programme {
        match (p.season, p.episode) {
            // Alphabetical arguments: `episode`, then `season`.
            (Some(season), Some(episode)) => {
                parts.push(msg::dispatcharr_guide_season_episode(episode as i64, season as i64))
            }
            (None, Some(e)) => parts.push(msg::dispatcharr_guide_episode(e as i64)),
            _ => {}
        }
        if let Some(sub) = &p.sub_title {
            parts.push(sub.clone());
        }
    }
    parts.join(" · ")
}

fn failure_disc(e: &Error) -> u8 {
    match e {
        Error::BadAddress => 0,
        Error::Unreachable => 1,
        Error::NotDispatcharr => 2,
        Error::NotFound => 3,
        Error::Unauthorized => 4,
        Error::Forbidden => 5,
        Error::TooManyRequests => 6,
        Error::Unavailable => 7,
        Error::Redirected(_) => 8,
        Error::Http(_) => 9,
        Error::BadResponse(_) => 10,
    }
}

/// The reason under a failed guide's verdict: what happened, in terms of what to do about it.
/// The group after `current` in `groups`: the first after every group, every group after the last
/// (or after one no longer listed).
fn next_group(groups: &[Group], current: Option<i64>) -> Option<i64> {
    match current {
        None => groups.first().map(|g| g.id),
        Some(id) => groups.iter().position(|g| g.id == id).and_then(|i| groups.get(i + 1)).map(|g| g.id),
    }
}

fn failure_reason(e: &Error) -> Cow<'static, str> {
    match e {
        Error::Unauthorized | Error::Forbidden => msg::dispatcharr_guide_refused().into(),
        Error::BadAddress | Error::Unreachable => msg::dispatcharr_setup_unreachable().into(),
        Error::NotDispatcharr | Error::NotFound => msg::dispatcharr_setup_not_dispatcharr().into(),
        Error::TooManyRequests => msg::dispatcharr_setup_throttled().into(),
        Error::Unavailable => msg::dispatcharr_setup_unavailable().into(),
        Error::Redirected(_) | Error::Http(_) | Error::BadResponse(_) => {
            msg::dispatcharr_setup_unexpected(&e.to_string()).into()
        }
    }
}

impl<H: AppLike> Focusable<H> for GuideScreen {
    fn groups(&self, cx: &Cx<'_, H>, out: &mut Vec<GroupSpec>) {
        let account = Self::account_rect(cx.measure);
        let switch = self.favourites_rect(cx.measure);
        let switched = self.source.is_some();
        let left = if self.has_groups() { self.group_rect(cx.measure).x } else { switch.x };
        out.push(GroupSpec {
            id: HEADER,
            kind: GroupKind::Row { wrap: false },
            seat: Seat::First,
            reachable: AxisMask::BOTH,
            edge: [EdgeRule::Stop, EdgeRule::Geometric, EdgeRule::Stop, EdgeRule::Stop],
            extent: if switched {
                Rect::new(left, account.y, account.x + account.w - left, account.h)
            } else {
                account
            },
            len: 1 + usize::from(switched) + usize::from(self.has_groups()),
            // `Bare`: OK on a `Control` arms a press that ends in `PressCommit`, which this page does
            // not handle; it acts on `Activate`, which OK sends a `Bare` element directly.
            elem: ElemKind::Bare,
        });
        match &self.phase {
            Phase::Ready => {
                let rows = self.listing.as_ref().map_or(0, Listing::rows);
                let lanes = layout::grid().lanes();
                out.push(GroupSpec {
                    id: GRID,
                    kind: GroupKind::Free,
                    seat: Seat::Remembered,
                    reachable: AxisMask::VERTICAL,
                    edge: [EdgeRule::Geometric, EdgeRule::Stop, EdgeRule::Stop, EdgeRule::Stop],
                    extent: lanes,
                    len: rows,
                    // A cell's press is holdable: a tap tunes, a hold makes it a favourite.
                    elem: ElemKind::Card,
                });
            }
            Phase::Failed(_) => {
                if let Some(rect) = self.retry_rect(cx.measure) {
                    out.push(GroupSpec {
                        id: STATUS,
                        kind: GroupKind::Row { wrap: false },
                        seat: Seat::First,
                        reachable: AxisMask::BOTH,
                        edge: [EdgeRule::Geometric, EdgeRule::Stop, EdgeRule::Stop, EdgeRule::Stop],
                        extent: rect,
                        len: 1,
                        elem: ElemKind::Bare,
                    });
                }
            }
            Phase::Loading | Phase::Empty => {}
        }
    }

    fn group_of(&self, key: &u32, _cx: &Cx<'_, H>) -> Option<GroupId> {
        match *key {
            ACCOUNT => Some(HEADER),
            FAVOURITES => self.source.is_some().then_some(HEADER),
            GROUP => self.has_groups().then_some(HEADER),
            RETRY => matches!(self.phase, Phase::Failed(_)).then_some(STATUS),
            elem => {
                let (row, slot) = split(elem)?;
                self.listing.as_ref()?.slot(row, slot)?;
                (self.phase == Phase::Ready).then_some(GRID)
            }
        }
    }

    fn neighbour(&self, key: FocusKey<u32>, dir: Dir, _cx: &Cx<'_, H>) -> Step<u32> {
        match (key.elem, dir) {
            (FAVOURITES, Dir::Right) => return Step::Move(self.key(ACCOUNT)),
            (FAVOURITES, Dir::Left) if self.has_groups() => return Step::Move(self.key(GROUP)),
            (GROUP, Dir::Right) => return Step::Move(self.key(FAVOURITES)),
            (ACCOUNT, Dir::Left) if self.source.is_some() => return Step::Move(self.key(FAVOURITES)),
            _ => {}
        }
        let Some((row, slot)) = split(key.elem) else { return Step::Edge };
        let Some(listing) = &self.listing else { return Step::Edge };
        let lane_len = listing.lanes.get(row).map_or(0, Vec::len);
        let to = match dir {
            Dir::Left => slot.checked_sub(1).map(|s| cell(row, s)),
            Dir::Right => (slot + 1 < lane_len).then(|| cell(row, slot + 1)),
            Dir::Up => row.checked_sub(1).and_then(|r| listing.at(r, self.anchor)),
            Dir::Down => (row + 1 < listing.rows()).then(|| listing.at(row + 1, self.anchor)).flatten(),
        };
        to.map_or(Step::Edge, |elem| Step::Move(self.key(elem)))
    }

    fn place(&self, key: &u32, cx: &Cx<'_, H>, at: At) -> Option<Placed> {
        let rect = match *key {
            ACCOUNT => Self::account_rect(cx.measure),
            FAVOURITES => self.favourites_rect(cx.measure),
            GROUP => self.group_rect(cx.measure),
            RETRY => self.retry_rect(cx.measure)?,
            elem => {
                let (row, slot) = split(elem)?;
                self.cell_rect(row, slot, at)?
            }
        };
        Some(Placed { rect, rest_rect: rect, clip: Rect::FULL, index: split(*key).map(|(row, _)| row as u32) })
    }

    fn reconcile(&self, want: FocusKey<u32>, cx: &Cx<'_, H>) -> FocusKey<u32> {
        if self.group_of(&want.elem, cx).is_some() {
            return want;
        }
        // A cell the latest listing no longer has: the same row (or the last one) at the anchor.
        if let (Some((row, _)), Some(listing)) = (split(want.elem), &self.listing) {
            if self.phase == Phase::Ready && listing.rows() > 0 {
                if let Some(elem) = listing.at(row.min(listing.rows() - 1), self.anchor) {
                    return self.key(elem);
                }
            }
        }
        self.key(match self.phase {
            Phase::Failed(_) if self.retry_rect(cx.measure).is_some() => RETRY,
            _ => ACCOUNT,
        })
    }

    fn seat(&self, g: GroupId, _from: Placed, cx: &Cx<'_, H>) -> FocusKey<u32> {
        match g {
            GRID => {
                let listing = self.listing.as_ref();
                let row = self.view.top.round().max(0.0) as usize;
                let elem = listing.and_then(|l| l.at(row.min(l.rows().saturating_sub(1)), self.anchor));
                self.key(elem.unwrap_or(ACCOUNT))
            }
            STATUS if self.retry_rect(cx.measure).is_some() => self.key(RETRY),
            _ => self.key(ACCOUNT),
        }
    }
}

impl<H: AppLike> Machine<H> for GuideScreen {
    type Ev = ScreenEvent<H>;
    fn step(&mut self, ev: &Self::Ev, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        let handled = self.step_inner(ev, cx, fx);
        self.sync_state();
        handled
    }
}

impl GuideScreen {
    fn step_inner<H: AppLike>(&mut self, ev: &ScreenEvent<H>, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        match ev {
            ScreenEvent::Mount => self.request(fx),
            ScreenEvent::Tick(t) => self.tick(*t, cx, fx),
            ScreenEvent::Activate(elem) => self.activate(*elem, fx),
            // A cell's holdable press (`ElemKind::Card`): the tap tunes, the hold is a favourite.
            ScreenEvent::PressCommit(_) => match cx.focus.current.filter(|k| k.entry == self.entry) {
                Some(k) => self.activate(k.elem, fx),
                None => return Handled::No,
            },
            ScreenEvent::PressHold(_) => self.toggle_favourite(cx, fx),
            ScreenEvent::FocusMoved { from, to, .. } => {
                self.focus_moved(*from, *to);
                fx.invalidate(Provenance::Input);
            }
            ScreenEvent::Input(InputEvent { kind: InputKind::Key { key: Key::Back, edge: Edge::Down, .. }, .. }) => {
                // The guide is the root: BACK hands the screen to the TV.
                fx.push(Fx::App(AppFx::Loop(LoopReq::BackAtRoot)));
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }
}

impl<H: PlayerLike> Screen<H> for GuideScreen {
    fn name(&self) -> &'static str {
        word::GUIDE
    }
    fn state(&self) -> &dyn LogicalState {
        &self.state
    }
    fn crumb(&self, _cx: &Cx<'_, H>) -> Option<Cow<'_, str>> {
        None
    }
    fn prepare(&mut self, _b: &mut Budget, _cx: &Cx<'_, H>) {}
    fn draw(&mut self, f: &mut DrawFrame<'_, '_, H>) {
        let p = f.painter;
        self.ground.draw_default(Painter::root());
        if H::session(f.cx).preview {
            // The channel being watched shows through here; a hairline gives the window an edge
            // against the ground while the stream is still black.
            let r = layout::preview();
            p.clear_through(r);
            let line = plx_ui::widgets::HAIRLINE_H;
            p.rring(Rect::new(r.x - line, r.y - line, r.w + 2.0 * line, r.h + 2.0 * line), 0.0, line, theme::HAIRLINE);
        }
        let focus = f.focus.current.filter(|k| k.entry == self.entry).map(|k| k.elem);
        self.draw_header(f, p, focus);
        if self.phase == Phase::Ready {
            let cursor = focus.and_then(split);
            if let Some((row, slot)) = cursor {
                self.draw_detail(f, p, row, slot);
            }
            self.draw_grid(f, p, cursor);
            return;
        }
        let Some((caption, reason, kind)) = self.status_text() else { return };
        if kind == StatusKind::Working {
            plx_ui::placeholder::note(p, plx_ui::placeholder::Reason::PageSpinner, "guide");
        }
        self.status_overlay(&caption, reason.as_deref(), kind, focus == Some(RETRY)).draw(&Env::inert(), p);
        if let Some(rect) = self.retry_rect(f.measure) {
            self.stop(f, p, RETRY, rect);
        }
    }
    fn render(&self) -> RenderStrategy {
        RenderStrategy::Page
    }
    fn focus_source(&self) -> FocusSource {
        FocusSource::Engine
    }
    fn hit_source(&self) -> HitSource {
        HitSource::Engine
    }
}

#[cfg(test)]
#[path = "guide_tests.rs"]
mod tests;
