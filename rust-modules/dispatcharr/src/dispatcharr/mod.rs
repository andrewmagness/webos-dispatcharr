//! **The typed Dispatcharr client.** Every Dispatcharr REST query the app makes is built here.
//!
//! The contract is Dispatcharr v0.32.0's, read from its source rather than its docs:
//!
//! * `GET /api/core/version/` is `AllowAny` and answers `{"version","timestamp"}` — the one request
//!   that tells "this URL is a Dispatcharr" apart from "this URL answers HTTP" before any
//!   credential is sent ([`auth::sign_in`] asks it first).
//! * `POST /api/accounts/token/` takes `{"username","password"}` and answers a simplejwt pair. The
//!   server's network policy can refuse it with a 403 before the credentials are looked at, and a
//!   login throttle answers 429.
//! * `GET /api/accounts/api-keys/` answers `{"key": <string|null>}` — the key is stored in clear
//!   on the user row, so this returns the user's CURRENT key. `POST …/api-keys/generate/` REPLACES
//!   it (one key per user), which would break whatever else already uses it; so a key is generated
//!   only when there is none. Both need only an authenticated user, not an admin.
//! * Every later request authenticates with `X-API-Key: <key>` (`ApiKeyAuthentication`); keys do
//!   not expire, which is why the app keeps the key and never the password or the JWT.
//! * `GET /api/channels/channels/summary/` and `GET /api/channels/groups/` are unpaginated lists.
//!   `channel_number` is a nullable FLOAT (`5.1` is a valid channel number).
//! * `GET /api/epg/epgdata/` is the whole EPG table, unfiltered; `GET /api/epg/grid/?start=&end=`
//!   answers `{"data": [...]}`, programmes keyed only by `tvg_id` ([`guide`] says how they map).
//!
//! Transport: [`client`] sends every request through libcurl ([`plx_net::net`]) for both schemes,
//! because sign-in needs a request BODY and the plaintext socket transport has no way to send one.
//! Redirects are never followed — a request carrying a password or a key must not be replayed at
//! an origin nobody chose.

pub mod api; // channels/groups reads with the API key
pub mod auth; // sign-in: version probe, JWT, existing-or-new API key
pub mod client; // the one request door: URL, auth header, body, status → Error
pub mod error; // what went wrong, in the terms a Setup screen explains to a person
pub mod guide; // programmes per channel: the tvg_id mapping and the rolling window
pub mod models; // the response shapes
pub mod time; // ISO 8601 <-> Unix seconds, the grid's two spellings

#[cfg(any(test, feature = "test-support"))]
pub mod test_double; // a loopback Dispatcharr that records requests, for this crate's and the app's tests
#[cfg(test)]
mod tests;

pub use client::Server;
pub use error::Error;
