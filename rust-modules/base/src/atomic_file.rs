//! **The 0600 atomic file write** — whole-old-or-whole-new, never a truncated file, and never a
//! secret that exists for an instant in a permissive mode.
//!
//! It lived in `plex::session` beside its first caller (the Plex session's legacy fallback file)
//! and `telemetry` already borrowed it from there. It moved down here when the Dispatcharr layer
//! needed it for its account file: that layer may not name `plex`, and a second copy of a routine
//! whose failure modes were found once, on the file holding the credentials, is the thing to
//! avoid. `plex::session` re-exports [`write_atomic`] and [`WriteFailure`] under their old names.

/// Write `json` to `path` so that whatever reads it sees the WHOLE previous file or the WHOLE new
/// one — never a truncated one, and never bytes of both. The `dispatcharrtv.new` → `mv` dance the
/// Makefile's deploy does, for the same reason and against a worse loss: the files this replaces
/// are credentials (the Plex session, the Dispatcharr account).
///
/// The old `O_TRUNC` in place had two windows, and the second is the one that took the file. A
/// reader between the truncate and the `write_all` sees zero bytes; a power cut or a kill in that
/// same gap leaves zero bytes *on disk*, and `peek` reads both as "no session" — sign in again.
///
/// The tmp file is a **sibling**, named off the resolved path. `rename(2)` is
/// only atomic within one filesystem, and the webOS jail's writable directories are separate mounts
/// (`/media/developer`, `/media/internal`, the app dir — see `plex::session::auth_paths`); a tmp under `/tmp`
/// would demote this to a cross-device copy, i.e. exactly the truncate-in-place it replaces. Its
/// suffix is random and opened with `create_new` + `O_NOFOLLOW`: the module lock serializes our
/// writers, but it does not serialize another uid able to create a sibling entry.
///
/// `sync_all` before the rename and on the parent after it is what makes the promise survive the
/// plug being pulled, which on a
/// television is an ordinary way to end a session: without it the rename can be visible while the
/// data behind it is not, and the file that comes back is the empty one. It costs a flush of a
/// couple of kilobytes on a path that runs at sign-in, at a profile switch, at a roster change and
/// at a committed search term — never per frame.
///
/// The 0600 mode is `plex::session::save`'s rule applied one file earlier: the secret must never *exist* in a
/// permissive mode, and the tmp file is where it exists first.
/// `pub` since 2026-08-29 so `crate::telemetry` writes its file the same way rather than
/// growing a second implementation of this. It is a generic 0600 atomic write that happens to live
/// beside its first caller; the alternative was two copies of a routine whose whole value is that
/// its failure modes have already been found once, on the file holding the credentials.
///
/// Returns the [`WriteFailure`] the OS actually gave, rather than a bare no: a caller trying
/// several candidates (session save's own fallback, the crash watermark, the telemetry spool) used
/// to be left with "none of them took it" and nothing that could tell EACCES (a jail whose
/// permissions changed) from EROFS (a mount gone read-only) from ENOENT (a directory a factory
/// reset removed) apart — see `plex::session::save_legacy_fallback_locked`, the one caller that now reports the
/// difference.
pub fn write_atomic(path: &std::path::Path, json: &[u8]) -> Result<(), WriteFailure> {
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = path.parent() else {
        return Err(WriteFailure::InvalidPath);
    };
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_file() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(WriteFailure::NotOwned);
        }
    }
    let (tmp, mut f) = create_private_temp(path)?;
    let write_result = f.write_all(json).and_then(|()| f.sync_all());
    drop(f); // the rename must not race our own open handle on a filesystem that cares
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(WriteFailure::WriteFailed(e.raw_os_error().unwrap_or(0)));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Leave no half-written credentials behind under a name the next writer would overwrite
        // anyway — and none at all if this candidate turned out to be unwritable.
        let _ = std::fs::remove_file(&tmp);
        return Err(WriteFailure::RenameFailed(e.raw_os_error().unwrap_or(0)));
    }
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    remove_temp_siblings(path);
    Ok(())
}

/// Why one candidate refused an atomic write. Errno-bearing where the OS actually returned one —
/// [`Self::errno`] — so a reader (the field log today; the failure read-out's Details card,
/// `screens::login::support_line`) can tell EACCES from EROFS from ENOENT instead of a bare no.
/// Never sent over the network: no path, uid, gid or mode belongs in `telemetry::incident`'s closed
/// vocabulary (see that module's doc), so this type stays local to the write path and its callers'
/// own logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFailure {
    /// The path has no parent directory, or no file name, to build a sibling temp name from — a
    /// degenerate candidate, not a syscall failure.
    InvalidPath,
    /// A file already exists at the destination under a uid this process does not own, or is not a
    /// regular file. Refused before any write was attempted, so there is no errno.
    NotOwned,
    /// The private temp file could not be created; the errno from the `open(2)` that failed.
    CreateFailed(i32),
    /// Every random temp suffix this attempt tried already existed on disk.
    Exhausted,
    /// The write or its `fsync` failed; the errno from whichever failed.
    WriteFailed(i32),
    /// The rename into place failed; the errno from `rename(2)`.
    RenameFailed(i32),
}

impl WriteFailure {
    /// The OS errno this failure carries, when it carries one — `None` for a refusal this process
    /// made itself before any syscall had a chance to fail.
    pub fn errno(self) -> Option<i32> {
        match self {
            Self::CreateFailed(e) | Self::WriteFailed(e) | Self::RenameFailed(e) => Some(e),
            Self::InvalidPath | Self::NotOwned | Self::Exhausted => None,
        }
    }
}

fn create_private_temp(
    path: &std::path::Path,
) -> Result<(std::path::PathBuf, std::fs::File), WriteFailure> {
    use std::os::unix::fs::OpenOptionsExt;
    for attempt in 0..16u64 {
        let Some(tmp) = random_tmp_path(path, attempt) else {
            return Err(WriteFailure::InvalidPath);
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(WriteFailure::CreateFailed(e.raw_os_error().unwrap_or(0))),
        }
    }
    Err(WriteFailure::Exhausted)
}

fn random_tmp_path(path: &std::path::Path, attempt: u64) -> Option<std::path::PathBuf> {
    use std::io::Read;
    static FALLBACK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut nonce = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut nonce))
        .is_err()
    {
        nonce = FALLBACK
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(attempt)
            .to_ne_bytes();
    }
    let mut name = path.file_name()?.to_os_string();
    name.push(format!(".tmp.{:016x}", u64::from_ne_bytes(nonce)));
    Some(path.with_file_name(name))
}

/// Remove the temporaries an interrupted [`write_atomic`] left beside `path`: the legacy fixed
/// [`tmp_path`] sibling and any random `.tmp.` sibling this uid owns (or a symlink planted there).
pub fn remove_temp_siblings(path: &std::path::Path) {
    use std::os::unix::fs::MetadataExt;
    if let Some(legacy) = tmp_path(path) {
        let _ = std::fs::remove_file(legacy);
    }
    let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) else {
        return;
    };
    let prefix = format!("{}.tmp.", file_name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        if let Ok(meta) = std::fs::symlink_metadata(entry.path()) {
            if meta.file_type().is_symlink() || meta.uid() == unsafe { libc::geteuid() } {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// The sibling [`write_atomic`] writes through, for a resolved candidate path. One definition
/// because `plex::session::clear` has to delete the same file, and a sign-out that missed it by spelling the
/// suffix differently would leave a live account token on the disk.
pub fn tmp_path(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut name = path.file_name()?.to_os_string();
    name.push(".tmp");
    Some(path.with_file_name(name))
}

/// Open `path` for reading without following a symlink at its name, and only if it is a regular
/// file this process owns. The metadata comes from the SAME descriptor the bytes will be read
/// from, so a caller that records the file's identity describes what it read. A missing file is
/// `NotFound`; anything else refused is `PermissionDenied`.
pub fn open_owned_regular(
    path: &std::path::Path,
) -> std::io::Result<(std::fs::File, std::fs::Metadata)> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    // O_NONBLOCK: a FIFO planted at the name must not block the open (it is refused just below);
    // it changes nothing for a regular file.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_file() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    Ok((file, meta))
}

/// The most [`read_owned_regular`] allocates for one file.
pub const MAX_OWNED_FILE: u64 = 4 * 1024 * 1024;

/// The whole of `path`, through [`open_owned_regular`]'s checks, or `None` when it is refused,
/// unreadable or larger than [`MAX_OWNED_FILE`].
pub fn read_owned_regular(path: &std::path::Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let (mut file, _) = open_owned_regular(path).ok()?;
    const MAX_FILE: u64 = MAX_OWNED_FILE;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_FILE).then_some(bytes)
}


#[cfg(test)]
mod owned_tests {
    use super::*;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("plx-owned-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn an_owned_regular_file_is_read_whole() {
        let d = dir("read");
        let p = d.join("f.json");
        std::fs::write(&p, b"{}").unwrap();
        assert_eq!(read_owned_regular(&p).as_deref(), Some(&b"{}"[..]));
        assert!(read_owned_regular(&d.join("missing")).is_none());
        assert_eq!(open_owned_regular(&d.join("missing")).unwrap_err().kind(), std::io::ErrorKind::NotFound);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A symlink or a FIFO planted at the name is refused rather than followed or blocked on.
    #[test]
    fn a_symlink_or_a_fifo_at_the_name_is_refused() {
        let d = dir("planted");
        let target = d.join("target");
        std::fs::write(&target, b"secret").unwrap();
        let link = d.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_owned_regular(&link).is_none(), "a symlink is not followed");
        let fifo = d.join("fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert_eq!(open_owned_regular(&fifo).unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_over_the_bound_is_not_read() {
        let d = dir("big");
        let p = d.join("big");
        std::fs::write(&p, vec![b'x'; MAX_OWNED_FILE as usize + 1]).unwrap();
        assert!(read_owned_regular(&p).is_none());
        std::fs::write(&p, vec![b'x'; MAX_OWNED_FILE as usize]).unwrap();
        assert_eq!(read_owned_regular(&p).map(|b| b.len()), Some(MAX_OWNED_FILE as usize));
        let _ = std::fs::remove_dir_all(&d);
    }
}
