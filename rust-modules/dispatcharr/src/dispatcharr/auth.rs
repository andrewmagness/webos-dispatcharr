//! Sign-in: from what a person typed (address, username, password) to the API key the app keeps.
//!
//! The password and the JWT live only for the duration of [`sign_in`]; what survives is the server
//! and its API key, which does not expire (module doc of [`super`]).

use super::client::{self, Auth, Method, Server};
use super::error::Error;
use super::models::{ApiKeyReply, Credentials, ServerVersion, TokenPair};

/// What a successful sign-in leaves behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedIn {
    pub server: Server,
    pub api_key: String,
    pub version: ServerVersion,
    /// The user had no API key and this sign-in generated one (as opposed to reusing theirs).
    pub key_generated: bool,
}

/// Ask a server what it is. Needs no credential, so it is safe to send to any address typed.
pub fn version(server: &Server) -> Result<ServerVersion, Error> {
    match client::send(server, Method::Get, "/api/core/version/", Auth::None, None) {
        Ok(body) => client::parse("version", &body).map_err(|_| Error::NotDispatcharr),
        // Something answered HTTP but has no version document: not a Dispatcharr (or a far older
        // one than this client speaks to).
        Err(Error::NotFound) => Err(Error::NotDispatcharr),
        Err(other) => Err(other),
    }
}

/// Sign in and obtain the user's API key.
///
/// 1. `GET /api/core/version/` — refuse early, before the password goes anywhere, when the
///    address is not a Dispatcharr.
/// 2. `POST /api/accounts/token/` — the JWT pair.
/// 3. `GET /api/accounts/api-keys/` — the user's existing key, reused if present: generating one
///    REPLACES it and would break any other client the user configured with it.
/// 4. Only when there is none, `POST /api/accounts/api-keys/generate/`.
pub fn sign_in(server: &Server, username: &str, password: &str) -> Result<SignedIn, Error> {
    let version = version(server)?;

    let body = serde_json::to_vec(&Credentials { username, password })
        .map_err(|e| Error::BadResponse(format!("credentials: {e}")))?;
    let reply = client::send(server, Method::Post, "/api/accounts/token/", Auth::None, Some(&body))?;
    let token: TokenPair = client::parse("token", &reply)?;

    let bearer = Auth::Bearer(&token.access);
    let existing: ApiKeyReply =
        client::parse("api-keys", &client::send(server, Method::Get, "/api/accounts/api-keys/", bearer, None)?)?;
    let (api_key, key_generated) = match existing.key.filter(|k| !k.is_empty()) {
        Some(key) => (key, false),
        None => {
            let reply = client::send(server, Method::Post, "/api/accounts/api-keys/generate/", bearer, Some(b"{}"))?;
            let generated: ApiKeyReply = client::parse("api-keys/generate", &reply)?;
            let key = generated
                .key
                .filter(|k| !k.is_empty())
                .ok_or_else(|| Error::BadResponse("api-keys/generate: no key".to_owned()))?;
            (key, true)
        }
    };

    Ok(SignedIn { server: server.clone(), api_key, version, key_generated })
}
