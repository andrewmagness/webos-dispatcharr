//! **The signed-in Dispatcharr account** — the server and its API key, which is everything the app
//! keeps from a sign-in (the password and the JWT die inside [`super::auth::sign_in`]).
//!
//! One small JSON file, written whole-old-or-whole-new and mode 0600 through
//! [`plx_base::atomic_file::write_atomic`], at the first of
//! [`plx_base::paths::dispatcharr_account_candidates`] that accepts it. On the webOS releases this
//! app targets the Plex session's key-manager envelope falls back to exactly such a file too
//! (`keymanager3` is a webOS 24 service), so this is the same protection, not a weaker one. The
//! canonical DB8 helper is not used yet: its schema has one protected auth slot and the Plex
//! session still owns it. When the Plex session is removed this account moves into that slot.
//!
//! Every function here BLOCKS on the filesystem. Boot reads it before the frame scope; everything
//! else calls it from a worker.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::api::Account;
use super::client::Server;

/// The file's format tag. A different tag, or a version this build does not know, is a file this
/// build does not read — never a guess at what it means.
const FORMAT: &str = "dispatcharr-account";
const VERSION: u32 = 1;
/// A real record is a few hundred bytes; anything larger is not one, and is not read into memory.
const MAX_FILE: u64 = 64 * 1024;

/// Serializes this module's writers (save, clear) so a sign-out cannot interleave with a save.
static IO: Mutex<()> = Mutex::new(());

/// What a sign-in leaves behind, as stored: the request handle every later read uses, and who
/// signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub account: Account,
    /// Who signed in, for the Setup screen to say so. Not a credential.
    pub username: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    format: String,
    version: u32,
    server: String,
    username: String,
    api_key: String,
}

/// What [`load`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Loaded {
    /// No account file anywhere: nobody has signed in on this install.
    None,
    /// The stored account, and whether the path it was read from survives a reboot.
    Signed(Record, Saved),
    /// A file is there and is not a record this build reads (truncated, another format, a bad
    /// address, not owned by this process). Treated as signed out; [`clear`] removes it.
    Unreadable,
}

/// Where [`save`] landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Saved {
    /// On a path that survives a reboot.
    Durable,
    /// Only in the runtime directory, which a reboot clears: the sign-in works until then.
    UntilReboot,
}

/// [`save`] found no candidate that accepted the write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaveFailed;

/// The account this install signed in with, from the first candidate that holds a file.
pub fn load() -> Loaded {
    for (path, durable) in candidates() {
        match read_owned(&path) {
            Ok(None) => continue,
            Ok(Some(bytes)) => {
                let saved = if durable { Saved::Durable } else { Saved::UntilReboot };
                return parse(&bytes).map_or(Loaded::Unreadable, |record| Loaded::Signed(record, saved));
            }
            Err(()) => return Loaded::Unreadable,
        }
    }
    Loaded::None
}

/// Store `record`, replacing any earlier one. The copy at every other candidate is removed, so a
/// stale account at a higher-priority path can never shadow the one just written.
pub fn save(record: &Record) -> Result<Saved, SaveFailed> {
    let bytes = serde_json::to_vec_pretty(&Stored {
        format: FORMAT.to_owned(),
        version: VERSION,
        server: record.account.server.base(),
        username: record.username.clone(),
        api_key: record.account.api_key.clone(),
    })
    .map_err(|_| SaveFailed)?;
    let _io = IO.lock().unwrap_or_else(|e| e.into_inner());
    let paths = candidates();
    for (i, (path, durable)) in paths.iter().enumerate() {
        if plx_base::atomic_file::write_atomic(path, &bytes).is_ok() {
            for (j, (other, _)) in paths.iter().enumerate() {
                if j != i {
                    remove(other);
                }
            }
            return Ok(if *durable { Saved::Durable } else { Saved::UntilReboot });
        }
    }
    plx_base::eventlog::log("dispatcharr: could not write the account to any candidate path");
    Err(SaveFailed)
}

/// Sign out: remove the account at every candidate, and any temporaries an interrupted save left.
pub fn clear() {
    let _io = IO.lock().unwrap_or_else(|e| e.into_inner());
    for (path, _) in candidates() {
        remove(&path);
    }
}

fn parse(bytes: &[u8]) -> Option<Record> {
    let stored: Stored = serde_json::from_slice(bytes).ok()?;
    if stored.format != FORMAT || stored.version != VERSION || stored.api_key.is_empty() {
        return None;
    }
    let server = Server::parse(&stored.server).ok()?;
    Some(Record { account: Account { server, api_key: stored.api_key }, username: stored.username })
}

/// The file at `path`: `Ok(None)` when there is none, `Err` when one is there that this process
/// must not read (not a regular file, another owner, too large, unreadable).
fn read_owned(path: &Path) -> Result<Option<Vec<u8>>, ()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    // The metadata of the descriptor the bytes come from, not of the name: a swap between a check
    // and the read cannot change what was checked.
    let meta = file.metadata().map_err(|_| ())?;
    if !meta.file_type().is_file() || meta.uid() != euid() || meta.len() > MAX_FILE {
        return Err(());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1).read_to_end(&mut bytes).map_err(|_| ())?;
    Ok(Some(bytes))
}

fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
    plx_base::atomic_file::remove_temp_siblings(path);
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Best first, each with whether it survives a reboot.
#[cfg(not(any(test, feature = "test-support")))]
fn candidates() -> Vec<(PathBuf, bool)> {
    plx_base::paths::dispatcharr_account_candidates()
}

/// The test build's candidate list: whatever a test set with [`redirect_for_test`], else one file
/// in this process's own scratch directory — never the device paths, and never the directory the
/// test binary runs from (which `in_app_dir` would resolve to).
#[cfg(any(test, feature = "test-support"))]
static TEST_CANDIDATES: Mutex<Option<Vec<(PathBuf, bool)>>> = Mutex::new(None);

#[cfg(any(test, feature = "test-support"))]
fn candidates() -> Vec<(PathBuf, bool)> {
    if let Some(list) = TEST_CANDIDATES.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return list;
    }
    // Once per process: `process_dir` empties the directory it hands out.
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let path = PATH.get_or_init(|| plx_base::testscratch::process_dir("dispatcharr-account").join("dispatcharr.json"));
    vec![(path.clone(), true)]
}

/// Point this module at `list` (best first, each with whether it survives a reboot), or back at the
/// per-process scratch file with `None`. The caller holds `plx_base::testlock::serial()`.
#[cfg(any(test, feature = "test-support"))]
pub fn redirect_for_test(list: Option<Vec<(PathBuf, bool)>>) {
    *TEST_CANDIDATES.lock().unwrap_or_else(|e| e.into_inner()) = list;
}
