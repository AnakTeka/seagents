//! Writing files other processes read while they are written: the one
//! atomic-write helper every cache and state file goes through.

use std::fs;
use std::path::{Path, PathBuf};

/// Replace `path` with `bytes` so that a concurrent reader sees the old
/// contents or the new, never a torn write: a tmp file beside it, renamed
/// over it. The tmp name carries our pid (`<path>.<pid>.tmp`), so writers in
/// different processes never share one — within a process the caller's lock
/// serializes them — and it ends in `.tmp`, so no reader that lists the
/// directory by extension mistakes it for a record. On any failure the tmp
/// file is removed: a failed write leaves nothing behind.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));
    let result = fs::write(&tmp, bytes).and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
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
}
