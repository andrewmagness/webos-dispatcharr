//! Parsing and error mapping against v0.32.0's response shapes, and the whole sign-in sequence
//! against a loopback double that records every request it was sent.

use std::net::TcpListener;

use super::api::Account;
use super::auth;
use super::client::{self, Server};
use super::error::Error;
use super::models::{Channel, ServerVersion};
use super::test_double::{curl_ready, dispatcharr, spawn, SUMMARY};

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
