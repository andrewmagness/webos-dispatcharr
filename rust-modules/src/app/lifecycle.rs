//! The app's names for the player's transport-pause contract, re-exported from
//! [`plx_media::player::lifecycle`] so every `app` caller spells them as it always did. The
//! app-switch park itself (background stops a channel, foreground tunes it again) is `app::run`'s.

pub(crate) use plx_media::player::lifecycle::{
    paused, set_paused, set_transport_paused, transport_target, viewer_paused,
};
