//! **The one request door.** Builds the URL, the auth header and the JSON body, sends it through
//! libcurl, and turns the status into [`Error`] — the only place in this layer that knows there is
//! a transport.

use plx_net::net;
use plx_net::net::origin::{Origin, Scheme};

use super::error::Error;

/// Dispatcharr's own default port (its Docker image and installers serve on 9191). A server typed
/// as a bare host means this port; a URL that names a scheme means that scheme's port.
pub const DEFAULT_PORT: i64 = 9191;

/// The largest body this client accepts. A summary of thousands of channels is a few hundred KB;
/// the cap only stops a misdirected request from filling memory on a 1.5 GB television.
const MAX_BODY: usize = 16 << 20;

/// A Dispatcharr server: where it is. Credentials travel separately so a server can be named
/// before any exist (the version probe of [`super::auth::sign_in`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    origin: Origin,
}

impl Server {
    /// Read what a person typed into the Setup screen's address field.
    ///
    /// `192.168.1.10` → `http://192.168.1.10:9191`; `192.168.1.10:8080` keeps its port;
    /// `https://tv.example.com` → port 443 and `http://tv.example.com` → port 80, because a written
    /// scheme with no port means that scheme's default. A path or trailing slash is dropped:
    /// Dispatcharr's API lives at the root.
    pub fn parse(input: &str) -> Result<Server, Error> {
        let text = input.trim();
        if text.is_empty() || text.chars().any(char::is_whitespace) {
            return Err(Error::BadAddress);
        }
        // A TV keyboard capitalises freely; `Origin` reads the scheme case-sensitively.
        let (url, fallback) = match text.split_once("://") {
            None => (text.to_owned(), DEFAULT_PORT),
            Some((scheme, rest)) => {
                let scheme = scheme.to_ascii_lowercase();
                let port = match scheme.as_str() {
                    "http" => 80,
                    "https" => 443,
                    _ => return Err(Error::BadAddress),
                };
                (format!("{scheme}://{rest}"), port)
            }
        };
        Origin::parse_connection(&url, fallback)
            .map(|origin| Server { origin })
            .ok_or(Error::BadAddress)
    }

    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// `scheme://host:port`, the persisted and logged form.
    pub fn base(&self) -> String {
        self.origin.base()
    }

    /// Whether credentials sent to this server travel in clear. Dispatcharr is usually reached over
    /// plain HTTP on the LAN; the Setup screen says so rather than refusing.
    pub fn is_plaintext(&self) -> bool {
        self.origin.scheme() == Scheme::Http
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{}", self.origin.base(), path)
    }
}

/// How a request authenticates.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Auth<'a> {
    None,
    Bearer(&'a str),
    ApiKey(&'a str),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Method {
    Get,
    Post,
}

/// Send one request and return the body of a 2xx, or the [`Error`] the outcome means.
pub(crate) fn send(
    server: &Server,
    method: Method,
    path: &str,
    auth: Auth<'_>,
    json_body: Option<&[u8]>,
) -> Result<Vec<u8>, Error> {
    if !(net::global_init() && net::available()) {
        return Err(Error::Unreachable);
    }
    let mut headers = vec!["Accept: application/json".to_owned()];
    match auth {
        Auth::None => {}
        Auth::Bearer(token) => headers.push(format!("Authorization: Bearer {token}")),
        Auth::ApiKey(key) => headers.push(format!("X-API-Key: {key}")),
    }
    if json_body.is_some() {
        headers.push("Content-Type: application/json".to_owned());
    }
    let verb = match method {
        Method::Get => "GET",
        Method::Post => "POST",
    };
    let resp = net::request(
        &server.url(path),
        &headers,
        verb,
        json_body,
        net::API,
        false,
        Some(MAX_BODY),
        None,
    )
    .ok_or(Error::Unreachable)?;
    match Error::from_status(resp.status) {
        None => Ok(resp.body),
        Some(err) => Err(err),
    }
}

/// Parse a 2xx body, naming the endpoint when it does not have the documented shape.
pub(crate) fn parse<T: serde::de::DeserializeOwned>(what: &str, body: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(body).map_err(|e| Error::BadResponse(format!("{what}: {e}")))
}
