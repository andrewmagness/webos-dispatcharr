//! Reads made with the API key — channels, groups, the EPG rows and the grid, and the guide built
//! from them — and the URLs a logo and a stream are served at.

use super::client::{self, Auth, Method, Server};
use super::error::Error;
use super::guide::{Guide, Window};
use super::models::{Channel, EpgData, GridReply, Group, Programme};
use super::time::format_iso8601;

/// A server and the key that opens it — what every read after sign-in needs.
#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    pub server: Server,
    pub api_key: String,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never put the key in a log line or a panic message.
        f.debug_struct("Account").field("server", &self.server.base()).field("api_key", &"<redacted>").finish()
    }
}

/// The television's wall clock in Unix seconds: the one reading behind the guide's "now" and a
/// live tune's "on now". Read here, in the data layer, so the app's logical clock stays ticks.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

impl Account {
    fn get(&self, path: &str) -> Result<Vec<u8>, Error> {
        client::send(&self.server, Method::Get, path, Auth::ApiKey(&self.api_key), None)
    }

    /// Every channel visible to this user, in guide order ([`Channel::guide_order`]).
    pub fn channels(&self) -> Result<Vec<Channel>, Error> {
        let mut channels: Vec<Channel> =
            client::parse("channels/summary", &self.get("/api/channels/channels/summary/")?)?;
        channels.sort_by(Channel::guide_order);
        Ok(channels)
    }

    /// The channel groups, in the server's order.
    pub fn groups(&self) -> Result<Vec<Group>, Error> {
        client::parse("groups", &self.get("/api/channels/groups/")?)
    }

    /// Every EPG row (`id` → `tvg_id`). The endpoint has no filter, so this is the whole table;
    /// only the two mapped fields are kept.
    pub fn epg_data(&self) -> Result<Vec<EpgData>, Error> {
        client::parse("epgdata", &self.get("/api/epg/epgdata/")?)
    }

    /// Programmes overlapping `window`, each with the `tvg_id` it was listed under. Rows the guide
    /// cannot place (unparseable times, no key) are dropped.
    pub fn epg_grid(&self, window: Window) -> Result<Vec<(String, Programme)>, Error> {
        let path = format!(
            "/api/epg/grid/?start={}&end={}",
            format_iso8601(window.start),
            format_iso8601(window.end)
        );
        let reply: GridReply = client::parse("epg/grid", &self.get(&path)?)?;
        Ok(reply.data.into_iter().filter_map(|row| row.into_programme()).collect())
    }

    /// The guide for `channels` over the window around `now`: the EPG rows and the grid, mapped.
    pub fn guide(&self, channels: &[Channel], now: i64) -> Result<Guide, Error> {
        let window = Window::around(now);
        let epg = self.epg_data()?;
        let programmes = self.epg_grid(window)?;
        Ok(Guide::build(channels, &epg, programmes, window))
    }

    /// The guide for `channels` around the television's clock now, and the second it was read at —
    /// the stamp a screen counts on from with its own logical ticks, so the wall clock is read here,
    /// once per fetch, and never by the UI.
    pub fn guide_now(&self, channels: &[Channel]) -> Result<(Guide, i64), Error> {
        let now = unix_now();
        Ok((self.guide(channels, now)?, now))
    }

    /// The URL a channel's logo is served at. Needs no credential on the server side.
    pub fn logo_url(&self, logo_id: i64) -> String {
        self.server.url(&format!("/api/channels/logos/{logo_id}/cache/"))
    }

    /// The URL of a channel's live MPEG-TS stream.
    pub fn stream_url(&self, channel: &Channel) -> String {
        self.server.url(&format!("/proxy/ts/stream/{}", channel.uuid))
    }
}
