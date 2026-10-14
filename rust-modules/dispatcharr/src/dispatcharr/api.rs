//! Reads made with the API key: channels and groups.

use super::client::{self, Auth, Method, Server};
use super::error::Error;
use super::guide::{Guide, Window};
use super::models::{Channel, EpgData, GridReply, Group, Programme};
use super::time::format_iso8601;

/// A server and the key that opens it — what every read after sign-in needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub server: Server,
    pub api_key: String,
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

    /// The URL a channel's logo is served at. Needs no credential on the server side.
    pub fn logo_url(&self, logo_id: i64) -> String {
        self.server.url(&format!("/api/channels/logos/{logo_id}/cache/"))
    }

    /// The URL of a channel's live MPEG-TS stream.
    pub fn stream_url(&self, channel: &Channel) -> String {
        self.server.url(&format!("/proxy/ts/stream/{}", channel.uuid))
    }
}
