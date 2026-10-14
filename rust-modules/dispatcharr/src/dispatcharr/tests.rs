//! Parsing and error mapping against v0.32.0's response shapes, and the whole sign-in sequence
//! against a loopback double that records every request it was sent.

use std::net::TcpListener;

use super::api::Account;
use super::auth;
use super::client::{self, Server};
use super::error::Error;
use super::models::{Channel, ServerVersion};
use super::guide::{Guide, Window};
use super::models::{EpgData, Programme};
use super::test_double::{curl_ready, dispatcharr, spawn, SUMMARY};
use super::time::{format_iso8601, parse_iso8601};

// ---- address parsing ----------------------------------------------------------------------

#[test]
fn a_bare_host_means_dispatcharrs_default_port_over_http() {
    assert_eq!(Server::parse("192.168.1.10").unwrap().base(), "http://192.168.1.10:9191");
    assert_eq!(Server::parse(" 192.168.1.10:8080/ ").unwrap().base(), "http://192.168.1.10:8080");
}

#[test]
fn a_written_scheme_without_a_port_means_that_schemes_port() {
    assert_eq!(Server::parse("http://tv.example.com").unwrap().base(), "http://tv.example.com:80");
    assert_eq!(Server::parse("https://tv.example.com/").unwrap().base(), "https://tv.example.com:443");
    assert_eq!(Server::parse("HTTPS://tv.example.com:8443").unwrap().base(), "https://tv.example.com:8443");
    assert!(!Server::parse("https://tv.example.com").unwrap().is_plaintext());
    assert!(Server::parse("tv.local").unwrap().is_plaintext());
}

#[test]
fn unusable_addresses_are_refused() {
    for bad in ["", "   ", "ftp://host", "http://", "host:99999", "two words"] {
        assert_eq!(Server::parse(bad), Err(Error::BadAddress), "{bad:?}");
    }
}

// ---- error mapping ------------------------------------------------------------------------

#[test]
fn each_status_keeps_its_own_meaning() {
    assert_eq!(Error::from_status(200), None);
    assert_eq!(Error::from_status(201), None);
    assert_eq!(Error::from_status(302), Some(Error::Redirected(302)));
    assert_eq!(Error::from_status(401), Some(Error::Unauthorized));
    assert_eq!(Error::from_status(403), Some(Error::Forbidden));
    assert_eq!(Error::from_status(404), Some(Error::NotFound));
    assert_eq!(Error::from_status(429), Some(Error::TooManyRequests));
    assert_eq!(Error::from_status(503), Some(Error::Unavailable));
    assert_eq!(Error::from_status(500), Some(Error::Http(500)));
    assert!(Error::Unauthorized.needs_sign_in());
    assert!(!Error::Forbidden.needs_sign_in());
}

// ---- response shapes ----------------------------------------------------------------------


#[test]
fn the_summary_parses_with_float_and_null_numbers_and_unknown_fields() {
    let channels: Vec<Channel> = client::parse("summary", SUMMARY.as_bytes()).unwrap();
    assert_eq!(channels.len(), 4);
    assert_eq!(channels[0].channel_number, Some(5.1));
    assert_eq!(channels[1].logo_id, None);
    let labels: Vec<String> = channels.iter().map(Channel::number_label).collect();
    assert_eq!(labels, ["5.1", "", "5", ""]);
}

#[test]
fn guide_order_is_by_number_then_numberless_by_name() {
    let mut channels: Vec<Channel> = client::parse("summary", SUMMARY.as_bytes()).unwrap();
    channels.sort_by(Channel::guide_order);
    let names: Vec<&str> = channels.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Movies", "News 24", "alpha", "Beta"]);
}

#[test]
fn a_misshapen_body_names_the_endpoint() {
    let err = client::parse::<Vec<Channel>>("channels/summary", br#"{"results": []}"#).unwrap_err();
    assert!(matches!(&err, Error::BadResponse(m) if m.starts_with("channels/summary:")), "{err:?}");
}

#[test]
fn versions_parse_and_only_the_032_line_is_tested() {
    let v = |s: &str| ServerVersion { version: s.to_owned(), timestamp: None };
    assert_eq!(v("0.32.0").triple(), Some((0, 32, 0)));
    assert_eq!(v("v0.32.4-dev").triple(), Some((0, 32, 4)));
    assert_eq!(v("1.2").triple(), Some((1, 2, 0)));
    assert_eq!(v("banana").triple(), None);
    assert!(v("0.32.7").is_tested());
    assert!(!v("0.31.0").is_tested());
    assert!(!v("0.33.0").is_tested());
}

// ---- the sign-in sequence against a loopback double --------------------------------------

#[test]
fn sign_in_reuses_an_existing_key_and_never_regenerates_it() {
    if !curl_ready() {
        return;
    }
    let (server, seen) = dispatcharr(Some("OLDKEY"));
    let signed = auth::sign_in(&server, "andrew", "p@ss \"word\"").unwrap();
    assert_eq!(signed.api_key, "OLDKEY");
    assert!(!signed.key_generated);
    assert!(signed.version.is_tested());

    let seen = seen.lock().unwrap();
    let calls: Vec<(&str, &str)> = seen.iter().map(|s| (s.method.as_str(), s.path.as_str())).collect();
    assert_eq!(
        calls,
        [("GET", "/api/core/version/"), ("POST", "/api/accounts/token/"), ("GET", "/api/accounts/api-keys/")]
    );
    // The version probe carries no credential; the password is JSON-escaped in the token body.
    assert_eq!(seen[0].header("authorization"), None);
    let creds: serde_json::Value = serde_json::from_str(&seen[1].body).unwrap();
    assert_eq!(creds["username"], "andrew");
    assert_eq!(creds["password"], "p@ss \"word\"");
    assert_eq!(seen[1].header("content-type"), Some("application/json"));
    assert_eq!(seen[2].header("authorization"), Some("Bearer a.a.a"));
}

#[test]
fn sign_in_generates_a_key_only_when_the_user_has_none() {
    if !curl_ready() {
        return;
    }
    let (server, seen) = dispatcharr(None);
    let signed = auth::sign_in(&server, "u", "p").unwrap();
    assert_eq!(signed.api_key, "NEWKEY");
    assert!(signed.key_generated);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.last().unwrap().path, "/api/accounts/api-keys/generate/");
    assert_eq!(seen.last().unwrap().header("authorization"), Some("Bearer a.a.a"));
}

#[test]
fn the_api_key_authenticates_the_channel_reads() {
    if !curl_ready() {
        return;
    }
    let (server, seen) = dispatcharr(Some("OLDKEY"));
    let account = Account { server, api_key: "OLDKEY".into() };
    let channels = account.channels().unwrap();
    assert_eq!(channels.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Movies", "News 24", "alpha", "Beta"]);
    assert_eq!(account.groups().unwrap()[0].name, "News");
    let seen = seen.lock().unwrap();
    assert!(seen.iter().all(|s| s.header("x-api-key") == Some("OLDKEY")));
    assert!(account.stream_url(&channels[0]).ends_with("/proxy/ts/stream/0b6c1c1e-0000-4000-8000-000000000009"));
}

#[test]
fn a_wrong_password_is_unauthorized_and_a_network_policy_refusal_is_forbidden() {
    if !curl_ready() {
        return;
    }
    for (status, want) in [(401, Error::Unauthorized), (403, Error::Forbidden), (429, Error::TooManyRequests)] {
        let (server, _) = spawn(move |method, path| match (method, path) {
            ("GET", "/api/core/version/") => (200, r#"{"version":"0.32.0"}"#.into()),
            _ => (status, r#"{"detail":"no"}"#.into()),
        });
        assert_eq!(auth::sign_in(&server, "u", "p"), Err(want));
    }
}

#[test]
fn an_http_server_that_is_not_dispatcharr_is_refused_before_the_password_is_sent() {
    if !curl_ready() {
        return;
    }
    for (status, body) in [(404, "<html>nope</html>"), (200, "<html>router login</html>")] {
        let (server, seen) = spawn(move |_, _| (status, body.into()));
        assert_eq!(auth::sign_in(&server, "u", "secret"), Err(Error::NotDispatcharr));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "only the version probe went out");
        assert!(!seen[0].body.contains("secret"));
    }
}

#[test]
fn nothing_listening_is_unreachable() {
    if !curl_ready() {
        return;
    }
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let server = Server::parse(&format!("127.0.0.1:{port}")).unwrap();
    assert_eq!(auth::version(&server), Err(Error::Unreachable));
}

#[test]
fn a_redirect_is_reported_and_not_followed() {
    if !curl_ready() {
        return;
    }
    let (server, seen) = spawn(|_, _| (302, String::new()));
    assert_eq!(auth::version(&server), Err(Error::Redirected(302)));
    assert_eq!(seen.lock().unwrap().len(), 1);
}

// ---- time ---------------------------------------------------------------------------------

#[test]
fn both_server_spellings_and_offsets_parse_to_the_same_instant() {
    let noon = 1_791_979_200; // 2026-10-14T12:00:00Z
    for spelling in [
        "2026-10-14T12:00:00Z",
        "2026-10-14T12:00:00+00:00",
        "2026-10-14T12:00:00.123456Z",
        "2026-10-14T14:00:00+02:00",
        "2026-10-14T07:00:00-0500",
        "2026-10-14 12:00:00",
    ] {
        assert_eq!(parse_iso8601(spelling), Some(noon), "{spelling}");
    }
    assert_eq!(format_iso8601(noon), "2026-10-14T12:00:00Z");
    assert_eq!(parse_iso8601("2024-02-29T00:00:00Z"), Some(1_709_164_800));
    assert_eq!(format_iso8601(1_709_164_800), "2024-02-29T00:00:00Z");
    assert_eq!(format_iso8601(0), "1970-01-01T00:00:00Z");
    for bad in ["", "2026-10-14", "2026-13-01T00:00:00Z", "2026-10-14T12:00:00+2", "2026-10-14T12:00:00.Z", "yesterday"] {
        assert_eq!(parse_iso8601(bad), None, "{bad}");
    }
}

#[test]
fn format_and_parse_round_trip_across_years() {
    let mut t = -86_400 * 365;
    while t < 4_102_444_800 {
        assert_eq!(parse_iso8601(&format_iso8601(t)), Some(t));
        t += 86_400 * 37 + 3_607;
    }
}

// ---- guide --------------------------------------------------------------------------------

fn channel(id: i64, uuid: &str, epg: Option<i64>) -> Channel {
    Channel { id, uuid: uuid.into(), name: format!("c{id}"), logo_id: None, channel_number: Some(id as f64),
        epg_data_id: epg, channel_group_id: None }
}

fn prog(start: i64, end: i64, title: &str) -> Programme {
    Programme { start, end, title: title.into(), sub_title: None, description: None, season: None,
        episode: None, is_new: false, is_live: false, is_premiere: false, is_finale: false }
}

const T0: i64 = 1_791_979_200; // 2026-10-14T12:00:00Z

#[test]
fn programmes_map_through_epg_rows_and_dummy_uuids() {
    let channels = [
        channel(1, "u-1", Some(10)),  // real EPG
        channel(2, "u-2", Some(10)),  // shares channel 1's EPG row
        channel(3, "u-3", None),      // no EPG: dummy programmes under its uuid
        channel(4, "u-4", Some(40)),  // assigned to a DUMMY source: programmes under its uuid
    ];
    let epg = [EpgData { id: 10, tvg_id: Some("real.tv".into()) }, EpgData { id: 40, tvg_id: Some("dummy-src".into()) }];
    let guide = Guide::build(&channels, &epg, [
        ("real.tv".to_string(), prog(T0, T0 + 1800, "Real")),
        ("u-3".to_string(), prog(T0, T0 + 14_400, "Dummy3")),
        ("u-4".to_string(), prog(T0, T0 + 14_400, "Dummy4")),
        ("nobody".to_string(), prog(T0, T0 + 1800, "Lost")),
    ], Window::around(T0));
    assert_eq!(guide.programmes(1)[0].title, "Real");
    assert_eq!(guide.programmes(2)[0].title, "Real");
    assert_eq!(guide.programmes(3)[0].title, "Dummy3");
    assert_eq!(guide.programmes(4)[0].title, "Dummy4");
    assert_eq!(guide.channels_with_programmes(), 4);
    assert!(guide.programmes(5).is_empty());
}

#[test]
fn listings_are_sorted_deduplicated_and_cut_to_the_window() {
    let w = Window::around(T0);
    let channels = [channel(1, "u-1", None)];
    let guide = Guide::build(&channels, &[], [
        ("u-1".to_string(), prog(T0 + 1800, T0 + 3600, "B")),
        ("u-1".to_string(), prog(T0, T0 + 1800, "A")),
        ("u-1".to_string(), prog(T0, T0 + 1800, "A")), // a second source listing the same slot
        ("u-1".to_string(), prog(w.end, w.end + 1800, "After")),
        ("u-1".to_string(), prog(w.start - 3600, w.start, "Before")),
        ("u-1".to_string(), prog(w.start - 600, w.start + 600, "Straddles")),
    ], w);
    let titles: Vec<&str> = guide.programmes(1).iter().map(|p| p.title.as_str()).collect();
    assert_eq!(titles, ["Straddles", "A", "B"]);
}

#[test]
fn now_and_next_follow_the_clock_and_gaps() {
    let channels = [channel(1, "u-1", None)];
    let guide = Guide::build(&channels, &[], [
        ("u-1".to_string(), prog(T0, T0 + 1800, "A")),
        ("u-1".to_string(), prog(T0 + 1800, T0 + 3600, "B")),
        ("u-1".to_string(), prog(T0 + 7200, T0 + 9000, "C")), // after a one-hour gap
    ], Window::around(T0));
    let titles = |t| {
        let (now, next) = guide.now_next(1, t);
        (now.map(|p| p.title.clone()), next.map(|p| p.title.clone()))
    };
    assert_eq!(titles(T0), (Some("A".into()), Some("B".into())));
    assert_eq!(titles(T0 + 1799), (Some("A".into()), Some("B".into())));
    assert_eq!(titles(T0 + 1800), (Some("B".into()), Some("C".into())));
    assert_eq!(titles(T0 + 4000), (None, Some("C".into())));
    assert_eq!(titles(T0 + 9000), (None, None));
    assert!((guide.at(1, T0 + 900).unwrap().progress(T0 + 900) - 0.5).abs() < 1e-6);
}

#[test]
fn an_overlapping_listing_resolves_to_the_one_that_started_latest() {
    let channels = [channel(1, "u-1", None)];
    let guide = Guide::build(&channels, &[], [
        ("u-1".to_string(), prog(T0, T0 + 7200, "Long")),
        ("u-1".to_string(), prog(T0 + 1800, T0 + 3600, "Inserted")),
    ], Window::around(T0));
    assert_eq!(guide.at(1, T0 + 2000).unwrap().title, "Inserted");
    assert_eq!(guide.at(1, T0 + 4000).unwrap().title, "Long");
}

#[test]
fn the_window_is_half_hour_aligned_and_goes_stale() {
    let w = Window::around(T0 + 1000);
    assert_eq!((w.start, w.end), (T0 - Window::BACK, T0 + Window::FORWARD));
    assert!(!w.stale(T0, T0 + 60));
    assert!(w.stale(T0, T0 + Window::MAX_AGE));
    assert!(w.stale(T0, w.end - Window::MIN_AHEAD + 1));
    assert!(w.stale(T0, w.start - 1));
}

#[test]
fn the_guide_reads_map_the_doubles_lineup() {
    if !curl_ready() {
        return;
    }
    let (server, seen) = dispatcharr(Some("K"));
    let account = Account { server, api_key: "K".into() };
    let channels = account.channels().unwrap();
    let programmes = account.epg_grid(Window::around(T0)).unwrap();
    assert_eq!(programmes.len(), 3, "the keyless row is dropped");
    let weather = &programmes[1].1;
    assert_eq!((weather.season, weather.episode, weather.sub_title.as_deref()), (Some(3), Some(12), None));
    let guide = account.guide(&channels, T0).unwrap();
    let id = |name: &str| channels.iter().find(|c| c.name == name).unwrap().id;
    let (now, next) = guide.now_next(id("News 24"), T0);
    assert_eq!(now.unwrap().title, "Midday News");
    assert!(now.unwrap().is_live);
    assert_eq!(next.unwrap().title, "Weather");
    assert_eq!(guide.at(id("alpha"), T0 - 60).unwrap().title, "alpha");
    assert!(guide.programmes(id("Movies")).is_empty());
    let seen = seen.lock().unwrap();
    let grid = seen.iter().find(|s| s.path.starts_with("/api/epg/grid/")).unwrap();
    assert_eq!(grid.path, "/api/epg/grid/?start=2026-10-14T11:00:00Z&end=2026-10-15T00:00:00Z");
    assert_eq!(grid.header("x-api-key"), Some("K"));
}
