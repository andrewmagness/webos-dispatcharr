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

/// One row of `GET /api/epg/epgdata/` — an EPG source's channel entry. Only the mapping half is
/// read: the grid names programmes by `tvg_id`, and a channel names its EPG row by `epg_data_id`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EpgData {
    pub id: i64,
    #[serde(default)]
    pub tvg_id: Option<String>,
}

/// `GET /api/epg/grid/` answers `{"data": [...]}`.
#[derive(Deserialize)]
pub(crate) struct GridReply {
    pub data: Vec<GridRow>,
}

/// A programme as the grid serializes it. Real programmes come from Django's JSON encoder
/// (`2026-10-14T05:00:00Z`, an integer `id`); generated dummy ones from `datetime.isoformat()`
/// (`…+00:00`, a string `id` such as `dummy-7-20261014T040000`). `season`/`episode` are copied
/// out of the XMLTV custom properties and may be a number or a string.
#[derive(Deserialize)]
pub(crate) struct GridRow {
    pub start_time: String,
    pub end_time: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub sub_title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tvg_id: Option<String>,
    #[serde(default)]
    pub season: Option<serde_json::Value>,
    #[serde(default)]
    pub episode: Option<serde_json::Value>,
    #[serde(default)]
    pub is_new: bool,
    #[serde(default)]
    pub is_live: bool,
    #[serde(default)]
    pub is_premiere: bool,
    #[serde(default)]
    pub is_finale: bool,
}

/// A programme, with its times as Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Programme {
    pub start: i64,
    pub end: i64,
    pub title: String,
    pub sub_title: Option<String>,
    pub description: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub is_new: bool,
    pub is_live: bool,
    pub is_premiere: bool,
    pub is_finale: bool,
}

impl Programme {
    /// Whether the programme is on the air at `t` (start inclusive, end exclusive).
    pub fn airs_at(&self, t: i64) -> bool {
        self.start <= t && t < self.end
    }

    /// How far through the programme `t` is, 0.0 to 1.0.
    pub fn progress(&self, t: i64) -> f32 {
        if self.end <= self.start {
            return 0.0;
        }
        ((t - self.start) as f32 / (self.end - self.start) as f32).clamp(0.0, 1.0)
    }
}

impl GridRow {
    /// The programme and the `tvg_id` it belongs to; `None` for a row with unusable times, no key
    /// or a non-positive length, which the guide cannot place.
    pub(crate) fn into_programme(self) -> Option<(String, Programme)> {
        let start = super::time::parse_iso8601(&self.start_time)?;
        let end = super::time::parse_iso8601(&self.end_time)?;
        let tvg_id = self.tvg_id.filter(|t| !t.is_empty())?;
        if end <= start {
            return None;
        }
        let number = |v: Option<serde_json::Value>| match v? {
            serde_json::Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
            serde_json::Value::String(s) => s.trim().parse().ok(),
            _ => None,
        };
        let text = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
        Some((
            tvg_id,
            Programme {
                start,
                end,
                title: self.title.unwrap_or_default(),
                sub_title: text(self.sub_title),
                description: text(self.description),
                season: number(self.season),
                episode: number(self.episode),
                is_new: self.is_new,
                is_live: self.is_live,
                is_premiere: self.is_premiere,
                is_finale: self.is_finale,
            },
        ))
    }
}
