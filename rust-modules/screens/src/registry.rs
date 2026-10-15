//! **The application bundle's SCREEN-SIDE half** (restructure spec §3.1, phase 5b): the effects a
//! screen may ask for (`AppFx`), the messages a machine receives (`AppMsg`), and the requests an
//! owned screen makes of the legacy LOOP (`LoopReq`) while the two coexist (§14).
//!
//! **…and, since restructure phase 10, the concrete `ScreenArg` ([`AppArg`]) and the one `mount`
//! match ([`AppMounter`])** — which §2.1 always put here and which lived in `app/bridge.rs` until
//! then. The blocker was stated in this doc and is gone: the argument carried the legacy `Route`,
//! `Route` was `app`-private, and a screen may not name `app::`. `Route` moved here in phase 10
//! and was FOLDED INTO [`AppArg`] in phase 12 — its seven mountable values are flat variants and
//! the enum is gone (§15.2), so a page argument is one value rather than a value inside a
//! value. What §0's criterion 5 buys for that is the thing this module is for: **a new screen touches its own `screens/<name>.rs`,
//! this file, `dev/scenarios.rs` and `tests/manifest.json` and nothing else** — the variant, the id,
//! the `mount` arm and the recorded shape ([`SCREEN_SHAPES`]) are all here.
//!
//! What stays in `app/bridge.rs` is the concrete `Host` and the rig it lends the dispatcher
//! (`AppHost`, `AppViews`, `Bridge`). The mounter is generic over the host for the same reason the
//! screens are: the views it needs arrive through the `*Like` accessors below, so it names no
//! application type.
//!
//! `LoopReq` is a DEBT with a phase number on each variant: a request the loop performs because
//! the machine that should own it (Session, Player, Navigation over the app's real stack) is not
//! on the dispatcher yet. The bridge drains them after every dispatcher frame.

use plx_data::stores::{StoreCmd, StoreId, StoreWork};
use plx_machine::machine::{
    Canon, Chrome, Cx, Effects, EntryId, Host, InstanceId, LogicalState, ScreenId,
};
use plx_ui::screen::{Mounter, ReturnState, Screen};

/// The one conversion from a catalog row to the plain facts `ui` draws a tile from. It lives here
/// because a screen may name `registry` but never a sibling screen, and `ui` cannot name the row.
pub mod tile_facts;

/// The application's effects (spec §3.1).
pub enum AppFx {
    /// A store command, executed as a `Deliver` to the store machine in the same drain.
    Store(StoreId, StoreCmd),
    /// Poll only the store work this visible route owns, after its read-only step returns.
    StoreWork(StoreWork),
    /// A request of the legacy loop (§14) — see [`LoopReq`].
    Loop(LoopReq),
    /// Content requests (a live channel to play), executed by the navigation bridge.
    Content(ContentReq),
    /// Player-route requests — what an overlay panel on the player's own `ModalStack` asks of the
    /// loop, because the thing being asked for needs the `MainThread` token, the route or the
    /// trail (§14). Executed by `app/playback.rs`'s drain.
    Player(PlayerReq),
    /// The Setup and Guide screens' account and server work — see [`DispatcharrCmd`].
    Dispatcharr(DispatcharrCmd),
}

/// **What the Setup and Guide screens ask of the application.** Each reads or writes the
/// Dispatcharr account file or talks to the server, so each BLOCKS, and a screen may not: the
/// application runs it on a worker and answers on `reply`, which the screen reads on its tick.
///
/// No `Debug`: a sign-in carries the password.
pub enum DispatcharrCmd {
    /// Read the stored account. Answers `SignedOut` or `Connected`, then — when connected and
    /// `lineup` asks for it — the `Lineup` the key opens.
    Load { lineup: bool, reply: std::sync::mpsc::Sender<SetupOutcome> },
    /// Sign in with what was typed and store the key. Answers `Failed`, `SaveFailed` or `Connected`
    /// (the boot page then hands over to the Guide, which reads the lineup itself).
    SignIn { address: String, username: String, password: String, reply: std::sync::mpsc::Sender<SetupOutcome> },
    /// Forget the account. Answers `SignedOut`.
    SignOut { reply: std::sync::mpsc::Sender<SetupOutcome> },
    /// Read the stored account, its channels and the guide around now. Answers once.
    Guide { reply: std::sync::mpsc::Sender<GuideOutcome> },
    /// Store the favourite channels (UUIDs, in the order added) for the signed-in server. No
    /// answer: the Guide already shows the change, and a failed write is logged.
    SaveFavourites { favourites: Vec<String> },
    /// Store whether the app starts on the last channel watched (the Server page's switch). No
    /// answer, for the same reason.
    SetResume { on: bool },
    /// Look for a Dispatcharr on the television's own network. Answers once, with the address of
    /// every server that answered (`http://<ip>:9191`), possibly none.
    Discover { reply: std::sync::mpsc::Sender<Vec<String>> },
}

/// The answer to [`DispatcharrCmd::Guide`].
pub enum GuideOutcome {
    /// No stored account: the page goes back to Setup.
    SignedOut,
    /// The channels in guide order and their programmes, read at `fetched_at` (Unix seconds by the
    /// television's clock — the page's only reading of the wall clock). `logos` is each channel's
    /// logo URL, in the same order (`None` for a channel without one), and `streams` each
    /// channel's live stream URL (what OK tunes). `favourites` are the UUIDs of the channels the
    /// viewer marked on this TV. `resume` is the channel to start on: the last one watched, on the
    /// app's first guide of a run, when the viewer has not turned that off.
    Loaded {
        channels: Vec<plx_dispatcharr::dispatcharr::models::Channel>,
        /// The server's channel groups (empty when they could not be read: the Guide then offers
        /// no group switch).
        groups: Vec<plx_dispatcharr::dispatcharr::models::Group>,
        logos: Vec<Option<String>>,
        streams: Vec<String>,
        favourites: Vec<String>,
        resume: Option<String>,
        guide: plx_dispatcharr::dispatcharr::guide::Guide,
        fetched_at: i64,
    },
    Failed(plx_dispatcharr::dispatcharr::Error),
}

/// One answer to a [`DispatcharrCmd`].
pub enum SetupOutcome {
    SignedOut,
    /// Signed in: the stored record, whether it is stored where it survives a reboot, and whether
    /// the app starts on the last channel watched.
    Connected {
        record: plx_dispatcharr::dispatcharr::account::Record,
        saved: plx_dispatcharr::dispatcharr::account::Saved,
        resume_on_launch: bool,
    },
    /// The channel count behind the connected account.
    Lineup(Result<usize, plx_dispatcharr::dispatcharr::Error>),
    Failed(plx_dispatcharr::dispatcharr::Error),
    /// The server accepted the sign-in and the account could not be stored anywhere.
    SaveFailed,
}

/// **What a player overlay asks for**, once its own `step` has decided.
///
/// A panel on the player's page-owned `ModalStack` owns its own state and its own input, but not
/// the playback: seeking, pausing, applying a quality rung and leaving for a detail page all need
/// the `MainThread` token and the container ops `app::bridge` owns, neither of which a screen may
/// name (§2.1).
/// So the panel decides and the loop performs, exactly as `LibraryReq` does for the Library.
#[derive(Clone, Debug, PartialEq)]
pub enum PlayerReq {
    /// Change channel: stop this stream and play `0`'s channel (the live player's CH▲/▼, UP/DOWN
    /// and typed number). The loop performs it because the stop and the start need the engine.
    Tune(plx_media::route::LiveTune),
    /// Accepted only after the owned player confirmation. Never emitted by boot or Play.
    RepairSandbox,
    /// The failure read-out's *Try again*: resolve the failed item again, unchanged, at the
    /// position the viewer was at (`app::playback::retry_failed_playback`).
    RetryPlayback,
    /// The failure read-out's fix for Force Direct Play: set the Direct Play preference to Auto —
    /// the same persisted value Settings writes — and resolve the failed item again under it.
    PlayAutomatically,
    /// A transport key FELL THROUGH the panel (§ `overlay_transport_key_tests`): a viewer holding
    /// the track menu, the Info card or the Chapters strip open still expects PAUSE/PLAY to work,
    /// and the panel stays up. `true` = the key was PLAY, `false` = PAUSE; a PLAYPAUSE toggle is
    /// neither and is carried as `None`.
    Transport(Option<bool>),
    /// Seek to this position in ns and resume if paused — the Chapters strip's OK.
    SeekTo(i64),
    /// **Commit a SCRUB to this position, and leave a paused film paused** (restructure phase 12,
    /// PX-PLAYER) — `app::playback::commit_seek`, the other half of the pair above.
    ///
    /// The difference is the reason there are two. [`SeekTo`](PlayerReq::SeekTo) RESUMES, because
    /// a viewer who picked a chapter asked to watch it. A scrub does not: the transport's bar is
    /// how a PAUSED film is moved, and starting it there is a behaviour nobody asked for. Holding
    /// the pause across the seek needs three pieces of state that are the loop's and no screen's —
    /// `App::repause_at`, `TX.resume_pend` and the bounded seek-preroll feed override that lets
    /// the pipeline decode the landed frame without publishing a viewer Resume — which is why this
    /// is a request rather than something `PlayerScreen` performs.
    CommitSeek(i64),
    /// Apply the `…` popover's chosen row.
    More(plx_appkit::more_menu::Action),
    /// Apply the Info card's focused action.
    Info(plx_appkit::info_panel::InfoAction),
    /// The panel took a DOWN past its own bottom: drop the HUD's ring onto the tabs row.
    FocusTabs,
    /// Keep the transport alive while a panel is being read (`HUD_MENU_MS`), or hand it the
    /// ordinary linger as a panel closes (`HUD_LINGER_MS`).
    ExtendHud(u32),
    /// The Info card's OK landed on a control FACE, which has a press dip of its own: arm the
    /// tvOS press and commit on the spring-back rather than acting now.
    ArmInfoPress,
    /// The track menu picked a row. `route::commit_audio_selection`/`commit_subtitle_selection`
    /// take the playback session's `&mut`, which a screen never has (§2.2) — so the panel decides
    /// and the loop performs, exactly as every other request in this enum.
    CommitTrack(plx_appkit::track_menu::TrackCommit),
    /// **Present one of the player's overlays directly** (restructure phase 12) — the tabs row's OK
    /// (`OverlayKind::Info`/`::Chapters`, the old `key_ok`'s `focus == 2` arm) and the failure
    /// read-out's own recovery escape (`OverlayKind::More { quality: true }`, the old
    /// `key_player_failed`'s `ChooseQuality` arm). Both used to reach
    /// `super::bridge::open_player_overlay` straight from the loop's key ladder; `PlayerScreen`
    /// may not name that function itself (§2.2), so it asks instead.
    OpenOverlay(crate::player::overlay::OverlayKind),
    /// **OK landed on the transport's own control row** — whichever disc, the Skip pill or the Up
    /// Next tile currently occupies it (restructure phase 12, the old `key_ok`'s `focus == 1`
    /// arm). Arms the same tvOS press dip [`ArmInfoPress`](PlayerReq::ArmInfoPress) does; the
    /// loop's existing commit-frame dispatch (`app/run.rs`'s `Route::Player =>
    /// activate_player_row(...)`) is unchanged and performs whatever the row decides once the
    /// spring-back has played.
    ArmControlRow,
    /// **Leave the player** — the one ritual the STOP key, a BACK with nothing else open, and the
    /// failure read-out's own BACK escape all perform (`exit_player`; restructure phase 12,
    /// replacing `app/run.rs`'s direct calls to it from the player's own key arms).
    Exit,
}

/// Application payload on the container's return state. Focus itself remains engine-owned; no
/// page this application mounts keeps memory of its own beyond that, so the one value is `None`.
/// It stays an enum because the container is generic over a host's memory and the recorder hashes
/// it ([`PAGE_MEMORY_SHAPE`]).
#[derive(Clone, Debug, Default)]
pub enum PageMemory {
    #[default]
    None,
}

impl PageMemory {
    /// What an entry far below the top keeps (`plx_ui::containers::stack::DEEP`): nothing to shed.
    pub fn shed_to_identity(&mut self, _focus: Option<u32>) {}
}

impl plx_machine::machine::LogicalState for PageMemory {
    fn write(&self, c: &mut plx_machine::machine::Canon) {
        match self {
            Self::None => { c.u32(0); }
        }
    }
    fn probe(&self, out: &mut String) { out.push_str("page_memory"); }
}

pub const PAGE_MEMORY_SHAPE: &str = "PageMemory{None}";

/// Effects cross the screen/loop boundary; screens do not poll one another's pending latches.
pub enum ContentReq {
    Play { play: PlayIntent, resume_ns: i64 },
}

/// **The play a page decided on, for the loop to start** (spec §2.2, phase 9).
///
/// Starting playback takes the playback session's `&mut`, and an owned screen is only ever shown
/// the frame's publication (`AppViews::session`) — so the page names what to play and the loop
/// performs the request. The loop also owns what happens when the request is REFUSED.
pub enum PlayIntent {
    /// A live channel (the Guide's OK): the channel, in the lineup the player changes channel in.
    Live(plx_media::route::LiveTune),
}

/// A host publishing the Metadata layer's read surface borrowed from its concrete store owner
/// for this frame, for a screen generic over `H` that needs `plx_data::metadata::MetadataView`
/// rather than the app-concrete `Bridge`.
pub trait MetadataLike: AppLike + Sized {
    fn metadata<'a>(cx: &Cx<'a, Self>) -> plx_data::metadata::MetadataView<'a>;
    /// The subtitle search the player's Search page reads. PROVIDED, defaulting to `None` — a host
    /// that offers no search shows no Search row — so the many test hosts that only need metadata
    /// owe nothing; the production host overrides it with the store's view.
    fn subtitle_search<'a>(_cx: &Cx<'a, Self>) -> Option<plx_data::subsearch::SubSearchView<'a>> {
        None
    }
}

/// A host that publishes this frame's playback session (spec §2.3). The player's owned screens
/// read the Player machine's decisions through it; a change is asked for as an effect
/// (`AppFx::Player`, `ContentReq::Play`), because the publication is a copy and there is no `&mut`
/// on this path by construction.
pub trait PlayerLike: AppLike<Memory = PageMemory> + Sized {
    fn session<'a>(cx: &Cx<'a, Self>) -> &'a plx_media::route::PlaybackSession;
}

/// The application's messages (spec §3.1).
pub enum AppMsg {
    Store(StoreCmd),
    StoreWork(StoreWork),
}

/// What an owned screen asks the LEGACY LOOP to do, because the owner of that decision is not on
/// the dispatcher yet. Each names the phase that retires it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoopReq {
    /// BACK at a root the platform owns (the Guide, Setup): hand the screen to the television.
    BackAtRoot,
    /// **The Dispatcharr pages' three moves.** A screen cannot name `AppArg` in a `NavOp` (the
    /// bundle does not bind `Host::Arg`), so each is asked of the loop. Setup, once an account is
    /// stored (found at boot, or just signed in): the Guide, as the new root.
    DispatcharrGuide,
    /// The Guide's *Account*: the Server page, pushed over it.
    DispatcharrAccount,
    /// Signed out (from the Server page), or the Guide found no account or a refused key: Setup,
    /// as the new root, with nothing of the old account's pages left behind.
    DispatcharrSetup,
}

/// Any host that carries this bundle. The screens under `screens/` are written against it, so the
/// bridge's `AppHost` and the Settings surface's inner host both mount them unchanged.
pub trait AppLike: Host<Elem = u32, Fx = AppFx, Msg = AppMsg> {}
impl<H: Host<Elem = u32, Fx = AppFx, Msg = AppMsg>> AppLike for H {}

/// The heartbeat words an owned screen can name (§15.3's word table) — the same alphabet
/// `app::route_word`/`overlay_word` print, so the fps tier's `overlay=` selection cannot drift
/// from the screen that owns the frame. The heartbeat word IS the top screen's own `Screen::name`,
/// and `app::words::heartbeat_word_tests` derives both tables rather than transcribing either.
/// `tests/run.py` selects fps samples by these words (`tests/manifest.json`'s `route` field), so a
/// changed spelling silently disarms a scene rather than failing anything visible.
pub mod word {
    /// Dispatcharr sign-in (`screens::setup::SetupScreen`), a `route=` word; the same spelling as
    /// `app::words::route_word`'s `AppArg::Setup` arm.
    pub const SETUP: &str = "setup";
    /// The Dispatcharr guide (`screens::guide::GuideScreen`), the root once an account is stored.
    pub const GUIDE: &str = "guide";
}

// ── The modal REPEAT CADENCE ─────────────────────────────────────────────────────────────────
//
// **Here, and not in `screens/player/input.rs`, because it is not the player's** (phase 10 merge).
// It lived there while the player's four overlay panels were the only screens that paced their own
// `Edge::Repeat`; the item context menu became a surface in the same phase and needs the identical
// cadence, and a screen may not reach into a sibling family for a shared word (§2.1, and
// `ci/check-deps.sh`'s `sibling` gate). The `app/` side — `App::modal_repeat`, seeded in
// `app/boot.rs` — has always used it too, so "the player's input module" was never an honest home:
// the callers are one surface family, one standalone surface and the loop.

/// Rate-limits a REPEAT-DRIVEN discrete step — a forwarded hardware auto-repeat, or one tick of a
/// scroll-wheel gesture — to a couch-comfortable cadence, independent of the SOURCE's own cadence.
/// A held hardware key repeats roughly every 50ms; a wheel gesture can deliver several ticks in one
/// pass. Settings, Consent and Legal move a whole table row — or, inside a document, a full page of
/// reading text — per step, so letting either source drive `on_updown` at its own rate reads as a
/// blur rather than a scroll: item 13's whole ask.
///
/// Pure and host-testable — no `SDL_GetTicks` inside; `now` is threaded in by the caller, the same
/// shape `HeldKey`'s own `wrapping_sub` timing takes, so it survives the tick wrap the same way.
pub struct RepeatGate {
    /// The tick of the last step this gate admitted; `None` before the first one.
    pub last: Option<u32>,
}
impl RepeatGate {
    /// Minimum time between two repeat-driven steps this gate allows. Slower than the discrete
    /// focus-list repeat (110ms, [`PANEL_REPEAT_MS`]) on purpose — a home-grid card is a glance, a
    /// settings row or a line of reading text is not.
    pub const STEP_MS: u32 = 160;
    pub const IDLE: RepeatGate = RepeatGate { last: None };
    /// True at most once per [`Self::STEP_MS`]; always true the first call, or after a gap at
    /// least that long (which is also what makes a long-idle gate behave like a fresh one).
    pub fn ready(&mut self, now: u32) -> bool {
        self.ready_every(now, Self::STEP_MS)
    }
    /// The same gate at a caller-chosen cadence. The player's overlay surfaces take
    /// [`PANEL_REPEAT_MS`] through this, which is what preserves the exact hold-to-move feel the
    /// loop's own client-side repeat timer gave them before phase 9 moved their input onto the
    /// dispatcher: the remote streams `Edge::Repeat` at roughly 50 ms, and a list walked at that
    /// rate reads as a blur (see [`PANEL_REPEAT_MS`]).
    pub fn ready_every(&mut self, now: u32, step_ms: u32) -> bool {
        let due = match self.last {
            None => true,
            Some(last) => now.wrapping_sub(last) >= step_ms,
        };
        if due {
            self.last = Some(now);
        }
        due
    }
    /// **A fresh press restarts the cadence from itself.** The press is acted on unconditionally
    /// by its own arm — a fresh press is never swallowed by the cadence of the press before it —
    /// and this records it as the step it is, so the FIRST hardware repeat of the new hold waits a
    /// full [`PANEL_REPEAT_MS`] rather than landing on top of it.
    ///
    /// It cleared `last` instead until the surface tests were written, which was wrong in exactly
    /// the direction that is invisible on a fresh gate: the remote streams `Edge::Repeat` at ~50 ms
    /// and the first of them arrives with `last: None`, so a held key stepped TWICE before the
    /// cadence engaged, once for the press and once for the repeat behind it.
    pub fn rearm(&mut self, now: u32) {
        self.last = Some(now);
    }
}

/// **The cadence a held direction walks a player panel's list at**, in ms.
///
/// 110 ms, which is the number `app/run.rs`'s client-side repeat timer used for exactly these four
/// panels (the track menu, the `…` popover, the Info card and the Chapters strip) while their keys
/// went through the loop's own ladder. That timer existed because the Magic Remote streams
/// hardware auto-repeat at ~50 ms and a list walked at that rate is unusable; phase 9 put the
/// panels' input on the dispatcher, where `Edge::Repeat` arrives at the hardware's rate, so the
/// cadence has to be applied by the surface that receives it. Same number, same feel, one owner.
pub const PANEL_REPEAT_MS: u32 = 110;

#[cfg(test)]
mod repeat_gate_tests {
    use super::{RepeatGate, PANEL_REPEAT_MS};

    #[test]
    fn a_gate_admits_the_first_step_then_holds_the_cadence() {
        let mut gate = RepeatGate::IDLE;
        assert!(gate.ready(1_000), "nothing has fired yet");
        assert!(!gate.ready(1_050), "too soon");
        assert!(!gate.ready(1_159), "still short of the step");
        assert!(gate.ready(1_160), "exactly one step later");
        assert!(!gate.ready(1_161));
    }

    /// SDL ticks wrap at 2^32ms; the same arithmetic `HeldKey`'s lost-keyup net and client-side
    /// repeat already rely on, so this gate must survive it the same way.
    #[test]
    fn the_gate_survives_the_tick_wrap() {
        let mut gate = RepeatGate::IDLE;
        let at = u32::MAX - 50;
        assert!(gate.ready(at));
        assert!(!gate.ready(at.wrapping_add(100)));
        assert!(gate.ready(at.wrapping_add(160)));
    }

    /// The player panels' own cadence, and the reason `ready_every` exists: the remote's ~50 ms
    /// hardware repeat is admitted at 110 ms, not at 160 (a settings row) and not at 50.
    #[test]
    fn a_player_panel_walks_a_held_direction_at_its_own_cadence() {
        let mut gate = RepeatGate::IDLE;
        assert!(gate.ready_every(1_000, PANEL_REPEAT_MS));
        assert!(!gate.ready_every(1_050, PANEL_REPEAT_MS), "the hardware's own rate is too fast");
        assert!(gate.ready_every(1_110, PANEL_REPEAT_MS));
        // …and a FRESH press restarts the cadence FROM ITSELF. The press is acted on by its own
        // arm without consulting the gate, so it is never swallowed; what this pins is the other
        // half, which was wrong until the surface tests caught it — the first hardware repeat of
        // the new hold must wait a full cadence rather than landing on top of the press.
        gate.rearm(1_115);
        assert!(!gate.ready_every(1_165, PANEL_REPEAT_MS), "the repeat behind a fresh press waits");
        assert!(gate.ready_every(1_225, PANEL_REPEAT_MS));
    }
}

// ---------------------------------------------------------------------------------------------
// the page alphabet, the screen argument and the one mount match
// ---------------------------------------------------------------------------------------------

/// **A screen argument: everything the container needs to mount one screen, and nothing else.**
///
/// Two families in one enum, distinguished by nothing but which variant it is: the PAGES the
/// application stack can hold and the surfaces a `ModalStack` presents.
///
/// **Both owned variants carry the page their inner stack is ROOTED at**, which is there for the
/// dev boot targets and for nothing else: `/tmp/plxnative-settings=privacy` has to put a headless
/// run on a page that is normally two presses inside the surface, and rooting the stack at the
/// target needs nothing from the page. The one thing it costs is that BACK at a dev-booted child
/// DISMISSES the surface rather than revealing the root.
#[derive(Clone, PartialEq, Eq)]
pub enum AppArg {
    /// One of the player's panels, presented on the PLAYER PAGE's own `ModalStack` (§6.2).
    /// It carries only which panel: the host is the page it is presented over, which the container
    /// already knows, and the panel's own state is the instance's.
    PlayerOverlay(crate::player::overlay::PlayerOverlayArg),
    /// Playback. Its panels are NOT here — they are entries on the player page's own
    /// `ModalStack` ([`Self::PlayerOverlay`]), and the container owns which one is up.
    Player,
    /// Dispatcharr sign-in (`screens::setup`) — the boot page. With an account stored it hands
    /// straight over to the Guide.
    Setup,
    /// The Dispatcharr guide (`screens::guide`) — the root once an account is stored.
    Guide,
    /// The signed-in server and account, with *Sign out* (`screens::setup` in its server mode),
    /// pushed from the Guide.
    Server,
}

pub const ARG_SHAPE: &str = "AppArg{Setup,Guide,Server,Player,\
     PlayerOverlay{Tracks(tab:i32),Info,Chapters,More(quality:bool)}}";

impl LogicalState for AppArg {
    fn write(&self, c: &mut Canon) {
        // A canon tag is a surface's identity in a recorded state, so the tags of retired
        // arguments are never reused: they are allocated forward, exactly as `ScreenId` is.
        match self {
            Self::PlayerOverlay(arg) => { c.u32(5); arg.write(c); }
            Self::Player => { c.u32(0).u32(10); }
            Self::Setup => { c.u32(13); }
            Self::Guide => { c.u32(14); }
            Self::Server => { c.u32(15); }
        }
    }
    fn probe(&self, out: &mut String) { out.push_str("app_arg"); }
}

impl plx_ui::screen::ScreenArg for AppArg {
    /// No page this application mounts draws the shared top tab bar. Exhaustive on purpose: a new
    /// screen must not be able to answer this by accident.
    fn chrome(&self) -> Chrome {
        match self {
            AppArg::Setup
            | AppArg::Guide
            | AppArg::Server
            | AppArg::Player
            | AppArg::PlayerOverlay(_) => Chrome::None,
        }
    }
    fn id(&self) -> ScreenId {
        // Ids are allocated forward and never reused, so a retired identity is never handed to a
        // screen that replaced it (1–10 and 14–24 belonged to the pages and panels of the Plex
        // client this app was derived from).
        ScreenId(match self {
            // One id for every player panel, exactly as `Settings(_)` collapses its root payload:
            // they are kinds of ONE screen, and `same_instance` must never let the container
            // think it is holding two of them.
            AppArg::PlayerOverlay(_) => 16,
            AppArg::Player => 11,
            AppArg::Setup => 25,
            AppArg::Guide => 26,
            AppArg::Server => 27,
        })
    }
    fn title(&self) -> Option<&str> {
        None
    }
    fn same_instance(&self, other: &Self) -> bool {
        // **A player overlay's identity is its KIND, never the playback under it** (§16.9): the
        // panel is a surface on the player's own stack, so the page beneath it is not part of what
        // "the same instance" means here.
        if let (Self::PlayerOverlay(a), Self::PlayerOverlay(b)) = (self, other) {
            return a.kind.slot() == b.kind.slot();
        }
        // …and the same reason `id` collapses the payload: `Settings(Root)` and
        // `Settings(Legal)` are the same SCREEN.
        <Self as plx_ui::screen::ScreenArg>::id(self) == <Self as plx_ui::screen::ScreenArg>::id(other)
    }
}

// ---------------------------------------------------------------------------------------------
// the mounter: the one match
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct AppMounter {
    /// **Where the NEXT player instance returns to** — the entry that was on top when the push was
    /// asked for, stamped at the press and consumed by the mount exactly as `player_hud_ms` is.
    pub player_origin: Option<crate::player::Origin>,
    /// How long the NEXT player instance pins its transport for, in ms — `HUD_LINGER_MS` for an
    /// ordinary start and `HUD_HEADLESS_MS` for a capture run. It is a seed rather than a constant
    /// because `start_playback` is what knows which, and because the deadline must be stamped from
    /// the instant the page MOUNTS, not from the keypress before a blocking start.
    pub player_hud_ms: Option<u32>,
}

/// **The one `mount` match** (spec §2.1) — the whole of what "add a screen" costs, in the module
/// that owns the alphabet it matches on.
///
/// **Generic over the host rather than written against `app::bridge::AppHost`**, for the layer
/// rule's sake and for one practical consequence of it: the screens are generic already, the
/// views they need arrive through the `*Like` accessors above, and nothing in this match wants a
/// concrete application type. So the mounter names no `app::` module and the bridge instantiates
/// it for its own host exactly as the dispatcher instantiates everything else.
impl<H> Mounter<H> for AppMounter
where
    H: plx_machine::machine::Host<Arg = AppArg> + PlayerLike + MetadataLike,
{
    fn mount(
        &mut self,
        id: InstanceId,
        arg: &AppArg,
        _ret: &ReturnState<u32, PageMemory>,
        cx: &Cx<'_, H>,
        _fx: &mut Effects<'_, H>,
    ) -> Box<dyn Screen<H>> {
        let entry = match cx.owner {
            plx_machine::machine::InputOwner::Entry(e) => e,
            _ => EntryId(0),
        };
        match arg {
            AppArg::PlayerOverlay(arg) => Box::new(
                crate::player::overlay::PlayerOverlayScreen::new_with(
                    H::session(cx), H::metadata(cx), entry, arg.kind, H::subtitle_search(cx).is_some()),
            ),
            AppArg::Setup => Box::new(crate::setup::SetupScreen::new(entry, id, crate::setup::Mode::Entry)),
            AppArg::Server => Box::new(crate::setup::SetupScreen::new(entry, id, crate::setup::Mode::Server)),
            AppArg::Guide => Box::new(crate::guide::GuideScreen::new(entry, id)),
            // The player is an OWNED screen — the instance that holds the HUD, the held-key timer
            // and the live controls, and that answers `RenderStrategy::VideoPlane`. Its transport
            // is pinned from the instant it mounts (`AppMounter::player_hud_ms`), never from the
            // keypress that asked for the playback.
            AppArg::Player => {
                let mut page = crate::player::PlayerScreen::new(entry);
                // **Where BACK, Stop and EOS land** (§5.1). Captured at the MOUNT, from the page
                // that was on top when the push was asked for — never read live, because an
                // overlay opening on this page's own stack changes the input owner and must not
                // be mistaken for a new origin.
                page.origin = self.player_origin.take();
                page.hud.extend(cx.tick.ms, self.player_hud_ms.take().unwrap_or(crate::player::input::HUD_LINGER_MS));
                page.publish();
                Box::new(page)
            }
        }
    }
}

/// **Every `AppArg` a SURFACE can be presented with, one per variant** — the domain the heartbeat's
/// ` overlay=` alphabet is derived over (`app::overlay_words`, restructure phase 10 item 4).
///
/// Exhaustive by the COMPILER, which is the whole reason it is written this way: the `match` below
/// names every variant of the enum, so adding one and forgetting it here does not compile. A word
/// that silently never joins the table is the failure this guards — `tests/manifest.json` selects
/// fps samples by these strings, and a scene keyed on a word the app cannot print fails on the
/// television as "only 0 post-warmup samples", which is indistinguishable from a real regression.
///
/// The PAGE variants (the seven flat pages and `Content`) are excluded on purpose and by name
/// rather than by a wildcard: a page's word is `route=`, and it is the top page's own
/// `Screen::name`. `Settings` and `FirstRunConsent` appear once each because their payload is
/// a boot ADDRESS rather than an identity (`ScreenArg::id` collapses it the same way), and the
/// family's inner stack is what decides its word at any moment — `RouteSurface::top_word`.
#[cfg(any(test, feature = "test-support"))]
pub fn every_surface_arg() -> Vec<AppArg> {
    use crate::player::overlay::{OverlayKind, PlayerOverlayArg};
    // The player's panels are ONE screen with several kinds, and each answers a different
    // `Screen::name` — so every kind is listed, not one representative.
    let args: Vec<AppArg> = OverlayKind::ALL.map(|kind| AppArg::PlayerOverlay(PlayerOverlayArg { kind })).into();
    for a in &args {
        match a {
            AppArg::PlayerOverlay(_) => {}
            // The PAGE variants. Named rather than swept into a `_`, so the exhaustiveness above
            // is real and a new SURFACE variant cannot land in a catch-all.
            AppArg::Setup | AppArg::Guide | AppArg::Server | AppArg::Player => {
                panic!("a page argument is not a surface: its word is `route=`")
            }
        }
    }
    args
}
// ---------------------------------------------------------------------------------------------
// the shape inventory
// ---------------------------------------------------------------------------------------------

/// **Every SCREEN-side shape the recorder's `state_fp` hashes** (spec §5.4), as one array in the
/// module a new screen is added to.
///
/// It was a hand-written list inside `app/recorder.rs::state_fp` until restructure phase 10, which
/// made `app/recorder.rs` a file every new screen had to touch — and §0's criterion 5 is that a
/// conversion touches its own `screens/<name>.rs`, this module, `dev/scenarios.rs` and
/// `tests/manifest.json` and nothing else. The recorder still owns the shapes that are the LOOP's
/// (the press machine, the input state, the frame line, the container tree, the return state, the
/// PMS fixtures, the app's own init) and folds them in FRONT of these; what belongs to a screen is
/// declared here, beside the argument that mounts it.
///
/// **Order is part of the hash** (`ui::rec::state_fp` writes the sequence), so an entry is
/// APPENDED, or [`SCREEN_SHAPES_PIN`] moves for that reason alone.
pub const SCREEN_SHAPES: &[&str] = &[
    ARG_SHAPE,
    PAGE_MEMORY_SHAPE,
    crate::player::SHAPE,
    crate::player::overlay::SHAPE,
    crate::setup::SHAPE,
    crate::guide::SHAPE,
];

/// The pin over [`SCREEN_SHAPES`] — bump it in the same edit that adds an entry, and say why.
///
/// A committed replay fixture recorded against a different value cannot be LOADED at all, which is
/// the cost this exists to make visible rather than silent; `tools/plxnative-rec rerecord` is the
/// verb (`tests/fixtures/replay/README.md`). The APP-side half is pinned separately in
/// `app/recorder.rs`, over the shapes that are the loop's, so neither pin moves for the other's
/// reason. `#[cfg(test)]` because the pin is an ASSERTION about the array above and never a value
/// the app reads — `state_fp()` hashes [`SCREEN_SHAPES`] itself.
///
/// History, newest last: the Dispatcharr Setup, Guide and Server pages joined; the live player
/// controls and the Guide's favourites, resume-on-launch, network discovery and the group switch
/// each grew a shape (0x72fb_3258_bff6_7e57 was the last pin with the Plex client's pages). Then
/// the Plex pages and panels and the Settings family left the alphabet, and `PageMemory` lost
/// every page's arm.
#[cfg(test)]
const SCREEN_SHAPES_PIN: u64 = 0xb076_c643_f4b1_5aac;

#[cfg(test)]
mod arg_tests {
    use super::*;

    /// **The screen half of the recorder's shape pin** (§5.4), asserted here rather than in
    /// `app/recorder.rs` because this is the array a new screen joins — so the bump lands in the
    /// same file, and the same commit, as the entry that caused it.
    #[test]
    fn the_screen_shape_inventory_is_pinned() {
        assert_eq!(plx_ui::rec::state_fp(SCREEN_SHAPES), SCREEN_SHAPES_PIN);
    }

    /// **One `ScreenId` per surface VARIANT.** Ids are allocated forward here precisely so a
    /// retired identity is never handed to the screen that replaced it, and `same_instance` is
    /// derived from `id` — so two different variants sharing one would let the container hold a
    /// mounted instance of the wrong screen and believe it was reusing the right one.
    ///
    /// Two arguments of ONE variant sharing an id is the DELIBERATE case (the four player panels,
    /// the three item-menu entry points, both Settings roots), which is why the assertion is over
    /// `mem::discriminant` rather than over equality of the arguments themselves.
    #[test]
    fn two_different_surface_variants_never_share_a_screen_id() {
        use plx_ui::screen::ScreenArg;
        let args = every_surface_arg();
        for (i, a) in args.iter().enumerate() {
            for b in args.iter().skip(i + 1) {
                if a.id() == b.id() {
                    assert_eq!(
                        std::mem::discriminant(a),
                        std::mem::discriminant(b),
                        "two different surfaces share ScreenId({})",
                        a.id().0
                    );
                }
            }
        }
    }
}
