//! Crash-safe file replacement — the one way project and state files are
//! written.
//!
//! `fs::write` truncates the file before writing it. A save that stops
//! partway — the process killed, the disk full, the power lost — has by then
//! already destroyed the previous version. Worse, the commonest torn state
//! is an empty file, and every optional project file (`iomap.toml`,
//! `tasks.toml`, `alarms.toml`, `northbound.toml`) parses an empty file
//! *successfully*, as "nothing configured": the IO mapping, the schedule or
//! the alarm definitions vanish without an error. A cut at a TOML table
//! boundary does the same for part of a file.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Replace `path` with `contents` so that a reader — or the next start after
/// a crash — sees either the whole previous file or the whole new one.
///
/// The data is written to a temporary in the same directory, flushed to
/// disk, and renamed over the target; the directory is then synced so the
/// rename itself survives a power cut. The temporary is a dot-file ending in
/// `.tmp`, which every project listing already skips; a crash can leave one
/// behind, and nothing reads it.
///
/// A symlinked `path` is written through to its target, as `fs::write`
/// would, rather than replaced by a regular file.
/// Existing file permissions are retained. On Unix a new file, and every
/// temporary while its contents are being written, is private (mode 0600,
/// subject to umask); saving a private device config must not expose its
/// credentials through a replacement created with the default 0666 mode.
pub fn write_atomic(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    write_atomic_receipt(path, contents).map(|_| ())
}

/// A replacement can succeed even when its directory could not be synced.
/// Existing callers retain their best-effort contract; durable state callers
/// must inspect this receipt and must not report that case as durable.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct AtomicWriteReceipt {
    pub replaced: bool,
    pub durable: bool,
    pub directory_sync_error: Option<String>,
}

pub fn write_atomic_receipt(
    path: impl AsRef<Path>,
    contents: impl AsRef<[u8]>,
) -> io::Result<AtomicWriteReceipt> {
    write_atomic_with(path.as_ref(), contents.as_ref(), |_| Ok(()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteStage {
    Write,
    FileSync,
    Rename,
    DirectorySync,
}

fn write_atomic_with(
    path: &Path,
    contents: &[u8],
    mut stage: impl FnMut(WriteStage) -> io::Result<()>,
) -> io::Result<AtomicWriteReceipt> {
    let target = resolve_symlink(path)?;
    let permissions = match fs::metadata(&target) {
        Ok(meta) => Some(meta.permissions()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let dir = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy()
        .into_owned();
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // Only clean up a temporary we created, never a pre-existing file or
    // symlink at this name. In particular, do not truncate through a link.
    let mut file = options.open(&tmp)?;
    let written = (|| {
        stage(WriteStage::Write)?;
        file.write_all(contents)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        stage(WriteStage::FileSync)?;
        file.sync_all()?;
        drop(file);
        stage(WriteStage::Rename)?;
        fs::rename(&tmp, &target)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    let directory_sync_error = stage(WriteStage::DirectorySync)
        .and_then(|_| fs::File::open(&dir).and_then(|d| d.sync_all()))
        .err()
        .map(|e| e.to_string());
    Ok(AtomicWriteReceipt {
        replaced: true,
        durable: directory_sync_error.is_none(),
        directory_sync_error,
    })
}

fn resolve_symlink(path: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => fs::canonicalize(path),
        _ => Ok(path.to_path_buf()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn replaces_and_creates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.toml");
        write_atomic(&path, "one").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "one");
        write_atomic(&path, "two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        assert!(leftovers(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn replacement_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.toml");
        for mode in [0o600, 0o640, 0o750] {
            fs::write(&path, "old").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            write_atomic(&path, "new").unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "new");
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                mode
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn new_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.toml");
        write_atomic(&path, "private configuration").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
    }

    #[test]
    fn a_failed_write_leaves_the_target_and_no_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.toml");
        write_atomic(&path, "kept").unwrap();
        // Renaming a file over a non-empty directory fails after the data
        // is written — the last step, so everything before it ran.
        let blocked = dir.path().join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("x"), "").unwrap();
        assert!(write_atomic(&blocked, "lost").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "kept");
        assert!(
            leftovers(dir.path()).is_empty(),
            "{:?}",
            leftovers(dir.path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn writes_through_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        let link = dir.path().join("link.toml");
        fs::write(&real, "old").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, "new").unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[cfg(test)]
mod durability_tests {
    use super::*;
    #[test]
    fn every_failure_boundary_reports_its_actual_commit_state() {
        for failing in [
            WriteStage::Write,
            WriteStage::FileSync,
            WriteStage::Rename,
            WriteStage::DirectorySync,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.json");
            fs::write(&path, "original").unwrap();
            let result = write_atomic_with(&path, b"replacement", |stage| {
                if stage == failing {
                    Err(io::Error::other(format!("injected {stage:?}")))
                } else {
                    Ok(())
                }
            });
            if failing == WriteStage::DirectorySync {
                let receipt = result.unwrap();
                assert!(receipt.replaced);
                assert!(!receipt.durable);
                assert!(receipt
                    .directory_sync_error
                    .unwrap()
                    .contains("DirectorySync"));
                assert_eq!(fs::read(&path).unwrap(), b"replacement");
            } else {
                assert!(result.is_err());
                assert_eq!(fs::read(&path).unwrap(), b"original");
            }
        }
    }
}
