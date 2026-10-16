//! The IMPURE half of the route split (spec §9, §15.2): session state, the synchronized
//! [`PlayerControl`], PMS/native I/O, and the encoder/scrobble/timeline machinery — everything
//! [`super::plan`] is not. `PlaybackSession` is the main-thread projection used to build URLs/payloads;
//! [`PLAYER_CONTROL`] is the synchronized authority for route ownership and route-changing
//! intents. The player engine reads the URL/session through the accessors here; appkit::player_hud
//! reads the HUD strings through title_cptr()/ctxline_cptr(). This file is exempt from the
//! `wall` gate that `plan.rs` must pass — a network/adapter effect is allowed to read wall time —
//! but as of this split it still contains none: the one wall-clock field this module owned
//! (`PlaybackSession::auto_last_switch`) is now a frame-tick millisecond stamp, not an `Instant`.
//!
//! The claim worker's plumbing (the work/fallback enums, the landing mailbox, the spawn, the drain
//! and the stale-landing discard rules) is [`super::flight`]; the route DECISIONS that feed it
//! (the claim table, the enhancement step, the plans and the PMS attempts) stay here.

use plx_plex::plex::ServerId;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicU32, Ordering};
use super::plan::*;
use super::prefs::*;

// ---- ONE playback session, as ONE value -----------------------------------------------------

/// Everything needed to resolve the item again after a terminal playback failure.
///
/// A failed `/decision` has no Engine and an HTTP-open failure has an Engine whose URL is already
/// terminal, so neither can be recovered by poking the live route.  The user-facing quality
/// picker starts a NEW resolve instead.  Keep the original request here, before the worker runs:
/// even a plan that returns no URL must remain retryable, and a numeric Part id alone cannot
/// reconstruct the source key PMS expects.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaybackRequest {
    sid: ServerId,
    rk: String,
    part: String,
    vcodec: String,
    acodec: String,
    title: String,
    ctx: String,
}

/// Everything the main thread needs to resolve and render the playback in progress, in one struct.
///
/// **Owned by [`crate::player::machine::Player`], a field of `App`, and reached ONLY as a
/// parameter** (restructure spec §2.2, phase 9). Every field below was its own `static mut`; phase
/// 9 collected them into `static mut SESSION` and then deleted that too, so the value now has one
/// owner and the borrow checker — rather than a `MAIN THREAD ONLY` comment — is what says a second
/// writer cannot exist. The SHAPE was the original hazard rather than any one field:
/// [`apply_plan`] installed all of them but the two HUD buffers, [`request_play`] owned those
/// two and the five the outgoing item leaves behind, and a dozen small functions each poked one or
/// two more on the side — so "what a session IS" was written down nowhere and no writer could be
/// read against the whole. The failure that shape produced is still documented at the line that
/// fixed it, in [`build_stream`] — the part id was published by the CALLER after the resolve
/// returned, so `put_selection`, which runs INSIDE the resolve, addressed the PREVIOUS item's part,
/// and the server-default subtitle that PUT exists to suppress was burned into the transcode
/// instead.
///
/// Route ownership and worker/main route transitions are deliberately not fields here: the
/// synchronized [`PLAYER_CONTROL`] is their authority. [`ResolveEnv`] exists for the other
/// direction: the resolve worker is handed owned copies and reads none of this. The accessors that
/// lend rather than copy — [`play_verdict`], [`up_next`] and [`with_queue`], plus the raw pointers
/// [`title_cptr`]/[`ctxline_cptr`] hand to `draw_text` — now borrow from the caller's `&`, so the
/// "valid until the next main-thread write" caveat those docs carried is enforced rather than
/// asserted: a frame's draw cannot hold one across [`apply_plan`] or [`request_play`], because
/// those take `&mut`.

pub struct PlaybackSession {
    /// Frozen for one logical playback, including its retries and track changes.
    direct_play_mode: DirectPlayMode,
    /// This playback was refused by the cached device sandbox preflight. No Engine exists.
    /// Cleared with the playback verdict on exit/reset, never a process-global error latch.
    pub jail_load_blocked: bool,
    /// The last FRESH native start was refused because a timed-out Load is still inside
    /// `sf_load` (an `AbandonedLoad` owns the one native object). No Engine exists, so nothing
    /// else would publish the failure and the player sat Idle with no read-out. `player::state`
    /// derives `Error` (cause `load_timeout`) from it. Cleared by the next start attempt and,
    /// like [`jail_load_blocked`](Self::jail_load_blocked), with the playback verdict.
    pub native_start_refused: bool,
    /// Read-only publication of Player.repair for the HUD. Never authorizes a resource effect.
    pub repair_status: plx_platform::tv::sandbox::State,
    /// The request which produced this attempt, retained for terminal Retry / Choose quality.
    /// Written synchronously by [`request_play`] rather than by [`apply_plan`], because the
    /// server can refuse before a playable plan exists.
    request: Option<PlaybackRequest>,
    /// Resume target which has not yet been proven by a presented frame.  It survives a refused
    /// retry plan so the next quality choice does not restart a two-hour film at zero; cleared by
    /// [`confirm_resume_presented`] once the replacement actually shows a frame.
    requested_resume_ns: i64,
    /// The stream URL for this playback (empty = nothing resolved, or torn down).
    url: String,
    /// The URL is a LIVE stream (a Dispatcharr channel): endless, unseekable, and timestamped
    /// wherever the broadcaster's clock happens to be, so the engine rebases its first keyframe
    /// to 0 exactly as it does a resume. Set with [`set_live_url`]; cleared by [`set_url`],
    /// [`clear_url`] and a new [`request_play`].
    live: bool,
    /// The channel a live tune is watching, in the lineup it came from ([`request_live`]).
    /// Cleared with the URL, like [`live`](Self::live).
    tune: Option<super::LiveTune>,
    /// The server-side transcode session id, EMPTY on a direct play — that emptiness is the
    /// "is this a transcode?" test ([`is_transcoding`]) and the key the stop is sent with.
    tsession: String,
    /// The server's PRE-FLIGHT refusal for the last resolve, or None.
    ///
    /// `Some(sentence)` means `/decision` answered "neither direct play nor conversion is
    /// available" (`generalDecisionCode` 2000) BEFORE a byte of video moved, so this playback never
    /// got a URL — see [`refusal`]. The String is the server's OWN sentence, carried so the
    /// player's read-out can quote it verbatim; it is `""` when the server named a code but no
    /// reason, which is why the refusal itself lives in the `Option` and not in the emptiness of
    /// the text.
    ///
    /// [`apply_plan`] installs it and [`request_play`] retires it, so it always describes the item
    /// the player is showing.
    play_verdict: Option<PlayVerdict>,
    /// The resolve itself could not produce a plan (no client, worker panic, or worker spawn
    /// refusal).  Unlike `play_verdict`, this is OUR failure rather than a PMS sentence; it makes
    /// an empty plan terminal and retryable instead of falling back to an idle black frame.
    resolve_failed: bool,
    /// This playback's encode shape — flavor, delivery, the no-copy switch, the fixed quality
    /// ceiling and (issue #266) the Plex Pass audio DSP. A seek or retranscode rebuilds the
    /// identical start.mkv query from (`cur_rk`, `sess`, the `cur_*_sid` pair, this contract) via
    /// `plex::TranscodeSpec` — replaces the old stored offset-free TBASE query string.
    ///
    /// Was four separate fields (`cur_remux`, `cur_delivery`, `cur_no_video_copy`, `cur_ceiling`),
    /// each carried for the same reason: a seek ([`plan_rebase`]) and an audio switch
    /// ([`retranscode`]) rebuild the start.mkv query from scratch, and a rebuild that read the
    /// LIVE selection instead of this stored shape would change the encode's resolution mid-film,
    /// hand the server back a copy permission mid-playback, or drop the enhancement DSP on the
    /// next seek. Folding the four into one `EncodeContract` is what
    /// keeps that coupling a single write instead of four fields that could drift independently.
    ///
    /// **[`set_quality`] is the ONE writer that may move `ceiling` mid-film**, and that is the
    /// whole distinction: an explicit pick is new information about what the link can carry, while
    /// a seek is not, so a seek rebuilds from what is stored here and a pick replaces it (and asks
    /// the pump for a fresh transcode when the answer actually changed). Nothing measures a link
    /// or moves a rung on its own — the adaptive switch is not here.
    cur_contract: plx_plex::plex::EncodeContract,
    /// What the last successful transcode decision actually did with the Plex Pass audio DSP
    /// (issue #266) — distinct from `cur_contract.audio`, which is what the accepted decision
    /// carried. Installed by `apply_plan` from `Plan::enhancement`; reset to `Off` by a new
    /// request. See `route::decision::EnhancementOutcome`.
    cur_enhancement: EnhancementOutcome,
    /// What the resolve measured the playing source at — `(kbps, w, h)`, `0` where nobody said.
    /// The input [`set_quality`] re-runs [`quality_policy`] on when a rung is picked mid-film, so
    /// that decision is made from the same numbers `build_stream` used rather than from a guess.
    cur_src: (i64, i64, i64),
    /// Whole Part transport bitrate, including audio. Auto's progressive watchdog compares the
    /// live socket against this rather than `cur_src.0` (video only), because the wire has to carry
    /// both lanes. `0` means PMS did not provide one and disables the watchdog fail-safely.
    cur_transport_kbps: i64,
    /// **Can this television decode the SOURCE video stream as it stands** — `video_direct_plays`
    /// evaluated by [`build_stream`], carried rather than re-derived.
    ///
    /// It answers the one question the word "Original" is a claim about: false means the server
    /// MUST re-encode the pixels, whatever rung is picked and whatever the link does, so the
    /// quality menu's Original row cannot deliver the original. MPEG-2 always, AV1 / VP9 unless
    /// the device table lists that decoder (`devcaps::Caps::av1` / `vp9`) are the cases; see the
    /// `!video_dp` arm's own comment.
    ///
    /// **Carried, because re-deriving it at draw time would be a second copy of the gate.** The
    /// menu is drawn from a different thread of control and a different set of facts than the
    /// resolve — `metadata::playing()` is `None` for the whole 0.5-3 s resolve window — and a
    /// second evaluation could disagree with the routing decision it is describing. This is the
    /// same argument [`PlaybackSession::cur_remux`] carries for the neighbouring question, and the reason
    /// `playback_preview_of` exists rather than a duplicate of the gate on the detail page.
    ///
    /// **`true` when nothing has resolved yet**, so an absent fact annotates nothing. A menu is
    /// only reachable inside a live player session, so the window is small; and the failure
    /// direction matters — claiming "the source cannot be preserved" about a source nobody has
    /// looked at is a worse read-out than saying nothing.
    cur_source_decodable: bool,
    /// **Auto chose Original for this playback, so the progressive transfer watchdog runs.**
    /// The only reader is [`auto_original_watch`], so this field's whole meaning is that question.
    ///
    /// It used to be `cur_auto_remote_original` and to carry `Location::Remote`, on the argument
    /// that a Local link "needs no throughput proof". That is true of the PRE-FLIGHT question —
    /// whether to spend a probe before choosing Original — and false of the runtime one:
    /// `Location` is decided from the address shape (`plex::probe::configured_tier`) and describes
    /// TOPOLOGY, which does not imply throughput. Wi-Fi, powerline, a busy switch or a second
    /// stream in the house all produce a LAN that cannot carry a 10 Mbps source.
    ///
    /// Measured 2026-08-27 (`docs/measurements/local-original-blind.md`): a 10 634 kbps
    /// direct-play source on the local PMS with the link held at 2 500 kbps ran at **8–25 % of
    /// real time for the rest of the playback**, with ZERO `abr:` lines in the whole log. The two
    /// `recover_auto_to_original` writers never carried the conjunct, so an Original REACHED from
    /// HLS was supervised on any link while the same state chosen at play time on a LAN was not —
    /// which is what makes it an oversight rather than a design.
    cur_auto_original_watched: bool,
    /// The zero-encode flavor Auto may return to after a remote link recovers. Kept even while
    /// HLS is active: the HLS worker owns only measurements, while the main thread owns the
    /// codec/session transition back to this exact source declaration.
    auto_original: Option<AutoOriginalCandidate>,
    /// Debug pipeline-tier substitute for PMS's fixed-rendition endpoints. Empty in every
    /// production plan; see [`arm_auto_fixture`].
    auto_fixture_base: String,
    /// **Visible mode switches this playback has already shown the viewer**, and when the last one
    /// was. Lives here, on the main thread, because it has to OUTLIVE the demux workers: each
    /// Original↔HLS transition replaces the engine, so a counter held by a worker would reset to
    /// zero on exactly the event it exists to count, and flapping would be invisible to the very
    /// controller meant to prevent it. Captured into each worker at spawn
    /// ([`crate::abr::TransitionHistory`]) and advanced there by the worker's own elapsed time.
    auto_switches: u32,
    /// Milliseconds, in the frame tick's units ([`PlaybackSession::now_ms`], a frame-tick-shaped
    /// stamp rather than an `Instant`; see [`note_visible_switch`]/[`auto_history`]). `None`
    /// before the first switch.
    auto_last_switch: Option<u32>,
    /// The startup probe's measurement, kept so a mode transition can hand the next worker a
    /// starting estimate instead of an empty one. Explicitly a weak prior, never a measurement of
    /// the request it is handed to — see [`crate::abr::CapacityEstimate::demote_to_prior`].
    auto_prior_kbps: u32,
    /// The HLS contingency [`crate::abr::bootstrap`] selected while it still owned the evidence.
    /// Usually installed immediately; retained while Auto tries Original so an HTTP-open refusal
    /// can take the same branch without calling the refusal a zero-rate sample or mistaking source
    /// demand for link capacity.
    auto_bootstrap_rung: Option<crate::abr::Rung>,
    /// ratingKey of the currently-playing item (movie or episode), so an audio-track
    /// switch can force a fresh transcode of the same item.
    cur_rk: String,
    /// The SERVER the currently-playing item came from — the other half of its identity.
    ///
    /// A ratingKey names an item only within one server: `1` is a real item on our own server and a
    /// different real item on a friend's share, and the same goes for `Part.id`, `Stream.id`,
    /// `playQueueID` and the resume point. Every PMS call in this file used to resolve its server
    /// implicitly, at the instant of the call, through `client_opt()` — i.e. whichever server
    /// happened to be CURRENT right then. Merged Home shelves make "the item is from B while
    /// current is A" ordinary, and every one of those calls would then land on A: the PlayQueue,
    /// the track PUT, the transcode stop, and — ten seconds at a time, forever — the progress
    /// report that writes the resume point.
    ///
    /// So the server is captured ONCE, at [`request_play`], and carried by value from there:
    /// `ResolveEnv` → `Plan` → here. Nothing below this line re-resolves it. `UNSET` before the
    /// first play and on a plan that never resolved, which resolves to no client at all rather than
    /// to slot 0.
    cur_sid: ServerId,
    /// current audio track carried by any TRANSCODE of the current item, and — on the Original
    /// family — the direct-played/remuxed track's own facts (codec, channels, loudness capability,
    /// immersive), when known. `None` means "server default, facts unknown" and every reader fails
    /// CLOSED on it (issue #266's `CarriedAudio` — `route::plan`). `cur_audio_sid()` below is the
    /// old accessor kept as a projection: `.map_or(0, |a| a.sid)`, so every existing caller that
    /// only ever wanted the wire id keeps compiling unchanged.
    cur_audio: Option<CarriedAudio>,
    /// current subtitle selection (0 = none) — the picked Plex stream id, regardless of how it
    /// renders: burned into any TRANSCODE (our client profile advertises no soft-sub support, so
    /// Plex's decision is burn), or client-rendered from the demuxer on direct play
    /// (`player::request_subtitle`).
    cur_sub_sid: i64,
    /// Do [`Self::cur_audio`] and [`Self::cur_sub_sid`] name a selection that is KNOWN rather than
    /// the cleared `0` a play request leaves (`0` is both "Off" and "nothing landed yet")? True
    /// only after a PLAYABLE plan landed ([`apply_plan`]; a refused plan installs too, with a
    /// selection nobody made) or after the viewer picked a track ([`commit_audio_selection`],
    /// [`commit_subtitle_selection`]); cleared with the selection by [`clear_track_selection`].
    /// [`RetryContext::selection`] is `Some` exactly when this held at capture time.
    selection_known: bool,
    /// The `(audio_sid, sub_sid)` a RETRY carried into the request now loading, kept while no
    /// playable plan has landed: the request clears the live selection ([`Self::selection_known`]
    /// goes false), so a second retry built before this one lands (a contract bump in flight, or
    /// *Switch to Auto and play* after a refusal) would otherwise find nothing to carry and lose
    /// the viewer's choice, an explicit subtitles Off included. Set by [`begin_track_selection`],
    /// kept across a refused landing, dropped by a playable one ([`apply_plan`]).
    /// [`current_retry_context`] reads it only while the selection is unknown.
    carried_selection: Option<(i64, i64)>,
    /// Is [`Self::cur_sub_sid`] an external sidecar (`metadata::Stream::sidecar_renderable`) rather
    /// than an embedded (in-container) track? Meaningless when `cur_sub_sid == 0`. Issue #266 I6:
    /// this is what tells `route::decision::facts`'s [`super::plan::SubtitleEffect::Sidecar`]
    /// (unaffected by the audio enhancement's remux) apart from
    /// [`super::plan::SubtitleEffect::Embedded`] (needs a forced burn to survive one, M7).
    cur_sub_sidecar: bool,
    /// Can the app itself draw [`Self::cur_sub_sid`] (an embedded ordinal the demuxer exposes, or a
    /// sidecar)? Set from `client_renderable` when the pick is committed and from the plan's
    /// `sub_render_ordinal` at a cold start. With the route shape it decides whether a remux may
    /// leave the subtitle to the app instead of having the server burn it ([`side_subs_allowed`]).
    /// Meaningless when `cur_sub_sid == 0`.
    cur_sub_client_drawable: bool,
    /// The 0-based position of [`Self::cur_sub_sid`] among the Part's embedded subtitle streams
    /// (`metadata::sub_render_ordinal`'s own value, the one direct play renders by); negative for a
    /// sidecar and while nothing is selected. What the side reader is told to decode.
    cur_sub_ordinal: i32,
    /// The app's own drawing of a subtitle over a remux was refused or failed on this playback,
    /// so the server burn is the only route left for it. Session-scoped: cleared with the item.
    side_subs_refused: bool,
    /// The subtitle-language preference this play resolved under — the show's pref if it set one,
    /// else the account's — carried straight from [`super::plan::Plan::sub_pref_lang`] so the
    /// Subtitles menu's "yours" grouping (`metadata::sub_layout::sub_sections`) survives a reload.
    cur_sub_pref_lang: Option<String>,
    /// the playing item's Part id (from the part key), so an audio switch can PUT the
    /// server-side stream selection — the transcoder encodes the part's SELECTED audio.
    cur_part_id: i64,
    /// Opaque internal playback generation, regenerated on each play_movie/play_episode. It is
    /// also the first encoder's PMS session id. Adaptive replacements keep this app generation
    /// stable but use their own coupled PMS wire id; the active encoder mutex supplies timeline,
    /// seek and teardown with the currently published wire identity.
    sess: String,
    /// GET /identity machineIdentifier, cached — needed for the PlayQueue uri.
    ///
    /// It is cached PER SERVER, which is what `machine_sid` is for: the id goes into
    /// `uri=server://{machineIdentifier}/…` on the PlayQueue POST, so one cached globally is a
    /// mis-addressed queue the moment a second server exists — server A's fingerprint POSTed to B,
    /// naming a server B has never heard of. Only a cache learned from THIS playback's server is
    /// usable, and `resolve_playqueue` prefers the registry's own per-server id over both.
    ///
    /// It is also one of the two CONDITIONAL writes in [`apply_plan`] (the codec quartet below is
    /// the other): the pair is left alone on a plan that fetched no id (`machine_id == ""`), so a
    /// cache learned by an earlier playback survives into the next one and spares it a `/identity`
    /// round trip.
    machine_id: String,
    machine_sid: ServerId,
    /// This playback's PlayQueue ids for the timeline (empty if /playQueues failed).
    pq_id: String,
    pq_item_id: String,
    /// The item's OWN codecs, as the file has them — captured once per playback and never
    /// overwritten by `apply_decision_codecs`, which replaces `stream_*` with the transcode OUTPUT.
    ///
    /// Two different questions, and the diagnostics read-out needs both: "what is this file" and
    /// "what is the server actually sending". With only the second recorded, a transcode reported
    /// its output as though it were the source and the whole server-side transform was invisible.
    src_vcodec: String,
    src_acodec: String,
    /// The streamed item's Media video/audio codec (h264/hevc, ac3/eac3/aac), so the player picks
    /// the H265 Load payload for a native HEVC direct-play and the matching audio codec.
    stream_vcodec: String,
    stream_acodec: String,
    /// Direct-play source video frame rate (0 = unknown/transcode → omit from the Load esInfo).
    stream_fps: f64,
    /// The direct-played file's raw Dolby Vision layering, retained for diagnostics. The Load
    /// payload consumes `stream_dv_decision` below: re-evaluating this record after a late probe
    /// would make a reload describe a different route from the one which passed the gate.
    ///
    /// `Dovi::NONE` on every transcode and remux: what arrives then is the server's output, and
    /// the only DV file that reaches those paths is one we refused to declare in the first place.
    stream_dovi: plx_data::metadata::Dovi,
    /// Capability and presentation frozen when `stream_dovi` entered this physical route. An
    /// audio switch, reload, Original recovery and rollback all copy this beside the raw record.
    stream_dv_decision: plx_data::metadata::DvDecision,
    /// **Does the audio elementary stream we are feeding carry Dolby Atmos?** The Load payload's
    /// `contents.immersive` node turns on it ([`crate::player::engine`]).
    ///
    /// Rides the session for exactly the reason `stream_dovi` does, and the failure it prevents is
    /// the audible twin of that one: an audio-track switch tears the engine down and rebuilds the
    /// payload from here, so a value that lived only in the plan would silently stop declaring
    /// Atmos the moment the user opened the track menu — on the very track they had just chosen
    /// *because* it is the Atmos one.
    ///
    /// `false` on every transcode and remux; see where it is set for why that is deliberate rather
    /// than an omission.
    stream_immersive: bool,
    /// HUD strings as fixed NUL-terminated C buffers, so title_cptr()/ctxline_cptr() hand
    /// draw_text (extern "C", *const c_char) a pointer that stays valid for the whole frame.
    ///
    /// The pair a landing does NOT install: [`request_play`] writes them synchronously, at the
    /// press, so the HUD has a title for the whole resolve — which is why [`apply_plan`]'s single
    /// assignment carries them across rather than overwriting them.
    title: [c_char; 128],
    ctxline: [c_char; 96],
    /// The next episode of the item now playing, or None (a movie, the last episode, or a queue
    /// that failed). Installed by [`apply_plan`]; [`request_play`] retires it the moment a new item
    /// resolves. Read through [`up_next`], which lends a `&'static`.
    ///
    /// Shared, not owned: [`publication`](Self::publication) copies the session every frame, and
    /// the Up Next still is a card whose placement history is keyed by the address of this row's
    /// `thumb` (`plx_ui::widgets::Art::motion_identity`). A deep copy gave the card a new identity
    /// every frame, so a still that was not cached yet was declined for ever and the player
    /// screen never came to rest.
    up_next: Option<std::sync::Arc<UpNext>>,
    /// The whole queue behind the item now playing — the playing row included, in queue order,
    /// projected to `plex::QueueRow` ON THE RESOLVE WORKER (a `Metadata` row carries its entire
    /// Media/Part/Stream/Role tree; a show's queue is dozens of them, and this device is 32-bit).
    /// Same lifecycle as `up_next`: installed by `apply_plan`, retired by `request_play`.
    ///
    /// Whatever the server sent is kept, uncapped — a projected row is ~300 bytes on this device,
    /// so even a whole show is tens of KB. The capping that matters is the DRAWING (a still per row
    /// is a GL texture); that belongs to the overlay, and `apply_plan` deliberately warms only
    /// `up_next`'s.
    queue: Vec<plx_plex::plex::QueueRow>,
    /// **This frame's millisecond stamp, mirrored from [`crate::player::machine::Player::now_ms`]**
    /// (spec §4.1), whose `set_now` is its ONLY writer — the loop calls it once per iteration from
    /// the same `fr.now` every other phase of the frame reads.
    ///
    /// It lives here rather than travelling as a parameter because the session travels alone
    /// through the whole route layer and the tick has to travel with it: the two readers below
    /// ([`note_visible_switch`] and [`auto_history`]) sit five and seven calls deep inside
    /// `start_bufferfeed` and `pump`, and a `now_ms` argument threaded to exactly those two would
    /// have to be carried by a dozen functions that have no other use for it — which is how the
    /// second clock read this replaces got there in the first place.
    now_ms: u32,
}

/// What the server actually did with a requested Plex Pass audio enhancement (issue #266) — as
/// opposed to `EncodeContract::audio`, which is what was ASKED for. Distinguishing "on and
/// working" from "the server ignored it" matters because PMS 1.43.4 does both: measurement M2
/// (`/tmp/plx266/measurements.md`) found an AC3 2.0 source where the params changed the decision
/// (a real DSP transcode) beside an AAC 5.1 source where the audio was transcoded to AC3 either
/// way, params or not — so "the decision shows a transcode" alone cannot tell the two apart.
///
/// Graded by `build_stream` (`classify_outcome`, or `Refused` from its fallback) and installed by
/// `apply_plan` beside `cur_contract` — written only after a decision, never at the selection (I9).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) enum EnhancementOutcome {
    #[default]
    Off,
    /// Remux, the audio decision was a transcode, and the source codec is in the profile's own
    /// copy list — the params demonstrably did something.
    Applied,
    /// The server refused the params outright, or silently ignored them (audio came back `copy`
    /// despite the ask) — an old or non-conforming PMS.
    Refused,
}

impl PlaybackSession {
    /// Nothing playing: what the module holds before the first play, and the value the static is
    /// born as. Every String empty, every id 0 or `UNSET`, both HUD buffers NUL.
    pub const IDLE: PlaybackSession = PlaybackSession {
        direct_play_mode: DirectPlayMode::Auto,
        jail_load_blocked: false,
        native_start_refused: false,
        repair_status: plx_platform::tv::sandbox::State::Idle,
        request: None,
        requested_resume_ns: 0,
        url: String::new(),
        live: false,
        tune: None,
        tsession: String::new(),
        play_verdict: None,
        resolve_failed: false,
        cur_contract: plx_plex::plex::EncodeContract::original(false, plx_plex::plex::AudioEnhancements::NONE),
        cur_enhancement: EnhancementOutcome::Off,
        cur_src: (0, 0, 0),
        cur_transport_kbps: 0,
        cur_source_decodable: true,
        cur_auto_original_watched: false,
        auto_original: None,
        auto_fixture_base: String::new(),
        auto_switches: 0,
        auto_last_switch: None,
        auto_prior_kbps: 0,
        auto_bootstrap_rung: None,
        cur_rk: String::new(),
        cur_sid: ServerId::UNSET,
        cur_audio: None,
        cur_sub_sid: 0,
        selection_known: false,
        carried_selection: None,
        cur_sub_sidecar: false,
        cur_sub_client_drawable: false,
        cur_sub_ordinal: -1,
        side_subs_refused: false,
        cur_sub_pref_lang: None,
        cur_part_id: 0,
        sess: String::new(),
        machine_id: String::new(),
        machine_sid: ServerId::UNSET,
        pq_id: String::new(),
        pq_item_id: String::new(),
        src_vcodec: String::new(),
        src_acodec: String::new(),
        stream_vcodec: String::new(),
        stream_acodec: String::new(),
        stream_fps: 0.0,
        stream_dovi: plx_data::metadata::Dovi::NONE,
        stream_dv_decision: plx_data::metadata::DvDecision::NONE,
        stream_immersive: false,
        title: [0; 128],
        ctxline: [0; 96],
        up_next: None,
        queue: Vec::new(),
        now_ms: 0,
    };
}

impl PlaybackSession {
    /// **The per-frame PUBLICATION a screen is shown** (spec §2.3).
    ///
    /// The container tree hands a screen its application state through `Cx.views`, and `AppViews`
    /// is built by `Bridge::split` out of the RIG — so a screen can only ever be shown state the
    /// rig owns, and the session is owned by `App.player`. Rather than widen the library's
    /// `Rig::split` seam (or hand a screen the `&mut` that would let it write the machine's state
    /// from inside a draw), the loop publishes this copy into the rig once per frame, exactly as
    /// `capture_views` publishes every store's snapshot beside it.
    ///
    /// **Everything except [`queue`](Self::queue).** The PlayQueue rows are the only unbounded
    /// field and nothing outside this module reads them ([`with_queue`] has no caller in `ui/`,
    /// `screens/` or `app/`), so they are the one thing a per-frame copy must not carry. The
    /// destructuring is deliberate and load-bearing: adding a field to [`PlaybackSession`] without
    /// deciding whether a screen may see it FAILS THE BUILD here rather than silently publishing
    /// it or silently dropping it.
    pub fn publication(&self) -> PlaybackSession {
        let PlaybackSession {
            direct_play_mode,
            jail_load_blocked,
            native_start_refused,
            repair_status,
            request,
            requested_resume_ns,
            url,
            live,
            tune,
            tsession,
            play_verdict,
            resolve_failed,
            cur_contract,
            cur_enhancement,
            cur_src,
            cur_transport_kbps,
            cur_source_decodable,
            cur_auto_original_watched,
            auto_original,
            auto_fixture_base,
            auto_switches,
            auto_last_switch,
            auto_prior_kbps,
            auto_bootstrap_rung,
            cur_rk,
            cur_sid,
            cur_audio,
            cur_sub_sid,
            selection_known,
            carried_selection,
            cur_sub_sidecar,
            cur_sub_client_drawable,
            cur_sub_ordinal,
            side_subs_refused,
            cur_sub_pref_lang,
            cur_part_id,
            sess,
            machine_id,
            machine_sid,
            pq_id,
            pq_item_id,
            src_vcodec,
            src_acodec,
            stream_vcodec,
            stream_acodec,
            stream_fps,
            stream_dovi,
            stream_dv_decision,
            stream_immersive,
            title,
            ctxline,
            up_next,
            now_ms,
            queue: _,
        } = self;
        PlaybackSession {
            direct_play_mode: *direct_play_mode,
            jail_load_blocked: *jail_load_blocked,
            native_start_refused: *native_start_refused,
            repair_status: *repair_status,
            request: request.clone(),
            requested_resume_ns: *requested_resume_ns,
            url: url.clone(),
            live: *live,
            tune: tune.clone(),
            tsession: tsession.clone(),
            play_verdict: play_verdict.clone(),
            resolve_failed: *resolve_failed,
            cur_contract: *cur_contract,
            cur_enhancement: *cur_enhancement,
            cur_src: *cur_src,
            cur_transport_kbps: *cur_transport_kbps,
            cur_source_decodable: *cur_source_decodable,
            cur_auto_original_watched: *cur_auto_original_watched,
            auto_original: auto_original.clone(),
            auto_fixture_base: auto_fixture_base.clone(),
            auto_switches: *auto_switches,
            auto_last_switch: *auto_last_switch,
            auto_prior_kbps: *auto_prior_kbps,
            auto_bootstrap_rung: *auto_bootstrap_rung,
            cur_rk: cur_rk.clone(),
            cur_sid: *cur_sid,
            cur_audio: cur_audio.clone(),
            cur_sub_sid: *cur_sub_sid,
            selection_known: *selection_known,
            carried_selection: *carried_selection,
            cur_sub_sidecar: *cur_sub_sidecar,
            cur_sub_client_drawable: *cur_sub_client_drawable,
            cur_sub_ordinal: *cur_sub_ordinal,
            side_subs_refused: *side_subs_refused,
            cur_sub_pref_lang: cur_sub_pref_lang.clone(),
            cur_part_id: *cur_part_id,
            sess: sess.clone(),
            machine_id: machine_id.clone(),
            machine_sid: *machine_sid,
            pq_id: pq_id.clone(),
            pq_item_id: pq_item_id.clone(),
            src_vcodec: src_vcodec.clone(),
            src_acodec: src_acodec.clone(),
            stream_vcodec: stream_vcodec.clone(),
            stream_acodec: stream_acodec.clone(),
            stream_fps: *stream_fps,
            stream_dovi: *stream_dovi,
            stream_dv_decision: *stream_dv_decision,
            stream_immersive: *stream_immersive,
            title: *title,
            ctxline: *ctxline,
            up_next: up_next.clone(),
            now_ms: *now_ms,
            queue: Vec::new(),
        }
    }
}

impl Default for PlaybackSession {
    fn default() -> Self {
        Self::IDLE
    }
}

impl PlaybackSession {
    /// **The frame tick, written once per iteration by [`crate::player::machine::Player::set_now`]
    /// and by nothing else** (spec §4.1). See the [`now_ms`](Self::now_ms) field.
    pub fn set_now(&mut self, now_ms: u32) {
        self.now_ms = now_ms;
    }
}

/// **A borrowable idle session**, for a test that builds a `Cx` and has no playback in it.
///
/// `AppViews::session` is a borrow, and `&PlaybackSession::IDLE` is a temporary that dies at the
/// end of the statement — this is the one long-lived idle value those fixtures share.
#[cfg(any(test, feature = "test-support"))]
pub fn idle_session_for_test() -> &'static PlaybackSession {
    static IDLE: std::sync::OnceLock<PlaybackSession> = std::sync::OnceLock::new();
    IDLE.get_or_init(|| PlaybackSession::IDLE)
}

// ---- accessors: the player reads the URL/session; the HUD reads the title/ctxline ----
// Their signatures and meanings are the module's whole public surface — `app.rs`, `player/` and
// `ui/` call them heavily — so collecting the state behind them changed the BODIES only.
pub fn url(ps: &PlaybackSession) -> String {
    ps.url.clone()
}
/// Is there a stream URL at all? The in-place twin of [`url`], for the callers that only want the
/// emptiness — [`is_transcoding`]'s idiom, and for the same reason: a universal-transcode
/// `start.mkv` URL is several hundred bytes, and the player route is exempt from the idle present
/// gate, so a `!url().is_empty()` in a draw is a heap allocation and a memcpy at ~60/s.
pub fn has_url(ps: &PlaybackSession) -> bool {
    !ps.url.is_empty()
}
/// Whole-file transport requirement captured by the playback resolve.
pub fn transport_kbps(ps: &PlaybackSession) -> i64 {
    ps.cur_transport_kbps
}
/// Install a file's URL (not live; see [`set_live_url`]).
pub fn set_url(ps: &mut PlaybackSession, s: &str) {
    ps.url = s.to_owned();
    ps.live = false;
    ps.tune = None;
}
pub fn clear_url(ps: &mut PlaybackSession) {
    ps.url.clear();
    ps.live = false;
    ps.tune = None;
}
/// Install a LIVE stream's URL (see [`PlaybackSession::live`]).
pub fn set_live_url(ps: &mut PlaybackSession, s: &str) {
    ps.url = s.to_owned();
    ps.live = true;
    ps.tune = None;
}
/// The live channel being watched and the lineup it is in, when the play came from the Guide.
pub fn live_tune(ps: &PlaybackSession) -> Option<&super::LiveTune> {
    ps.tune.as_ref().filter(|_| is_live(ps))
}
/// Is the stream being played live? Read by the engine when it starts a Load.
pub fn is_live(ps: &PlaybackSession) -> bool {
    ps.live && !ps.url.is_empty()
}

#[cfg(test)]
mod live_url_tests {
    use super::*;

    #[test]
    fn live_lasts_as_long_as_its_url() {
        let mut ps = PlaybackSession::IDLE;
        assert!(!is_live(&ps));
        set_live_url(&mut ps, "http://192.0.2.1:9191/proxy/ts/stream/x");
        assert!(is_live(&ps));
        assert!(ps.publication().live, "the published copy says so too");
        clear_url(&mut ps);
        assert!(!is_live(&ps), "teardown ends it");
        set_live_url(&mut ps, "http://192.0.2.1:9191/proxy/ts/stream/x");
        set_url(&mut ps, "http://192.0.2.1:32400/a.mkv");
        assert!(!is_live(&ps), "a file URL replaces it");
    }

    fn tune(urls: &[&str], index: usize) -> super::super::LiveTune {
        let channels = urls.iter().enumerate().map(|(i, u)| super::super::LiveChannel {
            id: i as i64, uuid: format!("u-{i}"), number: (i + 1).to_string(), name: format!("C{i}"), url: (*u).into(), logo: None,
            programmes: vec![super::super::LiveProgramme { start: 0, end: 100, title: format!("P{i}") }],
        }).collect();
        let lineup = super::super::LiveLineup { channels, vcodec: "h264".into(), acodec: "aac".into() };
        super::super::LiveTune { lineup: std::sync::Arc::new(lineup), index, previous: None, at: 0, codecs: None }
    }

}

/// Thread-safe active PMS resource identity. While transcoding it names the coupled physical
/// encoder/Streaming Resource; while direct-playing it names the logical Streaming Resource only.
/// `PlaybackSession::tsession` remains the main-thread playback classification bit, so owning a direct
/// resource does not relabel it as a transcode. Adaptive HLS can replace the server identity from
/// its demux worker without racing that `static mut` state. Teardown atomically takes this value,
/// so a late candidate can never publish itself after the stop owner has retired the playback.
#[derive(Clone)]
struct ActiveHlsRoute {
    url: String,
    rung: crate::abr::Rung,
}

pub(super) struct ActiveEncoderState {
    /// Monotone semantic route generation. The PMS id may deliberately stay unchanged while the
    /// route changes from HLS to direct Original, so the id alone is not an ownership token.
    pub(super) epoch: u64,
    pub(super) id: String,
    hls: Option<ActiveHlsRoute>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserRouteIntent {
    Retranscode,
    NativeAudioReload,
    AdaptiveReload,
    RecoverOriginal(RecoveryCause),
}

/// Why an HLS/remux → Original recovery is being attempted. The cause decides which applied
/// contract may authorise it and whether the Auto watchdog keeps watching the result, so it rides
/// the intent instead of being re-derived from a quality atomic that may have moved since.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryCause {
    /// The Auto worker proved the source fits: only an applied Auto contract may take it.
    Automatic,
    /// The viewer picked Original: the DESIRED quality must still say so at claim time.
    ManualOriginal,
    /// Issue #266: the enhanced remux is no longer wanted (toggle off, subtitle on, a track the
    /// enhancement cannot carry) and the candidate direct-plays. The route stays Original-family,
    /// so either applied Auto or Original may take it, and an Auto playback keeps its watchdog.
    EnhancementReleased,
}

impl RecoveryCause {
    /// Why the open this recovery makes is made, as a failed open reports it: the viewer asked for
    /// it (an Original pick, or the enhancement they turned off) or the Auto worker did.
    pub const fn attempt_origin(self) -> crate::player::report::AttemptOrigin {
        use crate::player::report::AttemptOrigin as O;
        match self {
            Self::Automatic => O::AutoHandoff,
            Self::ManualOriginal | Self::EnhancementReleased => O::QualityChange,
        }
    }

}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutomaticRouteIntent {
    OriginalToHls {
        ticket: WorkerTicket,
        conservative_kbps: u32,
        position_ns: i64,
    },
    HlsToOriginal {
        ticket: WorkerTicket,
        evidence_kbps: u32,
        position_ns: i64,
    },
}

/// Identity of one prepared route transaction.
///
/// PMS/Session preparation and native construction are two different external effects. A
/// transaction may mint multiple never-reused [`RouteStartAttempt`]s; the attempt, not this
/// transaction, prevents a late `Load` result from settling a retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteStartTransaction {
    pub(super) serial: u64,
}

/// Synchronous result of the native half of a prepared route transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteStartResult {
    Started,
    NoRoute,
    StartFailed,
}

/// Commit a contract change which requires no PMS/native route replacement.  This is still a
/// reducer event: otherwise a later rejected action restores the older snapshot and silently
/// undoes the already-visible quality/subtitle choice.
fn commit_in_place_route_projection(ps: &PlaybackSession, quality_contract: bool) {
    let audio_stream_id = ps.cur_audio.as_ref().map_or(0, |a| a.sid);
    let subtitle_stream_id = ps.cur_sub_sid;
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    match control.phase {
        ControlPhase::Stable | ControlPhase::StagingUser(_) | ControlPhase::Completing(_) => {
            if quality_contract {
                control.applied_revision = control.desired_revision;
                control.applied_quality = control.desired_quality;
            }
        }
        _ => return,
    }
    if let Some(timeline) = control.timeline.as_mut() {
        timeline.audio_stream_id = audio_stream_id;
        timeline.subtitle_stream_id = subtitle_stream_id;
    }
}

/// Immutable main-thread projection consumed by the periodic timeline worker. `Session` itself is
/// deliberately absent: it is main-thread-confined, while this owned clone lives under the same
/// mutex as the active encoder and engine epoch. A report therefore observes either the complete
/// old playback projection or the complete new one, never a hybrid assembled across a reload.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TimelineProjection {
    sid: ServerId,
    rating_key: String,
    logical_session: String,
    play_queue_id: String,
    play_queue_item_id: String,
    audio_stream_id: i64,
    subtitle_stream_id: i64,
}

/// One reporter's authority to sample the playback which spawned it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimelineLease {
    engine_epoch: u64,
    /// Every stop announced before this Engine was published must finish its final old-playback
    /// timeline effect before this reporter may send the replacement playback's first update.
    required_stop: u64,
}

#[derive(Clone)]
struct TimelineSnapshot {
    sid: ServerId,
    rating_key: String,
    state: plx_plex::plex::TimelineState,
    time_ms: i64,
    duration_ms: i64,
    session: String,
    play_queue_id: String,
    play_queue_item_id: String,
    audio_stream_id: i64,
    subtitle_stream_id: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlPhase {
    Idle,
    Stable,
    /// A user edit has crossed the desired-contract boundary but has not yet either committed
    /// in-place or entered `pending_user`. Automatic publication is Busy throughout this window.
    StagingUser(u64),
    /// Candidate preparation is in flight while the old Engine/route is still recoverable.
    Preparing(u64),
    /// Candidate preparation committed but no physical Load attempt currently owns it.
    Prepared(u64),
    /// One exact physical Load attempt is in flight: `(transaction, attempt)`.
    Starting(u64, u64),
    /// Native start/frame proof committed; transaction-attached user effects are being reduced
    /// while automatic workers remain fenced. `Stable` is published only after they are queued.
    Completing(u64),
    Failed(u64),
    Stopping,
}

impl ControlPhase {
}

/// The synchronized authority for route ownership and route-changing intents. `Session` remains
/// the main-thread projection used to build URLs/payloads; workers are never allowed to infer
/// ownership from it. PMS/native I/O is deliberately performed after an action is claimed and
/// this mutex is released, then completed through a typed transition below.
pub(super) struct PlayerControl {
    pub(super) active: ActiveEncoderState,
    pub(super) engine_epoch: u64,
    pub(super) media_epoch: u64,
    /// Latest user-visible contract edit. It fences automatic publication through pending/phase,
    /// but is not a worker credential.
    desired_revision: u64,
    /// Quality preference represented by `desired_revision`. The process-wide picker is only the
    /// durable user preference; it cannot also describe bytes PMS has not accepted yet.
    desired_quality: Quality,
    /// Revision represented by the physical route and therefore carried by WorkerTicket.
    pub(super) applied_revision: u64,
    /// Quality policy which owns the physical worker. A refused Fixed/Original request leaves this
    /// unchanged, so an already-accepted Auto handoff is never relabelled as the failed desire.
    applied_quality: Quality,
    next_action: u64,
    pending_user: Option<UserRouteIntent>,
    /// Issue #266: the pending user intent stands in for a track pick's reload (see
    /// [`reconcile_enhancement`]). Lives and dies with `pending_user`: cleared at every site that
    /// clears it, taken with it at claim, so it can never outlive the pick it describes.
    displaced_pick: bool,
    pending_auto: Option<AutomaticRouteIntent>,
    /// Latest requested playhead which has not yet crossed a real media discontinuity.
    pending_seek_ns: Option<i64>,
    phase: ControlPhase,
    /// The serial of the flight a worker is running (`flight.rs`): set when a claim or a seek
    /// dispatches its PMS half, cleared when the landing is drained or discarded. The PHASE stays
    /// the fence (`Applying(serial)` for a claim, `Preparing(serial)` for a seek), so a record a
    /// teardown strands cannot wedge anything; what the record adds is the one fact a phase cannot
    /// say, that a `Preparing` is a flight a worker will land rather than a preparation one frame
    /// runs inline.
    flight: Option<u64>,
    /// The serial of the flight that is a RECOVERY (`FlightOwner::Recovery`): a failed start's
    /// replacement route being prepared while the Engine that failed waits for it. Set by
    /// [`begin_recovery_flight`] beside `flight` and cleared with it ([`PlayerControl::clear_flight`]).
    flight_recovery: Option<u64>,
    /// The serial of the recovery flight that is a COLD RESUME's (`RebaseFor::Resume`): a resolved
    /// transcode's route being rebuilt at the saved position before its first Load, with no Engine
    /// anywhere. Set by [`begin_resume_flight`] beside `flight_recovery`, cleared with it. It is
    /// the one fact the HUD (`player::state` answers `Resolving`) and the app loop's drain
    /// ([`resume_flight_open`]) need that a recovery's own marker does not say.
    flight_resume: Option<u64>,
    /// The serial of the recovery flight that has NO ENGINE anywhere: the cold resume's
    /// ([`begin_resume_flight`]), the foreground restore's resume and the rollback of an Original
    /// trial whose Load failed before an Engine existed ([`begin_engineless_flight`]). No pump
    /// waits on such a flight, so the app loop drains it, the HUD reads `Resolving` through it
    /// ([`engineless_flight_open`]) and an app-switch suspend drops it. Set beside
    /// `flight_recovery`, cleared with it.
    flight_engineless: Option<u64>,
    /// The position, in nanoseconds, a RECOVERY flight rebuilds the route at: the offset the viewer
    /// is "at" while the flight is out. Set beside `flight_recovery` at the dispatch
    /// ([`begin_recovery_flight`]), cleared with it. An Engine-less flight (a cold or foreground
    /// resume, an Engine-less rollback) has seeded NO clock yet (`playpos_ns` is seeded at the
    /// landing), so [`recovery_flight_offset_ns`] is the only place the position survives for a
    /// suspend that snapshots it while the flight is still flying (`player::intended_pos_ns`).
    flight_offset_ns: Option<i64>,
    /// Native Load results cross back from the media thread here. The main thread drains them only
    /// after the Engine is installed, so a fast `sf_load` cannot publish Stable in front of its
    /// own Engine slot. Tokens make late results harmless rather than requiring queue erasure.
    next_start_attempt: u64,
    start_results: Vec<(RouteStartAttempt, RouteStartResult)>,
    last_start_result: Option<(RouteStartAttempt, RouteStartResult)>,
    /// Commands staged while an Original trial owns neither a proven candidate nor a restored
    /// HLS Engine. They belong to the exact rollback transaction and are consumed only after its
    /// matching native start succeeds.
    start_deferred: Option<(u64, DeferredOriginalEffects)>,
    timeline: Option<TimelineProjection>,
}

impl PlayerControl {
    /// Forget the flight a worker was running, of whichever kind.
    fn clear_flight(&mut self) {
        self.flight = None;
        self.flight_recovery = None;
        self.flight_resume = None;
        self.flight_engineless = None;
        self.flight_offset_ns = None;
    }
}

/// The physical stream the demux worker has committed, including the HLS declaration that has to
/// survive a main-thread reload.  `Session` remains main-thread-confined; keeping only the worker-
/// mutable projection here is what lets an ABR commit publish encoder + URL + rung atomically
/// without racing the rest of the route.
static PLAYER_CONTROL: std::sync::Mutex<PlayerControl> = std::sync::Mutex::new(PlayerControl {
    active: ActiveEncoderState {
        epoch: 0,
        id: String::new(),
        hls: None,
    },
    engine_epoch: 1,
    media_epoch: 1,
    desired_revision: 1,
    desired_quality: Quality::Original,
    applied_revision: 1,
    applied_quality: Quality::Original,
    next_action: 0,
    pending_user: None,
    displaced_pick: false,
    pending_auto: None,
    pending_seek_ns: None,
    phase: ControlPhase::Stable,
    flight: None,
    flight_recovery: None,
    flight_resume: None,
    flight_engineless: None,
    flight_offset_ns: None,
    next_start_attempt: 0,
    start_results: Vec::new(),
    last_start_result: None,
    start_deferred: None,
    timeline: None,
});

fn advance_route(active: &mut ActiveEncoderState) {
    active.epoch = next_route_epoch(active.epoch);
}

fn applied_quality() -> Quality {
    PLAYER_CONTROL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .applied_quality
}

fn retarget_automatic_intent(
    intent: &mut AutomaticRouteIntent,
    ticket: WorkerTicket,
    position_ns: Option<i64>,
) {
    match intent {
        AutomaticRouteIntent::OriginalToHls {
            ticket: current,
            position_ns: position,
            ..
        }
        | AutomaticRouteIntent::HlsToOriginal {
            ticket: current,
            position_ns: position,
            ..
        } => {
            *current = ticket;
            if let Some(position_ns) = position_ns {
                *position = position_ns.max(0);
            }
        }
    }
}

/// Cross one explicit desired-contract boundary while holding [`PLAYER_CONTROL`]. An automatic
/// handoff which was already accepted remains labelled with the *applied* ticket that produced it:
/// its producer may have stopped after publication, and relabelling that evidence as the new
/// desire is precisely the PMS-refusal race this split exists to prevent. Seek is different: it
/// changes the target carried by that same accepted handoff and is handled explicitly below.
fn advance_user_contract_locked(control: &mut PlayerControl) {
    control.desired_revision = next_generation(control.desired_revision);
}

/// Fence an outgoing worker before the main thread publishes any part of a new user contract.
/// Quality and track setters call this before changing their durable/session projection; the
/// later route-action request deliberately crosses a second boundary when it enters the action
/// queue, because either boundary can also be reached independently.
struct UserEditGuard {
    serial: Option<u64>,
}

impl Drop for UserEditGuard {
    fn drop(&mut self) {
        let Some(serial) = self.serial.take() else {
            return;
        };
        let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
        if control.phase == ControlPhase::StagingUser(serial) {
            // Every projection/pending-intent write happened before this edge. Workers can now
            // observe the complete edit, never the half between a persisted checkmark and action.
            control.phase = ControlPhase::Stable;
        }
    }
}

fn begin_user_contract_boundary() -> UserEditGuard {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    advance_user_contract_locked(&mut control);
    let serial = if control.phase == ControlPhase::Stable {
        control.next_action = next_generation(control.next_action);
        let serial = control.next_action;
        control.phase = ControlPhase::StagingUser(serial);
        Some(serial)
    } else {
        None
    };
    UserEditGuard { serial }
}

fn merge_user_route_intent(
    pending: Option<UserRouteIntent>,
    incoming: UserRouteIntent,
    preserve_original_recovery: bool,
) -> UserRouteIntent {
    use UserRouteIntent::{AdaptiveReload, NativeAudioReload, RecoverOriginal, Retranscode};
    match (pending, incoming) {
        (_, RecoverOriginal(cause)) => RecoverOriginal(cause),
        (Some(RecoverOriginal(cause)), Retranscode) if preserve_original_recovery => {
            RecoverOriginal(cause)
        }
        (Some(RecoverOriginal(_)), newer) => newer,
        (Some(Retranscode), NativeAudioReload | AdaptiveReload)
        | (Some(NativeAudioReload | AdaptiveReload), Retranscode) => Retranscode,
        (Some(NativeAudioReload), AdaptiveReload) | (Some(AdaptiveReload), NativeAudioReload) => {
            NativeAudioReload
        }
        (_, newer) => newer,
    }
}

/// Publish the PMS half of a resolve. A refused plan has no Engine and therefore lands in `Idle`;
/// a playable plan lands in `Prepared`. Claiming a physical `Load` moves it to `Starting`, and only
/// settling that exact attempt through [`settle_route_start`] may publish `Stable`. In particular,
/// installing a URL is not evidence that the television accepted it.
fn prepare_playback_landing(_ps: &PlaybackSession, playable: bool) -> Option<RouteStartTransaction> {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    control.desired_quality = quality();
    control.engine_epoch = next_generation(control.engine_epoch);
    control.media_epoch = next_generation(control.media_epoch);
    if playable {
        control.applied_revision = control.desired_revision;
        control.applied_quality = control.desired_quality;
    }
    control.pending_auto = None;
    if !playable {
        control.timeline = None;
    }
    let serial = if playable {
        // A dev fixture can install its real route from inside start_bufferfeed, after that call
        // already reserved a route-start transaction. Reuse the same transaction owner instead of
        // replacing it between construction and settlement.
        let serial = match control.phase {
            ControlPhase::Preparing(serial)
            | ControlPhase::Prepared(serial)
            | ControlPhase::Starting(serial, _) => serial,
            _ => {
                control.next_action = next_generation(control.next_action);
                control.next_action
            }
        };
        if !matches!(control.phase, ControlPhase::Starting(_, _)) {
            control.phase = ControlPhase::Prepared(serial);
        }
        serial
    } else {
        control.phase = ControlPhase::Idle;
        return None;
    };
    Some(RouteStartTransaction { serial })
}

/// Route unit tests install plans without a native Engine. Treat their explicit plan helper as
/// the successful native boundary; reducer-specific tests call `prepare_playback_landing`
/// directly to inspect `Prepared`, then explicitly claim and settle an attempt to inspect
/// `Starting` or `Failed`.
fn settle_plan_start_in_unit_test(ps: &mut PlaybackSession, start: Option<RouteStartTransaction>) {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(start) = start {
        if let Some(attempt) = claim_route_start_attempt(start) {
            let _ = settle_route_start(ps, attempt, RouteStartResult::Started);
        }
    }
    #[cfg(not(any(test, feature = "test-support")))]
    let _ = (ps, start);
}

/// Queue the latest explicit route contract. Unlike an automatic request it survives pre-roll and
/// an Original trial. Multiple user changes coalesce to the newest desired contract; their durable
/// fields already live in `Session`, so one later rebuild applies the whole projection.
pub fn request_user_route_intent(ps: &PlaybackSession, intent: UserRouteIntent) {
    queue_user_route_intent(ps, intent, false);
}

/// [`request_user_route_intent`], optionally marking that the queued intent stands in for a
/// track pick's own reload (issue #266's `displaced_pick`). One lock for both writes, so a claim
/// can never take the intent without the marker that explains it.
fn queue_user_route_intent(ps: &PlaybackSession, intent: UserRouteIntent, displaced_pick: bool) {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    control.displaced_pick |= displaced_pick;
    // Setters already crossed an early boundary before publishing their projection. Queueing the
    // resulting route action is a second, independently reachable boundary: callers such as the
    // automatic-recovery UI can request an action directly, and both paths must fence old tickets.
    advance_user_contract_locked(&mut control);
    // Subtitle Off is the one Retranscode which does not invalidate an in-flight Original
    // recovery. The candidate is updated to carry `None`, and a failed Original open necessarily
    // rebases the restored HLS route (`dispatch_rollback_rebase`), which reads the current subtitle id.
    // Audio, subtitle On, and a fixed/Auto quality pick invalidate either the candidate or the
    // `Original` selection before reaching this merge, so their newer actions still win.
    let preserve_original_recovery =
        quality() == Quality::Original && ps.auto_original.is_some();
    control.pending_user = Some(merge_user_route_intent(
        control.pending_user.take(),
        intent,
        preserve_original_recovery,
    ));
}

/// Record a desired seek without pretending it has already changed the physical media timeline.
/// Automatic publication is Busy while this obligation is pending; an already accepted handoff
/// is retargeted because its producer may already have stopped. Only [`commit_user_seek`] advances
/// the worker-visible media epoch.
pub fn note_user_seek_intent(position_ns: i64) {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    control.pending_seek_ns = Some(position_ns.max(0));
    let ticket = worker_ticket_of(&control);
    if let Some(automatic) = control.pending_auto.as_mut() {
        retarget_automatic_intent(automatic, ticket, Some(position_ns));
    }
}

/// Commit the exact point at which queues/route cross to the requested timeline. Calling this
/// before a real flush/reload revokes every pre-seek observation; calling it after a PMS refusal
/// would be a lie, so refusal uses [`reject_user_seek`] instead.
pub fn commit_user_seek() -> bool {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    if control.pending_seek_ns.take().is_none() {
        return false;
    }
    control.media_epoch = next_generation(control.media_epoch);
    let ticket = worker_ticket_of(&control);
    if let Some(automatic) = control.pending_auto.as_mut() {
        retarget_automatic_intent(automatic, ticket, None);
    }
    true
}

pub fn cancel_user_route_intent(intent: UserRouteIntent) {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    if control.pending_user == Some(intent) {
        control.pending_user = None;
        control.displaced_pick = false;
    }
}

/// Return the pending route-start transaction while the reducer is between preparation and a
/// `Load` result. The exact physical attempt is minted only by [`claim_route_start_attempt`]; a dev
/// fixture which prepares its route inside `start_bufferfeed` is covered too.
pub fn pending_route_start() -> Option<RouteStartTransaction> {
    let control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    match control.phase {
        ControlPhase::Preparing(serial)
        | ControlPhase::Prepared(serial)
        | ControlPhase::Starting(serial, _) => Some(RouteStartTransaction { serial }),
        _ => None,
    }
}

/// Return or reserve the semantic transaction for an Engine replacement which does not already
/// belong to a prepared plan/action. [`claim_route_start_attempt`] mints the exact physical attempt.
/// A failed-candidate retry gets a new transaction while retaining its already-prepared URL, so no
/// PMS decision is repeated merely because native construction failed synchronously.
pub fn begin_route_start() -> Option<RouteStartTransaction> {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    match control.phase {
        ControlPhase::Preparing(serial)
        | ControlPhase::Prepared(serial)
        | ControlPhase::Starting(serial, _) => Some(RouteStartTransaction { serial }),
        ControlPhase::Stable => {
            control.next_action = next_generation(control.next_action);
            let serial = control.next_action;
            control.phase = ControlPhase::Preparing(serial);
            Some(RouteStartTransaction { serial })
        }
        // `Failed` and `Idle` are the two phases which own no live Engine, so both go straight to
        // `Prepared`: there is nothing left for `Preparing`'s "old route is still recoverable"
        // window to protect, and entering it would let an ordinary `NoRoute` abort publish
        // `Stable` around a decoder that no longer exists. `Idle` is the phase a COMPLETED stop
        // publishes ([`finish_engine_teardown`]), and it is how a dev fixture replays a stream
        // that ran to EOS — that entry asks for a start owner directly rather than through
        // [`begin_playback_request`].
        ControlPhase::Failed(_) | ControlPhase::Idle => {
            control.next_action = next_generation(control.next_action);
            let serial = control.next_action;
            control.phase = ControlPhase::Prepared(serial);
            Some(RouteStartTransaction { serial })
        }
        _ => None,
    }
}

/// Candidate preparation succeeded and the caller is about to destroy/replace the native Engine.
/// An ordinary post-teardown failure cannot truthfully restore the old Engine; an Original trial
/// is the explicit exception, retaining its HLS rollback projection.
pub fn prepare_route_start(ticket: RouteStartTransaction) -> bool {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    match control.phase {
        ControlPhase::Preparing(serial) if serial == ticket.serial => {
            control.phase = ControlPhase::Prepared(serial);
            true
        }
        ControlPhase::Prepared(serial) | ControlPhase::Starting(serial, _)
            if serial == ticket.serial =>
        {
            true
        }
        _ => false,
    }
}

/// Mint the physical Load attempt only after candidate preparation and destructive teardown have
/// completed. A second attempt always receives a different id, even inside the same transaction.
pub fn claim_route_start_attempt(
    ticket: RouteStartTransaction,
) -> Option<RouteStartAttempt> {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    if control.phase != ControlPhase::Prepared(ticket.serial) {
        return None;
    }
    control.next_start_attempt = control
        .next_start_attempt
        .checked_add(1)
        .expect("native Load attempt identity exhausted");
    let attempt = control.next_start_attempt;
    control.phase = ControlPhase::Starting(ticket.serial, attempt);
    Some(RouteStartAttempt {
        serial: ticket.serial,
        attempt,
    })
}

/// Settle a transaction which never reached a callable native thread. This is distinct from the
/// media-thread result because no callback can race it; accepting either Prepared or Starting is
/// nevertheless useful for a thread-spawn refusal after an attempt id was minted.
pub fn abort_route_start(ticket: RouteStartTransaction, result: RouteStartResult) -> bool {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    let phase = match control.phase {
        ControlPhase::Preparing(serial) if serial == ticket.serial => {
            if result == RouteStartResult::NoRoute {
                ControlPhase::Stable
            } else {
                ControlPhase::Failed(serial)
            }
        }
        ControlPhase::Prepared(serial) | ControlPhase::Starting(serial, _)
            if serial == ticket.serial =>
        {
            ControlPhase::Failed(serial)
        }
        _ => return false,
    };
    control.start_deferred = control
        .start_deferred
        .take()
        .filter(|(serial, _)| *serial != ticket.serial);
    control.timeline = None;
    control.phase = phase;
    true
}

/// Publish the native half of one prepared route. An ordinary failed candidate deliberately
/// remains the applied projection in `Failed`: teardown may already have destroyed the old Engine
/// and PMS may already have retired its encoder, so restoring the old *description* would fabricate
/// a live route.
pub fn settle_route_start(ps: &mut PlaybackSession, ticket: RouteStartAttempt, result: RouteStartResult) -> bool {
    let deferred = {
        let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
        if control.phase != ControlPhase::Starting(ticket.serial, ticket.attempt) {
            return false;
        }
        control.last_start_result = Some((ticket, result));
        match result {
            RouteStartResult::Started => {
                let effects = control
                    .start_deferred
                    .take()
                    .filter(|(serial, _)| *serial == ticket.serial)
                    .map(|(_, effects)| effects);
                if effects.is_some() {
                    control.phase = ControlPhase::Completing(ticket.serial);
                } else {
                    control.phase = ControlPhase::Stable;
                }
                effects
            }
            RouteStartResult::NoRoute | RouteStartResult::StartFailed => {
                control.start_deferred = control
                    .start_deferred
                    .take()
                    .filter(|(serial, _)| *serial != ticket.serial);
                control.timeline = None;
                control.phase = ControlPhase::Failed(ticket.serial);
                None
            }
        }
    };
    if let Some(effects) = deferred {
        apply_deferred_original_effects(ps, effects);
        let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
        if control.phase == ControlPhase::Completing(ticket.serial) {
            control.phase = ControlPhase::Stable;
        }
    }
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveEngineStartRelation {
    /// The caller rediscovered the Engine which already owns this exact in-flight Load.
    CurrentAttempt,
    /// No replacement transaction is waiting; the live Engine is an ordinary idempotent start.
    NoPendingRoute,
    /// A different prepared transaction requires a new Engine and cannot borrow this one.
    Conflict(RouteStartTransaction),
}

/// Classify a start request which found a live native Engine. Keeping this comparison inside the
/// reducer avoids an ABA-prone pair of `pending_route_start`/`route_start_status` observations:
/// both the semantic transaction and physical attempt are compared under one lock.
pub fn classify_live_engine_start(existing: RouteStartAttempt) -> LiveEngineStartRelation {
    let control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    match control.phase {
        ControlPhase::Starting(serial, attempt)
            if serial == existing.serial && attempt == existing.attempt =>
        {
            LiveEngineStartRelation::CurrentAttempt
        }
        ControlPhase::Preparing(serial)
        | ControlPhase::Prepared(serial)
        | ControlPhase::Starting(serial, _) => {
            LiveEngineStartRelation::Conflict(RouteStartTransaction { serial })
        }
        _ => LiveEngineStartRelation::NoPendingRoute,
    }
}

/// Media-thread half of the start handshake. Queue rather than settling here: `sf_load` can return
/// before the spawning main thread has installed its Engine, and Stable must never lead that slot.
pub fn publish_route_start_result(ticket: RouteStartAttempt, result: RouteStartResult) {
    PLAYER_CONTROL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .start_results
        .push((ticket, result));
}

/// Main-thread publication point for every completed native Load call. Late/stale results are
/// intentionally drained too; [`settle_route_start`] rejects their exact serial.
pub fn drain_route_start_results(ps: &mut PlaybackSession) {
    let results = {
        let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut control.start_results)
    };
    for (ticket, result) in results {
        let _ = settle_route_start(ps, ticket, result);
    }
}

/// A route which had reached Stable later proved terminal (demux/HTTP/native callback failure).
/// Revoke the Engine/media tickets and expose Failed as one reducer edge rather than changing only
/// the UI playback enum while automatic workers still believe the route is publishable.
pub fn fail_current_engine() {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    let serial = match control.phase {
        ControlPhase::Preparing(serial) | ControlPhase::Prepared(serial) | ControlPhase::Starting(serial, _) => serial,
        ControlPhase::Stable | ControlPhase::Completing(_) => {
            control.next_action = next_generation(control.next_action);
            control.next_action
        }
        ControlPhase::StagingUser(serial) => serial,
        ControlPhase::Failed(_) | ControlPhase::Idle | ControlPhase::Stopping => return,
    };
    control.engine_epoch = next_generation(control.engine_epoch);
    control.media_epoch = next_generation(control.media_epoch);
    control.pending_auto = None;
    control.start_deferred = None;
    control.timeline = None;
    control.clear_flight();
    control.phase = ControlPhase::Failed(serial);
}

/// Invalidate the workers belonging to the Engine being torn down. Pending explicit contracts are
/// retained; a reload can therefore never consume a quality/track request merely by resetting the
/// atomics that used to carry it.
pub fn begin_engine_teardown(for_reload: bool) {
    if !for_reload {
        // A full stop (not an in-place reload) ends the item this claim was for. Any landing
        // still sitting in the mailbox after this point belongs to whatever plays next, not to
        // this one — see `discard_retranscode_claim_slot`'s doc.
    }
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    control.engine_epoch = next_generation(control.engine_epoch);
    control.media_epoch = next_generation(control.media_epoch);
    control.pending_auto = None;
    // A seek's flight does not survive the Engine it was going to replace. The route it was
    // preparing is still the live one (nothing was committed), so the transaction goes back to
    // `Stable`: an app-switch's foreground restore reserves its own start transaction next, and a
    // `Preparing` left behind would refuse it. The landing, if the worker has not posted it yet,
    // meets a phase that no longer matches and is discarded.
    if !for_reload {
        control.desired_revision = next_generation(control.desired_revision);
        control.pending_user = None;
        control.displaced_pick = false;
        control.pending_seek_ns = None;
        control.phase = ControlPhase::Stopping;
        control.clear_flight();
        control.timeline = None;
        } else {
        let current = control.phase;
        control.phase = match current {
            ControlPhase::Preparing(serial) if control.flight == Some(serial) => {
                control.clear_flight();
                ControlPhase::Stable
            }
            ControlPhase::Starting(serial, _) => {
                // The physical attempt being torn down can still publish from its media thread.
                // Return the transaction to Prepared so a retry mints a fresh attempt; the old
                // result no longer matches even when both attempts open the same candidate URL.
                ControlPhase::Prepared(serial)
            }
            phase => phase,
        };
    }
    drop(control);
}

/// The reducer's phase, for a log line. A refused start owner names the phase it was refused
/// from, because "refused" alone cannot be diagnosed from a device log.
pub fn control_phase_label() -> String {
    let control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    format!("{:?}", control.phase)
}

/// The synchronous main-thread teardown announced by `begin_engine_teardown(false)` has RETURNED:
/// every worker is joined, the native object is retired and the URL is cleared. `Stopping` is that
/// teardown's fence and nothing more — it exists so a worker which was still running when the stop
/// began cannot publish into the route it is destroying — so leaving it latched afterwards
/// describes a teardown which never finishes.
///
/// It cost LG App Self Checklist #46: a replayed `dispatcharrtv-playurl` stream asks
/// [`begin_route_start`] for an owner directly (no [`begin_playback_request`] resolve in front of
/// it, because there is no library item behind the fixture), and a latched `Stopping` refused it —
/// `start_bufferfeed: route reducer refused a start owner`, one Load where the case needs two.
///
/// `Idle` rather than `Stable`: a completed stop owns no publishable route, so automatic
/// publication and [`claim_route_action`] must stay closed. Only `Stopping` moves; a phase already
/// replaced by a newer request is left exactly as that request left it.
pub fn finish_engine_teardown() {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    if control.phase == ControlPhase::Stopping {
        control.phase = ControlPhase::Idle;
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn pending_user_route_intent(intent: UserRouteIntent) -> bool {
    PLAYER_CONTROL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pending_user
        == Some(intent)
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_player_control_for_test(ps: &PlaybackSession) {
    let _ = ps;
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    control.engine_epoch = next_generation(control.engine_epoch);
    control.media_epoch = next_generation(control.media_epoch);
    control.desired_revision = next_generation(control.desired_revision);
    control.desired_quality = quality();
    control.applied_revision = control.desired_revision;
    control.applied_quality = control.desired_quality;
    advance_route(&mut control.active);
    control.active.id.clear();
    control.active.hls = None;
    control.next_action = 0;
    // Physical attempts are process-monotonic. The result queue outlives an Engine reset, so
    // reusing an id here would make a late completion an ABA match for the next fixture/session.
    control.pending_user = None;
    control.displaced_pick = false;
    control.pending_auto = None;
    control.pending_seek_ns = None;
    control.phase = ControlPhase::Stable;
    control.clear_flight();
    control.start_results.clear();
    control.last_start_result = None;
    control.start_deferred = None;
    control.timeline = None;
}

fn install_active_encoder(value: &str) {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    let active = &mut control.active;
    advance_route(active);
    active.id = value.to_owned();
    active.hls = None;
}

fn install_active_hls(value: &str, url: &str, rung: crate::abr::Rung) {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    let active = &mut control.active;
    advance_route(active);
    active.id = value.to_owned();
    active.hls = Some(ActiveHlsRoute {
        url: url.to_owned(),
        rung,
    });
}

fn take_active_encoder() -> String {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    let active = &mut control.active;
    advance_route(active);
    active.hls = None;
    std::mem::take(&mut active.id)
}

/// The process route no longer belongs to the worker which tried to publish a bounded local
/// transition. Kept separate from [`HlsCommitRefusal`]: this door changes no HLS route and has no
/// controller-rejection or server-session arm.
// Dev-only: used only by the `#[cfg(feature = "devtriggers")]` tests in this module's `tests`
// submodule below (see the comment on the first one).
#[cfg(all(any(test, feature = "test-support"), feature = "devtriggers"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveEncoderRefusal {
    RouteMoved,
}

/// Reconcile the worker-owned adaptive projection into the main-thread session immediately before
/// an operation that rebuilds or snapshots the route.  Ordinary playback never needs this copy;
/// seek, manual Original and track/quality reloads do, because they construct a new URL from the
/// stream that is live NOW rather than from the bootstrap stream that created the worker.
fn sync_active_hls_to_session(ps: &mut PlaybackSession) -> Option<(WorkerTicket, ActiveHlsRoute)> {
    // Capture the physical HLS commit and advance only those same physical fields in the applied
    // projection while holding the route mutex.  A user may already have staged a different
    // audio/subtitle/quality contract in Session; cloning Session wholesale here would falsely
    // bless that proposal merely because an older HLS worker changed rung underneath it.
    let active = {
        let control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
        let hls = control.active.hls.clone()?;
        let ticket = worker_ticket_of(&control);
        (ticket, hls)
    };
    { let s = &mut *ps; {
        s.url = active.1.url.clone();
        s.tsession = active.0.encoder().to_owned();
        // An adaptive commit changes the encoder, URL and requested rung, not the delivery
        // contract. Preserve the route's negotiated segment duration instead of fabricating one
        // here: seek/reload must carry the exact server contract that created this worker.
        s.cur_contract.ceiling = Some(active.1.rung.ceiling());
        s.cur_contract.remux = false;
    } };
    // The applied clone retained above intentionally excludes Session's staged user fields:
    // rejection combines the newest physical HLS route with the last accepted track contract.
    Some(active)
}

/// **The feasibility filter, as one object the worker cannot argue with.** Built from two
/// independent facts and nothing else: what this device's own codec table says it decodes
/// (`devcaps`, which exists because "4K yes" was once a constant describing one television), and
/// what raster the source actually has. Neither is a preference and neither belongs in a utility
/// weight — a candidate outside these bounds is removed before anything is scored.
fn auto_catalog(ps: &PlaybackSession) -> crate::abr::HlsActuatorCatalog {
    let caps = plx_platform::devcaps::caps();
    let device = (
        u16::try_from(caps.hevc_max.0).unwrap_or(u16::MAX),
        u16::try_from(caps.hevc_max.1).unwrap_or(u16::MAX),
    );
    let (_, width, height) = ps.cur_src;
    let source = (
        u16::try_from(width).unwrap_or(u16::MAX),
        u16::try_from(height).unwrap_or(u16::MAX),
    );
    crate::abr::HlsActuatorCatalog::measured().limited_to(device, source)
}

/// The source-probe measurement this playback already paid for, as a weak prior. `None` once it is
/// too old to mean anything or when there never was one.
/// **What the next controller starts from, and the seek is why it has two sources** (I8).
///
/// The CARRIED estimate wins when there is one. A seek destroys the engine and builds a fresh
/// `Controller`, and before this the only thing that survived was `auto_prior_kbps` — whose writer
/// on the Original->HLS fallback path is `measured_kbps` *at the moment the link failed*. So after
/// one bad patch every subsequent seek re-seeded from the worst rate the playback had ever
/// measured, at `MAX_UNCERTAINTY_PM` with one sample, and the ladder re-ramped for five to ten
/// segments: ten to twenty seconds of visibly softer picture after every skip.
///
/// **`auto_prior_kbps` is not deleted and is not a fallback of convenience.** It remains the
/// BOOTSTRAP seed — the startup probe, and the rate measured when Original was abandoned — which
/// is the right seed when there is no live HLS estimate to carry, i.e. the first controller of a
/// playback. `from_prior` states its own weakness (uncertainty at the cap, one sample); the
/// carried snapshot states what was actually observed. Two different claims, two constructors.
///
/// Only the DELIVERY estimate crosses. The buffer, the risk history and any pending transaction
/// describe a position that no longer exists and are reset by the new `Controller`'s construction.
fn auto_prior(ps: &PlaybackSession) -> Option<crate::abr::CapacityEstimate> {
    let carried = crate::player::SHARED.abr_seed();
    carried.or_else(|| {
        let kbps = ps.auto_prior_kbps;
        (kbps > 0).then(|| crate::abr::CapacityEstimate::from_prior(kbps))
    })
}

/// Arm the no-Plex pipeline tier for the same Original→HLS transaction production uses. The only
/// substitution is URL allocation: [`HlsAbrControl`] maps a rung to fixture playlists rather than
/// asking PMS to create an encoder. Transport, FFmpeg, buffer measurement, the controller, pump
/// handoff and Starfish are unchanged, which makes a mid-request bandwidth profile testable on a
/// TV without a library, account, token, or external server.
///
/// `start_hls` skips the Original phase entirely, removes the synthetic Original candidate and
/// returns the playlist to open. Use it for every case that grades the HLS CONTROLLER; leave it
/// off only where the transition itself is what is being graded, and give that case a
/// `network_profile` that starves for real. Removing the candidate is load-bearing: otherwise a
/// loopback source probe can escape to Original before a request-indexed HLS cliff occurs.
/// [`crate::player::playurl::PlayUrl::auto_start_hls`] has the history — the alternative was declaring a
/// source rate no link could carry and relying on a starvation horizon that did not check whether
/// the reserve was draining.
pub fn arm_auto_fixture(
    ps: &mut PlaybackSession,
    original_url: &str,
    source_kbps: u32,
    hls_base: &str,
    start_hls: bool,
    source_raster: (u16, u16),
) -> Option<String> {
    { let s = &mut *ps; {
        s.url = original_url.to_owned();
        s.cur_rk = "__auto_fixture__".into();
        s.sess = "auto-fixture".into();
        s.cur_contract.delivery = plx_plex::plex::TranscodeDelivery::ProgressiveMkv;
        s.cur_contract.ceiling = None;
        // **Saying the source raster out loud is load-bearing rather than cosmetic**: an unknown
        // one is treated as unbounded (`HlsActuatorCatalog::limited_to`), which makes the 4K
        // actuator feasible on every case. It was a hardcoded 1080p because
        // `tests/serve_fixtures.py` served no 22000 rung, so such a candidate would 404 and read
        // on the television as a rejected encoder — a fixture gap standing in for a policy, and
        // the thing that kept the plan's I9 blocked. The server answers 22000 now, so the caller
        // declares it (`player::playurl::PlayUrl::source_raster`) and the default is still 1080p.
        s.cur_src = (
            i64::from(source_kbps),
            i64::from(source_raster.0),
            i64::from(source_raster.1),
        );
        s.cur_transport_kbps = i64::from(source_kbps);
        s.cur_auto_original_watched = true;
        s.auto_bootstrap_rung = Some(crate::abr::Rung::P480);
        s.auto_original = Some(AutoOriginalCandidate {
            probe_part: original_url.to_owned(),
            direct: true,
            dovi: plx_data::metadata::Dovi::NONE,
            dv_decision: plx_data::metadata::DvDecision::NONE,
            audio: Some(CarriedAudio {
                codec: "aac".into(),
                ..CarriedAudio::named(0, -1)
            }),
            audio_converted: false,
        });
        s.auto_fixture_base = hls_base.trim_end_matches('/').to_owned();
    } };
    install_active_encoder("");
    crate::player::log(&format!(
        "auto fixture: Original source={}kbps armed",
        source_kbps
    ));
    if !start_hls {
        // The fixture is a real playback entry point (used by the device harness), not a bag of
        // Session test setters.  Publish the installed Original through the same reducer landing
        // as a resolved Plex item so its worker owns the selected Auto contract.  Without this,
        // the durable picker said Auto while `applied_quality` still named the previous playback,
        // and the progressive watchdog was correctly refused as belonging to another contract.
        settle_plan_start_in_unit_test(ps, prepare_playback_landing(ps, true));
        return None;
    }
    // Install exactly the state `fallback_auto_to_hls` leaves behind, at the bootstrap rung, and
    // hand the caller the playlist to open. See `player::playurl::PlayUrl::auto_start_hls` for why this exists
    // at all: the alternative was declaring a source rate no link could carry and relying on the
    // starvation horizon to fire on a reserve that was visibly FILLING.
    let rung = crate::abr::Rung::P480;
    let base = hls_base.trim_end_matches('/');
    let url = format!(
        "{base}/{}/master.m3u8?X-Plex-Token=<plex-token>",
        rung.kbps()
    );
    let encoder = format!("auto-fixture-{}", rung.kbps());
    { let s = &mut *ps; {
        s.cur_auto_original_watched = false;
        // `start_hls` means this is an HLS-only controller fixture, not merely an HLS entry point.
        // A synthetic whole-file request on loopback is not constrained by a later HLS-only
        // segment profile and can therefore pre-empt the very collapse the case was built to
        // grade. Original recovery has its own end-to-end fixture with `start_hls == false`.
        s.auto_original = None;
        s.cur_contract.remux = false;
        s.cur_contract.delivery = plx_plex::plex::TranscodeDelivery::FixedHls {
            seconds_per_segment: 2,
        };
        s.cur_contract.ceiling = Some(rung.ceiling());
        s.url = url.clone();
        s.tsession = encoder.clone();
        s.stream_vcodec = "h264".into();
        s.stream_acodec = "aac".into();
    } };
    install_active_hls(&encoder, &url, rung);
    crate::player::log(&format!(
        "auto fixture: starting in {}kbps {}x{} HLS (no Original phase)",
        rung.kbps(),
        rung.raster().0,
        rung.raster().1,
    ));
    settle_plan_start_in_unit_test(ps, prepare_playback_landing(ps, true));
    Some(url)
}

#[derive(Default)]
pub struct DeferredOriginalEffects {
    quality: Option<Quality>,
    audio: Option<CarriedAudio>,
    reconcile: bool,
}

impl DeferredOriginalEffects {

}

pub struct OriginalRollback {
    pub offset_ns: i64,
}

impl OriginalRollback {
}

/// true while this playback is a server transcode (a live transcode session exists). Cheap
/// in-place check — the pump polls it every tick, so no String clone here.
pub fn is_transcoding(ps: &PlaybackSession) -> bool {
    !ps.tsession.is_empty()
}
/// Did the server REFUSE this item at `/decision`, before playback? Cheap in-place check —
/// `player::state()` derives `Error` from it on every frame of the player route.
pub fn play_refused(ps: &PlaybackSession) -> bool {
    ps.play_verdict.is_some()
}
/// The app/worker failed to produce a playable plan, as distinct from a PMS `/decision` refusal.
/// There is no Engine whose pump could publish Error, so `player::state()` derives it beside the
/// refusal case.
pub fn play_resolution_failed(ps: &PlaybackSession) -> bool {
    ps.resolve_failed
}
/// The refusal's own sentence for the read-out to quote — `None` when the server did not refuse,
/// `Some("")` when it refused without saying why. MAIN THREAD (see [`PlaybackSession::play_verdict`]).
///
/// Borrowed, not cloned: the read-out asks for this 2–3× on every frame of a failure (the HUD
/// caption, the read-out itself, and the diagnostics panel when it is open), and every one of them
/// only reads it. The borrow lives until the next main-thread write, which is `apply_plan` or
/// `request_play` — neither of which can run inside a frame's draw.
pub fn play_verdict(ps: &PlaybackSession) -> Option<&str> {
    ps.play_verdict.as_ref().map(PlayVerdict::text)
}
/// The verdict NUMBERS of a refusal the SERVER made at `/decision` — `None` when nothing was
/// refused, or when the refusal was the app's own (Direct Play off, Force Direct Play): those carry
/// no server codes because the server was never asked to decide. Numbers only; the sentence is
/// [`play_verdict`], and it never reaches a report. MAIN THREAD.
pub fn server_refusal_codes(ps: &PlaybackSession) -> Option<DecisionCodes> {
    match ps.play_verdict {
        Some(PlayVerdict::Server(_, codes)) => Some(codes),
        _ => None,
    }
}
/// Retire the refusal — "this playback request is withdrawn", the one thing besides a fresh
/// resolve that ends a verdict's life. [`request_play`] clears it because a NEW item is being
/// resolved; this is the other half, for leaving the player entirely.
///
/// Without it a refusal outlived the player: `player::state()` derives `Error` from this field
/// and takes no route, so a verdict left standing described the item the user walked away from —
/// on Home, in the Library, on any detail page — until they happened to start something else.
fn clear_play_verdict(ps: &mut PlaybackSession) {
    { let s = &mut *ps; {
        s.play_verdict = None;
        s.jail_load_blocked = false;
        s.native_start_refused = false;
        s.resolve_failed = false;
        s.requested_resume_ns = 0;
    } }
}
/// Test-only twin of [`clear_play_verdict`], for a test whose assertion presumes "no session":
/// `player::state()` derives `Error` from these fields, and a test that exercised a refusal on the
/// SAME session value may have left one standing. It no longer needs `plx_base::testlock::serial()`
/// for this reason — since phase 9 a test owns its session outright and cannot leave a refusal in
/// anybody else's.
#[cfg(any(test, feature = "test-support"))]
pub fn clear_play_verdict_for_test(ps: &mut PlaybackSession) {
    clear_play_verdict(ps)
}
/// Test-only: leave `ps` exactly as a refusing [`apply_plan`] does — the verdict installed, no URL,
/// no encoder session. For asserting what the failure report says about a playback that never got
/// a route.
#[cfg(any(test, feature = "test-support"))]
pub fn refuse_for_test(ps: &mut PlaybackSession, verdict: PlayVerdict) {
    ps.play_verdict = Some(verdict);
}
/// Test-only: leave `ps` as a refusing [`apply_plan`] does after the SERVER refused a transcode —
/// the verdict and its codes installed, the attempted route's contract and the source file's
/// codecs recorded, and still no URL and no encoder session.
#[cfg(any(test, feature = "test-support"))]
pub fn refuse_by_server_for_test(
    ps: &mut PlaybackSession,
    sentence: &str,
    codes: DecisionCodes,
    remux: bool,
    hls: bool,
    src_vcodec: &str,
    src_acodec: &str,
) {
    ps.play_verdict = Some(PlayVerdict::Server(sentence.to_owned(), codes));
    ps.cur_contract.remux = remux;
    ps.cur_contract.delivery = if hls {
        plx_plex::plex::TranscodeDelivery::FixedHls { seconds_per_segment: 2 }
    } else {
        plx_plex::plex::TranscodeDelivery::ProgressiveMkv
    };
    ps.src_vcodec = src_vcodec.to_owned();
    ps.src_acodec = src_acodec.to_owned();
}
/// Test-only: install a live transcode route (`encoder` session, optional remux, optional
/// fixed-HLS delivery) the way a successful [`apply_plan`] leaves it.
#[cfg(any(test, feature = "test-support"))]
pub fn install_transcode_for_test(ps: &mut PlaybackSession, remux: bool, hls: bool) {
    ps.tsession = "test-encoder".to_owned();
    ps.cur_contract.remux = remux;
    ps.cur_contract.delivery = if hls {
        plx_plex::plex::TranscodeDelivery::FixedHls { seconds_per_segment: 2 }
    } else {
        plx_plex::plex::TranscodeDelivery::ProgressiveMkv
    };
}
/// the subtitle stream id currently burned into the transcode (0 = none).
pub fn cur_sub_sid(ps: &PlaybackSession) -> i64 {
    ps.cur_sub_sid
}
/// ratingKey of the currently-playing item (for /:/timeline progress reports).
pub fn cur_rk(ps: &PlaybackSession) -> String {
    ps.cur_rk.clone()
}
/// The server the currently-playing item came from — see [`PlaybackSession::cur_sid`]. MAIN THREAD.
///
/// A worker must be handed this by value at its spawn site, never call it: read on a worker it is
/// "whatever is playing now", which is the very race capturing the id was meant to end.
pub fn cur_sid(ps: &PlaybackSession) -> ServerId {
    ps.cur_sid
}
/// Test-only: install the playing item's server directly, returning the previous value to restore.
///
/// In production [`PlaybackSession::cur_sid`] has exactly one writer — `apply_plan` — and a `Plan` cannot
/// be built outside this module, so a suite elsewhere that needs "this is playing from the share"
/// sets it through here rather than widening `Plan` for a test. `player::playing_subscription` is
/// the reader that needs it: the failure read-out's Plex Pass claim is about the server the failing
/// item came from, and there is no other way to say which that is. It returns the previous value
/// because it began life as a swap of a crate global; since phase 9 the session is the caller's own
/// value, so putting it back is the caller's convenience rather than an obligation to other tests.
#[cfg(any(test, feature = "test-support"))]
pub fn swap_cur_sid_for_test(ps: &mut PlaybackSession, sid: ServerId) -> ServerId {
    { let s = &mut *ps; std::mem::replace(&mut s.cur_sid, sid) }
}
/// The `Client` for the currently-playing item's server, `None` before the first play (or after a
/// plan that never resolved). The main-thread twin of `client_for(env.sid)` on the resolve worker
/// — every in-playback PMS call in this file goes through one of the two, and none through
/// `client_opt()`, which answers with whatever server is CURRENT rather than the one playing.
fn cur_client(ps: &PlaybackSession) -> Option<&'static plx_plex::plex::Client> {
    plx_plex::plex::client_for(cur_sid(ps))
}
/// Projection: the wire id of the carried audio track, `0` for "server default / none" — the
/// pre-#266 shape of this accessor, kept so every existing caller that only ever wanted the id
/// keeps compiling against `cur_audio` unchanged.
pub fn cur_audio_sid(ps: &PlaybackSession) -> i64 {
    ps.cur_audio.as_ref().map_or(0, |a| a.sid)
}
/// The currently-playing item's Part id. Written once per item by `build_stream` from its own
/// `part` argument. In-playback callers (audio switch, subtitle toggle, retranscode) want this;
/// `build_stream` must pass its freshly-derived local instead, since this is not yet updated
/// for the item being started.
fn cur_part_id(ps: &PlaybackSession) -> i64 {
    ps.cur_part_id
}
/// The stable app-owned playback generation (and the first encoder's PMS session id).
pub fn sess(ps: &PlaybackSession) -> String {
    ps.sess.clone()
}
pub fn pq_id(ps: &PlaybackSession) -> String {
    ps.pq_id.clone()
}
pub fn pq_item_id(ps: &PlaybackSession) -> String {
    ps.pq_item_id.clone()
}
/// The streamed item's Media video/audio codec, so the player picks the H265 Load payload for a
/// native HEVC direct-play and the matching audio codec.
pub fn stream_vcodec(ps: &PlaybackSession) -> String {
    ps.stream_vcodec.clone()
}
pub fn stream_acodec(ps: &PlaybackSession) -> String {
    ps.stream_acodec.clone()
}
/// direct-play source video fps for the Load esInfo (0 = unknown/transcode → omit)
pub fn stream_fps(ps: &PlaybackSession) -> f64 {
    ps.stream_fps
}
/// The direct-played file's Dolby Vision layering, for the Load payload's `DolbyHdrInfo` node.
/// `Dovi::NONE` for anything the server is transcoding or remuxing, and for a DV file we refused
/// to declare — in every one of those cases the payload must say nothing.
pub fn stream_dovi(ps: &PlaybackSession) -> plx_data::metadata::Dovi {
    let s = &*ps;
    if s.stream_vcodec.eq_ignore_ascii_case("hevc") {
        s.stream_dovi
    } else {
        // Last-line consistency guard for dev declarations and future route mutations: the LG
        // payload cannot truthfully describe Dolby Vision on a non-HEVC elementary stream.
        plx_data::metadata::Dovi::NONE
    }
}
/// The capability and presentation frozen into this installed route. Unlike the raw DOVI metadata,
/// this is the value the Load payload must consume without consulting the live capability cache.
pub fn stream_dv_presentation(ps: &PlaybackSession) -> plx_data::metadata::DvPresentation {
    if ps.stream_vcodec.eq_ignore_ascii_case("hevc") {
        ps.stream_dv_decision.presentation
    } else {
        plx_data::metadata::DvPresentation::NotDv
    }
}

pub fn stream_dv_decision(ps: &PlaybackSession) -> plx_data::metadata::DvDecision {
    ps.stream_dv_decision
}

/// Is the audio being fed a Dolby Atmos stream? — the Load payload's `contents.immersive` node.
/// See [`PlaybackSession::stream_immersive`].
pub fn stream_immersive(ps: &PlaybackSession) -> bool {
    let s = &*ps;
    // This pipeline's Atmos path is E-AC3 JOC.  AAC/AC3 are ordinary output even if a stale
    // source flag exists, so neither diagnostics nor Load may repeat that source-only claim.
    s.stream_acodec.eq_ignore_ascii_case("eac3") && s.stream_immersive
}
/// Override the audio codec used to build the Load payload — set by a native audio-track
/// switch to the chosen track's codec before the direct-play reload.
pub fn set_stream_acodec(ps: &mut PlaybackSession, codec: &str) {
    { let s = &mut *ps; s.stream_acodec = codec.to_owned() }
}
/// Record the streamed item's video+audio codec pair in one write (the Load-payload source of
/// truth) for route-policy tests that install a synthetic live HLS response.
#[cfg(any(test, feature = "test-support"))]
pub fn set_stream_codecs(ps: &mut PlaybackSession, vc: &str, ac: &str) {
    { let s = &mut *ps; {
        s.stream_vcodec = vc.to_owned();
        s.stream_acodec = ac.to_owned();
    } }
}

/// **The widest raster this session can put through the decoder**, for the Starfish Load's
/// `adaptiveStreaming` ceiling — `(0, 0)` when nobody said. Three routes, three answers:
///
/// * direct play / remux (`ProgressiveMkv`): the SOURCE's own coded size (`cur_src`), since the
///   file's elementary stream is what gets fed;
/// * a fixed quality's HLS: the source capped by that quality's [`plx_plex::plex::Ceiling`] — PMS
///   never upscales, so the smaller of the two is the largest picture it can send;
/// * Auto: the bounding box of every FEASIBLE actuator (`HlsActuatorCatalog::widest_feasible_raster`),
///   because a rung commit never re-issues `Load` and the controller may climb to the 4K point
///   inside the one declaration — a ceiling sized to the bootstrap rung (`plan.ceiling`, which is
///   the STARTING rung and not the maximum) would be exceeded on the first climb.
///
/// Main thread, like every other session read. Pure over the session it is given.
pub fn sink_max_raster(ps: &PlaybackSession) -> (u16, u16) {
    let s = &*ps;
    let clamp = |v: i64| u16::try_from(v).unwrap_or(u16::MAX);
    let source = (clamp(s.cur_src.1), clamp(s.cur_src.2));
    match s.cur_contract.delivery {
        plx_plex::plex::TranscodeDelivery::ProgressiveMkv => source,
        plx_plex::plex::TranscodeDelivery::FixedHls { .. } => {
            if applied_quality() == Quality::Auto {
                auto_catalog(ps).widest_feasible_raster()
            } else {
                match s.cur_contract.ceiling {
                    Some(c) => {
                        // 0 on either side means "nobody said"; the other side's number wins.
                        let axis = |src: u16, cap: i64| match (src, clamp(cap)) {
                            (0, c) => c,
                            (s, 0) => s,
                            (s, c) => s.min(c),
                        };
                        (axis(source.0, c.max_w), axis(source.1, c.max_h))
                    }
                    None => source,
                }
            }
        }
    }
}

/// The SOURCE raster for a stream the app did not select — the pipeline tier's
/// `dispatcharrtv-playurl` carries it as `source_raster`, the same field its Auto fixtures already
/// used to size the actuator catalog. One fact, one field: [`sink_max_raster`] reads it from the
/// same place a PMS-chosen item's dimensions land, so the synthetic tier declares exactly what the
/// production route would for a file of that size.
pub fn set_stream_source_raster(ps: &mut PlaybackSession, w: u16, h: u16) {
    { let s = &mut *ps; {
        s.cur_src.1 = i64::from(w);
        s.cur_src.2 = i64::from(h);
    } };
}

/// The whole Load-payload DECLARATION for a stream the app did not SELECT — the pipeline test
/// tier's `/tmp/dispatcharrtv-playurl` ([`crate::player::playurl::PlayUrl`]), whose entire point is that no PMS
/// chose anything and so `apply_plan` never runs.
///
/// ONE write for the same reason [`set_server_output_declaration`] is one write and [`apply_plan`]
/// is a single struct assignment: these five fields describe ONE stream, and a half-applied set is
/// a payload that describes nothing real — 4K HEVC declared with the default `""` audio, say,
/// which falls through the engine's `_ =>` arm to `"AC3"` and stalls the sink on a Dolby Digital
/// Plus track. Production route transitions likewise update the complete declaration together.
/// Four separate setters would be four ways to leave it half-written.
///
/// This touches neither `cur_rk`/`cur_sid` nor `tsession`, which is what keeps a URL-fed playback
/// free of Plex entirely: the `/:/timeline` reporter stays unspawned and `is_transcoding()` stays
/// false.
pub fn set_stream_declaration(
    ps: &mut PlaybackSession,
    vc: &str,
    ac: &str,
    fps: f64,
    dovi: plx_data::metadata::Dovi,
    immersive: bool,
) -> bool {
    set_stream_declaration_with_capability(
        ps,
        vc,
        ac,
        fps,
        dovi,
        immersive,
        plx_platform::devcaps::dv::capability(),
    )
}

fn set_stream_declaration_with_capability(
    ps: &mut PlaybackSession,
    vc: &str,
    ac: &str,
    fps: f64,
    dovi: plx_data::metadata::Dovi,
    immersive: bool,
    capability: plx_platform::devcaps::dv::DvCapability,
) -> bool {
    let decision = plx_data::metadata::DvDecision {
        capability,
        presentation: dovi.presentation(
            !plx_data::metadata::dv_withheld(),
            capability,
            vc.eq_ignore_ascii_case("hevc"),
        ),
    };
    if decision.presentation.refusal().is_some() {
        crate::player::log(&format!(
            "playurl: refusing Dolby Vision declaration capability={} presentation={}",
            decision.capability.label(),
            decision.presentation.label(),
        ));
        return false;
    }
    { let s = &mut *ps; {
        s.stream_vcodec = vc.to_owned();
        s.stream_acodec = ac.to_owned();
        s.stream_fps = fps;
        s.stream_dovi = dovi;
        s.stream_dv_decision = decision;
        s.stream_immersive = immersive;
    } }
    true
}

#[cfg(any(test, feature = "test-support"))]
pub fn set_stream_declaration_for_test(
    ps: &mut PlaybackSession,
    vc: &str,
    ac: &str,
    fps: f64,
    dovi: plx_data::metadata::Dovi,
    immersive: bool,
    capability: plx_platform::devcaps::dv::DvCapability,
) -> bool {
    set_stream_declaration_with_capability(ps, vc, ac, fps, dovi, immersive, capability)
}

// (`set_source_codecs` stood here: a two-line setter for `src_vcodec`/`src_acodec` whose one
// caller was `apply_plan`, which now installs them as part of its single assignment. The rule it
// carried survives on [`PlaybackSession::src_vcodec`] itself — those two are the FILE's codecs and
// `apply_decision_codecs`, which overwrites the stream pair with the transcode's output, must
// never touch them.)

/// Was this playback's transcode a container-only REMUX (codecs copied) rather than a re-encode?
/// Meaningless unless `is_transcoding()`. The diagnostics read-out's three-way Source row turns on
/// it: "the server touched the pixels" and "the server repackaged the bytes" are different facts
/// and only one of them can explain a decode problem.
pub fn is_remux(ps: &PlaybackSession) -> bool {
    ps.cur_contract.remux
}
/// The Plex Pass DSP the live route was ASKED for (issue #266) — `cur_contract.audio`, gated to
/// what the server DEMONSTRABLY applied: `NONE` unless `cur_enhancement` is `Applied`
/// (`Unverified`/`Refused`/`Off` say nothing was provably added to this stream, so a caller
/// quoting the params must not claim them). Read by the diagnostics Audio row's suffix, which used
/// to read the unfiltered ask and gate it on `cur_enhancement_label(ps) == Some("applied")` itself.
pub fn applied_audio_enhancements(ps: &PlaybackSession) -> plx_plex::plex::AudioEnhancements {
    if ps.cur_enhancement == EnhancementOutcome::Applied {
        ps.cur_contract.audio
    } else {
        plx_plex::plex::AudioEnhancements::NONE
    }
}
/// **Narrow diagnostics accessor** — deliberately not `pub fn cur_enhancement`, which would
/// widen [`EnhancementOutcome`] itself past `pub(super)` for one read-only caller. `None` for
/// `Off` (nothing to report); otherwise the wire word `app::diagnostics::route_line` appends as
/// `enh=<word>`.
pub fn cur_enhancement_label(ps: &PlaybackSession) -> Option<&'static str> {
    match ps.cur_enhancement {
        EnhancementOutcome::Off => None,
        EnhancementOutcome::Applied => Some("applied"),
        EnhancementOutcome::Refused => Some("refused"),
    }
}
/// Did this playback forbid the server a video stream COPY? Read by the seek and audio-switch
/// rebuilds so the constraint survives them — see [`PlaybackSession::cur_contract`].
fn is_no_video_copy(ps: &PlaybackSession) -> bool {
    ps.cur_contract.no_video_copy
}
/// The quality ceiling THIS playback was resolved under — read by the two query rebuilds
/// ([`plan_rebase`], [`retranscode`]) so a rung picked mid-film cannot reshape the encode
/// already on screen. See [`PlaybackSession::cur_contract`].
fn cur_ceiling(ps: &PlaybackSession) -> Option<plx_plex::plex::Ceiling> {
    ps.cur_contract.ceiling
}
fn cur_delivery(ps: &PlaybackSession) -> plx_plex::plex::TranscodeDelivery {
    ps.cur_contract.delivery
}

/// Whether the live route is the segmented HLS transport. The player uses this at the Starfish
/// Load boundary: HLS must prime both elementary-stream lanes before starting the audio-master
/// clock, even on an ordinary play-from-zero where no seek rebase is pending.
pub fn is_segmented_hls(ps: &PlaybackSession) -> bool {
    matches!(
        cur_delivery(ps),
        plx_plex::plex::TranscodeDelivery::FixedHls { .. }
    )
}
pub fn source_vcodec(ps: &PlaybackSession) -> String {
    ps.src_vcodec.clone()
}

pub fn source_acodec(ps: &PlaybackSession) -> String {
    ps.src_acodec.clone()
}
struct ScrobbleWork {
    client: Option<&'static plx_plex::plex::Client>,
    final_report: Option<(String, i64, i64)>,
    report_th: Option<std::thread::JoinHandle<()>>,
    session: String,
    play_queue_id: String,
    play_queue_item_id: String,
    audio_stream_id: i64,
    subtitle_stream_id: i64,
    transcode_session: String,
    timeline_stop: Option<TimelineStopCompletion>,
}

impl ScrobbleWork {
    fn run(mut self) {
        // The progress reporter's last `playing` POST attempt must finish BEFORE this `stopped`
        // attempt begins. Waiting here keeps that ordering off the SDL thread; the stop generation
        // was announced before this worker was spawned, so a replacement reporter waits without
        // blocking the old one.
        if let Some(t) = self.report_th.take() {
            plx_base::task::join("timeline", t);
        }
        if let Some((rk, t_ms, d_ms)) = self.final_report.take() {
            let ok = {
                let _effect = TIMELINE_EFFECT.lock().unwrap_or_else(|e| e.into_inner());
                self.client.is_some_and(|c| {
                    c.timeline(&plx_plex::plex::TimelineReport {
                        rating_key: &rk,
                        state: plx_plex::plex::TimelineState::Stopped,
                        time_ms: t_ms,
                        duration_ms: d_ms,
                        session: &self.session,
                        play_queue_id: &self.play_queue_id,
                        play_queue_item_id: &self.play_queue_item_id,
                        audio_stream_id: self.audio_stream_id,
                        subtitle_stream_id: self.subtitle_stream_id,
                    })
                })
            };
            plx_base::eventlog::log(&format!(
                "timeline stopped t={}s/{}s ok={}",
                t_ms / 1000,
                d_ms / 1000,
                ok as i32,
            ));
        }
        // This is the semantic publication boundary: old reporter joined, then old stopped was
        // attempted in the common effect lane. Wake replacement reporters before the unrelated
        // encoder-stop request, whose latency must not delay their progress heartbeat.
        if let Some(stop) = self.timeline_stop.take() {
            stop.finish();
        }
        if !self.transcode_session.is_empty() {
            let ok = self
                .client
                .is_some_and(|c| c.transcode_stop(&self.transcode_session));
            plx_base::eventlog::log(&format!("transcode stopped ok={}", ok as i32));
        }
    }
}

/// The end-of-playback PMS work, moved OFF the main thread: the `state=stopped` timeline report
/// (which commits the server-side resume point and watched state) and the server-side transcode
/// stop. Replaces the inline `report_timeline` + `stop_transcode` pair in `engine::teardown`.
///
/// Both ran inline on the SDL
/// thread — two blocking PMS round trips, each bounded by `CONNECT_TIMEOUT_MS` + `SO_RCVTIMEO`
/// (~17 s), on **100% of real stops**. That was the largest guaranteed main-loop park left in the
/// engine, and strictly bigger than the rare in-flight-POST window at the joins above it.
///
/// Everything the worker needs is read HERE, on the main thread, and the two fields a stop retires
/// are cleared here too: the [`Session`] is a `static mut`, and what keeps it sound is that the main
/// thread is its only writer. The worker gets owned copies and touches none of it — the same
/// capture the demux thread's `acodec` does, and for the same reason.
pub fn scrobble_stop(
    ps: &mut PlaybackSession,
    final_report: Option<(String, i64, i64)>,
    report_th: Option<std::thread::JoinHandle<()>>,
) {
    let (logical_session, pq, pqi) = (sess(ps), pq_id(ps), pq_item_id(ps));
    let (aud, sub) = (cur_audio_sid(ps), cur_sub_sid(ps)); // the selection this playback reported under
    let tsession = take_active_encoder();
    let session = if tsession.is_empty() {
        logical_session
    } else {
        tsession.clone()
    };
    // The two fields THIS function retires (teardown clears the URL a few lines later, and that is
    // the whole of what a stop resets). A partial write rather than a whole-session reset because
    // the rest is still read after teardown returns — see `reset_session`'s doc for the reader that
    // would break.
    { let s = &mut *ps; {
        s.tsession.clear();
        s.cur_contract.remux = false;
    } };
    if final_report.is_none() && tsession.is_empty() && report_th.is_none() {
        return; // nothing to post and nobody to wait for
    }
    // The server this playback came FROM, not whichever one is current — the resume point and the
    // transcode session both live there, and by the time a stop runs the user may well have walked
    // back to a different source's Home.
    let client = cur_client(ps);
    // Serialise against a previous stop still in flight: these carry a position for a specific
    // item, and letting two race would let an older one land last. Normally free — the measured
    // baseline for a finished worker is 0 ms.
    drain_scrobble();
    let timeline_stop =
        (final_report.is_some() || report_th.is_some()).then(|| TIMELINE_STOP_FENCE.announce());
    let work = std::sync::Arc::new(std::sync::Mutex::new(Some(ScrobbleWork {
        client,
        final_report,
        report_th,
        session,
        play_queue_id: pq,
        play_queue_item_id: pqi,
        audio_stream_id: aud,
        subtitle_stream_id: sub,
        transcode_session: tsession,
        timeline_stop,
    })));
    let worker = work.clone();
    let join_generation = SCROBBLE_JOIN.reserve();
    let h = plx_base::task::spawn_small_keeping("scrobble", move || {
        let work = { worker.lock().unwrap_or_else(|e| e.into_inner()).take() };
        if let Some(work) = work {
            work.run();
        }
    });
    if let Some(handle) = h {
        SCROBBLE_JOIN.install(join_generation, handle);
    } else {
        // Thread refusal is extraordinarily rare, but dropping the old reporter handle and
        // opening the stop fence would recreate the exact new-before-old race. Pay the old
        // synchronous cost on this failure path and preserve ordering.
        let _block = plx_base::task::allow_blocking(
            const { &plx_base::task::BlockingLabel::new("scrobble stop (worker thread refused)") },
        );
        let work = { work.lock().unwrap_or_else(|e| e.into_inner()).take() };
        if let Some(work) = work {
            work.run();
        }
        SCROBBLE_JOIN.complete_spawn_refusal(join_generation);
    }
}

struct ScrobbleJoinState {
    generation: u64,
    completed: u64,
    spawn_pending: bool,
    joining: bool,
    handle: Option<(u64, std::thread::JoinHandle<()>)>,
}

struct ScrobbleJoin {
    state: std::sync::Mutex<ScrobbleJoinState>,
    changed: std::sync::Condvar,
}

impl ScrobbleJoin {
    const fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(ScrobbleJoinState {
                generation: 0,
                completed: 0,
                spawn_pending: false,
                joining: false,
                handle: None,
            }),
            changed: std::sync::Condvar::new(),
        }
    }

    /// Publish unfinished work before spawning it. A concurrent drain then waits for handle
    /// installation (or synchronous refusal completion) instead of seeing a false idle window.
    fn reserve(&self) -> u64 {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        debug_assert_eq!(state.completed, state.generation);
        debug_assert!(!state.spawn_pending && !state.joining && state.handle.is_none());
        state.generation = state
            .generation
            .checked_add(1)
            .expect("scrobble generation exhausted");
        state.spawn_pending = true;
        state.generation
    }

    fn install(&self, generation: u64, handle: std::thread::JoinHandle<()>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        debug_assert_eq!(state.generation, generation);
        debug_assert!(state.spawn_pending && state.handle.is_none());
        state.handle = Some((generation, handle));
        state.spawn_pending = false;
        self.changed.notify_all();
    }

    fn complete_spawn_refusal(&self, generation: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        debug_assert_eq!(state.generation, generation);
        state.completed = state.completed.max(generation);
        state.spawn_pending = false;
        self.changed.notify_all();
    }

    fn drain(&self) {
        loop {
            let (generation, handle) = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if state.completed >= state.generation {
                        return;
                    }
                    if !state.spawn_pending && !state.joining {
                        if let Some((generation, handle)) = state.handle.take() {
                            state.joining = true;
                            break (generation, handle);
                        }
                    }
                    state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
                }
            };
            plx_base::task::join("scrobble", handle);
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.completed = state.completed.max(generation);
            state.joining = false;
            self.changed.notify_all();
            // Another generation may have been reserved as soon as this one completed.
        }
    }
}

/// The final scrobble still in flight, with a shared completion barrier so every concurrent
/// drainer waits even after one of them has taken ownership of the JoinHandle.
static SCROBBLE_JOIN: ScrobbleJoin = ScrobbleJoin::new();

/// One ordered network-effect lane for progress publication. The route lease is revalidated only
/// after this lock is acquired, then `PLAYER_CONTROL` is released before I/O. Consequently an old
/// reporter either lands before the replacement or observes its stale epoch and sends nothing;
/// it can never validate first, stall, and land after the replacement report.
static TIMELINE_EFFECT: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct TimelineStopFenceState {
    announced: u64,
    completed: u64,
}

struct TimelineStopFence {
    state: std::sync::Mutex<TimelineStopFenceState>,
    changed: std::sync::Condvar,
}

impl TimelineStopFence {
    const fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(TimelineStopFenceState {
                announced: 0,
                completed: 0,
            }),
            changed: std::sync::Condvar::new(),
        }
    }

    fn announce(&'static self) -> TimelineStopCompletion {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.announced = state
            .announced
            .checked_add(1)
            .expect("timeline stop generation exhausted");
        TimelineStopCompletion {
            generation: state.announced,
            finished: false,
        }
    }

    fn announced(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .announced
    }

    fn wait(&self, required: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.completed < required {
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn complete(&self, generation: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if generation > state.completed {
            state.completed = generation;
            self.changed.notify_all();
        }
    }
}

static TIMELINE_STOP_FENCE: TimelineStopFence = TimelineStopFence::new();

struct TimelineStopCompletion {
    generation: u64,
    finished: bool,
}

impl TimelineStopCompletion {
    fn finish(mut self) {
        TIMELINE_STOP_FENCE.complete(self.generation);
        self.finished = true;
    }
}

impl Drop for TimelineStopCompletion {
    fn drop(&mut self) {
        if !self.finished {
            // Panic or refused worker must not strand every later reporter behind this stop.
            TIMELINE_STOP_FENCE.complete(self.generation);
        }
    }
}

/// Wait for pending [`scrobble_stop`] work to finish its ordered timeline/transcode attempts.
///
/// Production uses this before another stop, before Retry starts a replacement encoder, and at
/// `plex_run` exit. The exit wait matters because the process is about to die and a detached worker
/// dies with it; the other waits preserve ordering without putting routine BACK teardown on the
/// SDL thread.
pub fn drain_scrobble() {
    SCROBBLE_JOIN.drain();
}

use plx_base::cbuf::set as set_c; // shared fixed-C-buffer write (the session's HUD title/ctxline)

// ---- the QUALITY ceiling: what the USER has asked this playback to come in under -------------

pub fn forced_direct_play(ps: &PlaybackSession) -> bool {
    ps.direct_play_mode == DirectPlayMode::Forced
}

pub fn audio_track_direct_plays(ps: &PlaybackSession, codec: &str, channels: i64) -> bool {
    audio_direct_plays(ps.direct_play_mode, codec, channels)
}

fn apply_quality_choice(ps: &mut PlaybackSession, q: Quality) {
    if forced_direct_play(ps) { return; }
    // A later non-Auto pick supersedes an Auto restart that the pump has not consumed yet. If the
    // live worker really was adaptive, the route comparison below schedules the symmetric restart
    // which removes its watchdog; if it was still Manual, this cancellation avoids a stale Auto
    // reload after the checkmark has already moved back.
    if q != Quality::Auto {
        crate::player::cancel_adaptive_reload();
    }
    // A worker-side ABR commit is the current route. Reconcile it before comparing or replacing
    // anything, so a menu action cannot make a decision against the bootstrap ceiling.
    let live_hls = sync_active_hls_to_session(ps);
    // An exact direct/remux candidate survives both fixed-rung and HLS transitions. An explicit
    // Original pick is an instruction to restore that native declaration now, not merely remove a
    // bitrate cap and start another encoder. Leave the current route intact until the pump owns
    // the same-position codec-changing reload.
    if q == Quality::Original && is_transcoding(ps) && ps.auto_original.is_some() {
        crate::player::log("quality: Original picked — restoring the native source");
        crate::player::request_original_recovery(ps);
        return;
    }
    if q == Quality::Auto && live_hls.is_some() {
        // The bytes/rung stay exactly where they are, but the running worker may have been born
        // while the picker was Manual Original (notably after an Original 500 rollback). Its HLS
        // controller then captured no Original candidate. Recreate only the worker at this exact
        // route so Auto means the same controller contract regardless of how HLS was reached.
        crate::player::log(
            "quality: Auto picked — retaining live HLS and refreshing its adaptive contract",
        );
        crate::player::request_adaptive_reload(ps);
        return;
    }
    let location = plx_plex::plex::client_for(cur_sid(ps)).and_then(|client| client.link());
    // A direct Remote already on screen is itself stronger evidence than a second prefix fetch:
    // selecting Auto must not start an encoder under a movie which is currently arriving as
    // Original. A fresh Remote play is measured in `build_stream`; an existing transcode has no
    // such original-file observation and stays on HLS.
    //
    // Link class cannot create a source candidate. Local normally needs no throughput proof, but
    // it still needs Original to be technically possible. `build_stream` records that fact as
    // `auto_original`: `None` means the source codec/container/audio combination already failed
    // feasibility. Treating Local alone as sufficient here turns a fixed-rung AV1 transcode (a set
    // whose table lists no AV1 decoder, or a source over its row) into progressive MKV when the
    // user returns to Auto, so no HLS controller is rebuilt.
    let original_feasible = ps.auto_original.is_some();
    let auto_original = q == Quality::Auto
        && original_feasible
        && (location == Some(plx_plex::plex::probe::Location::Local)
            || (location == Some(plx_plex::plex::probe::Location::Remote)
                && (!is_transcoding(ps) || is_remux(ps))));
    let adaptive = auto_uses_hls(q, auto_original);
    let delivery = if adaptive {
        plx_plex::plex::TranscodeDelivery::FixedHls {
            seconds_per_segment: 2,
        }
    } else {
        plx_plex::plex::TranscodeDelivery::ProgressiveMkv
    };
    let starting_rung = adaptive.then(|| {
        crate::abr::hls_reentry_rung(
            cur_ceiling(ps).and_then(crate::abr::Rung::from_ceiling),
            auto_prior(ps),
            &auto_catalog(ps),
            &crate::abr::AbrPolicy::measured(),
        )
    });
    let ceiling = starting_rung
        .map(crate::abr::Rung::ceiling)
        .or_else(|| q.ceiling());
    if cur_rk(ps).is_empty() {
        return;
    }
    let route_unchanged = cur_ceiling(ps) == ceiling && cur_delivery(ps) == delivery;
    let watched_before = ps.cur_auto_original_watched;
    let (kbps, w, h) = ps.cur_src;
    let admits = quality_policy(q, auto_original, kbps, w, h).direct_play;
    { let s = &mut *ps; {
        s.cur_contract.ceiling = ceiling;
        s.cur_contract.delivery = delivery;
        // Supervision does not depend on where the server is: `auto_original` already says Auto
        // is going to run Original, and that is the whole question the watchdog asks. See the
        // field. It was assigned to an `auto_original_watched` binding first, which named nothing
        // the right-hand side did not already say — a leftover from the `auto_remote_original`
        // refactor, where the two really were different.
        s.cur_auto_original_watched = auto_original;
        if matches!(delivery, plx_plex::plex::TranscodeDelivery::FixedHls { .. }) {
            s.cur_contract.remux = false;
        }
    } };
    // The bytes, URL and decoder declaration may be identical while the worker contract is not.
    // `engine::start_bufferfeed` captures `auto_original_watch()` BY VALUE at spawn, so toggling
    // Manual Original <-> Auto Original underneath the existing thread can never start/stop the
    // controller. Replace only that worker/pipeline at the same movie position; this is not a PMS
    // rendition change and must not go through the transcode-refresh path.
    if route_unchanged && watched_before != auto_original {
        crate::player::log(&format!(
            "quality: {} picked — restarting the current source to {} adaptive supervision",
            q.label(),
            if auto_original { "enable" } else { "disable" },
        ));
        crate::player::request_adaptive_reload(ps);
        return;
    }
    if route_unchanged {
        commit_in_place_route_projection(ps, true);
        return;
    }
    if admits && !is_transcoding(ps) {
        // The bytes already satisfy the new ceiling, but the demux worker captured adaptive
        // supervision by value. Auto <-> Manual therefore still needs a same-URL worker restart;
        // otherwise the old Auto watchdog can publish a fallback after the picker says Manual.
        if watched_before != auto_original {
            crate::player::log(&format!(
                "quality: {} picked — keeping direct bytes and refreshing adaptive supervision",
                q.label(),
            ));
            crate::player::request_adaptive_reload(ps);
        } else {
            commit_in_place_route_projection(ps, true);
        }
        return; // the picture on screen already satisfies the new rung
    }
    crate::player::log(&format!(
        "quality: {} picked — source {kbps}kbps {w}x{h}; re-transcoding this playback{}",
        q.label(),
        starting_rung
            .map(|rung| format!(" at {}kbps HLS", rung.kbps()))
            .unwrap_or_default(),
    ));
    // The pick is a new open of the route it names, whatever the pump makes of the refresh: say so
    // now, or a failure of that open reads as one of the route the playback had (S5a left this
    // path with no `DeliveryRequested`, so `attempted_delivery` fell back to the installed route).
    crate::player::report::note_delivery_requested_for(
        playback_trace_generation(),
        if adaptive {
            crate::player::report::DeliveryClass::Hls
        } else {
            crate::player::report::DeliveryClass::Transcode
        },
        starting_rung.map_or_else(
            || crate::player::report::selected_quality_class(q),
            crate::player::report::rung_quality_class,
        ),
        crate::player::report::DeliveryReason::QualityPick,
        crate::player::report::AttemptOrigin::QualityChange,
    );
    crate::player::request_transcode_refresh(ps);
}

/// Select the audio + subtitle streams server-side for the current part before a
/// transcode. The transcoder encodes the part's SELECTED audio and, when a subtitle id is
/// non-zero, BURNS that subtitle (query-param subtitleStreamID does NOT suppress a
/// default-selected sub, only the PUT does). The direct-play profile advertises text and
/// client-rendered bitmap codecs; burn is still the remux/re-encode path when we PUT a
/// positive subtitle id. We PUT subtitleStreamID=0 to keep subs OFF (no burn), or the chosen
/// id to burn it; audioStreamID only when the user switched (else keep default).
///
/// Never `0` for a subtitle the app draws itself: PMS discards a DOWNLOADED subtitle the moment the
/// selection moves off it (docs/pms-api.md, 2026-10-06), so `0` means the viewer chose Off or nothing
/// is selected; "do not burn" is the transcode wire's `subtitles=none` alone.
///
/// `sid` names the server that owns `part` — the resolve worker passes the id it was given, and the
/// in-playback callers pass [`cur_sid`]. A `Part.id` is server-local, so a PUT sent to the wrong
/// one either 404s or, worse, re-selects streams on a stranger's part that happens to share the
/// number.
pub(super) fn put_selection(_off: &plx_base::task::OffFrame, sid: ServerId, part: i64, aud: i64, sub: i64) {
    if part <= 0 {
        return;
    }
    let c = match plx_plex::plex::client_for(sid) {
        Some(c) => c,
        None => return,
    };
    let st = c.select_streams(&plx_plex::plex::StreamSelection {
        part_id: part,
        audio_stream_id: aud,
        subtitle_stream_id: sub,
    });
    crate::player::log(&format!(
        "select streams: part={part} audio={aud} sub={sub} -> HTTP {st}"
    ));
}

struct QueuedSelection {
    sid: ServerId,
    part: i64,
    aud: i64,
    sub: i64,
}

/// The pending selection PUTs and whether a worker is draining them. One lock covers both, so the
/// worker deciding to exit and a `queue_put_selection` deciding whether to spawn one cannot
/// interleave: either the push lands before the worker's final empty check, or it sees the worker
/// already gone and spawns its own.
struct SelectionQueue {
    pending: std::collections::VecDeque<QueuedSelection>,
    worker_running: bool,
}

static SELECTION_QUEUE: std::sync::Mutex<SelectionQueue> =
    std::sync::Mutex::new(SelectionQueue { pending: std::collections::VecDeque::new(), worker_running: false });

fn selection_queue() -> std::sync::MutexGuard<'static, SelectionQueue> {
    SELECTION_QUEUE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Same PUT as [`put_selection`], run on a worker instead of the frame thread
/// (`commit_audio_selection`/`commit_subtitle_selection` are both reached from
/// `app/playback.rs::commit_track` inside the run-loop's `FrameScope`). Requests are drained
/// strictly FIFO by one serial worker at a time, never in parallel — PMS applies whichever PUT it
/// receives last, so two picks made in quick succession (audio, then subtitle) must land on the
/// server in the order the viewer made them, not in whatever order two independent threads happen
/// to finish their requests.
pub(super) fn queue_put_selection(sid: ServerId, part: i64, aud: i64, sub: i64) {
    if part <= 0 {
        return;
    }
    let spawn = {
        let mut queue = selection_queue();
        queue.pending.push_back(QueuedSelection { sid, part, aud, sub });
        !std::mem::replace(&mut queue.worker_running, true)
    };
    if spawn {
        spawn_selection_worker();
    }
}

fn spawn_selection_worker() {
    let spawned = plx_base::task::spawn_off_frame("put-selection", |off| loop {
        let req = {
            let mut queue = selection_queue();
            let Some(req) = queue.pending.pop_front() else {
                // Nothing left: retire under the same lock the push takes, so a racing
                // `queue_put_selection` either pushed before this check or spawns a fresh worker.
                queue.worker_running = false;
                break;
            };
            req
        };
        put_selection(off, req.sid, req.part, req.aud, req.sub);
    });
    if !spawned {
        // The OS refused the thread. Drop the running flag so a later `queue_put_selection` gets a
        // chance to retry, rather than leaving every future selection stranded in the queue behind
        // a flag nobody will ever clear.
        selection_queue().worker_running = false;
    }
}

/// The queued next episode. Main-thread only, and — like `metadata::playing()` (via
/// `MetadataView`, which hands out a `&'a` borrow of the owner's state) — it hands out a
/// reference the Up Next control reads across a frame, so `apply_plan` (main thread) staying its
/// only writer is what keeps that reference sound. A caller that STARTS the next episode must
/// clone first: `request_play` clears this before the new plan lands.
pub fn up_next(ps: &PlaybackSession) -> Option<&UpNext> {
    ps.up_next.as_deref()
}

/// The current playback's queue rows, in queue order, the row now playing among them — locate it
/// with `plex::queue_index_of(rows, pq_item_id().parse().unwrap_or(0), &cur_rk())`, which is the
/// ONE implementation of the identity rule (item id, rating key as the fallback). Empty until a
/// plan lands, and whenever the queue POST failed.
///
/// MAIN THREAD ONLY. Unlike [`up_next`] this lends the rows to a closure instead of handing out a
/// `&'static`, and that is deliberate: `request_play` FREES this Vec as its first act, so a
/// borrowed row used to start playback would be read after free — exactly the aliasing bug
/// `request_play_up_next`'s by-value signature exists to make unrepresentable. A caller that wants
/// to keep a row past the call clones it out (`with_queue(|q| q.get(i).cloned())`); the borrow
/// checker cannot police a `&'static`, but it does police this.
#[allow(dead_code)] // nothing reads the rows yet — the queue overlay that draws them is its own batch
pub fn with_queue<R>(ps: &PlaybackSession, f: impl FnOnce(&[plx_plex::plex::QueueRow]) -> R) -> R {
    f(&ps.queue)
}

/// Trace generation owned by the plan that is actually installed. It deliberately remains the
/// outgoing generation while the next plan resolves, because that engine is still alive; its
/// workers carry the same token and are ignored by the newly reset report trace.
static ACTIVE_TRACE_GENERATION: AtomicU32 = AtomicU32::new(0);

pub fn playback_trace_generation() -> u32 {
    ACTIVE_TRACE_GENERATION.load(Ordering::SeqCst)
}

/// The server an item on a browsing surface came from. MAIN THREAD.
///
/// Today every surface is drawn from the CURRENT server, so this is `plex::current_server()` — and
/// reading it HERE, on the main thread, at the instant the user pressed Play, is the entire point:
/// from this line on the id travels by value and nothing downstream re-resolves it. When the stored
/// rows carry their own server (the shared-server data-model step), each call site passes `m.sid` /
/// `d.sid` instead and this helper goes away; it exists so there is exactly ONE line to change.
pub fn surface_sid() -> ServerId {
    plx_plex::plex::current_server()
}

/// **Tune a live channel** (a Dispatcharr stream): `tune`'s channel in its lineup. There is no
/// server resolve, so nothing runs on a worker. The URL and its Load declaration are installed at
/// once, as `dispatcharrtv-playurl` does, and [`play_pending`] stays false, so the start ritual's
/// `prepare_start` starts the Load directly. The HUD title is the channel, and the line under it
/// what airs at `now` (Unix seconds).
///
/// Refused, leaving everything as it was, while a PMS or native route transition owns the player
/// (the phases [`request_play`] refuses in), for a channel with no stream, and for a declaration
/// the television cannot be given.
pub fn request_live(ps: &mut PlaybackSession, mut tune: super::LiveTune, now: i64) -> bool {
    let Some(channel) = tune.channel().filter(|c| c.tunable()) else {
        return false;
    };
    if route_transition_owns_player() {
        crate::player::log("live: a route transition still owns the player; refusing the tune");
        return false;
    }
    let (vcodec, acodec) = tune.codecs.clone().unwrap_or_else(|| (tune.lineup.vcodec.clone(), tune.lineup.acodec.clone()));
    if !set_stream_declaration(ps, &vcodec, &acodec, 0.0, plx_data::metadata::Dovi::NONE, false) {
        return false;
    }
    let url = channel.url.clone();
    let title = channel.name.clone();
    let ctx = channel.on_at(now).map(|p| p.title.clone()).unwrap_or_default();
    { let s = &mut *ps; {
        s.request = None;
        s.requested_resume_ns = 0;
        s.play_verdict = None;
        s.resolve_failed = false;
        // SAFETY: as in `request_play_inner`, the lengths are the arrays' own.
        unsafe {
            set_c(s.title.as_mut_ptr(), s.title.len(), &title);
            set_c(s.ctxline.as_mut_ptr(), s.ctxline.len(), &ctx);
        }
    } };
    crate::player::log(&format!("live: tune #{} v={vcodec} a={acodec}", tune.index));
    set_live_url(ps, &url);
    tune.at = now;
    ps.tune = Some(tune);
    true
}

/// A PMS or native transaction is mid-flight: the phases [`begin_playback_request`] refuses in.
fn route_transition_owns_player() -> bool {
    let control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    matches!(
        control.phase,
        ControlPhase::Preparing(_)
            | ControlPhase::Prepared(_)
            | ControlPhase::Starting(_, _)
            | ControlPhase::StagingUser(_)
            | ControlPhase::Completing(_)
    )
}

/// The replacement has shown a frame; from now on the live playhead, including a later backward
/// seek, is the only truthful retry position.
pub fn confirm_resume_presented(ps: &mut PlaybackSession) {
    if ps.requested_resume_ns > 0 {
        { let s = &mut *ps; s.requested_resume_ns = 0 };
    }
}

/// The server an item's ids belong to: its own when it has one, else the browsed surface.
///
/// A row's `sid` is `UNSET` only before any server was registered (and in host tests, which build
/// rows by literal). Falling back to the surface there keeps the single-server app exactly as it
/// was, while never letting an item that DOES name its server be resolved against another.
pub fn item_sid(sid: ServerId) -> ServerId {
    if sid.is_set() {
        sid
    } else {
        surface_sid()
    }
}

/// Leaving the player retires a refusal verdict with it.
pub fn cancel_play(ps: &mut PlaybackSession) {
    clear_play_verdict(ps);
}

/// **Is the live session the Original remux's SHAPE**, whatever subtitle is on screen: a standing
/// Original candidate on Auto/Original quality, no fixed rung, progressive MKV, video still
/// copyable. [`plain_rebuild_is_remux`] adds the subtitle term; [`side_subs_allowed`] adds who
/// draws it.
fn original_remux_shape(ps: &PlaybackSession) -> bool {
    enhancement_quality()
        && ps.auto_original.is_some()
        && ps.cur_contract.ceiling.is_none()
        && cur_delivery(ps) == plx_plex::plex::TranscodeDelivery::ProgressiveMkv
        && !is_no_video_copy(ps)
        // A direct-played Dolby Vision Profile 5 never set `no_video_copy` (the declaration made
        // the direct play right); a copy of it, one container down, carries no declaration.
        && ps.auto_original.as_ref().is_some_and(|c| !c.dovi.base_layer_unusable())
}

/// The largest Part (whole-file bitrate, kbps) the app reads a second time beside a remux. The
/// reader's scan reads the Part's own bytes at about this rate, so a heavy Part would compete with
/// the remux for the same link; Part sizes above it keep the server's burn.
const SIDE_MAX_PART_KBPS: i64 = 30_000;

/// **May the app read an EMBEDDED track itself** — from a second, subtitle-only demux of the film's
/// original Part (`player::subside`) — so the server need not burn it? Only on a LOCAL link (a
/// relay or remote link would carry two reads of the film), with a real server Part to read (not
/// a fixture or a pinned URL), and a known, moderate Part bitrate: `0` means nobody measured it
/// and burns by design. A sidecar needs none of this (the app fetches it on its own).
fn side_reader_admitted(ps: &PlaybackSession) -> bool {
    side_reader_inputs_admit(
        cur_client(ps).is_some_and(|c| c.link() == Some(plx_plex::plex::probe::Location::Local)),
        ps.auto_original.as_ref().map_or("", |c| c.probe_part.as_str()),
        ps.cur_transport_kbps,
    )
}

/// The facts [`side_reader_admitted`] reads, as plain values, so the cold start (which has no
/// session yet) asks the same question.
pub(super) fn side_reader_inputs_admit(local_link: bool, part: &str, part_kbps: i64) -> bool {
    local_link && part.starts_with('/') && (1..=SIDE_MAX_PART_KBPS).contains(&part_kbps)
}

/// [`side_subs_allowed`] for a prospective pick: a subtitle is chosen, the app can draw it, no
/// failure has retired the app's drawing for this playback, the route is the Original remux's
/// shape, and nothing but the app has to read the track (a sidecar, or an admitted reader).
fn side_subs_allowed_for(ps: &PlaybackSession, sid: i64, drawable: bool, sidecar: bool) -> bool {
    sid != 0
        && drawable
        && !ps.side_subs_refused
        && original_remux_shape(ps)
        && (sidecar || side_reader_admitted(ps))
}

/// **May the app draw the selected subtitle over a remux** instead of the server burning it?
fn side_subs_allowed(ps: &PlaybackSession) -> bool {
    side_subs_allowed_for(ps, ps.cur_sub_sid, ps.cur_sub_client_drawable, ps.cur_sub_sidecar)
}

// ---- issue #266: the live audio-enhancement state machine ------------------------------------
//
// The enhancement decorates the Original route: turning it on makes a Direct/Remux playback an
// enhanced remux, turning it off (or losing the offer — a subtitle, a track the server cannot
// analyse) returns it to the frozen `AutoOriginalCandidate`. Every mid-play change funnels through
// ONE reconcile which only ever queues `UserRouteIntent::Retranscode`; the claim then asks
// `enhancement_step` what "Retranscode" means NOW. Deciding at claim rather than at the click is
// what lets several quick picks and toggles coalesce into the route their final state describes,
// with the merge table left exactly as it was.

/// The family of the route playing NOW (see [`RouteFamily`]). Mid-play code and the menu pass
/// this; the resolve and recovery pass the family of the route they are building instead.
pub(super) fn live_family(ps: &PlaybackSession) -> RouteFamily {
    if !is_transcoding(ps) {
        RouteFamily::Direct
    } else if ps.cur_contract.remux
        && ps.cur_contract.delivery == plx_plex::plex::TranscodeDelivery::ProgressiveMkv
    {
        RouteFamily::Remux
    } else {
        RouteFamily::Other
    }
}

/// The offer's inputs, read from the live session alone — no metadata store: the carried track's
/// capability was frozen into `cur_audio` when it was picked, and the candidate carries its own
/// DV facts.
pub(super) fn facts(ps: &PlaybackSession) -> EnhancementFacts<'_> {
    EnhancementFacts {
        pass: plx_plex::plex::serverinfo::subscription_of(cur_sid(ps)),
        base: ps.auto_original.as_ref(),
        carried: ps.cur_audio.as_ref(),
        subtitle_effect: subtitle_effect_of(ps),
        refused: ps.cur_enhancement == EnhancementOutcome::Refused,
    }
}

/// What the picked subtitle does to the route: nothing, a sidecar the client draws itself, or an
/// embedded track the server would have to burn to keep. Two session fields, read in place.
fn subtitle_effect_of(ps: &PlaybackSession) -> SubtitleEffect {
    match ps.cur_sub_sid {
        0 => SubtitleEffect::None,
        _ if ps.cur_sub_sidecar || side_subs_allowed(ps) => SubtitleEffect::Sidecar,
        _ => SubtitleEffect::Embedded,
    }
}

/// **Who draws the subtitle on screen right now** — the one route fact the Subtitles menu reads
/// (Style and Timing apply only to a subtitle the app draws) and the one the player draws by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubtitlePresenter {
    /// No subtitle is selected.
    None,
    /// The app draws it (direct play): the viewer's Style and Timing reach it.
    Client,
    /// The app draws it OVER a remux: the server copies the video and converts the audio, the
    /// subtitle is not in the stream at all, and Style and Timing reach it as on direct play.
    ClientOverRemux,
    /// The server burns it into the picture: nothing the app has can reach it.
    ServerBurn(BurnReason),
}

impl SubtitlePresenter {
}

/// Why the server is the one drawing the subtitle — worded for the viewer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BurnReason {
    /// The server is converting the audio (a remux, or the Original-quality burn), and a remux
    /// stream cannot carry a subtitle.
    AudioConversion,
    /// Any other burn: a quality-limited re-encode, a picture subtitle that must be burned.
    Quality,
}

/// **The live [`SubtitlePresenter`]**, from the committed selection (`cur_sub_sid`, written for
/// every route by `commit_subtitle_selection`) and the live route. A transcode burns whatever is
/// selected and the client then draws nothing (`player_hud::draw_subtitles` is silenced by
/// [`subtitles_burned`] the same way) — except a sidecar, or an admitted embedded track, over a remux, which the app draws
/// ([`SubtitlePresenter::ClientOverRemux`]); direct play draws it in the app. The reason is the
/// audio conversion when the live family is a remux or the burn is the enhancement's own or the
/// Original-quality one ([`enhancement_family`] reads exactly those as `Remux`), else quality.
pub fn subtitle_presenter(ps: &PlaybackSession) -> SubtitlePresenter {
    if ps.cur_sub_sid == 0 {
        SubtitlePresenter::None
    } else if !is_transcoding(ps) {
        SubtitlePresenter::Client
    } else if ps.cur_contract.remux
        && ps.cur_contract.delivery == plx_plex::plex::TranscodeDelivery::ProgressiveMkv
        && side_subs_allowed(ps)
    {
        SubtitlePresenter::ClientOverRemux
    } else if enhancement_family(ps) == RouteFamily::Remux {
        SubtitlePresenter::ServerBurn(BurnReason::AudioConversion)
    } else {
        SubtitlePresenter::ServerBurn(BurnReason::Quality)
    }
}

/// **Does the server burn the selected subtitle into the picture?** The gate every client-side
/// draw is silenced by: a transcode burns whatever is selected, except a remux the app draws the
/// subtitle over ([`SubtitlePresenter::ClientOverRemux`]).
pub fn subtitles_burned(ps: &PlaybackSession) -> bool {
    is_transcoding(ps)
        && subtitle_presenter(ps) != SubtitlePresenter::ClientOverRemux
        // Dev trigger `dispatcharrtv-subside` only: the reader draws an embedded track over a plain
        // remux while the route has NO subtitle selected (so the presenter says `None`, not
        // `ClientOverRemux`). A selected subtitle is the presenter's alone to answer, so a refused
        // or failed reader (`ServerBurn`) is never silenced by a reader that is still shutting down.
        && !(ps.cur_sub_sid == 0 && crate::player::subside::active())
}

/// **The side reader gave up** (every reopen failed, the Part has no such track, or it could not
/// start): the app's own drawing of this playback's subtitle is refused for the rest of the item
/// and the server's burn is all that is left, so the stream is rebuilt as the burn. The presenter
/// then reads `ServerBurn(AudioConversion)` and the menu's dimmed Style/Timing rows say why. Does
/// nothing when the reader was not drawing the selected subtitle (the dev trigger's run).
pub fn side_subtitles_failed(ps: &mut PlaybackSession) {
    // Only the reader's own track: a sidecar the app fetches itself is not what failed, so a
    // failure raised for the reader the viewer has since left says nothing about it.
    if ps.side_subs_refused
        || ps.cur_sub_sidecar
        || subtitle_presenter(ps) != SubtitlePresenter::ClientOverRemux
    {
        return;
    }
    crate::player::log("subtitles: the side reader failed; the server burns the subtitle");
    let _edit = begin_user_contract_boundary();
    ps.side_subs_refused = true;
    if reconcile_enhancement(ps, true) {
        return;
    }
    crate::player::request_transcode_refresh(ps);
}

/// What the side subtitle reader needs to read the film's original Part beside a plain remux.
pub struct SideReaderTarget {
    pub sid: plx_plex::plex::ServerId,
    /// The Part's key (path), never logged.
    pub part: String,
    /// The LIVE transcode session's identifier, never logged. The reader's request carries a fresh
    /// identifier derived from it; the live one itself is refused by PMS while the remux runs.
    pub session: String,
    /// The Part's whole-file bitrate, for the log line.
    pub part_kbps: u32,
    /// The 0-based position of the track to draw among the Part's subtitle streams.
    pub ordinal: i32,
}

/// The side reader's inputs for this playback, or `None` unless the live route is a plain remux of
/// a real playback on which the app draws an embedded track
/// ([`SubtitlePresenter::ClientOverRemux`], not a sidecar — the app fetches that itself), or the
/// dev trigger is armed with no subtitle selected in the route.
pub fn side_reader_target(ps: &PlaybackSession) -> Option<SideReaderTarget> {
    if live_family(ps) != RouteFamily::Remux {
        return None;
    }
    let ordinal = if ps.cur_sub_sid == 0 {
        crate::player::subside::dev_armed_ordinal()?
    } else if !ps.cur_sub_sidecar
        && ps.cur_sub_ordinal >= 0
        && subtitle_presenter(ps) == SubtitlePresenter::ClientOverRemux
    {
        ps.cur_sub_ordinal
    } else {
        return None;
    };
    let request = ps.request.as_ref()?;
    if request.part.is_empty() || ps.tsession.is_empty() {
        return None;
    }
    Some(SideReaderTarget {
        sid: ps.cur_sid,
        part: request.part.clone(),
        session: ps.tsession.clone(),
        part_kbps: u32::try_from(ps.cur_transport_kbps).unwrap_or(0),
        ordinal,
    })
}

/// **Is the live route the enhancement's own Burn (M7)?** A Burn forces `remux: false` to get PMS
/// to actually re-encode the video (burning text into pixels is not a codec copy), which is
/// exactly the contract shape of an ordinary `Other`-family re-encode picked for some unrelated
/// reason (a fixed rung, HLS, a relay). The two are told apart by the contract itself: only the
/// enhancement asks for a video-copying, no-ceiling, progressive-MKV re-encode carrying non-empty
/// `audio` params, whatever the server graded it (`Applied` or `Unverified`) and whether or not a
/// subtitle is still selected — a subtitle turned Off mid-play leaves this contract standing until
/// the rebuild lands, and that rebuild must keep the params.
pub fn live_is_own_burn(ps: &PlaybackSession) -> bool {
    ps.cur_contract.audio.any()
        && ps.cur_contract == enhanced_remux_contract(ps.cur_contract.audio, true)
}

/// **Is the live route Original quality with an embedded subtitle burned in?** The shape the
/// enhancement's own Burn asks for ([`enhanced_remux_contract`] with `force_burn`), standing with
/// or without the DSP params: a standing Original candidate on Auto/Original, a progressive MKV
/// re-encode with no ceiling and the video still copyable, and an embedded subtitle on screen —
/// the subtitle is the ONLY reason the video is re-encoded, nothing is downscaled. A fixed rung
/// (ceiling), HLS (`FixedHls`), a relay, a source that forbids a video copy (declared Dolby
/// Vision) or a sidecar the client draws itself all fail one of these terms and stay an
/// unrelated `Other`-family re-encode.
fn live_is_original_burn(ps: &PlaybackSession) -> bool {
    enhancement_quality()
        && ps.auto_original.is_some()
        && is_transcoding(ps)
        && !ps.cur_contract.remux
        && ps.cur_contract.delivery == plx_plex::plex::TranscodeDelivery::ProgressiveMkv
        && ps.cur_contract.ceiling.is_none()
        && !ps.cur_contract.no_video_copy
        && ps.cur_sub_sid != 0
        && !ps.cur_sub_sidecar
}

/// **The family the enhancement's own bookkeeping should read the live route as.** Identical to
/// [`live_family`] except a live Burn — which is wire-shaped `Other` (`remux: false`) — reads as
/// `Remux`: both the enhancement's own Burn and a plain Original-quality burn with no DSP params
/// (`live_is_original_burn`). Every predicate that asks "is the enhancement's route still
/// standing" must use this, not `live_family`, or an active Burn would appear "not offered" the
/// instant it took effect and get silently released back to the plain candidate. The track picks
/// (`commit_audio_selection`, `commit_subtitle_selection`) use it too; only `legacy_action`
/// still calls `live_family`, where a Burn genuinely needs re-encode-shaped handling.
fn enhancement_family(ps: &PlaybackSession) -> RouteFamily {
    if live_is_own_burn(ps) || live_is_original_burn(ps) {
        RouteFamily::Remux
    } else {
        live_family(ps)
    }
}

/// The [`EnhancementRoute`] a live `Retranscode` would ask for right now — `None` when the
/// enhancement is not offered at all. `want_live`/`audio_enhancements_offered_live` collapse this
/// to a bool for the menu's toggle; `enhancement_step` needs the flavour too, since a subtitle
/// change can flip Remux↔Burn without changing the boost/loudness preference at all.
fn want_route_live(ps: &PlaybackSession) -> Option<EnhancementRoute> {
    if !enhancement_quality() {
        return None;
    }
    enhancement_route(&facts(ps), enhancement_family(ps))
}

/// Whether the quality preference admits an Original-family route at all — Auto (which may run
/// Original) or Original itself. A fixed rung is a bitrate cap, and the enhanced route is not.
fn enhancement_quality() -> bool {
    matches!(quality(), Quality::Auto | Quality::Original)
}

/// The shape an enhanced Original takes: a codec-preserving progressive-MKV remux, video copied,
/// no ceiling — or, when `force_burn` (M7's [`EnhancementRoute::Burn`]), the same delivery with
/// `remux: false` so PMS actually re-encodes the video and burns the requested subtitle into it
/// (a codec copy cannot alter pixels). `no_video_copy` stays `false` in both cases: I7 keeps a
/// declared-DV source (the one `no_video_copy` reason) out of the offer entirely before this is
/// ever reached.
fn enhanced_remux_contract(
    audio: plx_plex::plex::AudioEnhancements,
    force_burn: bool,
) -> plx_plex::plex::EncodeContract {
    plx_plex::plex::EncodeContract {
        remux: !force_burn,
        delivery: plx_plex::plex::TranscodeDelivery::ProgressiveMkv,
        no_video_copy: false,
        ceiling: None,
        audio,
    }
}

/// What a claimed `Retranscode` must do about the enhancement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnhancementStep {
    /// The enhancement has nothing to change: today's rebuild (or the displaced pick's own
    /// legacy reload) runs.
    NotInvolved,
    /// Rebuild as this remux — on with new params, or a remux candidate's plain remux.
    Remux(plx_plex::plex::EncodeContract),
    /// The enhancement is no longer wanted and the candidate direct-plays: return to it.
    ReleaseToDirect,
}

/// **Pure over session state.** See the table in the plan's §3: a wanted enhancement that differs
/// from the applied one (in preference OR in route flavour — M7's Remux↔Burn) builds the enhanced
/// remux; an applied one no longer wanted returns to the candidate (direct, or its plain remux);
/// everything else — including no candidate at all — is not the enhancement's business.
pub fn enhancement_step(ps: &PlaybackSession) -> EnhancementStep {
    // Under a fixed rung the rebuild is the quality refresh's own (a capped re-encode, which
    // drops the params by family); a release to the uncapped candidate would erase that cap.
    if !enhancement_quality() {
        return EnhancementStep::NotInvolved;
    }
    let want_route = want_route_live(ps);
    let want = desired_audio(crate::player::audio_enhancements(), want_route.is_some());
    let applied = ps.cur_contract.audio;
    let want_burn = matches!(want_route, Some(EnhancementRoute::Burn));
    let applied_burn = live_is_own_burn(ps);
    // A subtitle change while enhanced can leave the boost/loudness preference untouched and still
    // need a rebuild: turning off a burned subtitle must drop the route from Burn back to a plain
    // enhanced remux (M7), which `want == applied` alone would miss.
    if want.any() && (want != applied || want_burn != applied_burn) {
        return EnhancementStep::Remux(enhanced_remux_contract(want, want_burn));
    }
    if applied.any() && !want.any() {
        return match ps.auto_original.as_ref() {
            Some(candidate) if candidate.feeds_part() => EnhancementStep::ReleaseToDirect,
            // An embedded subtitle still on screen is kept by the plain burn, never dropped by a
            // plain remux (which cannot carry it, M4/M7).
            Some(_) => EnhancementStep::Remux(enhanced_remux_contract(
                plx_plex::plex::AudioEnhancements::NONE,
                subtitle_effect_of(ps) == SubtitleEffect::Embedded,
            )),
            None => EnhancementStep::NotInvolved,
        };
    }
    EnhancementStep::NotInvolved
}

/// **Bring the route in line with the enhancement preference.** Returns `true` when it queued the
/// `Retranscode` that will do so — the caller then skips its own legacy reload, and
/// `legacy_reloads` records that it did (`displaced_pick`), so a claim that cannot honour the
/// enhancement still performs the pick's own reload. A pending Original trial owns the route:
/// the reconcile is deferred to whichever route that trial settles on.
pub fn reconcile_enhancement(ps: &PlaybackSession, legacy_reloads: bool) -> bool {
    if enhancement_step(ps) == EnhancementStep::NotInvolved {
        return false;
    }
    queue_user_route_intent(ps, UserRouteIntent::Retranscode, legacy_reloads);
    true
}

/// **Test-only session builder for the Audio tab's enhancement rows (issue #266 PR 4).** Every
/// `PlaybackSession` field is private to this module by design (see `PlaybackSession::IDLE`'s own
/// doc), so `appkit::track_menu`'s tests — which live outside `route` and see only this module's
/// `pub` surface — cannot build one field-by-field the way this module's own tests do.
/// This is the one door: it drives every input `audio_enhancements_offered_live`/
/// `displayed_audio_enhancements` read (I1-I7), registers a throwaway server carrying the given
/// Plex Pass tristate, and returns the session plus that server's id so the caller can
/// `plx_plex::plex::reset_servers_for_test()` when done. Caller holds `plx_base::testlock::serial()`.
#[cfg(any(test, feature = "test-support"))]
pub struct EnhTestFixture {
    pub pass: plx_plex::plex::serverinfo::Subscription,
    /// `None` = still direct-playing (Direct family). `Some(true)` = a progressive-MKV remux
    /// (Remux family — a plain Original remux, or an already-applied enhancement). `Some(false)`
    /// = any other transcode shape (Other family — HLS, a fixed rung, a relay: I5 excludes all of
    /// them identically, so one shape stands for the group).
    pub remux: Option<bool>,
    /// `auto_original` present at all — `false` reproduces I5's forced-direct-play/fixed-rung/
    /// relay/non-Original-MDE exclusion, which is exactly "no candidate was ever computed".
    pub base_present: bool,
    /// The base route's own Dolby Vision declaration (I7).
    pub dv_declared: bool,
    /// The base route's own [`plx_data::metadata::Dovi::base_layer_unusable`] (Profile 5 / P7 with an
    /// enhancement layer) — a source the offer must refuse regardless of `dv_declared`, since a
    /// declaration only ever accompanies a USABLE base layer.
    pub dv_base_unusable: bool,
    /// `None` = server default audio, facts unknown (fails closed). `Some(capable)` = a known
    /// carried track with or without `canNormalizeLoudness`.
    pub carried_capable: Option<bool>,
    /// What is on screen (M7/I6): none, an external sidecar the client draws itself, or an
    /// embedded track the server would have to burn to keep — the same [`SubtitleEffect`]
    /// `facts(ps)` reads off `cur_sub_sid`/`cur_sub_sidecar`.
    pub subtitle_effect: SubtitleEffect,
    /// This playback's server already refused or ignored the params once.
    pub refused: bool,
    /// The contract's own `audio` — what the live route has actually applied.
    pub applied: plx_plex::plex::AudioEnhancements,
    /// The applied route is a Burn (M7) — `cur_contract.remux` is `false` even though this is the
    /// enhancement's OWN route, exactly as [`live_is_own_burn`] reads it. Ignored unless `applied`
    /// carries a preference and `remux` is `Some(true)` (a Burn is still `ProgressiveMkv`).
    pub applied_burn: bool,
    /// Force `displayed_audio_enhancements`'s in-flight branch (a user edit queued, not yet
    /// settled), so it reads `want_live` instead of `cur_contract.audio`.
    pub in_flight: bool,
    /// An EMBEDDED subtitle the side reader may draw: a local link, a real Part path and a known
    /// Part bitrate, with the client able to draw it. Only meaningful with
    /// `subtitle_effect: Embedded` on a remux.
    pub side_reader: bool,
}

#[cfg(any(test, feature = "test-support"))]
impl Default for EnhTestFixture {
    fn default() -> Self {
        Self {
            pass: plx_plex::plex::serverinfo::Subscription::Yes,
            remux: Some(true),
            base_present: true,
            dv_declared: false,
            dv_base_unusable: false,
            carried_capable: Some(true),
            subtitle_effect: SubtitleEffect::None,
            refused: false,
            applied: plx_plex::plex::AudioEnhancements::NONE,
            applied_burn: false,
            in_flight: false,
            side_reader: false,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
fn test_original_candidate() -> AutoOriginalCandidate {
    AutoOriginalCandidate {
        probe_part: "https://example.invalid/source.mkv".into(),
        direct: true,
        dovi: plx_data::metadata::Dovi::NONE,
        dv_decision: plx_data::metadata::DvDecision::NONE,
        audio: Some(CarriedAudio {
            sid: 42,
            ordinal: 1,
            codec: "eac3".into(),
            channels: 6,
            can_normalize_loudness: false,
            immersive: true,
        }),
        audio_converted: false,
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn enhancement_test_session(route: EnhTestFixture) -> (PlaybackSession, ServerId) {
    let sid = plx_plex::plex::register_for_test("enh-menu-test", "127.0.0.1", 1, "token", "enh-menu-client");
    plx_plex::plex::serverinfo::store_for_test(sid, route.pass, "1.43.4");

    let mut ps = PlaybackSession::IDLE;
    ps.cur_sid = sid;
    ps.cur_sub_sid = if route.subtitle_effect == SubtitleEffect::None { 0 } else { 999 };
    ps.cur_sub_sidecar = route.subtitle_effect == SubtitleEffect::Sidecar;
    ps.cur_sub_client_drawable = ps.cur_sub_sidecar;
    ps.cur_enhancement = if route.refused {
        EnhancementOutcome::Refused
    } else if route.applied.any() {
        EnhancementOutcome::Applied
    } else {
        EnhancementOutcome::Off
    };
    ps.cur_contract = plx_plex::plex::EncodeContract {
        remux: matches!(route.remux, Some(true)) && !(route.applied_burn && route.applied.any()),
        delivery: plx_plex::plex::TranscodeDelivery::ProgressiveMkv,
        no_video_copy: false,
        ceiling: None,
        audio: route.applied,
    };
    ps.tsession = if route.remux.is_some() { "enh-menu-test-session".to_owned() } else { String::new() };
    ps.cur_audio = route.carried_capable.map(|capable| CarriedAudio {
        sid: 501,
        ordinal: 1,
        codec: "ac3".into(),
        channels: 2,
        can_normalize_loudness: capable,
        immersive: false,
    });
    ps.auto_original = route.base_present.then(|| AutoOriginalCandidate {
        direct: route.remux.is_none(),
        dovi: if route.dv_base_unusable {
            plx_data::metadata::Dovi { present: true, profile: 5, bl_compat: 0, ..plx_data::metadata::Dovi::NONE }
        } else {
            plx_data::metadata::Dovi::NONE
        },
        dv_decision: if route.dv_declared {
            plx_data::metadata::DvDecision {
                capability: plx_platform::devcaps::dv::DvCapability::Supported,
                presentation: plx_data::metadata::DvPresentation::Declare(plx_data::metadata::DolbyHdrInfo {
                    profile_id: 8,
                    track_type: "single",
                    encryption_type: "clear",
                }),
            }
        } else {
            plx_data::metadata::DvDecision::NONE
        },
        audio: ps.cur_audio.clone(),
        ..test_original_candidate()
    });

    if route.side_reader {
        ps.cur_sub_client_drawable = true;
        ps.cur_sub_ordinal = 0;
        ps.cur_transport_kbps = 7_400;
        if let Some(candidate) = ps.auto_original.as_mut() {
            candidate.probe_part = "/library/parts/1/1/file.mkv".into();
        }
        if let Some(client) = plx_plex::plex::client_for(sid) {
            client.set_link(plx_plex::plex::probe::Location::Local);
        }
    }

    if route.in_flight {
        let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
        control.pending_user = Some(UserRouteIntent::Retranscode);
        control.phase = ControlPhase::StagingUser(control.next_action);
    }

    (ps, sid)
}

// ---- selection commits: playback POLICY for the in-player track menu. The menu only reports
// what row was picked; whether that means a native stream switch, a server re-transcode, or a
// burn refresh is decided HERE, next to the codec sets and the transcode state it depends on. ----

/// Commit an audio-track pick: NATIVE switch (feed the chosen stream from the same direct-play
/// file — no transcode, keeps 4K HEVC) when the item direct-plays AND the target codec is
/// direct-playable; else a server re-transcode with that stream selected. `audio.ordinal` is the
/// CONTAINER audio ordinal (the menu converts its row via metadata::audio_ordinal); `audio` itself
/// is the frozen `CarriedAudio` snapshot (issue #266) the menu built via `CarriedAudio::from_stream`.
pub fn commit_audio_selection(ps: &mut PlaybackSession, audio: CarriedAudio) {
    if forced_direct_play(ps) && !audio_track_direct_plays(ps, &audio.codec, audio.channels) {
        ps.play_verdict = Some(PlayVerdict::Forced(ForcedFailure::AudioNeedsConversion));
        return;
    }
    // Audio always changes the decoder/server route contract, even when the eventual route stays
    // direct. Fence the old worker before publishing the selected stream or invalidating its
    // Original candidate; the queued reload below crosses its own action boundary afterwards.
    let _edit = begin_user_contract_boundary();
    // A standing Burn is wire-shaped `Other`, but it is an Original-family route: the pick must
    // retarget the candidate the release returns to, exactly as on a remux.
    let family = enhancement_family(ps);
    let direct_plays = audio_track_direct_plays(ps, &audio.codec, audio.channels);
    match family {
        // The recovery declaration captures one exact source/audio pairing. Once the user changes
        // that pairing while HLS is live, do not later resurrect the old track behind their back.
        // A new playback (or selecting Auto again from Original) can establish a fresh candidate.
        RouteFamily::Other => {
            if matches!(
                cur_delivery(ps),
                plx_plex::plex::TranscodeDelivery::FixedHls { .. }
            ) {
                ps.auto_original = None;
            }
        }
        // Issue #266: on the Original family the candidate is also the way back from an enhanced
        // remux, so it follows the pick instead (see `AutoOriginalCandidate::retarget_audio`).
        RouteFamily::Direct | RouteFamily::Remux => {
            if let Some(candidate) = ps.auto_original.as_mut() {
                if !candidate.retarget_audio(&audio, direct_plays) {
                    ps.auto_original = None;
                }
            }
        }
    }
    let stream_id = audio.sid;
    let native = family == RouteFamily::Direct && direct_plays;
    let (ordinal, codec) = (audio.ordinal, audio.codec.clone());
    { let s = &mut *ps; s.cur_audio = Some(audio); s.selection_known = true };
    if native {
        // record the pick: the timeline then reports the stream that actually plays, and a
        // later transcode event (subtitle burn refresh / transcode seek) keeps this track.
        // persist the USER's pick server-side (official-client behavior): /status/sessions'
        // selected-stream display keys on the part selection, not the timeline report. Only
        // user picks persist — the start-of-play auto-pick (eng preference) reports only. Reached
        // from `app/playback.rs::commit_track` inside the run-loop's `FrameScope`, so this PUT runs
        // on the serial selection worker instead of blocking the frame thread.
        queue_put_selection(cur_sid(ps), cur_part_id(ps), cur_audio_sid(ps), cur_sub_sid(ps));
    }
    // The new track may change what the enhancement offer says (capability, candidate). If the
    // route must change for that, the reconcile's Retranscode REPLACES this pick's own reload and
    // is marked as owing it, so a refused enhancement still switches the track.
    if reconcile_enhancement(ps, true) {
        // the reconcile's Retranscode owes this pick its reload
        crate::player::note_audio_pick();
        return;
    }
    if native {
        crate::player::request_audio_track(ps, ordinal, &codec);
    } else {
        crate::player::request_audio_switch(ps, stream_id);
    }
    // the rebuild now loading is this pick's: the read-out says "Switching audio…"
    crate::player::note_audio_pick();
}

/// Apply commands which were attached to one exact Original trial, after either the candidate or
/// its rollback Engine has really started. Consuming the value makes cross-trial leakage
/// impossible; dropping it on terminal start failure is the explicit cancellation edge.
pub fn apply_deferred_original_effects(ps: &mut PlaybackSession, mut effects: DeferredOriginalEffects) {
    if let Some(q) = effects.quality.take() {
        apply_quality_choice(ps, q);
    }
    if let Some(audio) = effects.audio.take() {
        commit_audio_selection(ps, audio);
    }
    if effects.reconcile {
        reconcile_enhancement(ps, false);
    }
}

/// Atomically publish the main-thread session fields a newly spawned timeline reporter may read.
/// Called at the reporter spawn site, before ownership crosses to its worker. The active encoder
/// remains in `PlayerControl`, so a later in-place ABR commit changes the wire session and this
/// projection under one lock without touching main-thread-only `Session`.
pub fn begin_timeline_reporting(ps: &PlaybackSession) -> Option<TimelineLease> {
    let projection = TimelineProjection {
        sid: cur_sid(ps),
        rating_key: cur_rk(ps),
        logical_session: sess(ps),
        play_queue_id: pq_id(ps),
        play_queue_item_id: pq_item_id(ps),
        audio_stream_id: cur_audio_sid(ps),
        subtitle_stream_id: cur_sub_sid(ps),
    };
    if projection.rating_key.is_empty() || !projection.sid.is_set() {
        return None;
    }
    let required_stop = TIMELINE_STOP_FENCE.announced();
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    control.timeline = Some(projection);
    Some(TimelineLease {
        engine_epoch: control.engine_epoch,
        required_stop,
    })
}

/// Linearize one worker sample against route replacement and engine teardown. No network work is
/// done while the mutex is held. `None` permanently invalidates this reporter after its Engine is
/// retired; transient Applying/Resolving phases skip a hybrid report without borrowing Session.
fn timeline_snapshot(
    lease: &TimelineLease,
    state: plx_plex::plex::TimelineState,
    time_ms: i64,
    duration_ms: i64,
) -> Option<TimelineSnapshot> {
    let control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    if control.engine_epoch != lease.engine_epoch {
        return None;
    }
    if !matches!(
        control.phase,
        ControlPhase::Stable
    ) {
        return None;
    }
    let projection = control.timeline.as_ref()?;
    let session = if control.active.id.is_empty() {
        projection.logical_session.clone()
    } else {
        control.active.id.clone()
    };
    Some(TimelineSnapshot {
        sid: projection.sid,
        rating_key: projection.rating_key.clone(),
        state,
        time_ms,
        duration_ms,
        session,
        play_queue_id: projection.play_queue_id.clone(),
        play_queue_item_id: projection.play_queue_item_id.clone(),
        audio_stream_id: projection.audio_stream_id,
        subtitle_stream_id: projection.subtitle_stream_id,
    })
}

/// POST one periodic timeline update for an exact [`TimelineLease`]. After waiting for the stop
/// fence captured by that lease, this serializes through `TIMELINE_EFFECT` and snapshots the
/// server, item, PlayQueue and selected tracks together from [`PlayerControl`]; a stale Engine lease
/// sends nothing. Final `Stopped` is emitted separately by [`ScrobbleWork`] from its owned teardown
/// snapshot.
pub fn report_timeline(
    lease: &TimelineLease,
    state: plx_plex::plex::TimelineState,
    t_ms: i64,
    d_ms: i64,
) -> bool {
    // This lease belongs to a replacement Engine only after every stop synchronously announced
    // before its publication has joined the old reporter and attempted old `stopped`. Old leases
    // captured an earlier generation and never wait on the stop worker which is joining them.
    TIMELINE_STOP_FENCE.wait(lease.required_stop);
    let _effect = TIMELINE_EFFECT.lock().unwrap_or_else(|e| e.into_inner());
    let Some(report) = timeline_snapshot(lease, state, t_ms, d_ms) else {
        return false;
    };
    let c = match plx_plex::plex::client_for(report.sid) {
        Some(c) => c,
        None => return true,
    };
    let ok = c.timeline(&plx_plex::plex::TimelineReport {
        rating_key: &report.rating_key,
        state: report.state,
        time_ms: report.time_ms,
        duration_ms: report.duration_ms,
        session: &report.session,
        play_queue_id: &report.play_queue_id,
        play_queue_item_id: &report.play_queue_item_id,
        audio_stream_id: report.audio_stream_id,
        subtitle_stream_id: report.subtitle_stream_id,
    });
    // FAILURES ONLY. The reporter thread logs `timeline <state> t=…s/…s` for every tick whichever
    // way the POST went (`player::threads`), so a report the server never took looks exactly like
    // one it did — for the whole length of a film, ten seconds at a time. The success half is
    // already on that line and this runs at 0.1 Hz, so only the silence needs a line of its own.
    if !ok {
        plx_base::eventlog::log(&format!(
            "timeline post failed rk={} state={} t={}s",
            report.rating_key,
            report.state.as_str(),
            report.time_ms / 1000,
        ));
    }
    true
}
