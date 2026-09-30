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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_atomic_write_replaces_the_file_and_leaves_no_tmp_even_when_it_fails() {
        let dir = std::env::temp_dir().join(format!("seagents-fsutil-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        // The rename fails (a directory is in the way): no tmp stays.
        let blocked = dir.join("d");
        fs::create_dir_all(blocked.join("inside")).unwrap();
        assert!(write_atomic(&blocked, b"x").is_err());
        let left: Vec<_> = fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 2, "{left:?}");
        fs::remove_dir_all(&dir).ok();
    }

    fn mode(p: &Path) -> u32 {
        fs::symlink_metadata(p).unwrap().mode() & 0o777
    }

    #[test]
    fn files_are_created_owner_only_and_loose_ones_tightened() {
        let dir = std::env::temp_dir().join(format!("seagents-fsutil-mode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let rec = dir.join("1.meta");
        write_atomic(&rec, b"{}").unwrap();
        assert_eq!(mode(&rec), 0o600);
        // A record an older binary left 0644 is 0600 after its next update.
        fs::set_permissions(&rec, fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&rec, b"{}").unwrap();
        assert_eq!(mode(&rec), 0o600);
        let tee = dir.join("1.raw");
        fs::write(&tee, b"old").unwrap();
        fs::set_permissions(&tee, fs::Permissions::from_mode(0o644)).unwrap();
        create_private(&tee, true).unwrap().write_all(b"+").unwrap();
        assert_eq!((mode(&tee), fs::read(&tee).unwrap()), (0o600, b"old+".to_vec()));
        create_private(&tee, false).unwrap();
        assert_eq!(fs::read(&tee).unwrap(), b"");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_private_dir_is_ours_0700_and_never_a_symlink() {
        let base = std::env::temp_dir().join(format!("seagents-fsutil-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let dir = base.join("a/b/cache");
        private_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        let link = base.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(private_dir(&link).unwrap_err().contains("refusing"));
        let file = base.join("file");
        fs::write(&file, b"").unwrap();
        assert!(private_dir(&file).unwrap_err().contains("refusing"));
        // Someone else's directory: / is root's.
        if unsafe { libc::getuid() } != 0 {
            assert!(private_dir(Path::new("/")).unwrap_err().contains("belongs to uid 0"));
        }
        fs::remove_dir_all(&base).ok();
    }
}
