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

/// Everything needed to restore Auto's zero-video-encode state after HLS. `url` is the cold-start
/// playback target; `probe_part` is the raw Part key used to bind runtime measurement and direct
/// playback to the exact live HLS Streaming Resource. `direct` says whether the Part itself is
/// playable or whether PMS must container-remux it while copying the video.
#[derive(Clone)]
pub(super) struct AutoOriginalCandidate {
    pub(super) probe_part: String,
    pub(super) direct: bool,
    pub(super) dovi: plx_data::metadata::Dovi,
    pub(super) dv_decision: plx_data::metadata::DvDecision,
    /// The audio track this candidate carries. `None` = server default, facts unknown (issue
    /// #266 fails closed on it: no enhancement is ever offered without a known, capable track).
    pub(super) audio: Option<CarriedAudio>,
    /// The carried audio is a track the TV cannot direct-play, picked while this candidate was
    /// standing. The candidate still describes a direct-playable Part (video, DV, fps, subtitle
    /// facts are all true of it), but the audio now carried must be converted by the server, so
    /// returning to Original means a remux naming that track, not the Part. Only
    /// [`retarget_audio`](Self::retarget_audio) writes it; a later direct-playable pick clears it.
    pub(super) audio_converted: bool,
}

impl AutoOriginalCandidate {
    /// Whether this candidate's route feeds the Part itself: direct, and with an audio the TV can
    /// decode. The reader of `direct` that means "may play the Part with this audio".
    pub(super) fn feeds_part(&self) -> bool {
        self.direct && !self.audio_converted
    }

    /// Follow a mid-play audio pick on a Direct or Remux route (issue #266). These two methods are
    /// the ONLY writers of the candidate's audio and subtitle halves once it is installed.
    ///
    /// Why retarget instead of dropping, as the HLS family does: on Direct/Remux the candidate is
    /// also the way BACK from an enhanced remux (`EnhancementReleased`), so it must describe the
    /// track the viewer is hearing now — a release that restored the capture-time track would
    /// undo their pick. On HLS the candidate is only a recovery target and the old drop stays.
    ///
    /// The track is taken as a unit (sid, ordinal, codec, channels, capability, immersive), so the
    /// release's Load payload can never pair one track's ordinal with another's codec. `immersive`
    /// survives only on a candidate that feeds the Part, for `carried_track`'s reason.
    ///
    /// A direct candidate given a track the TV cannot decode is KEPT, marked `audio_converted`:
    /// the server copies the video and converts only that audio, which is a remux, not a
    /// re-encode. `false` means no copy is legal at all — an unusable Dolby Vision base layer
    /// forbids copying the video — and the caller drops the candidate.
    pub(super) fn retarget_audio(&mut self, a: &CarriedAudio, direct_plays: bool) -> bool {
        if self.dovi.base_layer_unusable() && !direct_plays {
            return false;
        }
        self.audio_converted = self.direct && !direct_plays;
        let mut carried = a.clone();
        carried.immersive &= self.feeds_part();
        self.audio = Some(carried);
        true
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

/// The family of route a caller is BUILDING (or, mid-play, is on) — the only shape question the
/// Plex Pass audio enhancement (issue #266) asks. `Direct` = the raw Part; `Remux` = the
/// codec-preserving progressive-MKV copy; `Other` = every encoder rung, fixed quality, HLS and
/// relay. The enhancement decorates the Original route and nothing else (I5): M3 measured that on
/// a re-encode shape the params change nothing observable, because the audio is transcoded there
/// at baseline anyway.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum RouteFamily {
    Direct,
    Remux,
    Other,
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

/// Everything [`enhancement_availability`] reads, gathered by the caller from whichever side of
/// the playback it stands on (the resolve worker's locals, or the live session). Borrowed so the
/// predicate can never be handed a copy that has drifted from the thing it describes.
#[derive(Clone, Copy)]
pub(super) struct EnhancementFacts<'a> {
    /// The server's Plex Pass state (`serverinfo::subscription_of`), captured at the request.
    pub(super) pass: plx_plex::plex::serverinfo::Subscription,
    /// The frozen non-enhanced Original route. `None` = Original was never feasible here (forced
    /// direct play, a fixed rung, relay, a non-Original MDE…), which is I5's whole exclusion list.
    pub(super) base: Option<&'a AutoOriginalCandidate>,
    /// The audio track the route carries. `None` = server default, facts unknown: treated the same
    /// as "not analyzed yet" — a plain reason, never a silent absence (Plex Pass is already Yes).
    pub(super) carried: Option<&'a CarriedAudio>,
    /// What a subtitle on screen, if any, does to the offer (I6).
    pub(super) subtitle_effect: SubtitleEffect,
    /// This playback's server already refused or ignored the params once.
    pub(super) refused: bool,
}

/// What the enhancement, once offered, actually asks the server for — the one fork the menu, the
/// resolve and the recovery path must never disagree about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnhancementRoute {
    /// The ordinary uncapped progressive-MKV remux: no subtitle on screen, or an external one the
    /// client draws itself.
    Remux,
    /// The same remux, for a declared Dolby Vision source whose base layer IS self-displayable
    /// (P7/P8, `!dovi.base_layer_unusable()`). A remux never carries `DolbyHdrInfo` — the
    /// declaration rides the direct play only (`fill_direct_plan`'s own doc) — so this is the SAME
    /// wire shape as [`Self::Remux`]; only the menu's wording differs (Dolby Vision turns off).
    RemuxDropsDolbyVision,
    /// A forced re-encode carrying `subtitleStreamID=<id>&subtitles=burn` (M7): the only shape that
    /// keeps an EMBEDDED subtitle in view once the audio enhancement is on.
    Burn,
}

/// Why the toggle is disabled (drawn, dim, with a reason) rather than hidden or enabled. Distinct
/// from [`EnhancementAvailability::Hidden`], which — per the owner's decision — is reserved for the
/// ONE reason that stays a silent absence: no Plex Pass.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DisabledReason {
    /// The carried track has no `canNormalizeLoudness` yet (or the track itself is unknown) — the
    /// server has not analyzed this audio.
    NotAnalyzed,
    /// A Dolby Vision base layer that is not self-displayable (P5, `dovi.base_layer_unusable()`):
    /// no copy of it is ever made, so there is nothing the enhancement's remux could decorate.
    DolbyVisionUnusable,
    /// A declared Dolby Vision source with an embedded subtitle on screen: M7 measured PMS copying
    /// the DV video regardless of a burn request and silently dropping the subtitle — neither half
    /// of the ask is honoured, so it is never sent.
    DolbyVisionSubtitle,
    /// The live route is not Direct or Remux (a fixed quality rung, HLS, a relay…), or no Original
    /// candidate was ever computed for it (I5's exclusion list) — the enhanced route is always an
    /// uncapped Original.
    NotOriginalQuality,
    /// This playback's server already refused or ignored the params once.
    ServerRefused,
}

/// **What state is the Plex Pass audio enhancement in for a route of family `target`?** One
/// predicate for every caller — the resolve, recovery, and the menu — so "what the toggle looks
/// like" and "what the resolve does" can never disagree. Per the owner's decision, [`Hidden`] is
/// reserved for the ONE absence that stays silent (no Plex Pass); every other gate that used to
/// hide the rows now reports a [`DisabledReason`] instead.
///
/// [`Hidden`]: EnhancementAvailability::Hidden
pub(super) fn enhancement_availability(f: &EnhancementFacts, target: RouteFamily) -> EnhancementAvailability {
    use plx_plex::plex::serverinfo::Subscription;
    use EnhancementAvailability::{Disabled, Hidden, Offered};
    // I1/I2: `Unknown` fails closed exactly like `No` — an unknown server is not assumed to hold a
    // subscription it may not have, and with it hidden the app's URLs are byte-identical to a
    // build without the feature. The ONE state that stays a silent absence.
    if f.pass != Subscription::Yes {
        return Hidden;
    }
    if f.refused {
        return Disabled(DisabledReason::ServerRefused);
    }
    // I5: Direct or Remux only, and only when an Original candidate was actually computed for it
    // (forced direct play, a fixed rung, relay, a non-Original MDE… all read the same to a viewer:
    // "only at Original quality").
    if !matches!(target, RouteFamily::Direct | RouteFamily::Remux) {
        return Disabled(DisabledReason::NotOriginalQuality);
    }
    let Some(base) = f.base else {
        return Disabled(DisabledReason::NotOriginalQuality);
    };
    // I7: an unusable Dolby Vision base layer is never copied at all — nothing to decorate.
    if base.dovi.base_layer_unusable() {
        return Disabled(DisabledReason::DolbyVisionUnusable);
    }
    let dv_declared = base.dv_decision.presentation.declared().is_some();
    // M7: a declared DV source burning an embedded subtitle is refused by the server itself
    // (video copied regardless, subtitle silently dropped) — never attempted.
    if dv_declared && f.subtitle_effect == SubtitleEffect::Embedded {
        return Disabled(DisabledReason::DolbyVisionSubtitle);
    }
    // A known, capable carried track. `canNormalizeLoudness` is PMS 1.43.4's own per-stream
    // statement that it has the loudness analysis the DSP needs; unknown or absent reads the same
    // to a viewer as "not analyzed yet".
    let analyzed = f.carried.is_some_and(|a| a.can_normalize_loudness);
    if !analyzed {
        return Disabled(DisabledReason::NotAnalyzed);
    }
    Offered(match (dv_declared, f.subtitle_effect) {
        (_, SubtitleEffect::Embedded) => EnhancementRoute::Burn,
        (true, SubtitleEffect::None | SubtitleEffect::Sidecar) => EnhancementRoute::RemuxDropsDolbyVision,
        (false, SubtitleEffect::None | SubtitleEffect::Sidecar) => EnhancementRoute::Remux,
    })
}

/// **What the menu draws for the toggle**, and (via [`EnhancementAvailability::Offered`]'s payload)
/// what turning it on would actually ask the server for. See [`enhancement_availability`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnhancementAvailability {
    /// No Plex Pass — absent from the menu; no hint, no upsell (I1/I2).
    Hidden,
    /// Visible, toggle enabled.
    Offered(EnhancementRoute),
    /// Visible, toggle inert (dim), with a plain-language reason.
    Disabled(DisabledReason),
}

/// The concrete route the enhancement would take here, or `None` when it is not offered — Hidden
/// and every `Disabled` reason collapse to `None` for this projection, since the resolve only ever
/// needs to know "should this be asked for", never why not.
pub(super) fn enhancement_route(f: &EnhancementFacts, target: RouteFamily) -> Option<EnhancementRoute> {
    match enhancement_availability(f, target) {
        EnhancementAvailability::Offered(route) => Some(route),
        EnhancementAvailability::Hidden | EnhancementAvailability::Disabled(_) => None,
    }
}

/// The ONLY source of `EncodeContract::audio`: the viewer's preference where the enhancement is
/// offered, `NONE` everywhere else. Every contract that reaches the wire passes through here, so
/// a preference can never leak onto a route the predicate refused.
pub(super) fn desired_audio(
    pref: plx_plex::plex::AudioEnhancements,
    offered: bool,
) -> plx_plex::plex::AudioEnhancements {
    if offered {
        pref
    } else {
        plx_plex::plex::AudioEnhancements::NONE
    }
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

pub(super) fn audio_direct_plays(mode: DirectPlayMode, codec: &str, channels: i64) -> bool {
    if mode == DirectPlayMode::Forced {
        plx_plex::plex::DP_AUDIO_CODECS.split(',').any(|c| c.eq_ignore_ascii_case(codec))
    } else {
        plx_plex::plex::is_dp_audio_track(codec, channels)
    }
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

