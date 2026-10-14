//! plx_dispatcharr: the Dispatcharr layer of the application core.
//!
//! `dispatcharr` is the typed client for a self-hosted Dispatcharr server's REST API: signing in,
//! obtaining the per-user API key every later request carries, and the channel/group reads. It is
//! the `dispatcharr` layer of `ci/module-layers.ini`: it uses `net` and nothing above it, and it is
//! a peer of `plex`, not a client of it.

pub mod dispatcharr; // typed Dispatcharr REST client — sign-in, API key, channels and groups
