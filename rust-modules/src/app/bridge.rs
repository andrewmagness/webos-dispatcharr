//! **The bridge between the loop and the dispatcher** (restructure spec §14).
//!
//! What lives here: the application's `Host` ([`AppHost`]) — its screen argument [`AppArg`], the
//! whole page alphabet; the rig the dispatcher borrows ([`Bridge`]: the real `TtfMeasure`, the
//! requests an owned screen makes of the loop); [`frame`], which the loop
//! calls once per iteration with the inputs it collected for the dispatcher; and **the loop's
//! navigations as container ops** ([`nav_root`], [`nav_push`], [`nav_pop`], [`nav_pop_to`],
//! [`nav_cancel`], [`show_page`], [`background`], [`foreground`]). The container is the one
//! navigation authority: nothing else keeps a copy of which page is on top.
//!
//! The coexistence contract (§14), stated once:
//! - **Input.** The loop's ladders ask [`Dispatcher::owns_input`] before their first arm. While a
//!   surface is up, or the top page is an owned one, every key, pointer move, click and wheel is
//!   an `InputEvent` for the dispatcher and the ladders see nothing.
//! - **Draw.** Owned pages are drawn in the page closure; surfaces keep their own appear alpha. The
//!   dispatcher's own gate wanting a present becomes `idle::invalidate`.
//! - **The host under a surface.** The dispatcher's host fold is the loop's freeze/skip decision
//!   (`host_frozen`/`host_replaced`), and the surface's phases drive `popover`'s host-user
//!   counters ([`Bridge::sync_host`]).
//! - **Words.** The heartbeat's `overlay=` reads the topmost surface's `Screen::name`
//!   ([`overlay_word`]), so the fps tier's word table is the screens' own (§15.3).
//! - **The privileged calls.** `opaque_route` and `clear_opaque_region` delegate to
//!   `app/run.rs::rig_opaque_route`/`rig_clear_opaque_region` rather than naming
//!   `plx_platform::tv::window::` here, which keeps the OS-facing call text in one file (D4;
//!   `ci/check-deps.sh`'s `frame` gate).

use std::ffi::CStr;

use plx_screens::registry::{AppArg, AppFx, AppMounter, AppMsg, ContentReq, LoopReq, PageMemory};
use plx_ui::containers::modal::{HostRender, Phase};
use plx_ui::dispatch::{CxParts, Dispatcher, FrameReport, Rig, Split};
#[cfg(test)]
use plx_screens::registry::every_surface_arg;
#[cfg(test)]
use plx_ui::dispatch::NoTap;
use plx_ui::frame::Budget;
use plx_machine::machine::{
    Canon, Cx, Edge, Effects, EntryId, Handled, Host, InputEvent, InputKind,
    Key, LogicalState, MachineId, Measure, NavOp, Source, Tick, TimerId,
};
use plx_machine::present::Present;
use plx_ui::screen::{FocusSource, ReturnState};

// ---------------------------------------------------------------------------------------------
// the bundle
// ---------------------------------------------------------------------------------------------

pub(crate) struct AppHost;

#[derive(Clone, Copy)]
pub(crate) struct AppViews<'a> {
    /// **The playback session, as this frame's publication** (spec §2.3). The Player machine
    /// (`App.player`) owns the value; `Split` can only lend what the RIG owns, so the loop copies
    /// the decisions in once per frame ([`plx_media::route::PlaybackSession::publication`]) and a
    /// screen reads them here. A screen that wants to CHANGE the playback emits an effect
    /// (`AppFx::Player`, `ContentReq::Play`) — there is no `&mut` on this path by construction.
    pub(crate) session: &'a plx_media::route::PlaybackSession,
}

impl Bridge {

}

#[derive(Default)]
pub(crate) struct BridgeInit;

impl LogicalState for BridgeInit {
    fn write(&self, _w: &mut Canon) {}
    fn probe(&self, out: &mut String) {
        out.push_str("bridge");
    }
}

impl Host for AppHost {
    type Arg = AppArg;
    type Fx = AppFx;
    type Msg = AppMsg;
    type Elem = u32;
    type Views<'a> = AppViews<'a>;
    type Init = BridgeInit;
    type Memory = PageMemory;

    fn shed_memory(memory: &mut PageMemory, focus: Option<u32>) {
        memory.shed_to_identity(focus);
    }
}

impl plx_screens::registry::PlayerLike for AppHost {
    fn session<'a>(cx: &Cx<'a, Self>) -> &'a plx_media::route::PlaybackSession { cx.views.session }
}

// ---------------------------------------------------------------------------------------------
// the rig
// ---------------------------------------------------------------------------------------------

/// What the bridge lends the dispatcher, and what it collects for the loop.
pub(crate) struct Bridge {
    mounter: AppMounter,
    /// This frame's publication of the playback session — see `AppViews::session`. Refreshed by
    /// [`Bridge::publish_playback`] from the loop, once per iteration.
    playback: plx_media::route::PlaybackSession,
    /// Was the publication above refreshed on the last frame? The one bit that lets the retirement
    /// of a stale publication be a single assignment rather than a per-frame one.
    playback_live: bool,
    /// **Is the hardware video plane bound?** `Player::video_plane_bound`, published here by the
    /// loop on its edges — the rig's own copy of the one bit, for the privileged call at draw
    /// entry (`clear_opaque_region`), which the `Rig` trait gives no argument.
    video_plane: bool,
    /// **Erased, so the host suite can supply one that does not need a font.** The app's own
    /// measure is `text::TtfMeasure`, which reads the fonts `init_text` opened at boot and carries
    /// a deliberate `debug_assert!` when there are none. A host test has no font file, so any test
    /// that drives a real frame through a real screen would trip it. `Split::measure` is
    /// `&dyn Measure` already, so erasing it here costs the app nothing and buys
    /// [`Bridge::for_test`].
    measure: plx_ui::rec::Measurements,
    /// Requests the owned screens made of the loop this frame (§14), drained by [`frame`]'s caller.
    reqs: Vec<LoopReq>,
    content_reqs: Vec<(MachineId, ContentReq, ReturnState<u32, PageMemory>)>,
    /// What the player's overlay surfaces asked of the loop this frame (§14) — drained by
    /// `playback::player_requests`, which holds the `MainThread` token they cannot.
    player_reqs: Vec<plx_screens::registry::PlayerReq>,
    #[cfg(test)]
    keyboard_calls: Vec<bool>,
    #[cfg(test)]
    keyboard_adoptions: usize,
    effect_return: ReturnState<u32, PageMemory>,
    /// The surfaces whose host counters this bridge holds: (entry, cached, closing).
    held: Vec<(EntryId, bool, bool)>,
    now_us: fn() -> u64,
}

impl Bridge {

    pub(crate) fn new(now_us: fn() -> u64) -> Self {
        // A `static`, not `&TtfMeasure` inline: a unit-struct literal DOES const-promote to
        // `'static` today, but that is a rule about the expression rather than a promise about
        // this field, and a `static` states the lifetime outright.
        static TTF: plx_gfx::text::TtfMeasure = plx_gfx::text::TtfMeasure;
        Self::with_measure(&TTF, now_us)
    }

    /// The same bridge over a measure that needs no fonts — the ONLY constructor a host test may
    /// use. See [`Bridge::measure`] for what happens to a test that reaches for `new` instead.
    #[cfg(test)]
    pub(crate) fn for_test(now_us: fn() -> u64) -> Self {
        static FIXTURE: plx_ui::fixture::FixtureMeasure = plx_ui::fixture::FixtureMeasure;
        Self::with_measure(&FIXTURE, now_us)
    }

    fn with_measure(measure: &'static dyn Measure, now_us: fn() -> u64) -> Self {
        Self {
            mounter: AppMounter::default(),
            playback: plx_media::route::PlaybackSession::IDLE,
            playback_live: false,
            video_plane: false,
            measure: plx_ui::rec::Measurements::Live(measure),
            reqs: Vec::new(),
            content_reqs: Vec::new(),
            player_reqs: Vec::new(),
            #[cfg(test)]
            keyboard_calls: Vec::new(),
            #[cfg(test)]
            keyboard_adoptions: 0,
            effect_return: ReturnState::default(),
            held: Vec::new(),
            now_us,
        }
    }

    pub(crate) fn take_reqs(&mut self) -> Vec<LoopReq> {
        std::mem::take(&mut self.reqs)
    }

    pub(crate) fn take_content_reqs(&mut self) -> Vec<(MachineId, ContentReq, ReturnState<u32, PageMemory>)> {
        std::mem::take(&mut self.content_reqs)
    }

    pub(crate) fn take_player_reqs(&mut self) -> Vec<plx_screens::registry::PlayerReq> {
        std::mem::take(&mut self.player_reqs)
    }

    /// Where the next player instance returns to — see `AppMounter::player_origin`.
    pub(crate) fn seed_player_origin(&mut self, origin: plx_screens::player::Origin) {
        self.mounter.player_origin = Some(origin);
    }

    /// Drive `popover`'s host-user counters from the surface phases (module doc).
    fn sync_host(&mut self, d: &Dispatcher<AppHost>) {
        let live: Vec<(EntryId, bool, bool)> = d
            .nav
            .modals
            .surfaces
            .iter()
            .filter(|s| s.phase != Phase::Hidden)
            .map(|s| {
                // DERIVED from the container's own policy table, never a second `matches!` over
                // `Style` — see `modal::style_caches_host`.
                let cached = plx_ui::containers::modal::style_caches_host(s.style);
                (s.entry.id, cached, s.phase == Phase::Closing)
            })
            .collect();
        // released: held but no longer live
        let mut keep = Vec::new();
        for (id, cached, closing) in self.held.drain(..) {
            match live.iter().find(|(e, _, _)| *e == id) {
                None => plx_ui::popover::surface_released(cached, closing),
                Some((_, _, now_closing)) => {
                    if *now_closing && !closing {
                        plx_ui::popover::surface_closing(cached);
                    }
                    keep.push((id, cached, *now_closing));
                }
            }
        }
        for (id, cached, closing) in live {
            if !keep.iter().any(|(e, _, _)| *e == id) {
                plx_ui::popover::surface_held(cached);
                if closing {
                    plx_ui::popover::surface_closing(cached);
                }
                keep.push((id, cached, closing));
            }
        }
        self.held = keep;
    }
}

/// **Hand back every counter this bridge is still holding.**
///
/// `held` is not a cache — it is an OWNERSHIP list. Each entry means this bridge has called
/// `popover::surface_held` and owes the matching `surface_released`, and those counters are
/// process-globals that decide whether the page under a panel stays frozen. In the shipped app
/// one `Bridge` lives as long as the process; this is for the HOST SUITE, where every test builds
/// its own bridge, opens a surface and drops both, and a leaked count fails some other module's
/// test as cross-test pollution.
impl Drop for Bridge {
    fn drop(&mut self) {
        for (_, cached, closing) in self.held.drain(..) {
            plx_ui::popover::surface_released(cached, closing);
        }
    }
}

impl Bridge {

    /// **Publish the video plane's EDGE**, from `Player::set_video_plane_bound` and from nowhere
    /// else (spec §16 risk 10).
    pub(crate) fn publish_video_plane(&mut self, bound: bool) {
        self.video_plane = bound;
    }

    /// **Publish this frame's playback session** (spec §2.3) — see `AppViews::session`.
    ///
    /// Called once per iteration by the loop, BEFORE the dispatcher frame and the tree draw, so
    /// every screen in one frame reads one consistent picture of the playback. The copy is skipped
    /// entirely when nothing that reads it is mounted: off the player route the publication is
    /// `PlaybackSession::IDLE` and stays there.
    pub(crate) fn publish_playback(&mut self, session: &plx_media::route::PlaybackSession, live: bool) {
        if live {
            self.playback = session.publication();
            self.playback_live = true;
        } else if self.playback_live {
            self.playback = plx_media::route::PlaybackSession::IDLE;
            self.playback_live = false;
        }
    }

}

impl Rig<AppHost> for Bridge {
    fn page_alpha(&self) -> f32 { plx_ui::nav::page_alpha() }
    fn navigation_presentation(&self) -> plx_ui::screen::NavPresentation {
        plx_ui::screen::NavPresentation {
            page_alpha: plx_ui::nav::page_alpha(),
            chrome_alpha: plx_ui::nav::chrome_alpha(),
            view_tab: u32::try_from(plx_ui::nav::view_tab(-1)).ok(),
            blur_amount: plx_ui::nav::blur_amount(),
        }
    }
    fn surface_scope(&mut self) -> Option<plx_ui::popover::host::Live> {
        Some(plx_ui::popover::host::live())
    }
    fn split(&mut self) -> Split<'_, AppHost> {
        Split {
            mounter: &mut self.mounter,
            views: AppViews {
                session: &self.playback,
            },
            measure: &self.measure,
        }
    }
    fn deliver(&mut self, _to: MachineId, msg: &AppMsg, _parts: &CxParts<u32>, _fx: &mut Effects<'_, AppHost>) -> Handled {
        match *msg {}
    }

    fn timer(&mut self, _owner: MachineId, _id: TimerId, _parts: &CxParts<u32>, _fx: &mut Effects<'_, AppHost>) {}
    fn app_return(&mut self, _from: MachineId, ret: ReturnState<u32, PageMemory>) {
        self.effect_return = ret;
    }
    fn app_fx(&mut self, from: MachineId, fx: AppFx, _parts: &CxParts<u32>, _out: &mut Effects<'_, AppHost>) {
        self.app_effect(from, fx);
    }
    fn log(&mut self, line: &str) {
        plx_base::eventlog::log(line);
    }
    fn system_keyboard(&mut self, up: bool) {
        #[cfg(test)]
        self.keyboard_calls.push(up);
        #[cfg(not(test))]
        if up { crate::textinput::start(); } else { crate::textinput::stop(); }
    }
    fn adopt_system_keyboard(&mut self) {
        #[cfg(test)]
        { self.keyboard_adoptions += 1; }
        #[cfg(not(test))]
        crate::textinput::adopt();
    }
    /// §3.3 step 9's application half — the render cache's upload step — and it is EMPTY on
    /// purpose while the loop owns the frame: the loop calls `adapters::poster::prepare` itself, in
    /// the right window, spending the same one `Budget` this hook would have been handed.
    fn prepare(&mut self, _b: &mut Budget, _present: &mut Present) {}
    fn ls2_pump(&mut self) {}
    /// §3.3 step 9, every frame, presented or not. The argument is the dispatcher's
    /// `Present::video_plane()`, i.e. the Player machine's own bit, and `tv::window::opaque_route`
    /// only sends a wayland request when the answer CHANGES. Delegates to `run::rig_opaque_route`
    /// (D4) rather than naming `plx_platform::tv::window::opaque_route` here directly.
    fn opaque_route(&mut self, video_plane_bound: bool) {
        super::run::rig_opaque_route(video_plane_bound);
    }
    /// §3.3 step 10, at draw entry, conditional on the same bit: NULLing the opaque region on a
    /// frame with no plane under it would silently retract the claim `opaque_route` had just made.
    fn clear_opaque_region(&mut self) {
        if self.video_plane {
            super::run::rig_clear_opaque_region();
        }
    }
    fn now_us(&self) -> u64 {
        (self.now_us)()
    }
    fn back_at_root(&mut self) {
        self.reqs.push(LoopReq::BackAtRoot);
    }
}

impl Bridge {
    fn app_effect(&mut self, from: MachineId, fx: AppFx) {
        match fx {
            AppFx::Loop(req) => self.reqs.push(req),
            AppFx::Content(req) => self.content_reqs.push((from, req, self.effect_return.clone())),
            AppFx::Player(req) => self.player_reqs.push(req),
            AppFx::Dispatcharr(command) => super::dispatcharr::execute(command),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// the frame, and the loop's questions
// ---------------------------------------------------------------------------------------------

/// The dispatcher's frame, once per loop iteration: the pending navigation's own commit (at
/// [`PageDip`](plx_ui::containers::transition::PageDip)'s floor), then
/// the ten steps WITHOUT the draw (the loop draws at its own slot, on its own gate). Returns the
/// top page's heartbeat word.
#[cfg(test)]
pub(crate) fn frame(
    d: &mut Dispatcher<AppHost>, rig: &mut Bridge,
    tick: Tick, inputs: Vec<InputEvent<u32>>,
) -> (&'static str, FrameReport) {
    frame_with_tap(d, rig, tick, inputs, &mut NoTap)
}

pub(crate) fn frame_with_tap(
    d: &mut Dispatcher<AppHost>,
    rig: &mut Bridge,
    tick: Tick,
    inputs: Vec<InputEvent<u32>>,
    tap: &mut dyn plx_ui::dispatch::Tap<AppHost>,
) -> (&'static str, FrameReport) {
    frame_with_results(d, rig, tick, inputs, Vec::new, tap)
}

pub(crate) type AppResults = Vec<(plx_machine::machine::Addr, AppMsg)>;

/// One dispatcher path for live or supplied adapter results. The supplier runs at ingest, after
/// frame-view capture, and this function never additionally polls a live mailbox.
///
/// **The trunk every app frame passes through, so it is where the test-only lock rule is
/// ENFORCED**: a frame pumps the storage worker, which is process-wide state a concurrent test may
/// own.
pub(crate) fn frame_with_results(
    d: &mut Dispatcher<AppHost>,
    rig: &mut Bridge,
    tick: Tick,
    inputs: Vec<InputEvent<u32>>,
    take: impl FnOnce() -> AppResults,
    tap: &mut dyn plx_ui::dispatch::Tap<AppHost>,
) -> (&'static str, FrameReport) {
    let _frame_scope = plx_base::task::FrameScope::enter();
    #[cfg(test)]
    plx_base::testlock::assert_held("the storage pump behind an app::bridge frame");
    plx_base::storage_worker::pump_retained();
    let surface = d.surface_up();
    // A surface's MOTION is not attributed here: `ModalStack::tick` and `Dispatcher`'s per-surface
    // step and draw open one scope each (§4.4); the page's tick runs in none.
    let results = take();
    let report = plx_base::diag::spans::span("dframe", || d.frame_with(rig, tick, inputs, results, tap, false));
    d.prune(&report.unmounted);
    plx_base::diag::spans::span("dsync", || rig.sync_host(d));
    // On the simulator, a frame the video plane alone presented is not reported as damage: the
    // loop's gate presents it on its own video-plane term anyway, and the report would keep the
    // settled-capture clock (`plx_machine::idle::last_change_ms`) from ever seeing a paused player at
    // rest (`FrameReport::video_only`). The television keeps the unconditional report.
    let video_only = cfg!(feature = "hostsim") && report.video_only;
    if report.presented && !video_only {
        // The dispatcher's gate wants a frame: the loop's gate presents it. While a surface is up
        // this bump is the PANEL's.
        let _own = surface.then(plx_machine::idle::OwnScope::open);
        plx_machine::idle::invalidate();
    }
    // **Publish the transition's presentation** for the readers that draw outside a page's own
    // `DrawFrame` — see `ui::nav`'s module doc. No page of this application is a tab pill.
    plx_ui::nav::publish(d.nav.tabs.stack.page_alpha(), d.nav.tabs.stack.chrome_alpha(), None);
    // **The heartbeat's `route=` word IS the top page's own name** (§15.2).
    let word = d.top_screen().map_or("", |s| s.name());
    (word, report)
}

// ---------------------------------------------------------------------------------------------
// the loop's navigations, as container ops
// ---------------------------------------------------------------------------------------------

/// **Replace the WHOLE stack with `arg`** (spec §3.4 `NavOp::Root`).
///
/// Every entry, including the current root, leaves for good and `arg` is minted as the sole
/// survivor — unless the stack is already exactly `[arg]`, which is a `PopTo(root)` no-op instead
/// of empty churn. This is the sign-in/sign-out reset: there is no page underneath worth returning
/// to, so none is kept.
pub(crate) fn nav_root(d: &mut Dispatcher<AppHost>, arg: AppArg) {
    d.request(MachineId::Nav, NavOp::Root(arg));
}

/// **Put `want` on top, reusing an entry that already holds it.**
///
/// A LANDING rather than a navigation, which is why it reuses: pushing blindly would put a second
/// copy of a page the user is already standing on over the first. An empty stack is rooted.
pub(crate) fn show_page(d: &mut Dispatcher<AppHost>, want: AppArg) {
    use plx_ui::screen::ScreenArg;
    if d.nav.top_page().map(|e| e.arg.same_instance(&want)).unwrap_or(false) { return; }
    let existing = d.nav.tabs.stack.entries.iter().rev()
        .find(|e| e.arg.same_instance(&want)).map(|e| e.id);
    if let Some(id) = existing {
        nav_pop_to(d, id);
    } else if d.nav.top_page().is_none() {
        nav_root(d, want);
    } else {
        nav_push(d, want);
    }
}

/// **Stack a page** — the Server page, the player.
pub(crate) fn nav_push(d: &mut Dispatcher<AppHost>, arg: AppArg) {
    d.request(MachineId::Nav, NavOp::Push(arg));
}

/// …with the outgoing page's `ReturnState` supplied rather than read off the engine, for a request
/// an owned screen froze on ITS press frame.
pub(crate) fn nav_push_with_return(
    d: &mut Dispatcher<AppHost>, arg: AppArg, ret: ReturnState<u32, PageMemory>,
) {
    d.request_with_return(MachineId::Nav, NavOp::Push(arg), ret);
}

/// **Back to a NAMED entry** — the player's exit (§5.1: the origin is an `EntryId`).
///
/// `PopTo` is a no-op for an entry that is no longer on the stack, and a player whose origin has
/// gone would then have no way off the screen at all — so an absent origin falls back to the
/// floor of the stack (the Guide, or Setup before an account is stored), which is always there
/// once a page has been rooted.
pub(crate) fn nav_pop_to(d: &mut Dispatcher<AppHost>, entry: EntryId) {
    if d.nav.tabs.stack.entries.iter().any(|e| e.id == entry) {
        d.request(MachineId::Nav, NavOp::PopTo(entry));
    } else if let Some(root) = d.nav.tabs.stack.entries.first().map(|e| e.id) {
        d.request(MachineId::Nav, NavOp::PopTo(root));
    }
}

/// **Withdraw a transition that has not committed**, iff the page that asked for it is still the
/// top (`NavStack::cancel`'s own rule, §6.2). Returns whether there was one, so an input that
/// cancelled NOTHING falls through to its normal handling instead of being swallowed.
pub(crate) fn nav_cancel(d: &mut Dispatcher<AppHost>) -> bool {
    let Some(top) = d.nav.top_page().map(|e| e.id) else { return false };
    d.nav.tabs.stack.cancel(top)
}

/// **The OS took the screen** (SDL `0x103`/`0x104`): the tree is PARKED (§9, §12.1). The page
/// stack does not move: the player's origin is the entry beneath it, so a background that
/// destroyed entries would destroy the way back.
pub(crate) fn background(d: &mut Dispatcher<AppHost>) {
    d.suspend();
}

/// **…and gave it back** (SDL `0x105`/`0x106`). Idempotent, which is why the loop calls it on both
/// edges: webOS sends `will` and `did` and tells the app nothing about which arrives first.
pub(crate) fn foreground(d: &mut Dispatcher<AppHost>) {
    d.resume();
}

/// **The player instance, if one is mounted** — the one door to the state phase 9 moved off `App`.
///
/// `None` means "there is no playback on screen": the page is mounted when the
/// `NavOp::Push(AppArg::Player)` that `app::playback::enter_player` asks for commits, and unmounted
/// when the entry is popped or retired. An overlay presented on the player's page-owned
/// `ModalStack` is a SURFACE, so the player stays the top PAGE while a panel is up.
pub(crate) fn player(d: &Dispatcher<AppHost>) -> Option<&plx_screens::player::PlayerScreen> {
    d.nav
        .top_page()?
        .inst
        .as_ref()?
        .screen
        .as_any()?
        .downcast_ref::<plx_screens::player::PlayerScreen>()
}

pub(crate) fn player_mut(
    d: &mut Dispatcher<AppHost>,
) -> Option<&mut plx_screens::player::PlayerScreen> {
    let entry = d.nav.top_page()?.id;
    d.nav
        .entry_mut(entry)?
        .inst
        .as_mut()?
        .screen
        .as_any_mut()?
        .downcast_mut::<plx_screens::player::PlayerScreen>()
}

/// **The top page answers its own keys** — the engine owns the input for it.
pub(crate) fn page_owned(d: &Dispatcher<AppHost>) -> bool {
    d.top_screen().map_or(false, |s| s.focus_source() == FocusSource::Engine)
}

/// **Does the top page take the television keyboard's text?** Every owned page with nothing
/// presented over it: the text arrives as `InputKind::Text` and the page decides whether a field is
/// editing (Setup's three fields are the only consumer today). It was asked of the Search page by
/// name, which left a Setup page's typing queued in `textinput` where nothing read it.
pub(crate) fn text_owned(d: &Dispatcher<AppHost>) -> bool {
    text_reaches_page(d.surface_up(), page_owned(d))
}

/// [`text_owned`]'s rule over its two facts, so it can be graded without a container.
pub(crate) fn text_reaches_page(surface_up: bool, page_owned: bool) -> bool {
    !surface_up && page_owned
}

pub(crate) fn owns_input(d: &Dispatcher<AppHost>) -> bool {
    d.surface_up() || d.owns_input()
}

/// The host page is REPLACED: the loop skips drawing it.
pub(crate) fn host_replaced(d: &Dispatcher<AppHost>) -> bool {
    d.host_policy().1 == HostRender::Replaced
}

/// What the loop's PAGE half draws this frame, from the host fold and who owns the top page.
///
/// §8.3: a Replaced host "receives nothing" — and that includes its cached quad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PagePlan {
    /// The closure draws the page and, through `Dispatcher::draw(.., true)`, the surfaces.
    Owned,
    /// The closure draws the legacy fallback; the loop draws the surfaces after it.
    LegacyThenSurfaces,
    /// The host is replaced: no page pass, no cached quad — the surfaces alone.
    SurfacesOnly,
}

pub(crate) fn page_plan(host_replaced: bool, page_owned: bool) -> PagePlan {
    match (host_replaced, page_owned) {
        (true, _) => PagePlan::SurfacesOnly,
        (false, true) => PagePlan::Owned,
        (false, false) => PagePlan::LegacyThenSurfaces,
    }
}

/// **The heartbeat's ` overlay=` word IS the topmost surface's own `Screen::name`** — this
/// function is the read, and there is nothing else to it. The prefix belongs to the heartbeat's
/// own format string (`run`'s `route={rn}{ov}`), not to the word.
pub(crate) fn overlay_word(d: &Dispatcher<AppHost>) -> Option<&'static str> {
    d.top_surface_name()
}

/// …and the ` overlay=` word each surface PRESENTS as, read back through [`overlay_word`] on a real
/// tree, so this reads the same function the heartbeat does, on the same container. The player's
/// panels are presented over the Player page, which is where a panel needs its player.
#[cfg(test)]
pub(crate) fn every_surface_word() -> Vec<&'static str> {
    let mut out = Vec::new();
    for arg in every_surface_arg() {
        let mut d = Dispatcher::<AppHost>::new();
        let mut rig = Bridge::for_test(|| 0);
        show_page(&mut d, AppArg::Player);
        frame(&mut d, &mut rig, Tick { ms: 0, dt_us: 16_000 }, vec![]);
        // The style is the application's to choose per surface (`Navigation::next_style`), and it
        // does not change the word — `Compact` is enough for every one of them here.
        d.nav.next_style = plx_ui::containers::modal::Style::Compact;
        d.request(MachineId::Nav, NavOp::Present(arg));
        frame(&mut d, &mut rig, Tick { ms: 16, dt_us: 16_000 }, vec![]);
        out.push(overlay_word(&d).expect("a presented surface names a word"));
    }
    out
}

/// A key for the dispatcher: the raw SDL fields classified once (`ui::consts::classify`), the
/// fork's packed state read as an edge.
pub(crate) fn key_input(sym: u32, wcode: u32, state: u32, now: Tick, source: Source) -> InputEvent<u32> {
    use plx_ui::consts::{classify, Key as K};
    let key = match classify(sym, wcode) {
        K::Up => Key::Up,
        K::Down => Key::Down,
        K::Left { .. } => Key::Left,
        K::Right { .. } => Key::Right,
        K::Ok => Key::Ok,
        K::Back => Key::Back,
        _ => Key::Other,
    };
    let edge = if (state & 0xff) != 1 {
        Edge::Up
    } else if state & 0x100 != 0 {
        Edge::Repeat
    } else {
        Edge::Down
    };
    InputEvent {
        at: now,
        source,
        kind: InputKind::Key {
            key,
            sym,
            wcode,
            edge,
            at_edge: false,
        },
    }
}

pub(crate) fn pointer_input(x: f32, y: f32, now: Tick) -> InputEvent<u32> {
    InputEvent {
        at: now,
        source: Source::Sdl,
        kind: InputKind::Pointer { x, y, hit: None },
    }
}

pub(crate) fn click_input(x: f32, y: f32, now: Tick) -> InputEvent<u32> {
    InputEvent {
        at: now,
        source: Source::Sdl,
        kind: InputKind::Click { x, y, hit: None },
    }
}

/// **Pointer motion with a button held down** (§7.5) — the pointer kind that drives a CONTROL
/// rather than focus: `ui::hit` resolves the hit and then deliberately raises neither hover nor
/// activation for it. Its one consumer is the player's scrub bar.
pub(crate) fn drag_input(x: f32, y: f32, now: Tick) -> InputEvent<u32> {
    InputEvent {
        at: now,
        source: Source::Sdl,
        kind: InputKind::Drag { x, y, hit: None },
    }
}

/// A wheel tick as the key it stands for on a vertical flow.
pub(crate) fn wheel_input(dy: i32, now: Tick) -> Vec<InputEvent<u32>> {
    let key = if dy < 0 { Key::Down } else { Key::Up };
    let (sym, wcode) = (0, 0);
    vec![
        InputEvent {
            at: now,
            source: Source::Sdl,
            kind: InputKind::Key { key, sym, wcode, edge: Edge::Down, at_edge: false },
        },
        InputEvent {
            at: now,
            source: Source::Sdl,
            kind: InputKind::Key { key, sym, wcode, edge: Edge::Up, at_edge: false },
        },
    ]
}

/// **A pointer button-UP as the press machine's release.** The dispatcher's `InputKind` has no
/// release of its own: `InputMachine::release` is reached from ONE place, an `Ok` key on the `Up`
/// edge, whatever armed the press. Without it a mouse press would sit dipped until
/// `press::MAX_HOLD_MS` committed it.
pub(crate) fn release_input(now: Tick) -> InputEvent<u32> {
    InputEvent {
        at: now,
        source: Source::Sdl,
        kind: InputKind::Key {
            key: Key::Ok,
            sym: 0,
            wcode: 0,
            edge: Edge::Up,
            at_edge: false,
        },
    }
}

#[allow(dead_code)]
fn _measure_is_object_safe(m: &dyn Measure, s: &CStr) -> f32 {
    m.width(s, 1, false)
}

// Whole-app tests that moved up out of `ui` (docs/module-layers.md, step L13): the overscan audit
// over every layer's outermost rects. (`app/mod.rs` may not declare test modules: `check-deps.sh`
// testmod.)
#[cfg(test)]
#[path = "overscan_audit_tests.rs"]
mod overscan_audit_tests;
