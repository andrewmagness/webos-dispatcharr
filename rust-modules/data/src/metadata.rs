//! The stream facts the player still reads from the old Plex data layer: a video stream's Dolby
//! Vision layering ([`Dovi`]), the decision taken from it once per route ([`DvDecision`]), the two
//! DV diagnostics latches, and the demuxer's track names ([`track_names`]).

pub mod track_names;

/// What PMS says about a video stream's **Dolby Vision** layering — read together, because the
/// only question worth asking of them is a joint one: **is the base layer, alone, a correct
/// picture?**
///
/// It has to be a joint question because the buffer-feed pipeline has no other option. We feed one
/// elementary stream to a decoder that has never heard of an RPU, so whatever the base layer
/// contains is exactly what reaches the panel. That is fine for **Profile 8.1**, whose base layer
/// IS an HDR10 stream — ignoring the RPU costs the dynamic metadata and nothing else. It is not
/// fine for **Profile 5**, which is single-layer IPT-PQ with no HDR10 fallback: decoded as
/// ordinary HEVC it is not a dimmer picture but a WRONG one, with the washed, pink-green cast of
/// IPT read as YCbCr. And it cannot work at all for **Profile 7**, whose picture is split across
/// two layers we cannot interleave.
///
/// All zero is "the server said nothing", which is also what a non-DV stream produces — see
/// [`Dovi::base_layer_unusable`] for why silence must never convict.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Dovi {
    /// `DOVIPresent` — the file carries Dolby Vision at all. Everything below is meaningless
    /// without it, because a non-DV stream sends none of these fields and so reads as all-zero.
    pub present: bool,
    /// `DOVIProfile` — 5 / 7 / 8 …; 0 means the server did not say.
    pub profile: i64,
    /// `DOVIBLCompatID` — 0 none (P5) / 1 HDR10 / 2 SDR / 4 HLG. NB the dev server's P7 item
    /// sends **6**, so this field alone does not identify a dual-layer file.
    pub bl_compat: i64,
    /// `DOVIELPresent` — an enhancement layer is present (P7).
    pub el_present: bool,
    // ---- DESCRIPTIVE ONLY, below. The four fields above answer the PLAYBACK question this struct
    // exists for ("is the base layer a correct picture"); these three are read by
    // `screens::tracks_panel` and by nothing else. They live here rather than on `Detail` so there is
    // one home for "what the server said about Dolby Vision" — but do not reach for them in a
    // decision without the live sweep `plex::Stream`'s DOVI comment records.
    /// `DOVILevel` — the DV level (a bitrate/resolution tier); 0 = the server did not say.
    pub level: i64,
    /// `DOVIVersion`, DECOMPOSED — the server sends the dotted string `"1.0"`, and this holds it
    /// as `(1, 0)`. Rendered back by [`Dovi::version_str`].
    ///
    /// **Why not a `String`:** this struct is `Copy` and rides `route::Session`, whose `IDLE` is a
    /// `const`; more to the point `route::playback_preview_of` reads a `Dovi` **on the detail
    /// page's per-frame draw path**, so a `String` field would put a heap allocation in a hot
    /// frame — the same cost `appkit::info_panel` refuses when it declines to clone `route::url()`.
    /// A DV version is dotted-numeric by specification, so the decomposition is lossless for every
    /// shape the field can take; anything unparseable lands as `(0, 0)` and draws no row at all.
    pub version: (i64, i64),
    /// `DOVIBLPresent` — the base layer is in the file.
    pub bl_present: bool,
    /// `DOVIRPUPresent` — the RPU (the dynamic metadata) is in the file.
    pub rpu_present: bool,
}

impl Dovi {
    /// The all-zero record — "the server said nothing about Dolby Vision", which is also exactly
    /// what an ordinary SDR file produces. A `const` rather than [`Default::default`] because
    /// `route`'s idle `Session` is a `const` item and cannot call one.
    pub const NONE: Dovi = Dovi {
        present: false,
        profile: 0,
        bl_compat: 0,
        el_present: false,
        level: 0,
        version: (0, 0),
        bl_present: false,
        rpu_present: false,
    };

    /// PURE: **true when the base layer on its own is not a picture we can put on the panel
    /// correctly** — i.e. when this bitstream, decoded as ordinary HEVC by a pipeline that was
    /// never TOLD it is Dolby Vision, shows the user wrong colours (or nothing).
    ///
    /// Note the "never told" clause: it is the whole question this predicate asks, and since
    /// 2026-08-21 it is no longer the only thing we can do — [`Dovi::presentation`] can declare
    /// the stream to the pipeline, and a declared Profile 5 is displayed correctly. What survives
    /// unchanged is every path where the bitstream reaches a decoder with NO declaration attached,
    /// and that is now this predicate's job:
    ///
    /// - the direct-play gate **when no node will be sent** (`presentation` is written in terms of
    ///   this, so the two can never contradict each other), and
    /// - the server's permission to **COPY** the video — a remux or a `directStream` transcode
    ///   hands us the identical elementary stream one container down, and the Load payload built
    ///   for that path declares nothing. `route::build_stream` reads it for both
    ///   ([`plx_plex::plex::TranscodeSpec::no_video_copy`] and the `remux` gate) and that is why a
    ///   declared Profile 5 still refuses a copy: the declaration rides the DIRECT PLAY, not the
    ///   file, so the same pixels arriving by another route are as wrong as they ever were.
    ///
    /// Two disqualifiers, and they are found by different fields:
    /// - **an enhancement layer** (`el_present`, Profile 7) — one elementary stream is all the
    ///   pipeline feeds, so the other layer simply never arrives. This is the only test that
    ///   catches P7: measured live 2026-08-21, the dev server's P7 item reports `bl_compat = 6`,
    ///   which sails through any `== 0` check.
    /// - **no cross-compatible base layer** (Profile 5 / `bl_compat == 0`) — IPT-PQ, which an
    ///   HEVC decoder will happily decode and a panel will happily show, incorrectly.
    ///
    /// **Silence must not convict, and that is the whole subtlety here.** Every field is 0 both
    /// when the server omits it and when there is no Dolby Vision at all, so a bare
    /// `bl_compat == 0` would refuse direct play for *every ordinary SDR file in the library*.
    /// `present` guards the outer question and a KNOWN `profile` guards the compat-id test, so a
    /// server that reports `DOVIPresent` and nothing else falls through to the existing gates
    /// unchanged. That direction is deliberate, and the price of getting it wrong is higher than
    /// it first looks: a true answer here does not merely reroute an item, it also withdraws the
    /// server's permission to COPY the video
    /// ([`plx_plex::plex::TranscodeSpec::no_video_copy`] — without which the refusal accomplishes
    /// nothing at all), and a server that cannot encode the result then refuses the playback
    /// outright. So a false positive costs the film, not just its 4K and its HDR10. The same
    /// misread-degrades-to-assumed rule `crate::route::video_direct_plays` applies to an unknown
    /// frame size, applied to a field whose silence is indistinguishable from a legitimate zero.
    ///
    /// The one measured reassurance, and it is worth having before trusting the `profile > 0`
    /// guard: every Dolby Vision stream on the dev server (34 of them, swept 2026-08-21) sends all
    /// eight `DOVI*` keys together. No shape there reports a profile without also reporting a
    /// compatibility id, which is the only combination that guard could misread.
    pub fn base_layer_unusable(&self) -> bool {
        if !self.present {
            return false;
        }
        self.el_present || self.profile == 5 || (self.profile > 0 && self.bl_compat == 0)
    }

    /// PURE: **how this stream will be presented** — the ONE predicate behind both halves of the
    /// Dolby Vision decision. The direct-play gate and the eventual Load payload receive this
    /// same value, frozen into route state rather than re-reading capability at different times.
    ///
    /// `signal` retains `nodv`'s diagnostic asymmetry: it withholds a Profile-5-style declaration,
    /// but does not suppress a compatible Profile 8 on a supported set. `capability` must be a
    /// definite [`Supported`](plx_platform::devcaps::dv::DvCapability::Supported), and
    /// `video_is_hevc` closes the old Profile 9 disagreement where the gate declared AVC and the
    /// payload's H265 guard silently discarded the node.
    ///
    /// The four arms, and why each is where it is:
    ///
    /// - **not `present`** → [`DvPresentation::NotDv`]. No Dolby Vision, nothing to say, and
    ///   nothing to refuse. This is every ordinary file in the library.
    /// - **`el_present`** → refuse, always, `signal` or not. Profile 7 splits its picture across a
    ///   base and an enhancement layer; the pipeline feeds ONE elementary stream and cannot
    ///   interleave the other, so no payload key makes it displayable. (This is also the only
    ///   thing that identifies a dual-layer file: the dev server's P7 reports `bl_compat = 6`.)
    ///   It is deliberately checked BEFORE the declaration arm — which is what keeps the emitted
    ///   node's `trackType` at `"single"` and, with `encryptionType` fixed at `"clear"`, makes the
    ///   pipeline's `dv-dual-svp` secure-video-path flag unreachable. We cannot satisfy that flag.
    /// - **no node will be sent** (capability absent/unknown, non-HEVC output, a profile the
    ///   server never named, or Profile 5 with the trigger armed) → fall back to
    ///   exactly the pre-declaration rule: refuse iff [`base_layer_unusable`](Self::base_layer_unusable).
    ///   That is what makes "keep the refusal for any case where we would not send the node" a
    ///   property of the code rather than of a reviewer's memory. The `profile <= 0` half also
    ///   preserves the never-convict-on-silence rule: a server that reports `DOVIPresent` and
    ///   nothing else still falls through to `NotDv` and plays as it always has, because we cannot
    ///   tell that silence from an SDR file, and `getInt` wants a real profile id anyway.
    /// - otherwise → **declare it**, and direct play is then correct — including for Profile 5,
    ///   whose refusal this inverts. The decompile of this TV's own `libpf` (2026-08-21) is the
    ///   evidence: `CustomPipeline::parseOptionStringSpi` sets `hasDolbyHdrInfo` on the mere
    ///   PRESENCE of the key, and `getVideoCaps` then adds `dolby-vision=TRUE` (+ the profile
    ///   hint) to the `video/x-h265` caps it was already going to build. The codec string does not
    ///   change; the node is the entire difference between an IPT-PQ stream shown in wrong colours
    ///   and one the panel puts in Dolby Vision mode.
    pub fn presentation(
        &self,
        signal: bool,
        capability: plx_platform::devcaps::dv::DvCapability,
        video_is_hevc: bool,
    ) -> DvPresentation {
        if !self.present {
            return DvPresentation::NotDv;
        }
        if self.el_present {
            return DvPresentation::Refuse("dual-layer");
        }
        // Presence of our node enables libpf's DV path even on a television which cannot display
        // it. libplayerAPIs' own platform metadata does not protect that seam, so only this app's
        // affirmative configd result may make the declaration eligible.
        let declare = capability == plx_platform::devcaps::dv::DvCapability::Supported
            && video_is_hevc
            && (signal || !self.base_layer_unusable());
        if !declare || self.profile <= 0 {
            return if self.base_layer_unusable() {
                DvPresentation::Refuse("no cross-compatible base layer")
            } else {
                DvPresentation::NotDv
            };
        }
        DvPresentation::Declare(DolbyHdrInfo {
            profile_id: self.profile,
            // Honest derivation, and unreachable as `"dual"` while the `el_present` arm above
            // returns first — written this way so that the day an interleaver exists, the payload
            // follows the refusal being relaxed instead of quietly lying about the track.
            track_type: if self.el_present { "dual" } else { "single" },
            // Never `"all"`: paired with `trackType: "dual"` that is what sets `dv-dual-svp`, the
            // secure-video-path flag, which this app cannot satisfy.
            encryption_type: "clear",
        })
    }

}

/// The `option.externalStreamingInfo.contents.DolbyHdrInfo` node of the Starfish Load payload:
/// what we tell LG's pipeline about this stream's Dolby Vision.
///
/// The three fields are the ones the TV's own parser reads, at the paths and in the types the
/// decompile proved (`Options::checkKeyExistance` for the node itself, then `getInt` for
/// `profileId` and `getString` for the other two). `profileId` **must** be a JSON integer;
/// omitting it leaves the pipeline's `-1` sentinel, which still yields `dolby-vision=TRUE` with
/// only the profile hint missing — a legitimate fallback, not a failure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DolbyHdrInfo {
    /// `DOVIProfile` as the server reported it. 5 (single-layer IPT-PQ) is the case this exists
    /// for; 8.x declares fine too and gains the dynamic metadata its base layer alone lacks.
    pub profile_id: i64,
    /// `"single"` or `"dual"` — one elementary stream or a base + enhancement pair.
    pub track_type: &'static str,
    /// `"clear"`. See [`Dovi::presentation`] for why this is never `"all"`.
    pub encryption_type: &'static str,
}

/// What [`Dovi::presentation`] decided: the single value the direct-play gate and the Load payload
/// both read. Three states rather than a bool, because "there is no Dolby Vision here" and "there
/// is, and we are declaring it" are the same answer to the GATE and opposite answers to the
/// PAYLOAD — which is precisely the pair that used to be two predicates and could disagree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DvPresentation {
    /// Not Dolby Vision, or not identifiably so. Play it as ordinary HEVC, declare nothing.
    NotDv,
    /// Direct play, with this node spliced into the Load payload.
    Declare(DolbyHdrInfo),
    /// Direct play is refused, with the short reason for the log line at the decision.
    ///
    /// "cross-compatible" rather than "HDR10" deliberately: HDR10 (`bl_compat` 1) is merely the
    /// common case, and an SDR (2) or HLG (4) base layer is equally displayable. What Profile 5
    /// lacks is a base layer conformant to ANY ordinary transfer, which is what id 0 means.
    Refuse(&'static str),
}

/// The platform answer and the presentation derived from it at one route boundary. Both are
/// copyable so reload, recovery and rollback preserve the installed decision exactly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DvDecision {
    pub capability: plx_platform::devcaps::dv::DvCapability,
    pub presentation: DvPresentation,
}

impl DvDecision {
    pub const NONE: Self = Self {
        capability: plx_platform::devcaps::dv::DvCapability::Unknown,
        presentation: DvPresentation::NotDv,
    };
}

impl Default for DvDecision {
    fn default() -> Self {
        Self::NONE
    }
}

impl DvPresentation {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::NotDv => "base-layer",
            Self::Declare(_) => "declare",
            Self::Refuse(_) => "refuse",
        }
    }

    /// [`Self::label`] in the UI language, for the diagnostics read-out. Logs keep `label`.
    pub fn display(&self) -> &'static str {
        match self {
            Self::NotDv => plx_platform::i18n::msg::browse_diagnostics_dv_base_layer(),
            Self::Declare(_) => plx_platform::i18n::msg::browse_diagnostics_dv_declare(),
            Self::Refuse(_) => plx_platform::i18n::msg::browse_diagnostics_dv_refuse(),
        }
    }

    /// Direct play is refused (and, at `build_stream`, the reason for the log line).
    pub fn refusal(&self) -> Option<&'static str> {
        match self {
            Self::Refuse(why) => Some(why),
            _ => None,
        }
    }
    /// The node to splice into the Load payload, if any.
    pub fn declared(&self) -> Option<DolbyHdrInfo> {
        match self {
            Self::Declare(d) => Some(*d),
            _ => None,
        }
    }
}

plx_base::devtrig::latched_flag!(
    /// `/tmp/dispatcharrtv-dvnonode` — after a supported route has frozen `Declare`, keep its Dolby
    /// Vision **direct play** but send **no** `DolbyHdrInfo` node. Diagnostic only: it is the
    /// explicitly logged exception to gate/payload agreement.
    ///
    /// The two things that changed together the day Profile 5 first direct-played are the
    /// DECLARATION and the 4K HEVC direct play of a file that had never been fed before. Every
    /// other combination is reachable by choosing a title — a Profile 8 direct-plays with the node
    /// or without it, because its gate does not depend on the trigger — but the P5 file has only
    /// two states, "declared and direct-played" and "refused", since [`Dovi::presentation`]
    /// answers both halves from one value. This is the missing cell: the same bytes, the same
    /// path, the declaration alone removed.
    ///
    /// Applied at the payload (`crate::player::engine`), NEVER at the gate — suppressing the
    /// node at the gate would send the file to the transcoder and measure a different pipeline.
    /// Note the resulting picture is expected to be WRONG (an IPT-PQ stream shown as ordinary
    /// HDR); this knob is for judging cadence, not colour.
    pub fn dv_node_suppressed = "dvnonode";
);

plx_base::devtrig::latched_flag!(
    /// `/tmp/dispatcharrtv-nodv` — **withhold the Dolby Vision declaration**, for a bisect. The
    /// polarity is inverted from what it was, and the inversion is the point.
    ///
    /// This was `/tmp/dispatcharrtv-dv`, an opt-IN, default off, with a note in this doc saying to
    /// flip the default "once the node has been seen to put a correct picture on a real panel".
    /// The reason for the caution was real — the payload is the `sourceInfo` envelope, which the
    /// pipeline parses before anything decodes, and a malformed one does not fail loudly, it
    /// wedges the video sink. The condition has now been met, twice over and on the last profile
    /// that had not met it:
    ///
    /// - **the picture is correct** — Profile 5 direct-played, photographed, with the set's own
    ///   "Dolby Vision / Dolby Atmos" read-out on screen;
    /// - **and its one measured defect is fixed.** A declared P5 used to lose the display-
    ///   management lookup for 2 frames in 12 (a ~2 Hz tone pulse); that was one 90 kHz tick in
    ///   the LUT key and is gone — 3 misses in 90 s on the shipped default, against 160 in 45 s.
    ///   See `player::engine::pts_nudge_ns`.
    ///
    /// So every eligible Dolby Vision stream on a set with confirmed support is declared by
    /// default, and this knob only takes it away. Note what it does NOT do: withholding re-imposes
    /// the old refusal on Profile 5 (`base_layer_unusable`), so this bisects "declared vs
    /// transcoded", not "declared vs direct-played-undeclared". [`dv_node_suppressed`]
    /// (`/tmp/dispatcharrtv-dvnonode`) is the finer instrument for that, and is why both exist.
    ///
    /// Latched once per process so route decisions never observe a changing trigger. The stronger
    /// gate/payload guarantee now comes from storing [`DvDecision`] on the route: neither a later
    /// capability answer nor a reload re-evaluates the installed play.
    pub fn dv_withheld = "nodv";
);

/// Prewarm both payload diagnostics before the first frame scope. Their getters are subsequently
/// filesystem-free even when preview or payload construction first reaches them during a frame.
pub fn prewarm_dv_latches() {
    let _ = dv_withheld();
    let _ = dv_node_suppressed();
}

#[cfg(test)]
#[path = "metadata_dolby_vision_tests.rs"]
mod dolby_vision_tests;
