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

}
