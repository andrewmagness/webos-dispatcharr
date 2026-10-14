//! Reads made with the API key: channels and groups.

use super::client::{self, Auth, Method, Server};
use super::error::Error;
use super::models::{Channel, Group};

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

    /// The URL a channel's logo is served at. Needs no credential on the server side.
    pub fn logo_url(&self, logo_id: i64) -> String {
        self.server.url(&format!("/api/channels/logos/{logo_id}/cache/"))
    }

    /// The URL of a channel's live MPEG-TS stream.
    pub fn stream_url(&self, channel: &Channel) -> String {
        self.server.url(&format!("/proxy/ts/stream/{}", channel.uuid))
    }
}
