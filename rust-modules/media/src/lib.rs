//! plx_media: the media layer of the Dispatcharr TV application core.
//!
//! Demux (`ff`, the bundled FFmpeg), the HTTPS media plane (`curlio`), the access-unit queues
//! (`aq`), the buffer-feed engine (`player`) and route selection (`route`). It is the `media` layer
//! of `ci/module-layers.ini`: it uses `base`, `machine`, `platform`, `gfx`, `net`, `telemetry` and
//! `data` and nothing above them. Its members call each other in a cycle (`curlio`, `ff`, `player`
//! and `route`), which is why they are one crate.
//!
//! The Starfish/ACB surface is declared here and nowhere else: `sf_on_event` and `acb_on_event`
//! (`player`), which `src/starfish.c` calls, and the externs of `player::ffi`. The crate links nothing itself; the Makefile's final link resolves them.
//!
//! `test-support` exposes the `cfg(test)` seams the layers above test through (`player::restore_state_for_test`,
//! `player::swap_state_for_test`, ...). The application crate enables it in `[dev-dependencies]`
//! only, so no shipped build sees it.

pub mod aq;
pub mod curlio; // the HTTPS media plane: a remote file pulled by byte range over libcurl-multi (stream.rs is the plaintext-socket twin)
#[macro_use]
pub mod ff; // THE demuxer -- the FFmpeg 9.0 this app BUNDLES and pins (majors 63/63/61), dlopen'd by absolute path beside the binary, never the television's
pub mod player; // buffer-feed video engine (was playback.c) -- step 5
pub mod route; // play_movie route selection (direct-play vs transcode) -- step 3
