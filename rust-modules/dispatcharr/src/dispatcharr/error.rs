//! What went wrong talking to a Dispatcharr server, in the terms a person can act on.
//!
//! The classification is by STATUS, never folded: a refused connection, a 401 and a 403 send the
//! user to three different places (the address, the password, the server's network settings), so
//! reporting one as another is the bug this type exists to prevent.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The text entered is not a usable server address.
    BadAddress,
    /// Nothing answered: no route, refused, DNS, TLS failure or timeout.
    Unreachable,
    /// Something answered, but not as Dispatcharr (no version document, or not JSON).
    NotDispatcharr,
    /// 401: wrong username/password, or the stored API key was revoked.
    Unauthorized,
    /// 403: the server's network access policy refuses this client, or the user lacks permission.
    Forbidden,
    /// 404 on an endpoint this client relies on.
    NotFound,
    /// 429: the login throttle (on sign-in) or the user's stream limit (on a stream).
    TooManyRequests,
    /// 503: the server is up but cannot serve right now.
    Unavailable,
    /// A 3xx. Never followed, because the request carried a credential.
    Redirected(u16),
    /// Any other non-2xx status.
    Http(u16),
    /// A 2xx whose body did not parse as the documented shape.
    BadResponse(String),
}

impl Error {
    /// The error a completed non-2xx response means. `None` for a 2xx.
    pub fn from_status(status: u16) -> Option<Error> {
        Some(match status {
            200..=299 => return None,
            300..=399 => Error::Redirected(status),
            401 => Error::Unauthorized,
            403 => Error::Forbidden,
            404 => Error::NotFound,
            429 => Error::TooManyRequests,
            503 => Error::Unavailable,
            other => Error::Http(other),
        })
    }

    /// Whether the stored credential is no longer good and the app should return to Setup.
    pub fn needs_sign_in(&self) -> bool {
        matches!(self, Error::Unauthorized)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadAddress => write!(f, "not a server address"),
            Error::Unreachable => write!(f, "server unreachable"),
            Error::NotDispatcharr => write!(f, "not a Dispatcharr server"),
            Error::Unauthorized => write!(f, "unauthorized (401)"),
            Error::Forbidden => write!(f, "forbidden (403)"),
            Error::NotFound => write!(f, "not found (404)"),
            Error::TooManyRequests => write!(f, "too many requests (429)"),
            Error::Unavailable => write!(f, "unavailable (503)"),
            Error::Redirected(s) => write!(f, "redirected ({s})"),
            Error::Http(s) => write!(f, "HTTP {s}"),
            Error::BadResponse(what) => write!(f, "unexpected response: {what}"),
        }
    }
}
