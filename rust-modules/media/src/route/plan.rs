//! The PURE half of the route split (spec §9, §15.2): functions of their arguments alone, plus
//! the plain data types they need. Nothing here reads a `static mut`, touches [`super::decision`]'s
//! `SESSION`/`PLAYER_CONTROL`/`PLAY_SLOT`/`ENCODER_CLEANUP`/`SCROBBLE_JOIN`/`TIMELINE_STOP_FENCE`/
//! `QUALITY`, or calls `task::spawn*` — that is what makes [`build_stream`] safe to run on the
//! resolve worker. Everything else in the former `route.rs` (session state, the synchronized
//! `PlayerControl`, PMS/native I/O, the encoder/scrobble/timeline machinery) lives in
//! [`super::decision`]. `ci/check-deps.sh`'s `wall` gate holds this file to zero
//! `Instant::now`/`SystemTime::now`/`.elapsed()`; a function that needs wall time is not pure and
//! belongs in `decision.rs`.

use super::decision::{
    ActiveEncoderState, PlayerControl,
    };

/// One worker's right to observe or replace the active route. Both fields are required: `encoder`
/// addresses PMS, while `epoch` distinguishes semantic routes which intentionally reuse that
/// exact Streaming Resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteLease {
    pub(super) epoch: u64,
    pub(super) encoder: String,
}

impl RouteLease {
    pub fn encoder(&self) -> &str {
        &self.encoder
    }
}

/// Everything a media worker must still own before it may publish a route-affecting result.
/// `route` rejects same-id ABA, `engine_epoch` rejects a worker from an earlier Load,
/// `media_epoch` rejects evidence collected before an applied seek, and `applied_revision`
/// names the physical route contract this worker actually serves. Desired user edits deliberately
/// do not change this ticket until their PMS/native effect commits: a refusal must leave the
/// unchanged worker authorized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerTicket {
    pub(super) route: RouteLease,
    pub(super) engine_epoch: u64,
    pub(super) media_epoch: u64,
    pub(super) applied_revision: u64,
}

impl WorkerTicket {
    pub fn encoder(&self) -> &str {
        self.route.encoder()
    }
}

/// Identity of one physical `sf_load` attempt inside a prepared route transaction. Attempts are
/// never reused: a late result from A cannot settle retry B even though both open the same URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteStartAttempt {
    pub(super) serial: u64,
    pub(super) attempt: u64,
}

impl RouteStartAttempt {
    #[cfg(all(any(test, feature = "test-support"), feature = "hostsim"))]
    pub const fn fixture() -> Self {
        Self {
            serial: 1,
            attempt: 1,
        }
    }
}

pub(super) fn next_route_epoch(epoch: u64) -> u64 {
    let next = epoch.wrapping_add(1);
    if next == 0 {
        1
    } else {
        next
    }
}

pub(super) fn lease_of(active: &ActiveEncoderState) -> RouteLease {
    RouteLease {
        epoch: active.epoch,
        encoder: active.id.clone(),
    }
}

pub(super) fn next_generation(value: u64) -> u64 {
    let next = value.wrapping_add(1);
    if next == 0 {
        1
    } else {
        next
    }
}

pub(super) fn worker_ticket_of(control: &PlayerControl) -> WorkerTicket {
    WorkerTicket {
        route: lease_of(&control.active),
        engine_epoch: control.engine_epoch,
        media_epoch: control.media_epoch,
        applied_revision: control.applied_revision,
    }
}

/// The audio track a route has decided to carry, frozen at the moment of decision (issue #266's
/// vocabulary). Used as `Option<CarriedAudio>` everywhere: `None` means "server default, facts
/// unknown", and every reader fails CLOSED on it — an unknown track is never treated as
/// loudness-capable or immersive just because nobody has said otherwise.
#[derive(Clone, PartialEq, Debug)]
pub struct CarriedAudio {
    /// PMS `Stream.id` — the id this track is selected/burned by (`audioStreamID`,
    /// `PUT /library/parts`).
    pub sid: i64,
    /// The demuxer-facing ordinal (`metadata::Stream.index` order) — what `set_audio_track` feeds
    /// on a direct play. `-1` mirrors the pre-refactor "unknown" sentinel.
    pub ordinal: i32,
    pub codec: String,
    pub channels: i64,
    /// PMS 1.43.4+ Plex Pass: can the server honor `boostDialog`/`normalizeLoudness` for this
    /// track (issue #266; wire field is `plex::Stream::can_normalize_loudness`).
    pub can_normalize_loudness: bool,
    /// Dolby Atmos (`metadata::Stream::has_atmos`) — the Load payload's `contents.immersive`.
    pub immersive: bool,
}

impl CarriedAudio {

    /// A track the route NAMES (`sid`, e.g. a retry's or the session's earlier pick) whose stream
    /// this resolve never read — its track list was not fetched or does not contain it. Only the
    /// id is known: no codec, no channels, and every capability fact false, so every reader still
    /// fails closed. It exists so the playing session keeps reporting the id it asked for (the
    /// timeline's `audioStreamID`, a later rebuild's pick) exactly as before issue #266.
    pub fn named(sid: i64, ordinal: i32) -> Self {
        CarriedAudio {
            sid,
            ordinal,
            codec: String::new(),
            channels: 0,
            can_normalize_loudness: false,
            immersive: false,
        }
    }
}

pub use plx_plex::plex::session::{PlaybackQuality as Quality, DeckPress, DirectPlayMode, NextEpisodeMode, SkipInterval, SubtitleSize, SubtitlePosition};

/// The ladder IN ORDER, best first. The ONE place row order lives, so the picker's index mapping
/// cannot drift from what was drawn (`appkit::more_menu`'s rule, and its bug).
pub const QUALITY_LADDER: [Quality; 7] = [
    Quality::Auto,
    Quality::Original,
    Quality::P1080High,
    Quality::P1080,
    Quality::P720,
    Quality::P720Low,
    Quality::P480,
];

/// The explicit support/readiness gate for automatic playback. The measured PMS contract,
/// segmented demux, per-encoder wire identity, prime/commit transaction and single-Load LG
/// resolution gate are all present. Keeping this named (instead of deleting it after launch)
/// preserves one fail-closed switch should a future protocol change invalidate that evidence.
pub const fn auto_quality_ready() -> bool {
    true
}

pub(super) fn supported_quality(q: Quality) -> Quality {
    if q == Quality::Auto && !auto_quality_ready() {
        Quality::Original
    } else {
        q
    }
}

/// How a subtitle on screen interacts with the Plex Pass audio enhancement (issue #266 I6,
/// reworked per M7 and the owner's "do it the Plex way" decision: PMS burns a subtitle in rather
/// than dropping it, exactly as official Plex's playback lib does).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubtitleEffect {
    /// No subtitle on screen.
    None,
    /// An external file the CLIENT draws itself (`metadata::Stream::sidecar_renderable`) — fetched
    /// and rendered by `player::sidecar` entirely outside the transcoded stream, so an enhanced
    /// remux never touches it.
    Sidecar,
    /// An embedded (in-container) subtitle direct play renders from the demuxed stream — an
    /// enhanced remux DROPS it (M4); only a forced re-encode with `subtitles=burn` keeps it (M7).
    Embedded,
}

/// The two verdict NUMBERS of a `/decision` body — `generalDecisionCode` and `transcodeDecisionCode`
/// — kept as the integers PMS sent (`None` = the body carried none; never a defaulted 0).
///
/// They ride [`PlayVerdict::Server`] beside the sentence for one reason: a refused playback reaches
/// Sentry as a bare `decision_refused`, and the sentence that explains it is server copy that can
/// carry file names, paths and server details, so it must never leave the television. A code is a
/// protocol constant, not a quotation, so the report can classify it into a closed domain
/// (`player::report::DecisionCodeClass`) and say WHICH refusal this was without saying anything
/// about whose server or file it was.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecisionCodes {
    pub general: Option<i64>,
    pub transcode: Option<i64>,
}

impl DecisionCodes {
    pub fn of(mc: &plx_plex::plex::MediaContainer) -> Self {
        Self {
            general: mc.general_decision_code,
            transcode: mc.transcode_decision_code,
        }
    }
}

/// **Why a plan leaves without a URL on purpose**, as a typed verdict rather than a sentence.
///
/// The server's own refusal is quoted verbatim ([`PlayVerdict::Server`]: PMS wrote it, in the
/// server's language, and it may be empty). Every other arm is the APP's policy decision, so it is
/// stored as a variant and worded only where it is read ([`PlayVerdict::text`]) — a stored sentence
/// would freeze one language into playback state that tests, logs and replays also read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlayVerdict {
    /// `/decision`'s own sentence, quoted and never translated. `""` when it gave none. Beside it,
    /// the two NUMBERS the same body carried — the only part of a server refusal the failure report
    /// may ever send anywhere (see [`DecisionCodes`]); the sentence stays on this device.
    Server(String, DecisionCodes),
    /// Direct Play is Disabled, and this stream can only be played as the original.
    DirectPlayDisabled,
    /// Force Direct Play is on, and this is why the original cannot play.
    Forced(ForcedFailure),
}

/// The limitation a Force Direct Play verdict names. Each one carries the same recovery step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForcedFailure {
    NoOriginal,
    Container,
    Video,
    Audio,
    Unauthorized,
    /// The resolve reached the transcode path, which Force never takes.
    OpenFailed,
    /// A later audio-track pick needs conversion.
    AudioNeedsConversion,
}

impl PlayVerdict {
    /// The read-out's sentence: the server's verbatim, or the app's own from the catalog.
    pub fn text(&self) -> &str {
        use plx_platform::i18n::msg;
        match self {
            Self::Server(sentence, _) => sentence,
            Self::DirectPlayDisabled => msg::widgets_verdict_direct_play_disabled(),
            Self::Forced(why) => match why {
                ForcedFailure::NoOriginal => msg::widgets_verdict_forced_no_original(),
                ForcedFailure::Container => msg::widgets_verdict_forced_container(),
                ForcedFailure::Video => msg::widgets_verdict_forced_video(),
                ForcedFailure::Audio => msg::widgets_verdict_forced_audio(),
                ForcedFailure::Unauthorized => msg::widgets_verdict_forced_unauthorized(),
                ForcedFailure::OpenFailed => msg::widgets_verdict_forced_open(),
                ForcedFailure::AudioNeedsConversion => msg::widgets_verdict_forced_audio_conversion(),
            },
        }
    }
}

/// The episode queued after the one now playing — everything the Up Next control draws AND
/// everything [`request_play`] needs to start it, so playing it costs no PMS round trip either.
///
/// It comes free with the `continuous=1` PlayQueue every playback already creates (see
/// [`plx_plex::plex::Client::create_play_queue`]); nothing here asks the server "what's next".
#[derive(Clone, Default)]
pub struct UpNext {
    pub rk: String,
    pub part: String,
    pub vcodec: String,
    pub acodec: String,
    pub show_title: String, // grandparentTitle
    pub ep_title: String,
    pub season: i64,
    pub index: i64,
    pub thumb: String,
    pub dur_ms: i64,
    pub resume_ms: i64,
}

/// The detail page's "how this plays" answer, BEFORE anything is played — the same FOUR gates
/// `build_stream` will apply (codec+resolution via [`video_direct_plays`], container via
/// [`part_is_streamable`], whether the planner's intended audio track needs converting
/// ([`audio_intent_needs_conversion`]), and the user's quality ceiling via
/// [`quality_policy`] — applied last and able only to downgrade), asked of the loaded `Detail`.
/// The ceiling is the one a reader debugging "why does this ordinary h264/AC-3 MKV say Converts"
/// will not think of, which is why it is named in the list rather than left to the code.
/// An approximation by design: the real decision can still consult the server (`server_decision`
/// when no DP audio track is found), so this leans the same way that fallback usually lands.
/// It exists for `Details Screen.dc.html`'s facts row and must stay a READ-ONLY preview —
/// nothing in the playback path may branch on it (the path re-derives for itself).
///
/// **Four answers, and the split between a remux and a re-encode is the one a two-valued preview got wrong.** "The
/// server has to do something" and "the server has to re-encode the picture" are different facts
/// (`is_remux`'s doc says so for the LIVE session; this is the same distinction before Play), and
/// the UI hangs a Plex Pass claim on the difference: hardware conversion and HDR tone mapping are
/// both properties of an ENCODE, so naming either one for a stream where no encoder runs points
/// the user at a purchase that would fix nothing — `player::error_shape`'s own rule, and the
/// polarity issue #22 is about.
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum Preview {
    DirectPlay,
    /// Container-only REMUX — Plex's own "Direct Stream". The video (and usually the audio) is
    /// COPIED into progressive MKV because the container is not one the demuxer streams, or
    /// because the item lists no audio track at all; the pixels arrive untouched, 4K and HDR10 intact.
    /// `build_stream` spells this exact case `plan.contract.remux = video_dp` on the transcode
    /// branch.
    Remux,
    /// The Original remux with a CONVERTED audio: the video is copied untouched, and the track
    /// that will play is one the TV cannot decode (AAC 7.1, TrueHD, DTS-HD…), so the server
    /// converts only that audio. The planner's own "no direct-playable carrier for the intended
    /// track" answer ([`audio_intent_needs_conversion`]); it is `AutoOriginalCandidate::
    /// audio_converted` before Play. Never a re-encode, so it carries no Plex Pass claim.
    OriginalAudioConverted,
    /// A real re-encode: the server decodes and re-encodes the video.
    Converts,
}

// ---- async resolve: worker computes an owned Plan, main thread installs it ------------------
// The house idiom (metadata::load_season / browse.rs): generation counter + single-flight +
// a monotone one-slot mailbox + a per-frame pump that applies on the MAIN thread.
//
// Cancellation is FLAG-ONLY by design: `cancel_play` bumps the generation so a landing is
// discarded, but it cannot wake a worker blocked in recv(2) — publishing the socket fd to make
// that possible broke the seek path and was reverted (docs/async-model-decision.md). That costs
// nothing here: the freeze is fixed by getting the resolve OFF the loop, and a worker lingering
// in the background is invisible once the UI has already moved on.

