//! Strict parser and refresh state for the fixed PMS HLS shape.
//!
//! This deliberately is not a general HLS implementation. It accepts one master variant and
//! growing MPEG-TS media playlists, and rejects every feature that would change how bytes must be
//! assembled before they reach the demuxer (encryption, byte ranges, fMP4 maps and
//! discontinuities). URI resolution is also part of the parser: every child stays on the source
//! PMS origin, so a playlist cannot turn the media worker into an arbitrary URL fetcher.

use plx_plex::plex::{origin, Origin};
use std::fmt;
use std::time::Duration;

#[derive(Clone, PartialEq, Eq)]
pub struct Resource {
    pub origin: Origin,
    /// Absolute path plus optional query. Fragments are never accepted.
    pub path: String,
}

impl fmt::Debug for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The initial master URL normally carries X-Plex-Token in its query. Parsed values are
        // useful in assertions and diagnostics, but Debug must not become a second credential
        // logging surface.
        let path = match self.path.split_once('?') {
            Some((path, _)) => format!("{path}?<query>"),
            None => self.path.clone(),
        };
        f.debug_struct("Resource")
            .field("origin", &self.origin)
            .field("path", &path)
            .finish()
    }
}

impl Resource {
    pub fn new(origin: Origin, path: impl Into<String>) -> Result<Self, Error> {
        let path = normalize_absolute_path(&path.into())?;
        Ok(Self { origin, path })
    }

    /// Resolve one playlist URI. Relative and root-relative references inherit the base origin;
    /// an absolute HTTP(S) reference is accepted only when its parsed origin is exactly equal.
    pub fn resolve(&self, reference: &str) -> Result<Self, Error> {
        validate_uri_text(reference)?;
        if reference.is_empty() || reference.starts_with('?') {
            return Err(Error::InvalidUri("URI has no path"));
        }
        if reference.starts_with("//") {
            return Err(Error::CrossOrigin);
        }

        if has_scheme(reference) {
            if !(reference.starts_with("http://") || reference.starts_with("https://")) {
                return Err(Error::InvalidUri("unsupported URI scheme"));
            }
            let child_origin =
                Origin::parse(reference).ok_or(Error::InvalidUri("malformed absolute URI"))?;
            if child_origin != self.origin {
                return Err(Error::CrossOrigin);
            }
            let (_, path) = origin::split(reference);
            return Resource::new(child_origin, if path.is_empty() { "/" } else { path });
        }

        if reference.starts_with('/') {
            return Resource::new(self.origin.clone(), reference);
        }

        let base_path = self
            .path
            .split_once('?')
            .map_or(self.path.as_str(), |(path, _)| path);
        let slash = base_path
            .rfind('/')
            .ok_or(Error::InvalidUri("base URI has no directory"))?;
        Resource::new(
            self.origin.clone(),
            format!("{}{}", &base_path[..=slash], reference),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub sequence: u64,
    pub duration: Duration,
    pub resource: Resource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refresh {
    /// Segments not returned by any successful earlier refresh, in sequence order.
    pub new_segments: Vec<Segment>,
    /// Sticky: once a valid ENDLIST is observed, a later stale response cannot clear it.
    pub end_list: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    MissingHeader,
    Malformed { line: usize, reason: &'static str },
    InvalidUri(&'static str),
    CrossOrigin,
    UnsupportedTag(String),
    UnsupportedFeature(&'static str),
    MissingVariant,
    MultipleVariants,
    MissingTargetDuration,
    MissingMediaSequence,
    ChildIsNotPlaylist,
    SegmentIsNotMpegTs,
    SequenceOverflow,
    RefreshSourceChanged,
    SequenceGap { expected: u64, found: u64 },
    SegmentChanged(u64),
    UnknownOldSegment(u64),
    SegmentAfterEndList(u64),
    StaleEndList,
    MissingCredential,
    MultipleCredentials,
    CredentialChanged,
    DurationOverflow,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHeader => f.write_str("playlist does not start with #EXTM3U"),
            Self::Malformed { line, reason } => {
                write!(f, "malformed playlist at line {line}: {reason}")
            }
            Self::InvalidUri(reason) => write!(f, "invalid playlist URI: {reason}"),
            Self::CrossOrigin => f.write_str("playlist URI changes origin"),
            Self::UnsupportedTag(tag) => write!(f, "unsupported HLS tag {tag}"),
            Self::UnsupportedFeature(feature) => write!(f, "unsupported HLS feature: {feature}"),
            Self::MissingVariant => f.write_str("master playlist has no variant"),
            Self::MultipleVariants => f.write_str("master playlist has more than one variant"),
            Self::MissingTargetDuration => f.write_str("media playlist has no target duration"),
            Self::MissingMediaSequence => f.write_str("media playlist has no media sequence"),
            Self::ChildIsNotPlaylist => f.write_str("master child is not an m3u8 playlist"),
            Self::SegmentIsNotMpegTs => f.write_str("media segment is not MPEG-TS"),
            Self::SequenceOverflow => f.write_str("media sequence overflows u64"),
            Self::RefreshSourceChanged => f.write_str("refresh belongs to another media playlist"),
            Self::SequenceGap { expected, found } => {
                write!(f, "media sequence gap: expected {expected}, found {found}")
            }
            Self::SegmentChanged(sequence) => {
                write!(f, "segment {sequence} changed across refreshes")
            }
            Self::UnknownOldSegment(sequence) => {
                write!(f, "refresh introduced unseen old segment {sequence}")
            }
            Self::SegmentAfterEndList(sequence) => {
                write!(f, "segment {sequence} appeared after ENDLIST")
            }
            Self::StaleEndList => f.write_str("ENDLIST came from a playlist older than the cursor"),
            Self::MissingCredential => f.write_str("HLS master URL has no Plex credential"),
            Self::MultipleCredentials => {
                f.write_str("HLS master URL has multiple Plex credentials")
            }
            Self::CredentialChanged => f.write_str("playlist URI replaces the route credential"),
            Self::DurationOverflow => f.write_str("HLS timeline duration overflows"),
        }
    }
}

impl std::error::Error for Error {}

fn has_scheme(uri: &str) -> bool {
    let Some(colon) = uri.find(':') else {
        return false;
    };
    let first_delimiter = uri.find(['/', '?']).unwrap_or(uri.len());
    if colon > first_delimiter || colon == 0 {
        return false;
    }
    let scheme = &uri[..colon];
    scheme.as_bytes()[0].is_ascii_alphabetic()
        && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

fn normalize_absolute_path(path: &str) -> Result<String, Error> {
    validate_uri_text(path)?;
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(Error::InvalidUri("path is not origin-relative"));
    }
    let (raw_path, query) = path
        .split_once('?')
        .map_or((path, None), |(path, query)| (path, Some(query)));
    if raw_path.is_empty() {
        return Err(Error::InvalidUri("URI has no path"));
    }

    let mut components = Vec::new();
    for component in raw_path[1..].split('/') {
        match component {
            "" if raw_path == "/" => {}
            "" => return Err(Error::InvalidUri("empty path component")),
            "." => {}
            ".." => {
                if components.pop().is_none() {
                    return Err(Error::InvalidUri("path escapes origin root"));
                }
            }
            _ => components.push(component),
        }
    }
    let mut normalized = if components.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", components.join("/"))
    };
    if let Some(query) = query {
        if query.is_empty() {
            return Err(Error::InvalidUri("empty query"));
        }
        normalized.push('?');
        normalized.push_str(query);
    }
    Ok(normalized)
}

fn validate_uri_text(uri: &str) -> Result<(), Error> {
    if uri.contains('#') {
        return Err(Error::InvalidUri("fragments are unsupported"));
    }
    if uri.contains('\\') {
        return Err(Error::InvalidUri("backslash in URI"));
    }
    let bytes = uri.as_bytes();
    if bytes
        .iter()
        .any(|b| !b.is_ascii() || b.is_ascii_control() || b.is_ascii_whitespace())
    {
        return Err(Error::InvalidUri("non-ASCII or whitespace byte"));
    }
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return Err(Error::InvalidUri("malformed percent escape"));
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(path: &str) -> Resource {
        Resource::new(Origin::http("192.0.2.10", 32400), path).unwrap()
    }

    fn media(path: &str, sequence: u64, names: &[&str], end_list: bool) -> MediaPlaylist {
        let source = source(path);
        let mut text = format!(
            "#EXTM3U\n#EXT-X-VERSION:4\n#EXT-X-TARGETDURATION:3\n#EXT-X-MEDIA-SEQUENCE:{sequence}\n"
        );
        for name in names {
            text.push_str("#EXTINF:2.002000, nodesc\n");
            text.push_str(name);
            text.push('\n');
        }
        if end_list {
            text.push_str("#EXT-X-ENDLIST\n");
        }
        parse_media(&source, &text).unwrap()
    }

    #[test]
    fn observed_one_variant_master_resolves_quoted_attributes_and_relative_child() {
        let start = source("/video/:/transcode/universal/start.m3u8?X-Plex-Token=secret");
        let parsed = parse_master(
            &start,
            "#EXTM3U\r\n#EXT-X-VERSION:4\r\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=3538000,RESOLUTION=1280x720,FRAME-RATE=60.000000,CODECS=\"avc1.640028,mp4a.40.2\"\r\nsession/example/base/index.m3u8\r\n",
        )
        .unwrap();

        assert_eq!(parsed.version, Some(4));
        assert_eq!(parsed.variant.bandwidth, 3_538_000);
        assert_eq!(
            parsed.variant.resource.path,
            "/video/:/transcode/universal/session/example/base/index.m3u8"
        );
        assert_eq!(parsed.variant.resource.origin, start.origin);
    }

    #[test]
    fn media_sequence_relative_segments_program_time_and_endlist_are_preserved() {
        let child = source("/session/example/base/index.m3u8");
        let parsed = parse_media(
            &child,
            "#EXTM3U\n#EXT-X-VERSION:4\n#EXT-X-TARGETDURATION:3\n#EXT-X-MEDIA-SEQUENCE:41\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-PROGRAM-DATE-TIME:2024-05-22T17:18:48.044851000Z\n#EXTINF:0.551044, nodesc\n../segments/00041.ts\n#EXTINF:1.001000, nodesc\n/absolute/00042.ts?part=2\n#EXT-X-ENDLIST\n",
        )
        .unwrap();

        assert_eq!(parsed.media_sequence, 41);
        assert_eq!(parsed.playlist_type, Some(PlaylistType::Event));
        assert_eq!(parsed.segments[0].sequence, 41);
        assert_eq!(parsed.segments[0].duration, Duration::new(0, 551_044_000));
        assert_eq!(
            parsed.segments[0].resource.path,
            "/session/example/segments/00041.ts"
        );
        assert_eq!(parsed.segments[1].sequence, 42);
        assert_eq!(
            parsed.segments[1].resource.path,
            "/absolute/00042.ts?part=2"
        );
        assert!(parsed.end_list);
    }

    #[test]
    fn exact_same_origin_absolute_uri_is_allowed_but_other_origins_are_rejected() {
        let master = source("/start.m3u8");
        let same = parse_master(
            &master,
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nhttp://192.0.2.10:32400/session/index.m3u8\n",
        )
        .unwrap();
        assert_eq!(same.variant.resource.path, "/session/index.m3u8");

        let cross = parse_master(
            &master,
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nhttps://cdn.example/session/index.m3u8\n",
        );
        assert_eq!(cross, Err(Error::CrossOrigin));
        assert_eq!(
            master.resolve("//cdn.example/00000.ts"),
            Err(Error::CrossOrigin)
        );
    }

    #[test]
    fn overlapping_refreshes_emit_each_sequence_once_and_keep_endlist_sticky() {
        let first = media("/session/index.m3u8", 7, &["00007.ts", "00008.ts"], false);
        let second = media(
            "/session/index.m3u8",
            8,
            &["00008.ts", "00009.ts", "00010.ts"],
            true,
        );
        let stale_without_endlist = media(
            "/session/index.m3u8",
            8,
            &["00008.ts", "00009.ts", "00010.ts"],
            false,
        );
        let mut tracker = MediaTracker::default();

        assert_eq!(
            tracker
                .apply(&first)
                .unwrap()
                .new_segments
                .iter()
                .map(|segment| segment.sequence)
                .collect::<Vec<_>>(),
            vec![7, 8]
        );
        let refresh = tracker.apply(&second).unwrap();
        assert_eq!(
            refresh
                .new_segments
                .iter()
                .map(|segment| segment.sequence)
                .collect::<Vec<_>>(),
            vec![9, 10]
        );
        assert!(refresh.end_list);
        assert!(tracker.apply(&second).unwrap().new_segments.is_empty());
        assert!(tracker.apply(&stale_without_endlist).unwrap().end_list);
    }

    #[test]
    fn failed_refresh_does_not_advance_the_dedupe_cursor() {
        let first = media("/session/index.m3u8", 4, &["00004.ts"], false);
        let mut gap = media(
            "/session/index.m3u8",
            5,
            &["00005.ts", "00006.ts", "00008.ts"],
            false,
        );
        // The parser itself assigns consecutive numbers. Corrupt the pure input here to prove
        // the tracker's transaction boundary independently of parser validation.
        gap.segments[2].sequence = 8;
        let valid = media(
            "/session/index.m3u8",
            5,
            &["00005.ts", "00006.ts", "00007.ts"],
            false,
        );
        let mut tracker = MediaTracker::default();
        tracker.apply(&first).unwrap();

        assert_eq!(
            tracker.apply(&gap),
            Err(Error::SequenceGap {
                expected: 7,
                found: 8
            })
        );
        assert_eq!(
            tracker
                .apply(&valid)
                .unwrap()
                .new_segments
                .iter()
                .map(|segment| segment.sequence)
                .collect::<Vec<_>>(),
            vec![5, 6, 7]
        );
    }

    #[test]
    fn changed_overlap_gap_new_source_and_post_end_growth_are_rejected() {
        let first = media("/session/index.m3u8", 10, &["00010.ts", "00011.ts"], false);
        let changed = media(
            "/session/index.m3u8",
            11,
            &["changed.ts", "00012.ts"],
            false,
        );
        let gap = media("/session/index.m3u8", 13, &["00013.ts"], false);
        let other = media("/other/index.m3u8", 12, &["00012.ts"], false);
        let ending = media("/session/index.m3u8", 11, &["00011.ts", "00012.ts"], true);
        let growth = media("/session/index.m3u8", 12, &["00012.ts", "00013.ts"], true);
        let mut tracker = MediaTracker::default();
        tracker.apply(&first).unwrap();

        assert_eq!(tracker.apply(&changed), Err(Error::SegmentChanged(11)));
        assert_eq!(
            tracker.apply(&gap),
            Err(Error::SequenceGap {
                expected: 12,
                found: 13
            })
        );
        assert_eq!(tracker.apply(&other), Err(Error::RefreshSourceChanged));
        tracker.apply(&ending).unwrap();
        assert_eq!(tracker.apply(&growth), Err(Error::SegmentAfterEndList(13)));
    }

    #[test]
    fn multiple_variants_are_rejected() {
        let result = parse_master(
            &source("/start.m3u8"),
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000\na.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2000\nb.m3u8\n",
        );
        assert_eq!(result, Err(Error::MultipleVariants));
    }

    #[test]
    fn byte_assembly_features_are_rejected_explicitly() {
        let cases = [
            (
                "#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"",
                "encrypted segments",
            ),
            ("#EXT-X-BYTERANGE:1000@0", "byte-range segments"),
            ("#EXT-X-MAP:URI=\"init.mp4\"", "fMP4 initialization map"),
            ("#EXT-X-DISCONTINUITY", "discontinuities"),
            ("#EXT-X-DISCONTINUITY-SEQUENCE:4", "discontinuities"),
        ];
        for (tag, feature) in cases {
            let text = format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:3\n#EXT-X-MEDIA-SEQUENCE:0\n{tag}\n#EXTINF:1.0,\n00000.ts\n"
            );
            assert_eq!(
                parse_media(&source("/session/index.m3u8"), &text),
                Err(Error::UnsupportedFeature(feature)),
                "{tag}"
            );
        }
    }

    #[test]
    fn fmp4_segment_without_a_map_is_still_rejected() {
        let result = parse_media(
            &source("/session/index.m3u8"),
            "#EXTM3U\n#EXT-X-TARGETDURATION:3\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:1.0,\n00000.m4s\n",
        );
        assert_eq!(result, Err(Error::SegmentIsNotMpegTs));
    }

    #[test]
    fn malformed_and_unknown_forms_fail_closed() {
        let base = source("/session/index.m3u8");
        let cases = [
            ("#EXT-X-TARGETDURATION:3\n", Error::MissingHeader),
            (
                "#EXTM3U\n#EXT-X-TARGETDURATION:3\n#EXT-X-MEDIA-SEQUENCE:0\n00000.ts\n",
                Error::Malformed { line: 4, reason: "segment URI has no EXTINF" },
            ),
            (
                "#EXTM3U\n#EXT-X-TARGETDURATION:3\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:nan,\n00000.ts\n",
                Error::Malformed { line: 4, reason: "invalid EXTINF duration" },
            ),
            (
                "#EXTM3U\n#EXT-X-TARGETDURATION:3\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PART:DURATION=1,URI=\"part.ts\"\n",
                Error::UnsupportedTag("#EXT-X-PART".into()),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_media(&base, text), Err(expected));
        }
    }

    #[test]
    fn path_resolution_normalizes_dot_segments_and_rejects_ambiguous_uris() {
        let base = source("/a/b/index.m3u8?token=x");
        assert_eq!(
            base.resolve("../c/./00001.ts").unwrap().path,
            "/a/c/00001.ts"
        );
        assert!(matches!(
            base.resolve("../../../escape.ts"),
            Err(Error::InvalidUri(_))
        ));
        assert!(matches!(
            base.resolve("bad%2.ts"),
            Err(Error::InvalidUri(_))
        ));
        assert!(matches!(
            base.resolve("data:text/plain,x"),
            Err(Error::InvalidUri(_))
        ));
        assert!(matches!(
            base.resolve("space here.ts"),
            Err(Error::InvalidUri(_))
        ));
    }

    #[test]
    fn resource_debug_never_prints_a_playlist_query() {
        let resource = source("/session/index.m3u8?X-Plex-Token=not-for-a-log&part=2");
        let debug = format!("{resource:?}");
        assert!(debug.contains("/session/index.m3u8?<query>"));
        assert!(!debug.contains("not-for-a-log"));
        assert!(!debug.contains("part=2"));
    }

    #[test]
    fn observed_sanitized_plex_corpus_accepts_legacy_allow_cache() {
        let master = source("/video/:/transcode/universal/start.m3u8?X-Plex-Token=secret");
        let master = parse_master(
            &master,
            "#EXTM3U\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=575000,RESOLUTION=480x200,FRAME-RATE=24.000\nsession/sanitized/base/index.m3u8\n",
        )
        .unwrap();
        let media = parse_media(
            &master.variant.resource,
            "#EXTM3U\n#EXT-X-TARGETDURATION:3\n#EXT-X-START:TIME-OFFSET=39.000000\n#EXT-X-ALLOW-CACHE:NO\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2.002000,\n00000.ts\n#EXTINF:2.002000,\n00001.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        assert_eq!(media.start_offset_micros, Some(39_000_000));
        assert_eq!(media.segments.len(), 2);
        assert!(media.end_list);
    }

    #[test]
    fn start_offset_accepts_the_signed_decimal_shape_but_rejects_extensions() {
        let base = source("/session/index.m3u8");
        let playlist = |tag: &str| {
            format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:3\n{tag}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2,\n00000.ts\n"
            )
        };
        assert_eq!(
            parse_media(&base, &playlist("#EXT-X-START:TIME-OFFSET=-1.25"))
                .unwrap()
                .start_offset_micros,
            Some(-1_250_000)
        );
        for tag in [
            "#EXT-X-START:TIME-OFFSET=nan",
            "#EXT-X-START:TIME-OFFSET=1.0000001",
            "#EXT-X-START:TIME-OFFSET=1,PRECISE=YES",
            "#EXT-X-START:PRECISE=YES,TIME-OFFSET=1",
        ] {
            assert!(
                matches!(
                    parse_media(&base, &playlist(tag)),
                    Err(Error::Malformed { .. })
                ),
                "{tag}"
            );
        }
        let duplicate = playlist("#EXT-X-START:TIME-OFFSET=1").replacen(
            "#EXT-X-MEDIA-SEQUENCE",
            "#EXT-X-START:TIME-OFFSET=2\n#EXT-X-MEDIA-SEQUENCE",
            1,
        );
        assert_eq!(
            parse_media(&base, &duplicate),
            Err(Error::Malformed {
                line: 4,
                reason: "duplicate start tag"
            })
        );
    }

    #[test]
    fn start_offset_selects_its_segment_and_total_duration_is_the_whole_movie() {
        let base = source("/session/index.m3u8");
        let parsed = parse_media(
            &base,
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-START:TIME-OFFSET=3.000000\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2,\n00000.ts\n#EXTINF:2,\n00001.ts\n#EXTINF:2,\n00002.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        assert_eq!(parsed.total_duration(), Ok(Duration::from_secs(6)));
        assert_eq!(parsed.preferred_start_index(), Ok(1));

        let mut negative = parsed.clone();
        negative.start_offset_micros = Some(-2_000_000);
        assert_eq!(negative.preferred_start_index(), Ok(2));
        negative.start_offset_micros = Some(6_000_000);
        assert_eq!(negative.preferred_start_index(), Ok(3));
    }

    #[test]
    fn transport_inherits_auth_without_exposing_or_replacing_it() {
        let master = source("/start.m3u8?session=s&X-Plex-Token=top-secret");
        let auth = InheritedAuth::capture(&master).unwrap();
        let child = master.resolve("session/index.m3u8?part=2").unwrap();
        assert_eq!(
            auth.request_path(&child).unwrap(),
            "/session/index.m3u8?part=2&X-Plex-Token=top-secret"
        );
        assert_eq!(format!("{auth:?}"), "InheritedAuth(<redacted>)");
        assert_eq!(
            auth.request_path(&source("/segment.ts?X-Plex-Token=other")),
            Err(Error::CredentialChanged)
        );
        assert_eq!(
            auth.request_path(&source(
                "/segment.ts?X-Plex-Token=top-secret&x-plex-token=top-secret"
            )),
            Err(Error::MultipleCredentials)
        );
    }

    #[test]
    fn fresh_segment_clocks_form_one_normalized_millisecond_timeline() {
        let mut timeline = SegmentTimeline::default();
        let mut first = timeline.begin(Duration::new(2, 2_000_000)).unwrap();
        assert_eq!(first.normalize_video(900_000_000_000), MediaTimeMs(0));
        assert_eq!(first.normalize_video(901_125_900_000), MediaTimeMs(1_125));
        // A later physically-arriving audio packet gets its own lane origin, rather than an
        // arbitrary 1.1-second skew inherited from packet order.
        assert_eq!(first.normalize_audio(899_980_000_000), MediaTimeMs(0));
        assert_eq!(first.normalize_audio(900_000_000_000), MediaTimeMs(20));
        timeline.commit(first);

        let mut second = timeline.begin(Duration::from_secs(2)).unwrap();
        assert_eq!(second.normalize_video(7_000_000_000), MediaTimeMs(2_002));
        assert_eq!(second.normalize_video(7_020_000_000), MediaTimeMs(2_022));
        timeline.commit(second);
        assert_eq!(timeline.end(), MediaTimeMs(4_002));
        assert_eq!(timeline.end_ns(), 4_002_000_000);
    }

    #[test]
    fn an_exact_fractional_start_boundary_selects_the_next_segment() {
        let base = source("/session/index.m3u8");
        let parsed = parse_media(
            &base,
            "#EXTM3U\n#EXT-X-TARGETDURATION:3\n#EXT-X-START:TIME-OFFSET=2.002000\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2.002000,\n00000.ts\n#EXTINF:2.002000,\n00001.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        assert_eq!(parsed.preferred_start_index(), Ok(1));

        let mut rounded = parsed;
        rounded.start_offset_micros = Some(2_000_000);
        assert_eq!(
            rounded.preferred_start_index(),
            Ok(0),
            "whole-second flooring repeats the segment which already played",
        );
    }
}
