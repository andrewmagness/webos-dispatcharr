//! plx_data: the data layer of the Dispatcharr TV application core.
//!
//! What is left of the Plex data layer is `metadata`: the item and stream facts (codecs, Dolby
//! Vision, audio and subtitle tracks) the player still reads. It is the `data` layer of
//! `ci/module-layers.ini`: it uses `base`, `machine`, `platform`, `net`, `plex` and `telemetry` and
//! nothing above them, so it names no screen, widget or application type.
//!
//! `test-support` exposes the seams the layers above drive their tests through
//! (`metadata::set_current_for_test`, ...) and
//! switches the `cfg(not(test))` arms a dependent's tests need in their test form. The application
//! crate enables it in `[dev-dependencies]` only, so no shipped build sees it.

pub mod metadata; // item detail data layer (detail page): full metadata + seasons/episodes + cast + related
