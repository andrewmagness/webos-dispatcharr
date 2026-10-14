//! Response shapes, as Dispatcharr v0.32.0 serializes them. Unknown fields are ignored, so an
//! additive server change does not break parsing; a renamed or retyped field does, loudly, with the
//! endpoint named ([`super::client::parse`]).

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

/// `GET /api/core/version/`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServerVersion {
    pub version: String,
    #[serde(default)]
    pub timestamp: Option<String>,
}

impl ServerVersion {
    /// `(major, minor, patch)` of a `MAJOR.MINOR.PATCH[-suffix]` version; `None` when unparseable.
    pub fn triple(&self) -> Option<(u32, u32, u32)> {
        let core = self.version.trim().trim_start_matches('v');
        let core = core.split(['-', '+']).next()?;
        let mut parts = core.split('.').map(|p| p.parse::<u32>().ok());
        Some((parts.next()??, parts.next()??, parts.next().unwrap_or(Some(0))?))
    }

    /// Whether this client has been verified against this server version. The REST API had
    /// breaking changes in v0.28 and v0.32, so only the minor line this client was built against
    /// counts; anything else still signs in but is reported.
    pub fn is_tested(&self) -> bool {
        matches!(self.triple(), Some((0, 32, _)))
    }
}

/// `POST /api/accounts/token/` request body.
#[derive(Serialize)]
pub(crate) struct Credentials<'a> {
    pub username: &'a str,
    pub password: &'a str,
}

/// `POST /api/accounts/token/` answer (simplejwt). Only `access` is used.
#[derive(Deserialize)]
pub(crate) struct TokenPair {
    pub access: String,
}

/// `GET /api/accounts/api-keys/` and `POST /api/accounts/api-keys/generate/`.
#[derive(Deserialize)]
pub(crate) struct ApiKeyReply {
    #[serde(default)]
    pub key: Option<String>,
}

/// One row of `GET /api/channels/channels/summary/`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Channel {
    pub id: i64,
    /// The stream path key: `/proxy/ts/stream/<uuid>`.
    pub uuid: String,
    pub name: String,
    #[serde(default)]
    pub logo_id: Option<i64>,
    /// A float on the server (`5.1` is valid), and nullable.
    #[serde(default)]
    pub channel_number: Option<f64>,
    #[serde(default)]
    pub epg_data_id: Option<i64>,
    #[serde(default)]
    pub channel_group_id: Option<i64>,
}

impl Channel {
    /// The number as a viewer reads it: `5`, `5.1`, or empty when the channel has none.
    pub fn number_label(&self) -> String {
        match self.channel_number {
            None => String::new(),
            Some(n) if n.fract() == 0.0 => format!("{}", n as i64),
            Some(n) => {
                let s = format!("{n:.3}");
                s.trim_end_matches('0').trim_end_matches('.').to_owned()
            }
        }
    }

    /// Guide order: by number, numberless channels last, then by name, then by id so the order is
    /// total and stable across refreshes.
    pub fn guide_order(a: &Channel, b: &Channel) -> Ordering {
        let by_number = match (a.channel_number, b.channel_number) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        by_number
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.id.cmp(&b.id))
    }
}

/// One row of `GET /api/channels/groups/`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Group {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub channel_count: Option<i64>,
}
