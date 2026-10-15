//! **The application's OWNED screens** (restructure spec §2.1 `screens/`): the Dispatcharr Setup
//! and Server pages, the Guide and the player, as `Screen` impls the dispatcher mounts, steps,
//! focuses through the engine, hit-tests through the map and draws. No `static mut` here: every
//! screen's state is a field of the instance the container owns (§6.1).
//!
//! Layer rule (§2.1): a screen names `ui/`, `stores/`, the data crates and this directory's
//! `registry`; never `app/` and never a sibling screen module — a rule `ci/check-deps.sh`'s
//! `layer` and `sibling` gates hold. The `Arg` enum, the page alphabet and the mounter's one match
//! are `registry`'s; what is left in `app/bridge.rs` is the concrete host and the rig it lends the
//! dispatcher.

pub mod player;
pub mod registry;
pub mod setup; // Dispatcharr sign-in: the boot page while no account is stored
pub mod guide; // the Dispatcharr guide: channels against time, the root once an account is stored
