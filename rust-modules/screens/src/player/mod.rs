//! **The live player as an owned `Screen`** (restructure spec §9).
//!
//! `PlayerScreen` is the instance the container mounts for the player route while a channel plays.
//! It owns the live controls (the banner, the typed number, recall — [`live`]), the failure
//! read-out's row and the sandbox-repair question, and the subtitle render set, and it draws the
//! whole route: subtitles, the live chrome, and the read-out. The picture itself is the hardware
//! video plane.
//!
//! It answers `RenderStrategy::VideoPlane`, which is what tells the frame that what is behind this
//! page is a hardware plane GL cannot read: no `popover::host` snapshot, no `Glass` source pass, no
//! `RouteGround`, no capture. The CONSEQUENCES of that strategy — the present gate, the opaque
//! region and the capture skip — are the loop's privileged calls (§3.3 steps 1, 9 and 10) and are
//! not made here.
//!
//! **There is no transport.** Live TV has nothing to seek in: no scrubber, no control row, no
//! panels. Anything privileged a key asks for (tune, retry, repair, leave) is a
//! `PlayerReq` the application loop performs.

mod ass_subtitles;
pub mod input;
pub mod live;

use std::borrow::Cow;

use crate::registry::{AppFx, AppLike, PlayerLike, PlayerReq};
use plx_appkit::player_hud::{self, Busy, SubtitleBitmaps};
use plx_machine::machine::{
    Canon, Cx, Edge, Effects, EntryId, FocusKey, Fx, GroupId, Handled, InputKind, InstanceId,
    LogicalState, Machine,
};
use plx_ui::consts;
use plx_ui::frame::Budget;
use plx_ui::screen::{
    Activate, At, AxisMask, Dir, DrawFrame, EdgeRule, FocusSource, Focusable, GroupKind, GroupSpec,
    HitSource, Hover, Placed, RenderStrategy, Screen, ScreenEvent, Seat, Step, Stop,
};
use plx_ui::Rect;

/// The player's heartbeat word — byte-identical to `app::words::route_word(&AppArg::Player)`.
pub const WORD: &str = "player";

/// The fields [`PlayerScreen::write`] canonicalises, for the shape pin (§5.4).
pub const SHAPE: &str = "PlayerScreen{origin:Option<u32>,repair_alert:{open:bool,confirm:bool,scroll:u32},\
     live:{seen:Option<(u64,i64)>,seen_ms:u32,banner_until:u32,typed:str,typed_at:u32,missing:str,missing_until:u32,ok_down:Option<u32>,ok_fired:bool,browse:Option<u64>}}";

/// **The page this playback was launched from**, as the container's own identity.
///
/// A pair rather than the `EntryId` alone: an entry can be EVICTED at `CAP` and remounted, so its
/// `InstanceId` is what says "the same body", while the `EntryId` is what survives that eviction
/// and is what a `PopTo` names (§6.1's three tiers). Recorded once at `Mount`, from whatever page
/// was on top when the player was pushed, and never rewritten.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Origin {
    pub entry: EntryId,
    pub instance: Option<InstanceId>,
}

/// The route's RENDER resources — recreated freely, never read as logical state (§6.1).
#[derive(Default)]
pub struct PlayerRender {
    /// The decoded image-subtitle display set and its cache key.
    pub subs: SubtitleBitmaps,
    pub ass: ass_subtitles::AssSubtitles,
    /// The text cue on screen, wrapped and fitted once, and which page of it is showing.
    pub captions: player_hud::CaptionPager,
}

pub struct PlayerScreen {
    entry: EntryId,
    /// Who owns the "pipeline is working" signal this frame — resolved once by the loop, after
    /// `player::pump`, and pushed here.
    pub busy: Busy,
    /// Where this playback returns to — see [`Origin`].
    pub origin: Option<Origin>,
    render: PlayerRender,
    /// The page's one decision card: the sandbox repair's confirmation.
    repair_alert: plx_ui::decision_alert::DecisionAlert,
    repair_scroll: u32,
    /// Which control of the failure read-out's row holds focus — an index into
    /// `player_hud::FailureReadout::actions`, reset to the primary each time a failure takes the
    /// frame.
    failure_sel: usize,
    /// Did a failure own the frame at the last Tick — the edge that resets `failure_sel`.
    failure_up: bool,
    /// The live-TV controls: the banner, the typed number and the tune they belong to. Default
    /// (and inert) whenever nothing live plays.
    live: live::LiveControls,
}

impl PlayerScreen {
    pub fn new(entry: EntryId) -> Self {
        Self {
            entry,
            busy: Busy::None,
            origin: None,
            render: PlayerRender::default(),
            repair_alert: {
                let mut alert = plx_ui::decision_alert::DecisionAlert::new();
                alert.set_tone(plx_ui::decision_alert::Tone::Neutral);
                alert
            },
            repair_scroll: 0,
            failure_sel: 0,
            failure_up: false,
            live: live::LiveControls::default(),
        }
    }

    /// **Everything this page draws from a CLOCK, folded into ONE value** (spec §8.3, §9).
    ///
    /// The loop compares it with the previous frame's and raises `plx_machine::idle::invalidate()`
    /// when it changes. A frame is owed when the ANSWER changes, which is one comparison for the
    /// whole page instead of a call per animator. The terms:
    /// * the subtitle cue, its page and the image-subtitle key — a whole-screen content change
    ///   with no spring behind it, drawn on every frame of the route;
    /// * the live chrome: the banner coming and going (and its progress bar, which moves with the
    ///   wall-clock second), the typed digits and the no-such-channel note;
    /// * `busy` — the read-out appearing and vanishing, including the `Playing -> Error` edge
    ///   where nothing else in the frame moves at all.
    pub fn clock_fingerprint(&self, ps: &plx_media::route::PlaybackSession, now: u32) -> u64 {
        use std::sync::atomic::Ordering::Relaxed;
        let pos = plx_media::player::playpos_ns();
        let mut h: u64 = 0;
        let mut mix = |v: u64| {
            // FNV-1a's constants, one round per term: an equality fingerprint, not a digest.
            h = (h ^ v).wrapping_mul(0x0100_0000_01b3);
        };
        mix(u64::from(plx_media::player::TX.paused.load(Relaxed)));
        mix(self.render.ass.fingerprint());
        mix(plx_media::player::subtitle_cue_id(pos) as u64);
        mix(u64::from(player_hud::caption_page(&self.render.captions, false)));
        mix(plx_media::player::active_bitmap_key(pos).unwrap_or(0) as u64);
        if let Some(tune) = plx_media::route::live_tune(ps) {
            let banner = self.live.banner_visible(now);
            mix(u64::from(banner));
            if banner {
                mix(self.live.wall_now(tune, now) as u64);
            }
            mix(self.live.typed.len() as u64);
            mix(u64::from(self.live.missing_visible(now)));
            mix(self.live.browsing(now).map_or(u64::MAX, |i| i as u64));
        }
        mix(match self.busy {
            Busy::None => 1,
            Busy::Transport => 2,
            // the caption is a `&'static CStr`: its ADDRESS is a stable identity for the message
            Busy::Readout(k, c) => 4 ^ ((k as u64) << 32) ^ (c.as_ptr() as u64),
        });
        h
    }

    fn ask<H: AppLike>(fx: &mut Effects<'_, H>, req: PlayerReq) {
        fx.push(Fx::App(AppFx::Player(req)));
    }

    fn repair_focus<H: AppLike>(fx: &mut Effects<'_, H>, group: GroupId) {
        fx.push(Fx::Deliver(
            fx.from(),
            plx_machine::machine::Delivery::Screen(ScreenEvent::Enter(plx_ui::screen::Enter::Fresh {
                focus: plx_ui::screen::FocusTarget::ContainerGroup(group),
            })),
        ));
    }

    /// Perform control `idx` of the failure read-out's row — the ONE resolution a key and a click
    /// share, through the same table the row was drawn from (`player::failure_actions`), so a
    /// press can only ever do what the focused control says.
    fn failure_activate<H: AppLike>(&mut self, ps: &plx_media::route::PlaybackSession, idx: usize, fx: &mut Effects<'_, H>) {
        use plx_media::player::FailureAction as A;
        let readout = player_hud::FailureReadout::now(ps);
        let Some(action) = readout.actions().get(idx).copied() else { return };
        self.failure_sel = idx;
        match action {
            A::TryAgain => Self::ask(fx, PlayerReq::RetryPlayback),
            A::Repair => {
                if ps.repair_status == plx_platform::tv::sandbox::State::Idle && !self.repair_alert.visible() {
                    self.repair_scroll = 0;
                    self.repair_alert.open_with_body(plx_platform::i18n::msg::widgets_repair_question_c(), plx_platform::i18n::msg::widgets_repair_body());
                    Self::repair_focus(fx, GROUP_REPAIR);
                }
            }
            A::Back => Self::ask(fx, PlayerReq::Exit),
        }
    }

    fn repair_answer<H: AppLike>(&mut self, confirm: bool, fx: &mut Effects<'_, H>) {
        if !self.repair_alert.is_open() { return; }
        self.repair_alert.dismiss();
        if confirm { Self::ask(fx, PlayerReq::RepairSandbox); }
        Self::repair_focus(fx, GROUP_FAILURE);
    }

    fn repair_rect(&self, confirm: bool, measure: &dyn plx_machine::machine::Measure) -> Rect {
        let (cancel, repair) = self.repair_alert.frames(measure);
        if confirm { repair } else { cancel }
    }

    /// One registered click's element resolves to an action — the pointer twin of `handle_key`.
    /// A click on no control at all (the picture) shows the live banner, as any key does.
    fn handle_click<H: AppLike>(
        &mut self,
        ps: &plx_media::route::PlaybackSession,
        hit: Option<u32>,
        now: u32,
        fx: &mut Effects<'_, H>,
    ) -> Handled {
        use player_hud::{ELEM_FAILURE_BASE, ELEM_FAILURE_END};
        match hit {
            None => {
                if !player_hud::transport_hidden(ps) {
                    if let Some(tune) = plx_media::route::live_tune(ps) {
                        self.live.key(tune, live::LivePress::Wake, now);
                        plx_machine::idle::invalidate();
                    }
                }
                Handled::Yes
            }
            Some(e) if (ELEM_FAILURE_BASE..ELEM_FAILURE_END).contains(&e) => {
                // the same gate `group_drawn` registers the row behind: a row that is not drawn
                // is not clickable
                if player_hud::failure_row_drawn(self.busy) {
                    self.failure_activate(ps, (e - ELEM_FAILURE_BASE) as usize, fx);
                }
                Handled::Yes
            }
            Some(_) => Handled::No,
        }
    }

    /// **A key on live TV** (see [`live`]): `Some` when the live player answered it, `None` for a
    /// key the player answers below (BACK, STOP, EXIT) or when nothing live plays. A failure
    /// read-out keeps OK and its own row: a channel that failed can still be changed with CH, and
    /// its *Try again* still answers OK. It covers the banner, so there is no browsing under it.
    /// BACK is the live player's only while it ends a browse.
    #[allow(clippy::too_many_arguments)]
    fn live_key<H: AppLike>(
        &mut self,
        ps: &plx_media::route::PlaybackSession,
        key: consts::Key,
        sym: std::os::raw::c_uint,
        wcode: std::os::raw::c_uint,
        edge: Edge,
        now: u32,
        fx: &mut Effects<'_, H>,
    ) -> Option<Handled> {
        let tune = plx_media::route::live_tune(ps)?;
        let failed = player_hud::transport_hidden(ps);
        if matches!(key, consts::Key::Back) && !failed && edge == Edge::Down && self.live.back(now) {
            plx_machine::idle::invalidate();
            return Some(Handled::Yes);
        }
        let press = live::press_of(key, sym, wcode)?;
        if failed && matches!(press, live::LivePress::Ok | live::LivePress::Swallow | live::LivePress::Browse(_)) {
            return None;
        }
        // OK is answered on all three edges (tap on release, hold for recall). A browse steps on
        // the press and on every repeat, so a held UP runs through the banner, which tunes
        // nothing. Everything else acts on the press alone: a held CH▲ does not race through the
        // lineup, one Load a repeat.
        let action = match press {
            live::LivePress::Ok => self.live.ok(tune, edge, now),
            live::LivePress::Browse(_) if edge != Edge::Up => self.live.key(tune, press, now),
            _ if edge == Edge::Down => self.live.key(tune, press, now),
            _ => return Some(Handled::Yes),
        };
        Self::live_act(action, fx);
        plx_machine::idle::invalidate();
        Some(Handled::Yes)
    }

    fn live_act<H: AppLike>(action: live::LiveAction, fx: &mut Effects<'_, H>) {
        if let live::LiveAction::Tune(tune) = action {
            Self::ask(fx, PlayerReq::Tune(tune));
        }
    }

    /// The live chrome: the banner while it is up (on the channel being browsed, if one is), and
    /// the typed number (or that it matched
    /// nothing) in the corner.
    fn draw_live<H: AppLike>(&self, ps: &plx_media::route::PlaybackSession, now: u32, f: &mut DrawFrame<'_, '_, H>) {
        let Some(tune) = plx_media::route::live_tune(ps) else { return };
        if self.live.banner_visible(now) {
            let browsing = self.live.browsing(now).is_some();
            if let Some(channel) = tune.lineup.channels.get(self.live.banner_index(tune, now)) {
                plx_appkit::live_banner::draw(f.painter, channel, self.live.wall_now(tune, now), browsing, f.measure);
            }
        }
        if !self.live.typed.is_empty() {
            plx_appkit::live_banner::draw_typed(f.painter, &self.live.typed, false, f.measure);
        } else if self.live.missing_visible(now) {
            plx_appkit::live_banner::draw_typed(f.painter, &self.live.missing, true, f.measure);
        }
    }

    /// The keys the live controls leave to the page: the failure read-out's row, the transport
    /// keys (pause and resume the channel), and leaving.
    fn handle_key<H: AppLike>(
        &mut self,
        ps: &plx_media::route::PlaybackSession,
        key: consts::Key,
        edge: Edge,
        fx: &mut Effects<'_, H>,
    ) -> Handled {
        use consts::Key;
        // **A terminal failure owns the whole frame**: its row is the only thing that may be
        // driven. EXIT, the remote's own key, is not swallowed (LG checklist item 38): it falls to
        // `Handled::No` and the loop's own arm takes it, as everywhere else.
        if player_hud::transport_hidden(ps) && !matches!(key, Key::Exit) {
            if edge == Edge::Down {
                let key = match key {
                    Key::Ok => input::FailedKey::Ok,
                    Key::Back | Key::Stop => input::FailedKey::Back,
                    Key::Left { .. } => input::FailedKey::Left,
                    Key::Right { .. } => input::FailedKey::Right,
                    _ => input::FailedKey::Other,
                };
                let n = player_hud::FailureReadout::now(ps).actions().len();
                match input::failed_key_action(key, self.failure_sel, n) {
                    input::FailedKeyAction::Focus(i) => {
                        self.failure_sel = i;
                        plx_machine::idle::invalidate();
                    }
                    input::FailedKeyAction::Activate(i) => self.failure_activate(ps, i, fx),
                    input::FailedKeyAction::Return => Self::ask(fx, PlayerReq::Exit),
                    input::FailedKeyAction::Ignore => {}
                }
            }
            return Handled::Yes;
        }
        match key {
            Key::Play | Key::Pause | Key::PlayPause => {
                if edge == Edge::Down {
                    let play = match key {
                        Key::Play => Some(true),
                        Key::Pause => Some(false),
                        _ => None,
                    };
                    Self::ask(fx, PlayerReq::Transport(play));
                }
                Handled::Yes
            }
            Key::Stop | Key::Back => {
                if edge == Edge::Down {
                    Self::ask(fx, back_request(key, plx_media::route::live_tune(ps).is_some()));
                }
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    /// **Every element this screen's `Focusable` declares and the frame DRAWS, registered as a
    /// pointer stop at the rect `place` gives it** — the rect D-pad focus uses, so "reachable by
    /// the D-pad" and "clickable with the pointer" are one statement (issue #162).
    fn record_stops<H: PlayerLike>(&self, f: &mut DrawFrame<'_, '_, H>) {
        if !f.records_stops() { return; }
        let mut groups = Vec::new();
        Focusable::<H>::groups(self, f.cx, &mut groups);
        for g in groups.iter().filter(|g| self.group_drawn(g.id)) {
            // The repair alert's buttons are ordinary pressable controls (hover parks focus, the
            // press dips and commits on release); the read-out's row is `Direct`.
            let (hover, activate) = if g.id == GROUP_REPAIR {
                (Hover::Focus, Activate::Press)
            } else {
                (Hover::Ignore, Activate::Direct)
            };
            for i in 0..g.len as u32 {
                let elem = Self::elem_of(g.id, i);
                let Some(p) = Focusable::<H>::place(self, &elem, f.cx, At::Drawn) else { continue };
                f.stop(
                    plx_ui::Painter::root(),
                    Stop {
                        key: FocusKey { entry: self.entry, elem },
                        rect: p.rect,
                        rest_rect: p.rest_rect,
                        clip: p.clip,
                        hover,
                        activate,
                    },
                );
            }
        }
    }

    /// Is group `g` on screen this frame — the same gates `player_hud::draw_readout` paints behind.
    fn group_drawn(&self, g: GroupId) -> bool {
        match g {
            GROUP_REPAIR => self.repair_alert.is_open() && self.repair_alert.settled(),
            GROUP_FAILURE => !self.repair_alert.visible() && player_hud::failure_row_drawn(self.busy),
            _ => false,
        }
    }

    /// Element `i` of group `g` — the inverse of `group_of`.
    fn elem_of(g: GroupId, i: u32) -> u32 {
        match g {
            GROUP_FAILURE => player_hud::ELEM_FAILURE_BASE + i,
            _ => REPAIR_CANCEL + i,
        }
    }
}

/// The focus groups [`PlayerScreen`] registers: the failure read-out's row of actions (only while
/// a failure owns the frame) and the repair question's two answers.
const GROUP_FAILURE: GroupId = GroupId(3);
const GROUP_REPAIR: GroupId = GroupId(4);
const REPAIR_CANCEL: u32 = 40_000;
const REPAIR_CONFIRM: u32 = REPAIR_CANCEL + 1;

/// What a STOP or BACK asks for: BACK off a live channel goes to the Guide still watching (the
/// corner preview); STOP, and BACK off anything else, leaves the player.
fn back_request(key: consts::Key, live: bool) -> PlayerReq {
    match key {
        consts::Key::Back if live => PlayerReq::ToGuide,
        _ => PlayerReq::Exit,
    }
}

impl<H: PlayerLike> Machine<H> for PlayerScreen {
    type Ev = ScreenEvent<H>;
    fn step(&mut self, ev: &Self::Ev, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        if self.repair_alert.visible() {
            match ev {
                ScreenEvent::FocusMoved { to, .. } => {
                    self.repair_alert.set_choice(if to.elem == REPAIR_CONFIRM { plx_ui::decision_alert::Choice::Destructive } else { plx_ui::decision_alert::Choice::Cancel });
                    return Handled::Yes;
                }
                ScreenEvent::PressCommit(_) => {
                    if let Some(key) = cx.focus.current {
                        if matches!(key.elem, REPAIR_CANCEL | REPAIR_CONFIRM) {
                            self.repair_answer(key.elem == REPAIR_CONFIRM, fx);
                        }
                    }
                    return Handled::Yes;
                }
                ScreenEvent::Activate(_) => return Handled::Yes,
                ScreenEvent::Input(input) => {
                    if !self.repair_alert.is_open() { return Handled::Yes; }
                    return match input.kind {
                        InputKind::Key { key, sym, wcode, edge, .. } => match consts::classify_input(key, sym, wcode) {
                            consts::Key::Back | consts::Key::Stop if edge == Edge::Down => {
                                self.repair_answer(false, fx); Handled::Yes
                            }
                            direction @ (consts::Key::Up | consts::Key::Down) if edge != Edge::Up => {
                                self.repair_scroll = self.repair_alert.scroll_by(cx.measure,
                                    if matches!(direction, consts::Key::Up) { -1 } else { 1 });
                                Handled::Yes
                            }
                            consts::Key::Left { .. } | consts::Key::Right { .. } | consts::Key::Ok | consts::Key::Exit => Handled::No,
                            _ => Handled::Yes,
                        },
                        InputKind::Pointer { .. } | InputKind::Click { .. } => Handled::No,
                        _ => Handled::Yes,
                    };
                }
                _ => {}
            }
        }
        match ev {
            ScreenEvent::Tick(tick) => {
                self.render.ass.update(H::session(cx), tick.ms);
                let failed = player_hud::readout_owns_frame(self.busy);
                if failed && !self.failure_up {
                    self.failure_sel = 0;
                }
                self.failure_up = failed;
                let stale = !H::session(cx).jail_load_blocked;
                if self.repair_alert.visible() && stale { self.repair_alert.close(); }
                self.repair_alert.update(tick.dt());
                let action = self.live.tick(plx_media::route::live_tune(H::session(cx)), tick.ms);
                Self::live_act(action, fx);
                Handled::Yes
            }
            ScreenEvent::Unmount => {
                // The GL names in the subtitle set are OWNED. A static could never be told that a
                // playback had ended; an instance is told exactly once.
                self.render.subs.release();
                self.render.ass.release();
                Handled::Yes
            }
            ScreenEvent::Input(input) => {
                let ps = H::session(cx);
                match &input.kind {
                    InputKind::Key { key, sym, wcode, edge, .. } => {
                        let classified = consts::classify_input(*key, *sym, *wcode);
                        if let Some(handled) = self.live_key(ps, classified, *sym, *wcode, *edge, input.at.ms, fx) {
                            return handled;
                        }
                        self.handle_key(ps, classified, *edge, fx)
                    }
                    InputKind::Click { hit, .. } => self.handle_click(ps, *hit, input.at.ms, fx),
                    // Pointer motion over the picture is how a Magic Remote user asks what is on:
                    // it shows the banner.
                    InputKind::Pointer { .. } => {
                        if let Some(tune) = plx_media::route::live_tune(ps) {
                            self.live.key(tune, live::LivePress::Wake, input.at.ms);
                            plx_machine::idle::invalidate();
                        }
                        Handled::Yes
                    }
                    _ => Handled::No,
                }
            }
            _ => Handled::No,
        }
    }
}

impl<H: PlayerLike> Focusable<H> for PlayerScreen {
    fn groups(&self, cx: &Cx<'_, H>, out: &mut Vec<GroupSpec>) {
        if self.repair_alert.visible() {
            let len = match self.repair_alert.answers() { plx_ui::decision_alert::Answers::One => 1, _ => 2 };
            out.push(GroupSpec { id: GROUP_REPAIR, kind: GroupKind::Row { wrap: false }, seat: Seat::First,
                reachable: AxisMask::BOTH, edge: [EdgeRule::Stop; 4], extent: self.repair_rect(false, cx.measure).union(self.repair_rect(true, cx.measure)), len, elem: plx_ui::screen::ElemKind::Control });
            return;
        }
        if matches!(self.busy, Busy::Readout(plx_ui::widgets::StatusKind::Failed, _)) {
            let frames = player_hud::FailureReadout::now(H::session(cx)).frames(cx.measure);
            let extent = frames.iter().flatten().copied().reduce(|a, b| a.union(b)).unwrap_or(Rect::FULL);
            out.push(GroupSpec {
                id: GROUP_FAILURE,
                kind: GroupKind::Row { wrap: false },
                seat: Seat::First,
                reachable: AxisMask::BOTH,
                edge: [EdgeRule::Screen; 4],
                extent,
                len: frames.iter().flatten().count(),
                elem: plx_ui::screen::ElemKind::Control,
            });
        }
    }
    fn group_of(&self, key: &u32, _cx: &Cx<'_, H>) -> Option<GroupId> {
        if self.repair_alert.visible() {
            return matches!(*key, REPAIR_CANCEL | REPAIR_CONFIRM).then_some(GROUP_REPAIR);
        }
        (player_hud::ELEM_FAILURE_BASE..player_hud::ELEM_FAILURE_END).contains(key).then_some(GROUP_FAILURE)
    }
    fn neighbour(&self, key: FocusKey<u32>, dir: Dir, _cx: &Cx<'_, H>) -> Step<u32> {
        if self.repair_alert.is_open() {
            let two = self.repair_alert.answers() == plx_ui::decision_alert::Answers::Two;
            let elem = match (key.elem, dir) {
                (REPAIR_CANCEL, Dir::Right) if two => REPAIR_CONFIRM,
                (REPAIR_CONFIRM, Dir::Left) => REPAIR_CANCEL,
                _ => return Step::Edge,
            };
            return Step::Move(FocusKey { entry: self.entry, elem });
        }
        Step::Edge
    }
    fn place(&self, key: &u32, cx: &Cx<'_, H>, _at: At) -> Option<Placed> {
        if self.repair_alert.visible() {
            if !matches!(*key, REPAIR_CANCEL | REPAIR_CONFIRM) { return None; }
            let rect = self.repair_rect(*key == REPAIR_CONFIRM, cx.measure);
            return Some(Placed { rect, rest_rect: rect, clip: Rect::FULL, index: None });
        }
        if !(player_hud::ELEM_FAILURE_BASE..player_hud::ELEM_FAILURE_END).contains(key) {
            return None;
        }
        let rect = player_hud::FailureReadout::now(H::session(cx)).frames(cx.measure)[(*key - player_hud::ELEM_FAILURE_BASE) as usize]?;
        Some(Placed { rect, rest_rect: rect, clip: Rect::FULL, index: None })
    }
    fn reconcile(&self, want: FocusKey<u32>, _cx: &Cx<'_, H>) -> FocusKey<u32> {
        want
    }
    fn seat(&self, g: GroupId, _from: Placed, _cx: &Cx<'_, H>) -> FocusKey<u32> {
        let elem = if g == GROUP_REPAIR { REPAIR_CANCEL } else { player_hud::ELEM_FAILURE_BASE };
        FocusKey { entry: self.entry, elem }
    }
}

impl LogicalState for PlayerScreen {
    fn write(&self, c: &mut Canon) {
        c.option(self.origin.as_ref(), |c, o| {
            c.u32(o.entry.0);
        });
        c.bool(self.repair_alert.is_open()).bool(self.repair_alert.choice() == plx_ui::decision_alert::Choice::Destructive).u32(self.repair_scroll);
        c.option(self.live.seen.as_ref(), |c, (index, at)| {
            c.u64(*index as u64).u64(*at as u64);
        });
        c.u32(self.live.seen_ms)
            .u32(self.live.banner_until)
            .str(&self.live.typed)
            .u32(self.live.typed_at)
            .str(&self.live.missing)
            .u32(self.live.missing_until);
        c.option(self.live.ok_down.as_ref(), |c, t| {
            c.u32(*t);
        });
        c.bool(self.live.ok_fired);
        c.option(self.live.browse.as_ref(), |c, i| {
            c.u64(*i as u64);
        });
    }
    fn probe(&self, out: &mut String) {
        out.push_str(WORD);
    }
}

impl<H: PlayerLike> Screen<H> for PlayerScreen {
    fn name(&self) -> &'static str {
        WORD
    }
    fn state(&self) -> &dyn LogicalState {
        self
    }
    fn crumb(&self, _cx: &Cx<'_, H>) -> Option<Cow<'_, str>> {
        None
    }
    fn prepare(&mut self, _b: &mut Budget, _cx: &Cx<'_, H>) {}
    fn draw(&mut self, f: &mut DrawFrame<'_, '_, H>) {
        let ps = H::session(f.cx);
        let now = f.cx.tick.ms;
        self.render.ass.draw();
        if let Some(message) = self.render.ass.error() {
            player_hud::draw_subtitle_message(message, false);
        }
        player_hud::draw_subtitle_bitmap(&mut self.render.subs, false); // PGS/VobSub image subs
        player_hud::draw_subtitles(&self.render.captions, false);
        self.draw_live(ps, now, f);
        // The read-out over everything but the repair question.
        player_hud::draw_readout(ps, self.busy, now, self.failure_sel, f.measure);
        if self.repair_alert.visible() {
            self.repair_alert.draw_scrim();
            self.repair_alert.draw(plx_platform::i18n::msg::settings_cancel_c(), plx_platform::i18n::msg::widgets_repair_action_c(), f.measure);
        }
        self.record_stops(f);
    }
    fn render(&self) -> RenderStrategy {
        RenderStrategy::VideoPlane
    }
    /// The subtitle display sets are the only render this screen owns (§8.3 rule (c)): the picture
    /// is the hardware video plane, and the chrome's glyphs come from the shared caches.
    fn render_report(&self) -> plx_ui::frame::RenderReport {
        let bitmap = self.render.subs.render_report();
        let ass = self.render.ass.render_report();
        plx_ui::frame::RenderReport { textures: bitmap.textures + ass.textures, bytes: bitmap.bytes + ass.bytes }
    }
    fn focus_source(&self) -> FocusSource {
        FocusSource::Engine
    }
    fn hit_source(&self) -> HitSource {
        HitSource::Engine
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::PlayerScreen;
    use crate::registry::{AppFx, AppMsg, PageMemory, PlayerReq};
    use plx_machine::machine::{
        Cx, Edge, Effects, EntryId, Fx, Handled, Host, InputEvent, InputKind, InputOwner, InstanceId,
        Machine, MachineId, Source, Tick,
    };
    use plx_ui::consts::{WCODE_BACK, WCODE_PLAYPAUSE, WCODE_STOP};
    use plx_ui::fixture::FixtureMeasure;
    use plx_ui::screen::ScreenEvent;

    struct TestHost;
    impl Host for TestHost {
        type Arg = plx_ui::fixture::FixtureArg;
        type Fx = AppFx;
        type Msg = AppMsg;
        type Elem = u32;
        type Views<'a> = ();
        type Init = plx_ui::fixture::FixtureArg;
        type Memory = PageMemory;
    }
    impl crate::registry::PlayerLike for TestHost {
        fn session<'a>(_cx: &Cx<'a, Self>) -> &'a plx_media::route::PlaybackSession {
            plx_media::route::idle_session_for_test()
        }
    }

    const ENTRY: EntryId = EntryId(44);

    fn cx() -> Cx<'static, TestHost> {
        Cx {
            views: (),
            tick: Tick::default(),
            measure: &FixtureMeasure,
            focus: Default::default(),
            press: Default::default(),
            owner: InputOwner::Entry(ENTRY),
        }
    }

    fn deliver(page: &mut PlayerScreen, ev: ScreenEvent<TestHost>) -> (Handled, Vec<PlayerReq>) {
        let mut out = Vec::new();
        let mut present = plx_machine::present::Present::new();
        let handled = page.step(&ev, &cx(), &mut Effects::new(&mut out, MachineId::Instance(InstanceId(7)), &mut present));
        let reqs = out
            .into_iter()
            .filter_map(|e| match e.fx {
                Fx::App(AppFx::Player(req)) => Some(req),
                _ => None,
            })
            .collect();
        (handled, reqs)
    }

    fn key(page: &mut PlayerScreen, wcode: u32, edge: Edge) -> (Handled, Vec<PlayerReq>) {
        deliver(
            page,
            ScreenEvent::Input(InputEvent {
                kind: InputKind::Key { key: plx_machine::machine::Key::Other, sym: 0, wcode, edge, at_edge: false },
                at: Tick { ms: 1_000, dt_us: 0 },
                source: Source::Sdl,
            }),
        )
    }

    #[test]
    fn back_and_stop_both_ask_to_leave() {
        let _guard = plx_base::testlock::serial();
        for wcode in [WCODE_BACK, WCODE_STOP] {
            let mut page = PlayerScreen::new(ENTRY);
            let (handled, reqs) = key(&mut page, wcode, Edge::Down);
            assert_eq!(handled, Handled::Yes);
            assert!(matches!(reqs.as_slice(), [PlayerReq::Exit]), "{wcode}: {reqs:?}");
        }
    }

    /// BACK off a live channel keeps it playing in the Guide's corner; STOP still stops it, and
    /// BACK off anything that is not a channel still leaves.
    #[test]
    fn back_off_a_live_channel_goes_to_the_guide_still_watching() {
        use plx_ui::consts::Key;
        assert!(matches!(super::back_request(Key::Back, true), PlayerReq::ToGuide));
        assert!(matches!(super::back_request(Key::Stop, true), PlayerReq::Exit));
        assert!(matches!(super::back_request(Key::Back, false), PlayerReq::Exit));
        assert!(matches!(super::back_request(Key::Stop, false), PlayerReq::Exit));
    }

    #[test]
    fn the_transport_key_asks_for_the_shared_toggle() {
        let _guard = plx_base::testlock::serial();
        let mut page = PlayerScreen::new(ENTRY);
        let (_, reqs) = key(&mut page, WCODE_PLAYPAUSE, Edge::Down);
        assert!(matches!(reqs.as_slice(), [PlayerReq::Transport(None)]), "{reqs:?}");
        let (_, reqs) = key(&mut page, WCODE_PLAYPAUSE, Edge::Up);
        assert!(reqs.is_empty(), "the release asks nothing");
    }

    #[test]
    fn a_click_on_the_picture_asks_for_nothing() {
        let _guard = plx_base::testlock::serial();
        let mut page = PlayerScreen::new(ENTRY);
        let (handled, reqs) = deliver(
            &mut page,
            ScreenEvent::Input(InputEvent {
                kind: InputKind::Click { hit: None, x: 960.0, y: 540.0 },
                at: Tick { ms: 1_000, dt_us: 0 },
                source: Source::Sdl,
            }),
        );
        assert_eq!(handled, Handled::Yes);
        assert!(reqs.is_empty(), "no transport to toggle: {reqs:?}");
    }

    #[test]
    fn the_fingerprint_moves_when_the_read_out_appears() {
        let _guard = plx_base::testlock::serial();
        let ps = plx_media::route::idle_session_for_test();
        let mut page = PlayerScreen::new(ENTRY);
        let quiet = page.clock_fingerprint(ps, 1_000);
        assert_eq!(quiet, page.clock_fingerprint(ps, 1_000), "a still page fingerprints the same");
        page.busy = plx_appkit::player_hud::Busy::Readout(plx_ui::widgets::StatusKind::Failed, c"x");
        assert_ne!(quiet, page.clock_fingerprint(ps, 1_000));
    }
}
