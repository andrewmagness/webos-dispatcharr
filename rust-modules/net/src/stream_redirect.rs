//! **HTTP redirects on the plaintext MEDIA path** — where a `3xx` answer to a plaintext media
//! open is turned into the next request. An https open is followed by `curlio`'s `next_hop`,
//! built on this module's `is_redirect`, `resolve_location` and `MAX_HOPS`.
//!
//! PMS answers some Part URLs (Plex-hosted trailers: `/services/iva/assets/…`) with a `302` to a
//! presigned CDN URL on another origin — typically https, sometimes a relative `Location`. The
//! socket client in [`super`] is one request per open and cannot speak TLS, so following is a
//! loop ABOVE it: [`open_following`] re-opens the same [`HttpStream`] for each plaintext hop and
//! stops at the first https hop, handing that target back for the caller to open through
//! `curlio` (which follows any further hops itself, one request per hop with libcurl's own
//! following off, applies this module's `MAX_HOPS`, and refuses any hop from TLS back to
//! plaintext).
//!
//! A hop is requested exactly as its `Location` spells it: the media plane carries no credential,
//! so nothing rides a hop but a `Range`, which rides every one. Log lines carry an origin and a
//! query-less path, never a query.
//!
//! The Dispatcharr API client does not come through here; its redirect policy is the per-request
//! `follow_redirects` option in `crate::net`, off by default.

use std::ffi::CString;
use std::os::raw::c_int;
use std::time::Instant;

use super::{
    hs_redirect_location, http_open_with_timeouts, log_endpoint, HttpOpenError, HttpStream,
    CONNECT_TIMEOUT_MS, MEDIA_RECV_TIMEOUT_MS, MEDIA_SEND_TIMEOUT_MS,
};
use plx_base::checkpoint::Checkpoint;
use crate::net::origin::{Origin, Scheme};

/// Hops followed after the first request. The first request plus this many redirects is the most
/// a media open will send before failing with [`FollowError::TooManyHops`]. `curlio` imports this
/// constant and counts the hops it follows itself against it; a target handed over from here
/// starts a fresh count there.
pub const MAX_HOPS: u32 = 5;

/// The statuses that carry a `Location` to re-request with the same method. 300 and 304 are not
/// redirects in that sense; 305/306 are deprecated.
pub fn is_redirect(status: c_int) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// One request target: an origin plus an absolute path with optional query (no fragment).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub origin: Origin,
    pub path: String,
}

impl Target {
    /// The full URL, for the curl source. Carries whatever query the target has — never log it.
    pub fn url(&self) -> String {
        format!("{}{}", self.origin.base(), self.path)
    }
}

/// Where a followed open ended.
#[derive(Debug)]
pub enum Opened {
    /// The stream holds a 2xx response for this target — the EFFECTIVE URL, which every later
    /// reopen (a Range seek) must use.
    Socket(Target),
    /// The chain reached an https target. Nothing was dialled for it: the socket client cannot
    /// speak TLS, so the caller opens [`Target::url`] through `curlio`.
    Tls(Target),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FollowError {
    /// A request of the chain failed; the stream's status field still holds its code.
    Open(HttpOpenError),
    /// More than [`MAX_HOPS`] redirects.
    TooManyHops,
    /// A redirect with no `Location`, or one this client cannot request (another scheme, a
    /// malformed authority, control bytes).
    BadLocation(c_int),
}

/// One media GET to be opened with redirects followed.
pub struct Request<'a> {
    pub origin: &'a Origin,
    /// Absolute path plus optional query.
    pub path: &'a str,
    /// `Range: bytes=N-` on every hop when set.
    pub range_from: Option<i64>,
    /// Absolute setup deadline for the whole chain (connect/send/headers of every hop).
    pub deadline: Option<Instant>,
}

/// Open `req` on `hs`, following plaintext redirects.
pub fn open_following(
    hs: *mut HttpStream,
    req: &Request,
    checkpoint: &mut dyn Checkpoint,
) -> Result<Opened, FollowError> {
    let mut cur = Target {
        origin: req.origin.clone(),
        path: req.path.to_owned(),
    };
    let mut hop: u32 = 0;
    loop {
        if cur.origin.is_tls() {
            return Ok(Opened::Tls(cur));
        }
        let mut extra = String::new();
        if let Some(at) = req.range_from {
            extra.push_str(&format!("Range: bytes={at}-\r\n"));
        }
        let (Ok(host), Ok(path), Ok(extra)) = (
            CString::new(cur.origin.host()),
            CString::new(cur.path.as_str()),
            CString::new(extra),
        ) else {
            return Err(FollowError::Open(HttpOpenError::Transport));
        };
        let extra_ptr = if extra.as_bytes().is_empty() {
            std::ptr::null()
        } else {
            extra.as_ptr()
        };
        let opened = http_open_with_timeouts(
            hs,
            host.as_ptr(),
            cur.origin.port() as c_int,
            path.as_ptr(),
            extra_ptr,
            "GET",
            CONNECT_TIMEOUT_MS,
            MEDIA_RECV_TIMEOUT_MS,
            MEDIA_SEND_TIMEOUT_MS,
            req.deadline,
            req.deadline.is_some(),
            checkpoint,
        );
        let status = match opened {
            Ok(()) => return Ok(Opened::Socket(cur)),
            Err(HttpOpenError::Status(status)) if is_redirect(status) => status,
            Err(e) => return Err(FollowError::Open(e)),
        };
        let next = hs_redirect_location(hs).and_then(|loc| resolve_location(&cur, &loc));
        let Some(next) = next else {
            plx_base::eventlog::log(&format!(
                "stream: redirect {status} from {}{} has no usable Location",
                cur.origin.log_form(),
                log_endpoint(&cur.path)
            ));
            return Err(FollowError::BadLocation(status));
        };
        if hop >= MAX_HOPS {
            plx_base::eventlog::log(&format!(
                "stream: redirect {status} -> {}{} REFUSED: more than {MAX_HOPS} hops",
                next.origin.log_form(),
                log_endpoint(&next.path)
            ));
            return Err(FollowError::TooManyHops);
        }
        hop += 1;
        let same = same_origin(&next.origin, req.origin);
        plx_base::eventlog::log(&format!(
            "stream: redirect {status} -> {}{} hop={hop} same_origin={same}",
            next.origin.log_form(),
            log_endpoint(&next.path)
        ));
        cur = next;
    }
}

/// Same scheme, host (ASCII case-insensitive) and port.
pub fn same_origin(a: &Origin, b: &Origin) -> bool {
    a.scheme() == b.scheme() && a.port() == b.port() && a.host().eq_ignore_ascii_case(b.host())
}

/// Resolve a `Location` against the target that answered it (RFC 3986 §5.2, restricted to what
/// an HTTP client needs). `None` for anything this client cannot request: a scheme other than
/// http/https, a malformed authority, or any whitespace/control byte (which would otherwise be
/// written into a request line).
pub fn resolve_location(base: &Target, location: &str) -> Option<Target> {
    let loc = location.trim_matches(|c| c == ' ' || c == '\t');
    if loc.is_empty() || loc.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return None;
    }
    let loc = loc.split('#').next().unwrap_or("");
    if loc.is_empty() {
        return None;
    }
    let lower = loc.get(..8).unwrap_or(loc).to_ascii_lowercase();
    let (scheme, rest) = if lower.starts_with("https://") {
        (Some(Scheme::Https), &loc[8..])
    } else if lower.starts_with("http://") {
        (Some(Scheme::Http), &loc[7..])
    } else if let Some(rest) = loc.strip_prefix("//") {
        (Some(base.origin.scheme()), rest)
    } else {
        (None, loc)
    };
    if let Some(scheme) = scheme {
        let end = rest.find(['/', '?']).unwrap_or(rest.len());
        let origin = parse_authority(scheme, &rest[..end])?;
        let tail = &rest[end..];
        let path = if tail.starts_with('/') {
            tail.to_owned()
        } else {
            format!("/{tail}")
        };
        return Some(Target {
            origin,
            path: normalize(&path),
        });
    }
    // Relative. A colon before the first '/' or '?' is another scheme (`ftp:`, `data:`).
    let first = loc.find(['/', '?']).unwrap_or(loc.len());
    if loc[..first].contains(':') {
        return None;
    }
    let base_path = base.path.split('?').next().unwrap_or("/");
    let path = if loc.starts_with('/') {
        loc.to_owned()
    } else if loc.starts_with('?') {
        format!("{base_path}{loc}")
    } else {
        let dir = match base_path.rfind('/') {
            Some(i) => &base_path[..=i],
            None => "/",
        };
        format!("{dir}{loc}")
    };
    Some(Target {
        origin: base.origin.clone(),
        path: normalize(&path),
    })
}

/// `host[:port]` or `[v6][:port]`, userinfo refused; the scheme's default port when none.
fn parse_authority(scheme: Scheme, auth: &str) -> Option<Origin> {
    if auth.contains('@') {
        return None;
    }
    let (host, port) = if let Some(rest) = auth.strip_prefix('[') {
        let close = rest.find(']')?;
        let after = &rest[close + 1..];
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p),
            None if after.is_empty() => None,
            None => return None,
        };
        (&rest[..close], port)
    } else {
        match auth.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (auth, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        None | Some("") => {
            if scheme.is_tls() {
                443
            } else {
                80
            }
        }
        Some(p) => match p.parse::<u16>() {
            Ok(n) if n > 0 => n as i32,
            _ => return None,
        },
    };
    Some(Origin::new(scheme, host, port))
}

/// Remove `.` and `..` segments from the path part (RFC 3986 §5.2.4); the query is untouched.
fn normalize(path_and_query: &str) -> String {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };
    let segs: Vec<&str> = path.split('/').collect();
    let mut out: Vec<&str> = Vec::new();
    let last = segs.len().saturating_sub(1);
    for (i, s) in segs.iter().enumerate().skip(1) {
        match *s {
            "." => {
                if i == last {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if i == last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    let mut p = format!("/{}", out.join("/"));
    if let Some(q) = query {
        p.push('?');
        p.push_str(q);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(scheme: Scheme, host: &str, port: i32, path: &str) -> Target {
        Target {
            origin: Origin::new(scheme, host, port),
            path: path.into(),
        }
    }

    fn pms() -> Target {
        base(
            Scheme::Http,
            "192.168.1.10",
            32400,
            "/library/parts/1/file.mp4?X-Plex-Token=tok",
        )
    }

    #[test]
    fn redirect_statuses() {
        for s in [301, 302, 303, 307, 308] {
            assert!(is_redirect(s), "{s}");
        }
        for s in [200, 206, 300, 304, 305, 400] {
            assert!(!is_redirect(s), "{s}");
        }
    }

    #[test]
    fn an_absolute_https_location_is_a_tls_target_on_443() {
        let t = resolve_location(&pms(), "https://cdn.example.com/v/1.mp4?sig=a&e=2").unwrap();
        assert!(t.origin.is_tls());
        assert_eq!(t.origin.host(), "cdn.example.com");
        assert_eq!(t.origin.port(), 443);
        assert_eq!(t.path, "/v/1.mp4?sig=a&e=2");
        assert_eq!(t.url(), "https://cdn.example.com:443/v/1.mp4?sig=a&e=2");
        assert!(!same_origin(&t.origin, &pms().origin));
    }

    #[test]
    fn an_absolute_http_location_defaults_to_80_and_keeps_explicit_ports() {
        let t = resolve_location(&pms(), "HTTP://cdn.example.com?x=1").unwrap();
        assert_eq!((t.origin.port(), t.path.as_str()), (80, "/?x=1"));
        let t = resolve_location(&pms(), "http://[::1]:8080/a#frag").unwrap();
        assert_eq!((t.origin.host(), t.origin.port()), ("::1", 8080));
        assert_eq!(t.path, "/a");
        let same = resolve_location(&pms(), "http://192.168.1.10:32400/x").unwrap();
        assert!(same_origin(&same.origin, &pms().origin));
    }

    #[test]
    fn relative_locations_resolve_against_the_answering_target() {
        let b = pms();
        assert_eq!(resolve_location(&b, "/a/b?c=1").unwrap().path, "/a/b?c=1");
        assert_eq!(
            resolve_location(&b, "other.mp4").unwrap().path,
            "/library/parts/1/other.mp4"
        );
        assert_eq!(
            resolve_location(&b, "../2/./x.mp4").unwrap().path,
            "/library/parts/2/x.mp4"
        );
        assert_eq!(
            resolve_location(&b, "?download=1").unwrap().path,
            "/library/parts/1/file.mp4?download=1"
        );
        let sr = resolve_location(&b, "//cdn.example.com/z").unwrap();
        assert_eq!(sr.origin.scheme(), Scheme::Http);
        assert_eq!(
            (sr.origin.host(), sr.origin.port()),
            ("cdn.example.com", 80)
        );
        assert!(same_origin(
            &resolve_location(&b, "x").unwrap().origin,
            &b.origin
        ));
    }

    #[test]
    fn unusable_locations_are_refused() {
        let b = pms();
        for bad in [
            "",
            "   ",
            "ftp://x/y",
            "data:text/plain,hi",
            "http://",
            "http://h:0/x",
            "http://h:99999/x",
            "http://user:pw@h/x",
            "http://[::1/x",
            "/a\r\nX-Injected: 1",
            "/a b",
        ] {
            assert_eq!(resolve_location(&b, bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_tls_origin_is_handed_back_without_dialling() {
        let origin = Origin::new(Scheme::Https, "127.0.0.1", 1);
        let mut hs = super::super::http_stream_boxed();
        let req = Request {
            origin: &origin,
            path: "/x",
            range_from: None,
            deadline: None,
        };
        match open_following(&mut *hs, &req, &mut plx_base::checkpoint::NoCheckpoint) {
            Ok(Opened::Tls(t)) => assert_eq!(t.url(), "https://127.0.0.1:1/x"),
            other => panic!("{other:?}"),
        }
    }

    /// A `Range` is not tied to an origin: it rides the cross-origin hop as well as the first.
    #[test]
    fn the_range_rides_a_cross_origin_hop() {
        use std::io::{Read, Write};
        let _serial = plx_base::testlock::serial();
        let serve = |reply: Vec<u8>| {
            let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = srv.local_addr().unwrap().port();
            let h = std::thread::spawn(move || {
                // A blocking listener: the accepted socket is blocking too.
                let (mut s, _) = srv.accept().unwrap();
                let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let mut raw = Vec::new();
                let mut c = [0u8; 2048];
                while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut c) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => raw.extend_from_slice(&c[..n]),
                    }
                }
                let _ = s.write_all(&reply);
                String::from_utf8_lossy(&raw).into_owned()
            });
            (port, h)
        };
        let (cdn, cdn_h) = serve(
            b"HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                .to_vec(),
        );
        let (pms, pms_h) = serve(
            format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{cdn}/v\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .into_bytes(),
        );
        let origin = Origin::http("127.0.0.1", pms as i32);
        let mut hs = super::super::http_stream_boxed();
        let req = Request {
            origin: &origin,
            path: "/p",
            range_from: Some(7),
            deadline: None,
        };
        let got = open_following(&mut *hs, &req, &mut plx_base::checkpoint::NoCheckpoint);
        super::super::http_close(&mut *hs);
        let (pms_head, cdn_head) = (pms_h.join().unwrap(), cdn_h.join().unwrap());
        match got {
            Ok(Opened::Socket(t)) => assert_eq!(t.origin.port(), cdn as i32),
            other => panic!("{other:?}"),
        }
        assert!(pms_head.contains("Range: bytes=7-"), "{pms_head}");
        assert!(cdn_head.contains("Range: bytes=7-"), "{cdn_head}");
    }
}
