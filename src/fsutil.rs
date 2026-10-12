//! Files and directories other processes read while seagents writes them,
//! and nobody else may read at all: the one atomic-write helper, the one
//! owner-only file create, and the one owner/symlink-checked directory —
//! for the cache, the state dir and the runtime dir alike.
//!
//! Private because of what they hold: the tee (`.raw`) is everything the
//! agent put on screen, `.agent` and the tombstones its last message, the
//! transcript index where conversations live. The directories are 0700 and
//! every file is created 0600 besides, so a file carried out of its
//! directory (a copy, a restore from backup) stays private too.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Replace `path` with `bytes` so that a concurrent reader sees the old
/// contents or the new, never a torn write: a tmp file beside it, renamed
/// over it. The tmp name carries our pid (`<path>.<pid>.tmp`), so writers in
/// different processes never share one — within a process the caller's lock
/// serializes them — and it ends in `.tmp`, so no reader that lists the
/// directory by extension mistakes it for a record. On any failure the tmp
/// file is removed: a failed write leaves nothing behind.
/// The file is 0600 (the tmp is created so, and the rename keeps it).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));
    let result = create_private(&tmp, false)
        .and_then(|mut f| f.write_all(bytes))
        .and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Open `path` for writing, owner-only: created 0600, and an existing file
/// of a looser mode (one an older binary made) tightened. `append` keeps
/// what is there; otherwise it is truncated.
pub(crate) fn create_private(path: &Path, append: bool) -> std::io::Result<File> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(append)
        .truncate(!append)
        .mode(0o600)
        .open(path)?;
    if file.metadata()?.mode() & 0o077 != 0 {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// Make `dir` (its missing parents as usual, itself 0700) and make sure it
/// is ours alone: a real directory — `lstat`, so a symlink is refused, not
/// followed — owned by this uid, with a loosened mode (a umask, a hand
/// `chmod`, a binary from before this rule) tightened back to 0700. Err
/// says why it can't be had; what to do then is the caller's call — in a
/// shared `/tmp` it must be refused (anyone could have made the name first),
/// under `HOME` a symlink is the user's own doing.
pub(crate) fn private_dir(dir: &Path) -> Result<(), String> {
    if let Some(parent) = dir.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::DirBuilder::new().mode(0o700).create(dir);
    let meta = fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.file_type().is_dir() {
        return Err(format!("{} is not a directory (a symlink?); refusing it", dir.display()));
    }
    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid {
        return Err(format!("{} belongs to uid {}, not us; refusing it", dir.display(), meta.uid()));
    }
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    Ok(())
}

/// `<tmp>/seagents-<uid>`, [`private_dir`]-checked: where the runtime dir
/// falls back to, and the cache when there is no `HOME`.
pub(crate) fn tmp_dir_in(tmp: &Path) -> Result<PathBuf, String> {
    let dir = tmp.join(format!("seagents-{}", unsafe { libc::getuid() }));
    private_dir(&dir)?;
    Ok(dir)
}
