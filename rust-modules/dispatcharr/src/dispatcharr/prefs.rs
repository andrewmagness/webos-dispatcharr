//! **What the viewer chose on this TV**: the favourite channels, the channel last watched, and
//! whether the app starts on it.
//!
//! Channels are named by their UUID, which a server keeps when it renumbers or renames a channel.
//! The file belongs to one server: [`load`] answers empty for any other, so signing in somewhere
//! else starts clean, and signing back in to the same server finds the favourites where they were.
//! Not a credential, so it outlives a sign-out.
//!
//! The account file's storage rules ([`super::account`]): one small JSON file, written atomically,
//! at the first candidate that accepts it. Every function here BLOCKS on the filesystem; the app
//! calls it from a worker.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

const FORMAT: &str = "dispatcharr-prefs";
const VERSION: u32 = 1;
/// The largest file read: a guard against reading something that is not this file into memory,
/// not a limit on favourites. A UUID favourite costs about 45 bytes, so this holds every channel of
/// any lineup a server could list hundreds of times over.
const MAX_PREFS_FILE: u64 = 8 * 1024 * 1024;

static IO: Mutex<()> = Mutex::new(());

/// The viewer's choices for one server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prefs {
    /// Favourite channels' UUIDs, in the order they were added.
    pub favourites: Vec<String>,
    /// The channel last watched, for resuming on launch.
    pub last: Option<String>,
    /// Start the app on [`last`](Self::last) (the Server page's switch). On unless turned off.
    pub resume_on_launch: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { favourites: Vec::new(), last: None, resume_on_launch: true }
    }
}

impl Prefs {
    pub fn is_favourite(&self, uuid: &str) -> bool {
        self.favourites.iter().any(|f| f == uuid)
    }

    /// Add `uuid` if it is not a favourite, remove it if it is. Answers whether it is one now.
    pub fn toggle(&mut self, uuid: &str) -> bool {
        if let Some(i) = self.favourites.iter().position(|f| f == uuid) {
            self.favourites.remove(i);
            false
        } else {
            self.favourites.push(uuid.to_owned());
            true
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Stored {
    format: String,
    version: u32,
    /// The server the choices belong to, as [`super::client::Server::base`] spells it.
    server: String,
    #[serde(default)]
    favourites: Vec<String>,
    #[serde(default)]
    last: Option<String>,
    #[serde(default = "on")]
    resume_on_launch: bool,
}

fn on() -> bool {
    true
}

/// The choices stored for `server` (its `base()`); empty when there are none, or they belong to
/// another server, or the file is not one this build reads.
pub fn load(server: &str) -> Prefs {
    for (path, _) in candidates() {
        match super::account::read_owned_up_to(&path, MAX_PREFS_FILE) {
            Ok(None) => continue,
            Ok(Some(bytes)) => return parse(&bytes, server).unwrap_or_default(),
            Err(()) => return Prefs::default(),
        }
    }
    Prefs::default()
}

/// Store `prefs` for `server`, replacing whatever was stored (for any server).
pub fn save(server: &str, prefs: &Prefs) -> Result<(), ()> {
    let bytes = serde_json::to_vec_pretty(&Stored {
        format: FORMAT.to_owned(),
        version: VERSION,
        server: server.to_owned(),
        favourites: prefs.favourites.clone(),
        last: prefs.last.clone(),
        resume_on_launch: prefs.resume_on_launch,
    })
    .map_err(|_| ())?;
    let _io = IO.lock().unwrap_or_else(|e| e.into_inner());
    let paths = candidates();
    for (i, (path, _)) in paths.iter().enumerate() {
        if plx_base::atomic_file::write_atomic(path, &bytes).is_ok() {
            for (j, (other, _)) in paths.iter().enumerate() {
                if j != i {
                    super::account::remove(other);
                }
            }
            return Ok(());
        }
    }
    plx_base::eventlog::log("dispatcharr: could not write the viewing preferences anywhere");
    Err(())
}

fn parse(bytes: &[u8], server: &str) -> Option<Prefs> {
    let stored: Stored = serde_json::from_slice(bytes).ok()?;
    if stored.format != FORMAT || stored.version != VERSION || stored.server != server {
        return None;
    }
    let mut favourites = stored.favourites;
    favourites.retain(|f| !f.is_empty());
    favourites.dedup();
    Some(Prefs { favourites, last: stored.last.filter(|l| !l.is_empty()), resume_on_launch: stored.resume_on_launch })
}

#[cfg(not(any(test, feature = "test-support")))]
fn candidates() -> Vec<(PathBuf, bool)> {
    plx_base::paths::dispatcharr_prefs_candidates()
}

/// One file in this process's own scratch directory: never the device paths.
#[cfg(any(test, feature = "test-support"))]
fn candidates() -> Vec<(PathBuf, bool)> {
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let path = PATH.get_or_init(|| plx_base::testscratch::process_dir("dispatcharr-prefs").join("prefs.json"));
    vec![(path.clone(), true)]
}

#[cfg(test)]
mod tests {
    use super::*;

    const HERE: &str = "http://192.0.2.10:9191";

    #[test]
    fn toggling_adds_then_removes() {
        let mut p = Prefs::default();
        assert!(p.toggle("a"));
        assert!(p.toggle("b"));
        assert!(p.is_favourite("a"));
        assert!(!p.toggle("a"));
        assert_eq!(p.favourites, vec!["b".to_owned()]);
    }

    #[test]
    fn choices_round_trip_and_belong_to_their_server() {
        let _g = plx_base::testlock::serial();
        let p = Prefs { favourites: vec!["u-1".into(), "u-9".into()], last: Some("u-9".into()), resume_on_launch: false };
        save(HERE, &p).unwrap();
        assert_eq!(load(HERE), p);
        assert_eq!(load("http://192.0.2.11:9191"), Prefs::default(), "another server starts clean");
    }

    #[test]
    fn a_file_this_build_does_not_read_is_no_choices() {
        let bad = br#"{"format":"dispatcharr-prefs","version":2,"server":"http://192.0.2.10:9191"}"#;
        assert_eq!(parse(bad, HERE), None);
        let ok = br#"{"format":"dispatcharr-prefs","version":1,"server":"http://192.0.2.10:9191","favourites":["a","a",""],"last":""}"#;
        assert_eq!(parse(ok, HERE), Some(Prefs { favourites: vec!["a".into()], last: None, resume_on_launch: true }),
            "a file from before the switch existed starts on the last channel");
    }
}
