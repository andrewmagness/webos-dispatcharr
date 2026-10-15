//! **Setup — signing this television in to a Dispatcharr server.** One screen, two routes
//! ([`Mode`]): the boot page (`AppArg::Setup`), which hands straight over to the Guide once an
//! account is stored — found at boot, or just signed in — and the Server page (`AppArg::Server`),
//! pushed from the Guide's *Account*, which names the server and account and signs out.
//!
//! Two faces on one route-family layout:
//!
//! * **The form.** The narrative column says what is asked and that the TV keeps an API key, never
//!   the password; the content column holds three [`TextField`]s (address, username, password —
//!   masked) and *Sign in*, with one status line under it: the spinner while signing in, or why
//!   the last attempt failed. A failure is a reason in `TEXT_SECONDARY`, never red (product idiom
//!   4), and it names where to go: the address, the password, or the server's network settings.
//! * **Connected** (the Server page). The verdict, the server and account, the channel count when
//!   the lineup lands, a note when the account is stored only until a restart, and *Sign out* in
//!   the action band. Signing out lands on the boot page as the new root.
//!
//! **Typing.** The television draws the keyboard (`textinput`); OK on a field raises it, typed
//! text arrives as `TextEdit`s, and OK on the panel moves on to the next field WITHOUT closing it —
//! a panel that is dismissed cannot always be raised again on LG's own SDL (moonlight-tv#435), so
//! the three fields are filled in one keyboard session where the set allows. OK after the password
//! closes it and lands on *Sign in*.
//!
//! **Nothing here blocks.** The account file and the sign-in are the application's to run
//! ([`DispatcharrCmd`], on a worker); this page holds the receiver and reads it on each tick. The
//! password lives only in its field's buffer, goes out with the one sign-in it is typed for, and is
//! wiped the moment a sign-in succeeds.

use std::borrow::Cow;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use plx_dispatcharr::dispatcharr::account::Saved;
use plx_dispatcharr::dispatcharr::Error;
use plx_machine::machine::{
    Canon, Cx, Delivery, Edge, Effects, EntryId, FocusKey, Fx, GroupId, Handled, InputEvent,
    InputKind, InputOwner, InstanceId, Key, LogicalState, Machine, MachineId, NavOp, TextEdit, Tick,
};
use plx_machine::present::Provenance;
use plx_platform::i18n::msg;
use plx_ui::frame::Budget;
use plx_ui::label::Label;
use plx_ui::route_screen::{RouteGround, RouteLayout};
use plx_ui::screen::{
    Activate, At, AxisMask, Dir, DrawFrame, EdgeRule, ElemKind, Enter, FocusSource, FocusTarget,
    Focusable, GroupKind, GroupSpec, HitSource, Hover, Placed, RenderStrategy, Screen, ScreenEvent,
    Seat, Step, Stop,
};
use plx_ui::text_buffer::TextBuffer;
use plx_ui::text_field::{Look, TextField};
use plx_ui::text_view::TextView;
use plx_ui::widgets::{Button, Spinner};
use plx_ui::{theme, Env, Painter, Rect, Spring, View};

use super::registry::{word, AppFx, AppLike, DispatcharrCmd, LoopReq, SetupOutcome};

/// Which of its two routes this screen is mounted as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `AppArg::Setup`, the boot page: the form, until an account is stored.
    Entry,
    /// `AppArg::Server`, pushed from the Guide: the stored account, and *Sign out*.
    Server,
}

/// The form's elements, top to bottom: one column, walked in this order.
const ADDRESS: u32 = 0;
const USERNAME: u32 = 1;
const PASSWORD: u32 = 2;
const SIGN_IN: u32 = 3;
/// Connected's controls: *Sign out*, and the start-on-the-last-channel switch.
const SIGN_OUT: u32 = 4;
const RESUME: u32 = 5;
/// The page's one focus group. `GroupId(0)` is the container's own fresh-mount target
/// (`stack.rs::fresh`), so a fresh mount seats here with no correction of its own.
const GROUP: GroupId = GroupId(0);

const FIELDS: usize = 3;
const BLINK_US: u32 = 530_000;

/// The recorded shape of [`SetupScreen`]'s logical state (`registry::SCREEN_SHAPES`). The password
/// is recorded as its length and caret only: a replay must never carry it.
pub const SHAPE: &str = "SetupScreen{mode:u8,phase:u8,editing:Option<u32>,address:{text:str,caret:u64},\
    username:{text:str,caret:u64},password:{len:u64,caret:u64},blink_us:u32,failure:Option<u8>,\
    pending:bool,lineup:u8,saved:u8,resume:u8,discovery:u8}";

/// Where the page is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Waiting for the stored account to be read (milliseconds; nothing is drawn but the ground).
    Loading,
    Form,
    /// A sign-in is in flight. The form stays on screen; its controls do nothing.
    Working,
    Connected,
}

/// Why the form's last attempt did not end signed in — what the status line says.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Failure {
    /// Sign in pressed with a field empty.
    Missing,
    Sign(Error),
    /// Signed in, but the account could not be stored.
    NotSaved,
    /// The worker went away without answering (the OS refused it a thread).
    NoAnswer,
}

impl Failure {
    fn disc(&self) -> u8 {
        match self {
            Failure::Missing => 0,
            Failure::Sign(_) => 1,
            Failure::NotSaved => 2,
            Failure::NoAnswer => 3,
        }
    }
}

/// The status line for `failure`: where the person should look, in their language.
fn failure_text(failure: &Failure) -> Cow<'static, str> {
    match failure {
        Failure::Missing => msg::dispatcharr_setup_missing().into(),
        Failure::NotSaved => msg::dispatcharr_setup_save_failed().into(),
        Failure::NoAnswer => msg::dispatcharr_setup_unexpected("no answer").into(),
        Failure::Sign(e) => match e {
            Error::BadAddress => msg::dispatcharr_setup_bad_address().into(),
            Error::Unreachable => msg::dispatcharr_setup_unreachable().into(),
            Error::NotDispatcharr | Error::NotFound => msg::dispatcharr_setup_not_dispatcharr().into(),
            Error::Unauthorized => msg::dispatcharr_setup_unauthorized().into(),
            Error::Forbidden => msg::dispatcharr_setup_forbidden().into(),
            Error::TooManyRequests => msg::dispatcharr_setup_throttled().into(),
            Error::Unavailable => msg::dispatcharr_setup_unavailable().into(),
            // The technical detail is the server's half of the story; the sentence around it is ours.
            Error::Redirected(_) | Error::Http(_) | Error::BadResponse(_) => {
                msg::dispatcharr_setup_unexpected(&e.to_string()).into()
            }
        },
    }
}

/// The channel list behind a connected account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lineup {
    Pending,
    Channels(usize),
    Failed,
}

impl Lineup {
    fn disc(self) -> u8 {
        match self {
            Lineup::Pending => 0,
            Lineup::Channels(_) => 1,
            Lineup::Failed => 2,
        }
    }
}

/// What Connected shows, kept from the outcome that put the page there.
struct Connected {
    server: String,
    username: String,
    lineup: Lineup,
    saved: Saved,
    /// The app starts on the last channel watched.
    resume: bool,
}

impl Connected {
    /// The lines under the verdict, each its own block: who and where, then the lineup, then (only
    /// when the save is not durable) that the TV will ask again after a restart. Separate blocks,
    /// not one string with line breaks: `TextView` reads a newline as a space, by design.
    fn lines(&self) -> Vec<String> {
        let mut lines = vec![msg::dispatcharr_setup_signed_in_as(&self.server, &self.username)];
        match self.lineup {
            Lineup::Pending => {}
            Lineup::Channels(n) => lines.push(msg::dispatcharr_setup_channels(n as i64)),
            Lineup::Failed => lines.push(msg::dispatcharr_setup_channels_failed().to_owned()),
        }
        if self.saved == Saved::UntilReboot {
            lines.push(msg::dispatcharr_setup_until_reboot().to_owned());
        }
        lines
    }
}

/// The content column's geometry: one field block per field, then the button, then the status.
mod layout {
    use plx_ui::route_screen::RouteLayout;
    use plx_ui::{theme, Rect};

    /// A field's caption line (its label, `CAPTION`).
    pub const LABEL_H: f32 = 36.0;
    /// The value's line box, at `TITLE`.
    pub const VALUE_H: f32 = 64.0;
    /// The note under the address (`Not encrypted on your network`), reserved whether or not it
    /// shows so the fields below never jump as the address changes scheme.
    pub const NOTE_H: f32 = 32.0;
    pub const FIELD_GAP: f32 = theme::space::LG;
    pub const BUTTON_H: f32 = 60.0;
    pub const STATUS_GAP: f32 = theme::space::LG;

    /// The column the form stands in: the route layout's content column, from its top.
    pub fn column() -> Rect {
        RouteLayout::screen().content
    }

    /// Field `i`'s whole block (label and value line).
    pub fn field(i: usize) -> Rect {
        let c = column();
        let mut y = c.y;
        for j in 0..i {
            y += LABEL_H + VALUE_H + FIELD_GAP + if j == 0 { NOTE_H } else { 0.0 };
        }
        Rect::new(c.x, y, c.w, LABEL_H + VALUE_H)
    }

    /// Field `i`'s value line — the focus rect, and what the [`super::TextField`] draws into.
    pub fn value(i: usize) -> Rect {
        let b = field(i);
        Rect::new(b.x, b.y + LABEL_H, b.w, VALUE_H)
    }

    pub fn note() -> Rect {
        let v = value(0);
        Rect::new(v.x, v.y + v.h, v.w, NOTE_H)
    }

    pub fn button_top() -> f32 {
        let last = field(2);
        last.y + last.h + FIELD_GAP
    }

    pub fn status_top() -> f32 {
        button_top() + BUTTON_H + STATUS_GAP
    }
}

/// The logical state this page records (`LogicalState`), refreshed after every change.
struct SetupState {
    mode: Mode,
    phase: Phase,
    editing: Option<usize>,
    address: (String, usize),
    username: (String, usize),
    password: (usize, usize),
    blink_us: u32,
    failure: Option<u8>,
    pending: bool,
    lineup: u8,
    saved: u8,
    /// 0 not connected, 1 off, 2 on.
    resume: u8,
    /// [`Discovery::disc`].
    discovery: u8,
}

impl LogicalState for SetupState {
    fn write(&self, c: &mut Canon) {
        c.u8(self.mode as u8).u8(self.phase as u8);
        c.option(self.editing, |c, i| {
            c.u32(i as u32);
        });
        c.str(&self.address.0).u64(self.address.1 as u64);
        c.str(&self.username.0).u64(self.username.1 as u64);
        c.u64(self.password.0 as u64).u64(self.password.1 as u64);
        c.u32(self.blink_us);
        c.option(self.failure, |c, d| {
            c.u8(d);
        });
        c.bool(self.pending).u8(self.lineup).u8(self.saved).u8(self.resume).u8(self.discovery);
    }
    fn probe(&self, out: &mut String) {
        out.push_str(&format!(
            "setup phase={:?} editing={:?} failure={:?} pending={}",
            self.phase, self.editing, self.failure, self.pending
        ));
    }
}

/// Where the form's network search is.
enum Discovery {
    /// Not asked (an address was there already, or the page is not the form).
    Idle,
    Searching(Receiver<Vec<String>>),
    /// The address it filled in.
    Found(String),
    /// It found nothing, or the address was typed before it answered.
    Done,
}

impl Discovery {
    fn disc(&self) -> u8 {
        match self {
            Discovery::Idle => 0,
            Discovery::Searching(_) => 1,
            Discovery::Found(_) => 2,
            Discovery::Done => 3,
        }
    }
}

pub struct SetupScreen {
    entry: EntryId,
    instance: InstanceId,
    mode: Mode,
    phase: Phase,
    fields: [TextBuffer; FIELDS],
    views: [TextField; FIELDS],
    /// Which field the keyboard types into, while it is up.
    editing: Option<usize>,
    blink_us: u32,
    /// Each field's focus crossfade.
    hot: [Spring; FIELDS],
    failure: Option<Failure>,
    pending: Option<Receiver<SetupOutcome>>,
    connected: Option<Connected>,
    /// Whether the address as typed is plain HTTP (the note under it).
    plaintext: bool,
    /// The search for a Dispatcharr on the TV's own network, which fills in the address.
    discovery: Discovery,
    spin_ms: f32,
    ground: RouteGround,
    state: SetupState,
}

impl SetupScreen {
    pub fn new(entry: EntryId, instance: InstanceId, mode: Mode) -> Self {
        let size = theme::size::TITLE;
        let mut screen = Self {
            entry,
            instance,
            mode,
            phase: Phase::Loading,
            fields: std::array::from_fn(|_| TextBuffer::new(String::new(), 0)),
            views: [TextField::new(size), TextField::new(size), TextField::new(size).masked()],
            editing: None,
            blink_us: 0,
            hot: [Spring::at(0.0); FIELDS],
            failure: None,
            pending: None,
            connected: None,
            plaintext: false,
            discovery: Discovery::Idle,
            spin_ms: 0.0,
            ground: RouteGround::new(),
            state: SetupState {
                mode,
                phase: Phase::Loading, editing: None, address: (String::new(), 0),
                username: (String::new(), 0), password: (0, 0), blink_us: 0, failure: None,
                pending: false, lineup: 0, saved: 0, resume: 0, discovery: 0,
            },
        };
        screen.sync_state();
        screen
    }

    fn key(&self, elem: u32) -> FocusKey<u32> {
        FocusKey { entry: self.entry, elem }
    }

    fn sync_state(&mut self) {
        let text = |b: &TextBuffer| (b.text().to_owned(), b.caret());
        self.state = SetupState {
            mode: self.mode,
            phase: self.phase,
            editing: self.editing,
            address: text(&self.fields[0]),
            username: text(&self.fields[1]),
            password: (self.fields[2].text().chars().count(), self.fields[2].caret()),
            blink_us: self.blink_us,
            failure: self.failure.as_ref().map(Failure::disc),
            pending: self.pending.is_some(),
            lineup: self.connected.as_ref().map_or(0, |c| c.lineup.disc()),
            saved: match self.connected.as_ref().map(|c| c.saved) {
                None => 0,
                Some(Saved::Durable) => 1,
                Some(Saved::UntilReboot) => 2,
            },
            resume: self.connected.as_ref().map_or(0, |c| 1 + u8::from(c.resume)),
            discovery: self.discovery.disc(),
        };
    }

    /// The elements focus can stand on in this phase, in walk order.
    fn elems(&self) -> &'static [u32] {
        match self.phase {
            Phase::Loading => &[],
            Phase::Form | Phase::Working => &[ADDRESS, USERNAME, PASSWORD, SIGN_IN],
            Phase::Connected => &[RESUME, SIGN_OUT],
        }
    }

    fn rect(&self, elem: u32, measure: &dyn plx_machine::machine::Measure) -> Option<Rect> {
        match elem {
            ADDRESS | USERNAME | PASSWORD => Some(layout::value(elem as usize)),
            SIGN_IN => {
                let w = Button::pill_w_measured(msg::dispatcharr_setup_sign_in_c(), theme::size::BODY, false, false, measure);
                let c = layout::column();
                Some(Rect::new(c.x, layout::button_top(), w, layout::BUTTON_H))
            }
            RESUME => {
                let w = Button::pill_w_measured(self.resume_label(), theme::size::BODY, false, false, measure);
                let c = layout::column();
                Some(Rect::new(c.x, c.y, w.min(c.w), layout::BUTTON_H))
            }
            SIGN_OUT => {
                let w = Button::pill_w_measured(msg::dispatcharr_setup_sign_out_c(), theme::size::BODY, false, false, measure);
                let a = RouteLayout::screen().action;
                Some(Rect::new(a.x, a.y + (a.h - layout::BUTTON_H) * 0.5, w, layout::BUTTON_H))
            }
            _ => None,
        }
    }

    fn resume_label(&self) -> &'static std::ffi::CStr {
        if self.connected.as_ref().is_some_and(|c| c.resume) {
            msg::dispatcharr_setup_resume_on_c()
        } else {
            msg::dispatcharr_setup_resume_off_c()
        }
    }

    fn reseat<H: AppLike>(&self, elem: u32, fx: &mut Effects<'_, H>) {
        fx.push(Fx::Deliver(
            MachineId::Instance(self.instance),
            Delivery::Screen(ScreenEvent::Enter(Enter::Fresh { focus: FocusTarget::Elem(self.key(elem)) })),
        ));
    }

    /// Raise (`Some(field)`) or dismiss (`None`) the television's keyboard. Moving the target from
    /// one field to another while it is up keeps the panel up.
    fn keyboard<H: AppLike>(&mut self, target: Option<usize>, fx: &mut Effects<'_, H>) {
        let was_up = self.editing.is_some();
        self.editing = target;
        self.blink_us = 0;
        if let Some(i) = target {
            // Typing resumes at the end of what is there, as Search's field does.
            let text = self.fields[i].text().to_owned();
            let len = text.len();
            self.fields[i] = TextBuffer::new(text, len);
        }
        if was_up != target.is_some() {
            fx.push(Fx::Deliver(MachineId::Instance(self.instance), Delivery::Keyboard { up: target.is_some() }));
        }
        fx.invalidate(Provenance::Input);
    }

    fn edit<H: AppLike>(&mut self, edit: &TextEdit, fx: &mut Effects<'_, H>) {
        let Some(i) = self.editing else { return };
        self.fields[i].edit(edit);
        if i == 0 {
            self.plaintext = address_is_plaintext(self.fields[0].text());
        }
        self.blink_us = 0;
        fx.invalidate(Provenance::Input);
    }

    fn activate<H: AppLike>(&mut self, elem: u32, fx: &mut Effects<'_, H>) {
        match (self.phase, elem) {
            (Phase::Form, ADDRESS | USERNAME | PASSWORD) => {
                let i = elem as usize;
                let target = if self.editing == Some(i) { None } else { Some(i) };
                self.keyboard(target, fx);
            }
            (Phase::Form, SIGN_IN) => self.sign_in(fx),
            (Phase::Connected, RESUME) => {
                if let Some(c) = &mut self.connected {
                    c.resume = !c.resume;
                    fx.push(Fx::App(AppFx::Dispatcharr(DispatcharrCmd::SetResume { on: c.resume })));
                }
            }
            (Phase::Connected, SIGN_OUT) => {
                let (reply, rx) = mpsc::channel();
                self.pending = Some(rx);
                fx.push(Fx::App(AppFx::Dispatcharr(DispatcharrCmd::SignOut { reply })));
            }
            _ => {}
        }
        fx.invalidate(Provenance::Input);
    }

    fn sign_in<H: AppLike>(&mut self, fx: &mut Effects<'_, H>) {
        self.keyboard(None, fx);
        let address = self.fields[0].text().trim().to_owned();
        let username = self.fields[1].text().trim().to_owned();
        let password = self.fields[2].text().to_owned();
        if address.is_empty() || username.is_empty() || password.is_empty() {
            self.failure = Some(Failure::Missing);
            return;
        }
        self.failure = None;
        self.phase = Phase::Working;
        self.spin_ms = 0.0;
        let (reply, rx) = mpsc::channel();
        self.pending = Some(rx);
        fx.push(Fx::App(AppFx::Dispatcharr(DispatcharrCmd::SignIn { address, username, password, reply })));
    }

    /// Fill the address and username from an account, so signing back in is one password away.
    fn prefill(&mut self, server: &str, username: &str) {
        self.fields[0] = TextBuffer::new(server.to_owned(), server.len());
        self.fields[1] = TextBuffer::new(username.to_owned(), username.len());
        self.plaintext = address_is_plaintext(server);
    }

    /// Read whatever the application has answered since the last tick.
    fn poll<H: AppLike>(&mut self, fx: &mut Effects<'_, H>) {
        loop {
            let Some(rx) = &self.pending else { return };
            let outcome = match rx.try_recv() {
                Ok(outcome) => outcome,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.pending = None;
                    if matches!(self.phase, Phase::Working | Phase::Loading) {
                        self.phase = Phase::Form;
                        self.failure = Some(Failure::NoAnswer);
                        self.reseat(SIGN_IN, fx);
                        fx.invalidate(Provenance::Input);
                    }
                    return;
                }
            };
            self.land(outcome, fx);
            fx.invalidate(Provenance::Input);
        }
    }

    fn land<H: AppLike>(&mut self, outcome: SetupOutcome, fx: &mut Effects<'_, H>) {
        match outcome {
            // The Server page has no form: signed out (or the account gone from under it), the
            // boot page takes over as the root.
            SetupOutcome::SignedOut if self.mode == Mode::Server => {
                fx.push(Fx::App(AppFx::Loop(LoopReq::DispatcharrSetup)));
            }
            SetupOutcome::SignedOut => {
                if let Some(c) = self.connected.take() {
                    self.prefill(&c.server, &c.username);
                }
                self.phase = Phase::Form;
                self.reseat(if self.fields[0].text().is_empty() { ADDRESS } else { PASSWORD }, fx);
                // Nothing to sign in to yet: look for a server on the TV's own network.
                if self.fields[0].text().is_empty() && matches!(self.discovery, Discovery::Idle) {
                    let (reply, rx) = mpsc::channel();
                    self.discovery = Discovery::Searching(rx);
                    fx.push(Fx::App(AppFx::Dispatcharr(DispatcharrCmd::Discover { reply })));
                }
            }
            SetupOutcome::Connected { record, saved, resume_on_launch } => {
                let server = record.account.server.base();
                self.prefill(&server, &record.username);
                // The password has done its one job; it does not stay in memory.
                self.fields[2] = TextBuffer::new(String::new(), 0);
                self.failure = None;
                self.connected = Some(Connected {
                    server,
                    username: record.username,
                    lineup: Lineup::Pending,
                    saved,
                    resume: resume_on_launch,
                });
                match self.mode {
                    // The boot page's job is done: the Guide replaces it. The page keeps its
                    // spinner (or nothing, at boot) for the frame the replacement takes.
                    Mode::Entry => fx.push(Fx::App(AppFx::Loop(LoopReq::DispatcharrGuide))),
                    Mode::Server => {
                        self.phase = Phase::Connected;
                        self.reseat(RESUME, fx);
                    }
                }
            }
            SetupOutcome::Lineup(result) => {
                if let Some(c) = &mut self.connected {
                    c.lineup = match result {
                        Ok(n) => Lineup::Channels(n),
                        Err(_) => Lineup::Failed,
                    };
                }
            }
            SetupOutcome::Failed(e) => {
                self.phase = Phase::Form;
                self.failure = Some(Failure::Sign(e.clone()));
                // Seat where the fix is typed: the password for a refused one, the address for one
                // that went nowhere, Sign in otherwise.
                self.reseat(match e {
                    Error::Unauthorized => PASSWORD,
                    Error::BadAddress | Error::Unreachable | Error::NotDispatcharr | Error::NotFound => ADDRESS,
                    _ => SIGN_IN,
                }, fx);
            }
            SetupOutcome::SaveFailed => {
                self.phase = Phase::Form;
                self.failure = Some(Failure::NotSaved);
                self.reseat(SIGN_IN, fx);
            }
        }
    }

    fn tick<H: AppLike>(&mut self, t: Tick, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) {
        self.poll(fx);
        self.poll_discovery(fx);
        if self.phase == Phase::Working {
            self.spin_ms += t.dt_us as f32 / 1000.0;
        }
        if step_blink(&mut self.blink_us, self.editing.is_some(), t.dt_us) {
            fx.invalidate(Provenance::Input);
        }
        let focus = cx.focus.current.filter(|k| k.entry == self.entry).map(|k| k.elem);
        let mut present = fx.present();
        for (i, hot) in self.hot.iter_mut().enumerate() {
            let target = if self.editing == Some(i) || focus == Some(i as u32) { 1.0 } else { 0.0 };
            plx_machine::motion::spring(&mut hot.pos, &mut hot.vel, target, plx_ui::consts::K_SCALE, t, &mut present);
        }
    }

    /// The network search's answer: the first server found fills in the address, unless one was
    /// typed meanwhile.
    fn poll_discovery<H: AppLike>(&mut self, fx: &mut Effects<'_, H>) {
        let Discovery::Searching(rx) = &self.discovery else { return };
        let found = match rx.try_recv() {
            Ok(found) => found,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Vec::new(),
        };
        self.discovery = match found.into_iter().next() {
            Some(address) if self.fields[0].text().is_empty() && self.editing != Some(0) => {
                let len = address.len();
                self.fields[0] = TextBuffer::new(address.clone(), len);
                self.plaintext = address_is_plaintext(&address);
                Discovery::Found(address)
            }
            _ => Discovery::Done,
        };
        fx.invalidate(Provenance::Input);
    }

    fn draw_form<H: AppLike>(&mut self, f: &mut DrawFrame<'_, '_, H>, p: Painter, focus: Option<u32>) {
        let layout = RouteLayout::screen();
        layout.draw_narrative(p, self.back_to(), msg::dispatcharr_setup_title(), msg::dispatcharr_setup_copy(),
            theme::size::BODY, f.measure);
        let labels = [msg::dispatcharr_setup_address_c(), msg::dispatcharr_setup_username_c(), msg::dispatcharr_setup_password_c()];
        let placeholders = [msg::dispatcharr_setup_address_example_c(), msg::dispatcharr_setup_empty_field_c(),
            msg::dispatcharr_setup_empty_field_c()];
        for i in 0..FIELDS {
            let block = layout::field(i);
            let hot = self.hot[i].pos;
            Label::new(labels[i].as_ptr(), theme::size::CAPTION,
                theme::cross(theme::TEXT_TERTIARY, theme::TEXT_SECONDARY, hot))
                .draw(p, Rect::new(block.x, block.y, block.w, layout::LABEL_H));
            let buffer = &self.fields[i];
            self.views[i].prepare(buffer.text(), buffer.caret(), placeholders[i], f.measure);
            self.views[i].draw(p, layout::value(i), Look {
                editing: self.editing == Some(i),
                hot,
                caret_on: self.blink_us < BLINK_US,
            });
        }
        let found = matches!(&self.discovery, Discovery::Found(a) if a.as_str() == self.fields[0].text());
        let note = match &self.discovery {
            Discovery::Searching(_) if self.fields[0].text().is_empty() => Some(msg::dispatcharr_setup_searching_c()),
            _ if found => Some(msg::dispatcharr_setup_found_c()),
            _ if self.plaintext => Some(msg::dispatcharr_setup_plaintext_c()),
            _ => None,
        };
        if let Some(note) = note {
            Label::new(note.as_ptr(), theme::size::CAPTION, theme::TEXT_TERTIARY).draw(p, layout::note());
        }
        if let Some(rect) = self.rect(SIGN_IN, f.measure) {
            Button::new(msg::dispatcharr_setup_sign_in_c().as_ptr(), theme::size::BODY, rect)
                .focused(focus == Some(SIGN_IN) && self.phase == Phase::Form)
                .draw(&Env::inert(), p);
        }
        self.draw_status(f, p);
        if self.phase == Phase::Form {
            for elem in [ADDRESS, USERNAME, PASSWORD, SIGN_IN] {
                self.stop(f, p, elem);
            }
        }
    }

    /// The one line under *Sign in*: the spinner while working, else why the last attempt failed.
    fn draw_status<H: AppLike>(&self, f: &mut DrawFrame<'_, '_, H>, p: Painter) {
        let c = layout::column();
        let top = layout::status_top();
        if self.phase == Phase::Working {
            let line_h = f.measure.line_h(theme::size::BODY);
            let cy = top + line_h * 0.5;
            plx_ui::placeholder::note(p, plx_ui::placeholder::Reason::PageSpinner, "setup");
            Spinner::leading(c.x, cy).phase(self.spin_ms as u32).tint(theme::TEXT_SECONDARY).draw(&Env::inert(), p);
            let x = c.x + Spinner::inline_gutter();
            Label::new(msg::dispatcharr_setup_signing_in_c().as_ptr(), theme::size::BODY, theme::TEXT_SECONDARY)
                .draw(p, Rect::new(x, top, c.w - (x - c.x), line_h));
        } else if let Some(failure) = &self.failure {
            let text = failure_text(failure);
            TextView::new(&text, theme::size::BODY, theme::TEXT_SECONDARY)
                .max_lines(3)
                .draw(p, Rect::new(c.x, top, c.w, (RouteLayout::screen().content.y + RouteLayout::screen().content.h - top).max(0.0)));
        }
    }

    /// Where BACK goes, as the crumb names it: the Guide from the Server page, nowhere from the boot
    /// page.
    fn back_to(&self) -> Option<&'static str> {
        (self.mode == Mode::Server).then(msg::dispatcharr_guide_title)
    }

    fn draw_connected<H: AppLike>(&mut self, f: &mut DrawFrame<'_, '_, H>, p: Painter, focus: Option<u32>) {
        let Some(connected) = &self.connected else { return };
        let layout = RouteLayout::screen();
        let lines = connected.lines();
        let mut y = layout.draw_narrative(p, self.back_to(), msg::dispatcharr_setup_connected(), &lines[0],
            theme::size::BODY, f.measure);
        for line in &lines[1..] {
            y += theme::space::SM;
            let size = theme::size::BODY;
            y += TextView::new(line, size, theme::TEXT_READING)
                .leading(size as f32 + theme::space::XS)
                .max_lines(3)
                .draw(p, Rect::new(layout.narrative.x, y, layout.narrative.w, (layout.action.y - y).max(0.0)));
        }
        if let Some(rect) = self.rect(RESUME, f.measure) {
            Button::new(self.resume_label().as_ptr(), theme::size::BODY, rect)
                .focused(focus == Some(RESUME))
                .draw(&Env::inert(), p);
            self.stop(f, p, RESUME);
            let c = layout::column();
            let top = rect.y + rect.h + theme::space::SM;
            TextView::new(msg::dispatcharr_setup_resume_note(), theme::size::BODY, theme::TEXT_SECONDARY)
                .max_lines(3)
                .draw(p, Rect::new(c.x, top, c.w, (c.y + c.h - top).max(0.0)));
        }
        if let Some(rect) = self.rect(SIGN_OUT, f.measure) {
            Button::new(msg::dispatcharr_setup_sign_out_c().as_ptr(), theme::size::BODY, rect)
                .focused(focus == Some(SIGN_OUT))
                .draw(&Env::inert(), p);
            self.stop(f, p, SIGN_OUT);
        }
    }

    fn stop<H: AppLike>(&self, f: &mut DrawFrame<'_, '_, H>, p: Painter, elem: u32) {
        if !f.records_stops() {
            return;
        }
        let Some(rect) = self.rect(elem, f.measure) else { return };
        f.stop(p, Stop { key: self.key(elem), rect, rest_rect: rect, clip: Rect::FULL, hover: Hover::Focus,
            activate: Activate::Direct });
    }
}

/// Whether an address as typed would carry the password in clear: an explicit `http://`, or no
/// scheme at all (Dispatcharr's own default is plain HTTP on port 9191).
fn address_is_plaintext(address: &str) -> bool {
    let a = address.trim();
    !a.is_empty() && !a.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://"))
}

/// Advance a caret blink by `dt_us`; whether its visible phase flipped (a redraw is owed). At rest
/// (not editing) the clock is held at the on phase and reports nothing.
fn step_blink(blink_us: &mut u32, editing: bool, dt_us: u32) -> bool {
    if !editing {
        *blink_us = 0;
        return false;
    }
    let was = *blink_us < BLINK_US;
    *blink_us = ((*blink_us as u64 + dt_us as u64) % (2 * BLINK_US) as u64) as u32;
    was != (*blink_us < BLINK_US)
}

impl<H: AppLike> Focusable<H> for SetupScreen {
    fn groups(&self, cx: &Cx<'_, H>, out: &mut Vec<GroupSpec>) {
        let elems = self.elems();
        let Some(extent) = elems.iter().filter_map(|&e| self.rect(e, cx.measure)).reduce(|a, b| a.union(b)) else {
            return;
        };
        out.push(GroupSpec {
            id: GROUP,
            kind: GroupKind::Column,
            seat: Seat::First,
            reachable: AxisMask::BOTH,
            // Nothing else on this page is focusable: a walk off either end stays put.
            edge: [EdgeRule::Stop; 4],
            extent,
            len: elems.len(),
            elem: ElemKind::Bare,
        });
    }
    fn group_of(&self, key: &u32, _cx: &Cx<'_, H>) -> Option<GroupId> {
        self.elems().contains(key).then_some(GROUP)
    }
    fn neighbour(&self, key: FocusKey<u32>, dir: Dir, _cx: &Cx<'_, H>) -> Step<u32> {
        let elems = self.elems();
        let Some(at) = elems.iter().position(|&e| e == key.elem) else { return Step::Edge };
        let next = match dir {
            Dir::Up => at.checked_sub(1),
            Dir::Down => Some(at + 1).filter(|&i| i < elems.len()),
            Dir::Left | Dir::Right => None,
        };
        next.map_or(Step::Edge, |i| Step::Move(self.key(elems[i])))
    }
    fn place(&self, key: &u32, cx: &Cx<'_, H>, _at: At) -> Option<Placed> {
        let index = self.elems().iter().position(|e| e == key)?;
        let rect = self.rect(*key, cx.measure)?;
        Some(Placed { rect, rest_rect: rect, clip: Rect::FULL, index: Some(index as u32) })
    }
    fn reconcile(&self, want: FocusKey<u32>, _cx: &Cx<'_, H>) -> FocusKey<u32> {
        if self.elems().contains(&want.elem) {
            return want;
        }
        self.key(self.elems().first().copied().unwrap_or(ADDRESS))
    }
    fn seat(&self, _g: GroupId, _from: Placed, _cx: &Cx<'_, H>) -> FocusKey<u32> {
        self.key(self.elems().first().copied().unwrap_or(ADDRESS))
    }
}

impl<H: AppLike> Machine<H> for SetupScreen {
    type Ev = ScreenEvent<H>;
    fn step(&mut self, ev: &Self::Ev, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        let handled = self.step_inner(ev, cx, fx);
        self.sync_state();
        handled
    }
}

impl SetupScreen {
    fn step_inner<H: AppLike>(&mut self, ev: &ScreenEvent<H>, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        match ev {
            ScreenEvent::Mount => {
                let (reply, rx) = mpsc::channel();
                self.pending = Some(rx);
                // Only the Server page shows the lineup; the boot page would throw it away.
                fx.push(Fx::App(AppFx::Dispatcharr(DispatcharrCmd::Load { lineup: self.mode == Mode::Server, reply })));
            }
            ScreenEvent::Tick(t) => self.tick(*t, cx, fx),
            ScreenEvent::Activate(elem) => self.activate(*elem, fx),
            ScreenEvent::WillLeave(_) | ScreenEvent::Unmount | ScreenEvent::Cover | ScreenEvent::Suspend => {
                self.keyboard(None, fx);
            }
            ScreenEvent::FocusMoved { to, .. } => {
                // The keyboard follows focus between the fields and closes when focus leaves them.
                if self.editing.is_some() {
                    let target = (to.elem <= PASSWORD).then_some(to.elem as usize);
                    self.keyboard(target, fx);
                }
                fx.invalidate(Provenance::Input);
            }
            ScreenEvent::Input(InputEvent { kind, .. }) => return self.input(kind, cx, fx),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn input<H: AppLike>(&mut self, kind: &InputKind<u32>, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        match kind {
            InputKind::SystemKeyboard(up) => {
                // The panel went away on its own (its own close key): stop editing. One that rose
                // without our asking types into whichever field has focus.
                if !*up {
                    self.editing = None;
                } else if self.editing.is_none() {
                    self.editing = cx.focus.current.map(|k| k.elem).filter(|&e| e <= PASSWORD).map(|e| e as usize);
                }
                self.blink_us = 0;
                fx.invalidate(Provenance::Input);
                Handled::Yes
            }
            InputKind::Text(edit) if self.editing.is_some() || matches!(cx.owner, InputOwner::System(_)) => {
                self.edit(edit, fx);
                Handled::Yes
            }
            InputKind::Key { key, sym, edge, .. } if *edge != Edge::Up => {
                if let Some(i) = self.editing {
                    let edit = match (*key, *sym) {
                        (Key::Left, _) => Some(TextEdit::Left),
                        (Key::Right, _) => Some(TextEdit::Right),
                        (_, plx_ui::consts::SDLK_BACKSPACE) => Some(TextEdit::Backspace),
                        (_, plx_ui::consts::SDLK_CLEAR) => Some(TextEdit::Clear),
                        _ => None,
                    };
                    if let Some(edit) = edit {
                        self.edit(&edit, fx);
                        return Handled::Yes;
                    }
                    match key {
                        Key::Back => {
                            self.keyboard(None, fx);
                            return Handled::Yes;
                        }
                        // OK on the panel: on to the next field with the panel still up, and from
                        // the password to *Sign in* with it closed.
                        Key::Ok => {
                            if i + 1 < FIELDS {
                                self.keyboard(Some(i + 1), fx);
                                self.reseat(i as u32 + 1, fx);
                            } else {
                                self.keyboard(None, fx);
                                self.reseat(SIGN_IN, fx);
                            }
                            return Handled::Yes;
                        }
                        _ => {}
                    }
                }
                if *key == Key::Back && *edge == Edge::Down {
                    match self.mode {
                        // There is nowhere inside the app to go back to: hand the screen to the TV.
                        Mode::Entry => fx.push(Fx::App(AppFx::Loop(LoopReq::BackAtRoot))),
                        Mode::Server => fx.push(Fx::Nav(NavOp::Pop)),
                    }
                    return Handled::Yes;
                }
                Handled::No
            }
            _ => Handled::No,
        }
    }
}

impl<H: AppLike> Screen<H> for SetupScreen {
    fn name(&self) -> &'static str {
        word::SETUP
    }
    fn state(&self) -> &dyn LogicalState {
        &self.state
    }
    fn crumb(&self, _cx: &Cx<'_, H>) -> Option<Cow<'_, str>> {
        // The boot page: BACK does not go anywhere inside the app (`ui/CLAUDE.md`'s route-family
        // rule). The Server page: BACK returns to the Guide.
        self.back_to().map(Cow::Borrowed)
    }
    fn prepare(&mut self, _b: &mut Budget, _cx: &Cx<'_, H>) {}
    fn draw(&mut self, f: &mut DrawFrame<'_, '_, H>) {
        let p = f.painter;
        // `Painter::root()` for the ground, as the sign-in screen does: the ambient wash must not
        // ride the page-transition cascade this page's own painter carries.
        self.ground.draw_default(Painter::root());
        let focus = f.focus.current.filter(|k| k.entry == self.entry).map(|k| k.elem);
        match self.phase {
            Phase::Loading => {}
            Phase::Form | Phase::Working => self.draw_form(f, p, focus),
            Phase::Connected => self.draw_connected(f, p, focus),
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
#[path = "setup_tests.rs"]
mod tests;
