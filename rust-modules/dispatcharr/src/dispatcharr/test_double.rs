//! A loopback Dispatcharr for tests: a plaintext HTTP/1.1 double that records every request, and
//! the canned v0.32.0 responses. Compiled for this crate's tests and, through the `test-support`
//! feature, for the application's (`dev::dispatcharr_report`); never in a shipped build.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use super::client::Server;

/// Verbatim shape of v0.32.0's `ChannelViewSet.summary` (apps/channels/api_views.py).
pub const SUMMARY: &str = r#"[
  {"id": 7, "uuid": "0b6c1c1e-0000-4000-8000-000000000007", "name": "News 24", "logo_id": 3,
   "channel_number": 5.1, "epg_data_id": 12, "channel_group_id": 2},
  {"id": 3, "uuid": "0b6c1c1e-0000-4000-8000-000000000003", "name": "alpha", "logo_id": null,
   "channel_number": null, "epg_data_id": null, "channel_group_id": null},
  {"id": 9, "uuid": "0b6c1c1e-0000-4000-8000-000000000009", "name": "Movies", "logo_id": 4,
   "channel_number": 5.0, "epg_data_id": 13, "channel_group_id": 2, "added_later": true},
  {"id": 1, "uuid": "0b6c1c1e-0000-4000-8000-000000000001", "name": "Beta", "logo_id": null,
   "channel_number": null, "epg_data_id": null, "channel_group_id": 1}
]"#;

/// `GET /api/epg/epgdata/` for [`SUMMARY`]: rows 12 and 13 back "News 24" and "Movies"; 99 is
/// another source's row nobody uses.
pub const EPGDATA: &str = r#"[
  {"id": 12, "tvg_id": "news.example", "name": "News", "icon_url": null, "epg_source": 1},
  {"id": 13, "tvg_id": "movies.example", "name": "Movies", "icon_url": null, "epg_source": 1},
  {"id": 99, "tvg_id": "unused.example", "name": "Unused", "icon_url": null, "epg_source": 2}
]"#;

/// `GET /api/epg/grid/` for 2026-10-14 around 12:00Z, in both spellings the server writes: a real
/// programme from Django's encoder, a generated dummy one from `isoformat()` keyed by the
/// numberless channel's UUID, and one row with no key that the guide must drop.
pub const GRID: &str = r#"{"data":[
  {"id": 501, "start_time": "2026-10-14T11:30:00Z", "end_time": "2026-10-14T12:30:00Z", "title": "Midday News",
   "sub_title": null, "description": "Headlines.", "tvg_id": "news.example", "season": null, "episode": null,
   "is_new": true, "is_live": true, "is_premiere": false, "is_finale": false},
  {"id": 502, "start_time": "2026-10-14T12:30:00Z", "end_time": "2026-10-14T13:00:00Z", "title": "Weather",
   "sub_title": "", "description": null, "tvg_id": "news.example", "season": "3", "episode": 12,
   "is_new": false, "is_live": false, "is_premiere": false, "is_finale": false},
  {"id": "dummy-3-20261014T080000", "start_time": "2026-10-14T08:00:00+00:00", "end_time": "2026-10-14T12:00:00+00:00",
   "title": "alpha", "sub_title": null, "description": "alpha", "tvg_id": "0b6c1c1e-0000-4000-8000-000000000003",
   "custom_properties": null, "season": null, "episode": null, "is_new": false, "is_live": false, "is_premiere": false, "is_finale": false},
  {"id": 503, "start_time": "2026-10-14T12:00:00Z", "end_time": "2026-10-14T13:00:00Z", "title": "Orphan",
   "tvg_id": null}
]}"#;

/// One request the double received: method, path, the header lines, and the body.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: Vec<String>,
    pub body: String,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        let prefix = format!("{}:", name.to_ascii_lowercase());
        self.headers
            .iter()
            .find(|h| h.to_ascii_lowercase().starts_with(&prefix))
            .map(|h| h[prefix.len()..].trim())
    }
}

/// A plaintext HTTP/1.1 double: `route(method, path)` answers `(status, body)`; every request is
/// recorded. One connection per request (`Connection: close`).
pub fn spawn(route: impl Fn(&str, &str) -> (u16, String) + Send + 'static) -> (Server, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let head_end = loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(i + 4);
                }
            };
            let Some(head_end) = head_end else { continue };
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let mut lines = head.split("\r\n");
            let mut request_line = lines.next().unwrap_or_default().split_whitespace();
            let method = request_line.next().unwrap_or_default().to_owned();
            let path = request_line.next().unwrap_or_default().to_owned();
            let headers: Vec<String> = lines.filter(|l| !l.is_empty()).map(str::to_owned).collect();
            let length = headers
                .iter()
                .find_map(|h| h.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                .unwrap_or(0);
            while buf.len() < head_end + length {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let body = String::from_utf8_lossy(&buf[head_end..]).into_owned();
            let (status, reply) = route(&method, &path);
            log.lock().unwrap().push(Seen { method, path, headers, body });
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (Server::parse(&format!("127.0.0.1:{port}")).unwrap(), seen)
}

pub fn curl_ready() -> bool {
    let ready = plx_net::net::global_init() && plx_net::net::available();
    if !ready {
        eprintln!("libcurl unavailable on this host; skipping");
    }
    ready
}

/// A Dispatcharr double whose user starts with `existing_key` (or none).
pub fn dispatcharr(existing_key: Option<&'static str>) -> (Server, Arc<Mutex<Vec<Seen>>>) {
    spawn(move |method, target| match (method, target.split('?').next().unwrap_or_default()) {
        ("GET", "/api/core/version/") => (200, r#"{"version":"0.32.0","timestamp":null}"#.into()),
        ("POST", "/api/accounts/token/") => (200, r#"{"refresh":"r.r.r","access":"a.a.a"}"#.into()),
        ("GET", "/api/accounts/api-keys/") => match existing_key {
            Some(k) => (200, format!(r#"{{"key":"{k}"}}"#)),
            None => (200, r#"{"key":null}"#.into()),
        },
        ("POST", "/api/accounts/api-keys/generate/") => (201, r#"{"key":"NEWKEY","user":{"id":1}}"#.into()),
        ("GET", "/api/channels/channels/summary/") => (200, SUMMARY.into()),
        ("GET", "/api/channels/groups/") => (200, r#"[{"id":2,"name":"News","channel_count":2}]"#.into()),
        ("GET", "/api/epg/epgdata/") => (200, EPGDATA.into()),
        ("GET", "/api/epg/grid/") => (200, GRID.into()),
        _ => (404, r#"{"detail":"Not found."}"#.into()),
    })
}

