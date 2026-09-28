//! A deployment uploads the same private project copy that passed preflight.

use std::path::{Path, PathBuf};

/// Only `prepare` constructs this value. Keeping it alive retains the checked
/// files until the upload finishes (or the request is cancelled).
pub(crate) struct DeploySnapshot {
    _directory: tempfile::TempDir,
    project_dir: PathBuf,
}

impl DeploySnapshot {
    pub(crate) fn prepare(source: &Path) -> Result<Self, String> {
        let directory = tempfile::Builder::new()
            .prefix("ia2-deploy-")
            .tempdir()
            .map_err(|e| format!("creating deploy snapshot: {e}"))?;
        let project_dir = directory.path().join("project");
        copy_tree(source, &project_dir).map_err(|e| format!("copying deploy snapshot: {e}"))?;
        // Concurrent edits can affect the copy while it is being created, but
        // from here on validation and upload use these same independent bytes.
        ironplc_bridge::load_edge_project(&project_dir)?;
        Ok(Self {
            _directory: directory,
            project_dir,
        })
    }

    pub(crate) fn project_dir(&self) -> &Path {
        &self.project_dir
    }
}

fn copy_tree(source: &Path, destination: &Path) -> std::io::Result<()> {
    let kind = std::fs::symlink_metadata(source)?.file_type();
    if kind.is_dir() {
        std::fs::create_dir(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if kind.is_file() {
        std::fs::copy(source, destination)?;
    } else {
        // Keeping symlinks would reopen the mutable-source window at compile
        // or upload time. Do not silently omit or deploy special files either.
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is a symlink or special file; deploy requires regular files and directories",
                source.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use project::ProjectStore;

    #[test]
    fn edits_after_preflight_cannot_change_the_uploaded_archive() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStore::create(dir.path().join("source"), "source").unwrap();
        let snapshot = DeploySnapshot::prepare(store.root()).unwrap();
        let checked_source = std::fs::read(snapshot.project_dir().join("pous/main.st")).unwrap();
        store
            .write_pou_source(
                "main",
                "PROGRAM main\nVAR x : INT; END_VAR\nx := ghost;\nEND_PROGRAM",
            )
            .unwrap();
        assert!(ironplc_bridge::load_edge_project(store.root()).is_err());

        let archive = dir.path().join("upload.tar");
        assert!(std::process::Command::new("tar")
            .args(["-cf"])
            .arg(&archive)
            .arg("-C")
            .arg(snapshot.project_dir().parent().unwrap())
            .arg("project")
            .status()
            .unwrap()
            .success());
        let unpacked = dir.path().join("unpacked");
        std::fs::create_dir(&unpacked).unwrap();
        assert!(std::process::Command::new("tar")
            .arg("-xf")
            .arg(&archive)
            .arg("-C")
            .arg(&unpacked)
            .status()
            .unwrap()
            .success());
        let uploaded = unpacked.join("project");
        assert_eq!(
            std::fs::read(uploaded.join("pous/main.st")).unwrap(),
            checked_source
        );
        assert!(ironplc_bridge::load_edge_project(&uploaded).is_ok());
    }

    #[test]
    fn invalid_content_never_produces_a_deployable_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStore::create(dir.path().join("source"), "source").unwrap();
        store
            .write_pou_source("main", "PROGRAM main\nx := ghost;\nEND_PROGRAM")
            .unwrap();
        let error = DeploySnapshot::prepare(store.root()).err().unwrap();
        assert!(error.contains("compiling project"), "{error}");
    }

    #[test]
    fn snapshot_lifetime_is_independent_of_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStore::create(dir.path().join("source"), "source").unwrap();
        let snapshot = DeploySnapshot::prepare(store.root()).unwrap();
        std::fs::remove_dir_all(store.root()).unwrap();
        let copied = snapshot.project_dir().to_path_buf();
        assert!(ironplc_bridge::load_edge_project(&copied).is_ok());
        drop(snapshot);
        assert!(!copied.exists(), "temporary snapshot must be cleaned up");
    }

    #[cfg(unix)]
    #[test]
    fn mutable_symlink_targets_are_refused_before_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStore::create(dir.path().join("source"), "source").unwrap();
        let original = store.root().join("pous/main.st");
        let target = dir.path().join("linked.st");
        std::fs::rename(&original, &target).unwrap();
        std::os::unix::fs::symlink(&target, &original).unwrap();
        let error = DeploySnapshot::prepare(store.root()).err().unwrap();
        assert!(error.contains("symlink or special file"), "{error}");
    }
}
