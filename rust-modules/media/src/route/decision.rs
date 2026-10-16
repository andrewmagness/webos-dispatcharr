//! The IMPURE half of the route split (spec §9, §15.2): session state, the synchronized
//! [`PlayerControl`], PMS/native I/O, and the route-retire bookkeeping — everything
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
            cur_src: (0, 0, 0),
        cur_transport_kbps: 0,
        cur_source_decodable: true,
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
            cur_src,
            cur_transport_kbps,
            cur_source_decodable,
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
            cur_src: *cur_src,
            cur_transport_kbps: *cur_transport_kbps,
            cur_source_decodable: *cur_source_decodable,
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
pub(super) struct ActiveEncoderState {
    /// Monotone semantic route generation. The PMS id may deliberately stay unchanged while the
    /// route changes from HLS to direct Original, so the id alone is not an ownership token.
    pub(super) epoch: u64,
    pub(super) id: String,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlPhase {
    Idle,
    Stable,
    /// Candidate preparation is in flight while the old Engine/route is still recoverable.
    Preparing(u64),
    /// Candidate preparation committed but no physical Load attempt currently owns it.
    Prepared(u64),
    /// One exact physical Load attempt is in flight: `(transaction, attempt)`.
    Starting(u64, u64),
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
    /// Revision represented by the physical route and therefore carried by WorkerTicket.
    pub(super) applied_revision: u64,
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
    },
    engine_epoch: 1,
    media_epoch: 1,
    desired_revision: 1,
    applied_revision: 1,
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
});

fn advance_route(active: &mut ActiveEncoderState) {
    active.epoch = next_route_epoch(active.epoch);
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
    control.phase = phase;
    true
}

/// Publish the native half of one prepared route. An ordinary failed candidate deliberately
/// remains the applied projection in `Failed`: teardown may already have destroyed the old Engine
/// and PMS may already have retired its encoder, so restoring the old *description* would fabricate
/// a live route.
pub fn settle_route_start(ticket: RouteStartAttempt, result: RouteStartResult) -> bool {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    if control.phase != ControlPhase::Starting(ticket.serial, ticket.attempt) {
        return false;
    }
    control.last_start_result = Some((ticket, result));
    match result {
        RouteStartResult::Started => control.phase = ControlPhase::Stable,
        RouteStartResult::NoRoute | RouteStartResult::StartFailed => {
            control.phase = ControlPhase::Failed(ticket.serial);
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
pub fn drain_route_start_results(_ps: &mut PlaybackSession) {
    let results = {
        let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut control.start_results)
    };
    for (ticket, result) in results {
        let _ = settle_route_start(ticket, result);
    }
}

/// A route which had reached Stable later proved terminal (demux/HTTP/native callback failure).
/// Revoke the Engine/media tickets and expose Failed as one reducer edge rather than changing only
/// the UI playback enum while automatic workers still believe the route is publishable.
pub fn fail_current_engine() {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    let serial = match control.phase {
        ControlPhase::Preparing(serial) | ControlPhase::Prepared(serial) | ControlPhase::Starting(serial, _) => serial,
        ControlPhase::Stable => {
            control.next_action = next_generation(control.next_action);
            control.next_action
        }
        ControlPhase::Failed(_) | ControlPhase::Idle | ControlPhase::Stopping => return,
    };
    control.engine_epoch = next_generation(control.engine_epoch);
    control.media_epoch = next_generation(control.media_epoch);
    control.pending_auto = None;
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
    control.applied_revision = control.desired_revision;
    advance_route(&mut control.active);
    control.active.id.clear();
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
}

fn take_active_encoder() -> String {
    let mut control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    let active = &mut control.active;
    advance_route(active);
    std::mem::take(&mut active.id)
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
/// The stable app-owned playback generation (and the first encoder's PMS session id).
pub fn sess(ps: &PlaybackSession) -> String {
    ps.sess.clone()
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
/// `adaptiveStreaming` ceiling — `(0, 0)` when nobody said. Two routes, two answers:
///
/// * direct play / remux (`ProgressiveMkv`): the SOURCE's own coded size (`cur_src`), since the
///   file's elementary stream is what gets fed;
/// * a fixed quality's HLS: the source capped by that quality's [`plx_plex::plex::Ceiling`] — PMS
///   never upscales, so the smaller of the two is the largest picture it can send.
///
/// Main thread, like every other session read. Pure over the session it is given.
pub fn sink_max_raster(ps: &PlaybackSession) -> (u16, u16) {
    let s = &*ps;
    let clamp = |v: i64| u16::try_from(v).unwrap_or(u16::MAX);
    let source = (clamp(s.cur_src.1), clamp(s.cur_src.2));
    match s.cur_contract.delivery {
        plx_plex::plex::TranscodeDelivery::ProgressiveMkv => source,
        plx_plex::plex::TranscodeDelivery::FixedHls { .. } => match s.cur_contract.ceiling {
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
        },
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
/// Retire a playback's route at a real stop: the active route identity, plus the two session
/// fields a stop resets (teardown clears the URL right after). A partial write rather than a
/// whole-session reset because the rest is still read after teardown returns — see
/// `reset_session`'s doc for the reader that would break.
pub fn retire_route(ps: &mut PlaybackSession) {
    take_active_encoder();
    ps.tsession.clear();
    ps.cur_contract.remux = false;
}

use plx_base::cbuf::set as set_c; // shared fixed-C-buffer write (the session's HUD title/ctxline)

// ---- the QUALITY ceiling: what the USER has asked this playback to come in under -------------

pub fn forced_direct_play(ps: &PlaybackSession) -> bool {
    ps.direct_play_mode == DirectPlayMode::Forced
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

// ---- selection commits: playback POLICY for the in-player track menu. The menu only reports
// what row was picked; whether that means a native stream switch, a server re-transcode, or a
// burn refresh is decided HERE, next to the codec sets and the transcode state it depends on. ----

