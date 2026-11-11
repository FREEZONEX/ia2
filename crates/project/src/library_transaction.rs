//! Recoverable updates to library blocks and their manifest entry.
//!
//! All cooperating readers/writers lock the project before inspecting the
//! journal. Only a prepared journal can authorize rollback. The journal's
//! rename to "committed" and its directory sync are the commit point.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::library_permissions;
use crate::store::{validate_single_segment, LIBRARY_SLUG_PREFIX};
use crate::StoreError;

const STAGE: &str = ".ia2-library-stage";
const ACTIVE: &str = ".ia2-library-import";
const LOCK: &str = ".ia2-library-lock";
const PREPARED: &str = "prepared.toml";
const COMMITTED: &str = "committed.toml";
const MANIFEST: &str = "project.toml";
const OWNER: &str = "ia2-library-transaction-v1";

/// Root-level storage paths reserved for imports. Exclude them when copying
/// a project under the read guard, even if deferred cleanup left one behind.
pub fn is_library_transaction_artifact(name: &std::ffi::OsStr) -> bool {
    [STAGE, ACTIVE, LOCK].iter().any(|path| name == *path)
}

fn lock_file(root: &Path) -> Result<File, StoreError> {
    // An existing directory is a stable inode on Unix and needs no writes
    // for a read-only project. Elsewhere a regular lock file is persistent
    // (unlinking a locked file would allow two independent lock owners).
    #[cfg(unix)]
    let file = File::open(root)?;
    #[cfg(not(unix))]
    let file = {
        let path = root.join(LOCK);
        if let Some(meta) = metadata(&path)? {
            require_type(&path, meta.is_file())?;
            File::open(path)?
        } else {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .or_else(|e| match e.kind() {
                    ErrorKind::AlreadyExists => {
                        require_type(&path, fs::symlink_metadata(&path)?.is_file())?;
                        File::open(path)
                    }
                    _ => Err(e),
                })?
        }
    };
    Ok(file)
}

fn try_lock_error(error: fs::TryLockError) -> StoreError {
    match error {
        fs::TryLockError::WouldBlock => StoreError::LibraryBusy,
        fs::TryLockError::Error(error) => error.into(),
    }
}

pub(crate) fn read_guard(root: &Path) -> Result<File, StoreError> {
    read_access(root, false)
}

pub(crate) fn open_guard(root: &Path) -> Result<File, StoreError> {
    read_access(root, true)
}

fn read_access(root: &Path, clean_staging: bool) -> Result<File, StoreError> {
    let guard = lock_file(root)?;
    guard.try_lock_shared().map_err(try_lock_error)?;
    if !(active_exists(root)? || clean_staging && metadata(&root.join(STAGE))?.is_some()) {
        return Ok(guard);
    }
    // Do not hold a registry lock waiting for another project reader or
    // writer. Recheck the recovery record under exclusive access.
    drop(guard);
    let guard = lock_file(root)?;
    guard.try_lock().map_err(try_lock_error)?;
    recover(root)?;
    if clean_staging && !active_exists(root)? {
        warn_cleanup(root, cleanup_stage(root));
    }
    Ok(guard)
}

pub(crate) fn write_guard(root: &Path) -> Result<File, StoreError> {
    let guard = lock_file(root)?;
    guard.lock()?;
    check_writable_project(root)?;
    Ok(guard)
}

pub(crate) fn try_write_guard(root: &Path) -> Result<File, StoreError> {
    let guard = lock_file(root)?;
    guard.try_lock().map_err(try_lock_error)?;
    check_writable_project(root)?;
    Ok(guard)
}

fn check_writable_project(root: &Path) -> Result<(), StoreError> {
    require_type(root, fs::symlink_metadata(root)?.is_dir())?;
    recover(root)?;
    if active_exists(root)? {
        return Err(StoreError::LibraryRecoveryRequired(
            "committed import cleanup must finish before another library write".into(),
        ));
    }
    Ok(())
}

fn active_exists(root: &Path) -> Result<bool, StoreError> {
    metadata(&root.join(ACTIVE))
        .map(|entry| entry.is_some())
        .map_err(recovery_required)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    format: u32,
    library: String,
    version: String,
    existing_directories: usize,
    blocks: Vec<String>,
    // The manifest is always the last entry. Paths are reconstructed from
    // validated basenames, never read as arbitrary paths from the journal.
    files: Vec<Change>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Change {
    before: Option<Image>,
    after: Image,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Image {
    sha256: String,
    mode: u32,
    #[serde(default)]
    access: String,
}

impl Image {
    fn of(bytes: &[u8], mode: u32, access: String) -> Self {
        Self {
            sha256: format!("{:x}", Sha256::digest(bytes)),
            mode,
            access,
        }
    }
}

impl Journal {
    fn target(&self, root: &Path, index: usize) -> PathBuf {
        match self.blocks.get(index) {
            Some(name) => library_dirs(root, &self.library)[2].join(name),
            None => root.join(MANIFEST),
        }
    }

    fn validate(&self) -> Result<(), StoreError> {
        validate_single_segment(&self.library)?;
        let mut names = HashSet::new();
        for name in &self.blocks {
            validate_block(name)?;
            if !names.insert(name) {
                return Err(invalid("duplicate journal block").into());
            }
        }
        if self.format != 2 {
            return Err(
                invalid("library journal format lacks supported permission evidence").into(),
            );
        }
        if self.existing_directories > 3
            || self.files.len() != self.blocks.len() + 1
            || self
                .files
                .last()
                .is_none_or(|change| change.before.is_none())
            || (self.existing_directories < 3
                && self.files[..self.blocks.len()]
                    .iter()
                    .any(|change| change.before.is_some()))
        {
            return Err(invalid("inconsistent library journal").into());
        }
        for change in &self.files {
            let original_mode = change.before.as_ref().map_or(0o600, |image| image.mode);
            if original_mode & !0o7777 != 0
                || change.after.mode != original_mode
                || change.after.access.is_empty()
                || change
                    .before
                    .as_ref()
                    .is_some_and(|image| image.access != change.after.access)
            {
                return Err(invalid("inconsistent permissions in library journal").into());
            }
        }
        Ok(())
    }
}

fn validate_block(name: &str) -> Result<(), StoreError> {
    validate_single_segment(name)?;
    if !name.ends_with(".st") {
        return Err(StoreError::InvalidName(name.into()));
    }
    Ok(())
}

pub(crate) fn import(
    root: &Path,
    library: &str,
    version: &str,
    files: &[(String, String)],
) -> Result<(), StoreError> {
    validate_single_segment(library)?;
    let _guard = write_guard(root)?;
    checkpoint(Phase::BeforeStage)?;
    let stage = create_stage(root)?;
    let active = root.join(ACTIVE);
    let journal = match prepare(root, &stage, library, version, files).and_then(|journal| {
        checkpoint(Phase::Prepared)?;
        fs::rename(&stage, &active)?;
        Ok(journal)
    }) {
        Ok(journal) => journal,
        Err(error) => {
            warn_cleanup(root, cleanup_stage(root));
            return Err(error);
        }
    };
    // Ensure the recovery record is reachable before any formal edit.
    let install = (|| {
        sync_directory(root)?;
        let dirs = library_dirs(root, library);
        for dir in &dirs[journal.existing_directories..] {
            fs::create_dir(dir)?;
            sync_directory(dir.parent().expect("library directory has parent"))?;
        }
        check_current_library_path(root, library)?;
        for (index, change) in journal.files.iter().enumerate() {
            checkpoint(Phase::BeforeInstall(index))?;
            existing_library_dirs(root, library)?;
            let target = journal.target(root, index);
            if current(&target)?.as_ref() != change.before.as_ref() {
                return Err(conflict(&target).into());
            }
            replace(&active, &target, index, &change.after, "new")?;
            checkpoint(Phase::AfterInstall(index))?;
        }
        checkpoint(Phase::BeforeCommit)?;
        verify_installed(root, &journal)?;
        fs::rename(active.join(PREPARED), active.join(COMMITTED))?;
        Ok::<(), StoreError>(())
    })();
    if let Err(error) = install {
        return match rollback(root, &active, &journal)
            .and_then(|()| retire(root).map_err(Into::into))
        {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(recovery_required(format!(
                "import failed ({error}); rollback stopped ({rollback_error})"
            ))),
        };
    }
    // A visible committed record whose directory sync failed is ambiguous.
    // Never roll it back or report success. Recovery revalidates and syncs it.
    checkpoint(Phase::SyncCommit)
        .and_then(|()| sync_directory(&active))
        .map_err(|error| StoreError::LibraryCommitUnconfirmed(error.to_string()))?;
    warn_cleanup(root, retire(root));
    Ok(())
}

pub(crate) fn remove(root: &Path, library: &str) -> Result<(), StoreError> {
    validate_single_segment(library)?;
    let _guard = write_guard(root)?;
    let manifest = root.join(MANIFEST);
    let (before, contents) = read_image(&manifest)?
        .ok_or_else(|| StoreError::NotFound(manifest.display().to_string()))?;
    let mut table = decode_manifest(&contents)?;
    let count = check_library_path(root, library, &table, None)?;
    let dirs = library_dirs(root, library);
    let Some(toml::Value::Table(libraries)) = table.get_mut("libraries") else {
        return Ok(remove_library_dirs(&dirs, count)?);
    };
    if libraries.remove(library).is_none() {
        return Ok(remove_library_dirs(&dirs, count)?);
    }
    if libraries.is_empty() {
        table.remove("libraries");
    }
    #[cfg(windows)]
    if before.mode & 0o200 == 0 {
        return Err(StoreError::LibraryPermissionConflict {
            path: manifest.display().to_string(),
            reason: "the manifest is read-only".into(),
        });
    }

    // Verify the new manifest before removing the library directory.
    let stage = create_stage(root)?;
    let result = (|| {
        let path = stage.join("replace.tmp");
        let file = write_private(
            &path,
            toml::to_string_pretty(&table)?.as_bytes(),
            before.mode,
        )?;
        check_replacement_access(&file, &manifest, before.mode, Some(&before.access))?;
        drop(file);
        remove_library_dirs(&dirs, count)?;
        fs::rename(path, &manifest)?;
        Ok(sync_directory(root)?)
    })();
    warn_cleanup(root, cleanup_stage(root));
    result
}

fn remove_library_dirs(dirs: &[PathBuf; 3], count: usize) -> io::Result<()> {
    if count == 3 {
        fs::remove_dir_all(&dirs[2])?;
    }
    if count >= 2 && fs::read_dir(&dirs[1])?.next().is_none() {
        fs::remove_dir(&dirs[1])?;
    }
    Ok(())
}

fn create_stage(root: &Path) -> io::Result<PathBuf> {
    cleanup_stage(root)?;
    let stage = root.join(STAGE);
    let owner = owner_marker(&stage)?;
    private_directory(&stage)?;
    write_private(&stage.join(owner), b"", 0o600)?;
    sync_directory(&stage)?;
    Ok(stage)
}

fn prepare(
    root: &Path,
    stage: &Path,
    library: &str,
    version: &str,
    files: &[(String, String)],
) -> Result<Journal, StoreError> {
    let manifest = root.join(MANIFEST);
    let (old, old_manifest) = read_image(&manifest)?
        .ok_or_else(|| StoreError::NotFound(manifest.display().to_string()))?;
    let table = decode_manifest(&old_manifest)?;
    let names = stage.join("names");
    private_directory(&names)?;
    let existing_directories = check_library_path(root, library, &table, Some(&names))?;
    let new_manifest = updated_manifest(table, library, version)?;
    let mut journal = Journal {
        format: 2,
        library: library.into(),
        version: version.into(),
        existing_directories,
        blocks: Vec::new(),
        files: Vec::new(),
    };

    // Check aliases among selected block names on this filesystem.
    // Exact repeats still appear in the response and need one write.
    let mut unique = HashSet::new();
    for (name, contents) in files {
        validate_block(name)?;
        if !unique.insert(name) {
            continue;
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(names.join(name))
        {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                return Err(StoreError::LibraryPathConflict(format!(
                    "selected block '{name}' aliases another selected file"
                )));
            }
            Err(error) => return Err(error.into()),
        }
        let index = journal.blocks.len();
        journal.blocks.push(name.clone());
        let target = journal.target(root, index);
        journal.files.push(stage_change(
            stage,
            index,
            &target,
            read_image(&target)?,
            contents.as_bytes(),
        )?);
    }
    fs::remove_dir_all(&names)?;
    journal.files.push(stage_change(
        stage,
        journal.blocks.len(),
        &manifest,
        Some((old, old_manifest)),
        new_manifest.as_bytes(),
    )?);
    write_private(
        &stage.join(PREPARED),
        toml::to_string_pretty(&journal)?.as_bytes(),
        0o600,
    )?;
    sync_directory(stage)?;
    Ok(journal)
}

fn decode_manifest(bytes: &[u8]) -> Result<toml::Table, StoreError> {
    let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
    let typed: crate::ProjectManifest = toml::from_str(text)?;
    typed.governance.validate().map_err(invalid)?;
    Ok(toml::from_str(text)?)
}

fn updated_manifest(
    mut table: toml::Table,
    library: &str,
    version: &str,
) -> Result<String, StoreError> {
    let libraries = table
        .entry("libraries")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| invalid("[libraries] must be a table"))?;
    libraries.insert(library.into(), toml::Value::String(version.into()));
    Ok(toml::to_string_pretty(&table)?)
}

fn stage_change(
    stage: &Path,
    index: usize,
    target: &Path,
    before: Option<(Image, Vec<u8>)>,
    after: &[u8],
) -> Result<Change, StoreError> {
    let original = before.as_ref().map(|(image, _)| image);
    let mode = original.map_or(0o600, |image| image.mode);
    #[cfg(windows)]
    if original.is_some_and(|image| image.mode & 0o200 == 0) {
        return Err(StoreError::LibraryPermissionConflict {
            path: target.display().to_string(),
            reason: "the destination is read-only".into(),
        });
    }
    // The published journal is renamed in place. A replacement created
    // under it will inherit the same access metadata as this empty probe.
    let probe = stage.join("replace.tmp");
    let file = write_private(&probe, b"", 0o600)?;
    let access = check_replacement_access(
        &file,
        target,
        0o600,
        original.map(|image| image.access.as_str()),
    )?;
    drop(file);
    fs::remove_file(probe)?;
    let before = match before {
        Some((image, bytes)) => {
            checkpoint(Phase::BeforeBackup(index))?;
            write_private(&blob(stage, "old", index), &bytes, 0o600)?;
            Some(image)
        }
        None => None,
    };
    checkpoint(Phase::BeforeStageContents(index))?;
    write_private(&blob(stage, "new", index), after, 0o600)?;
    Ok(Change {
        before,
        after: Image::of(after, mode, access),
    })
}

fn recover(root: &Path) -> Result<(), StoreError> {
    if !active_exists(root)? {
        return Ok(());
    }
    require_type(root, fs::symlink_metadata(root)?.is_dir()).map_err(recovery_required)?;
    let active = root.join(ACTIVE);
    let (journal, committed) = read_journal(&active).map_err(recovery_required)?;
    if committed {
        verify_installed(root, &journal).map_err(recovery_required)?;
        sync_directory(&active).map_err(recovery_required)?;
        warn_cleanup(root, retire(root));
    } else {
        rollback(root, &active, &journal)
            .and_then(|()| retire(root).map_err(Into::into))
            .map_err(recovery_required)?;
    }
    Ok(())
}

fn verify_installed(root: &Path, journal: &Journal) -> Result<(), StoreError> {
    check_current_library_path(root, &journal.library)?;
    for (index, change) in journal.files.iter().enumerate() {
        let target = journal.target(root, index);
        if current(&target)?.as_ref() != Some(&change.after) {
            return Err(conflict(&target).into());
        }
    }
    Ok(())
}

fn read_journal(active: &Path) -> Result<(Journal, bool), StoreError> {
    require_type(active, fs::symlink_metadata(active)?.is_dir())?;
    verify_owner(active)?;
    let prepared = metadata(&active.join(PREPARED))?;
    let committed = metadata(&active.join(COMMITTED))?;
    let (filename, committed) = match (prepared, committed) {
        (Some(meta), None) if meta.is_file() => (PREPARED, false),
        (None, Some(meta)) if meta.is_file() => (COMMITTED, true),
        _ => return Err(invalid("expected exactly one regular library journal").into()),
    };
    let journal: Journal = toml::from_str(&fs::read_to_string(active.join(filename))?)?;
    journal.validate()?;
    let manifest_index = journal.blocks.len();
    let change = &journal.files[manifest_index];
    let before = read_blob(
        active,
        "old",
        manifest_index,
        change.before.as_ref().expect("validated manifest"),
    )?;
    let after = read_blob(active, "new", manifest_index, &change.after)?;
    let expected = updated_manifest(
        decode_manifest(&before)?,
        &journal.library,
        &journal.version,
    )?;
    if after != expected.as_bytes() {
        return Err(invalid("library journal would change unrelated manifest data").into());
    }
    Ok((journal, committed))
}

fn rollback(root: &Path, active: &Path, journal: &Journal) -> Result<(), StoreError> {
    check_current_library_path(root, &journal.library)?;
    // Validate the entire rollback first, including each backup, before
    // changing any file. Preserve an unexpected external edit in place.
    for (index, change) in journal.files.iter().enumerate() {
        let target = journal.target(root, index);
        let now = current(&target)?;
        if now != change.before && now.as_ref() != Some(&change.after) {
            return Err(conflict(&target).into());
        }
        if let Some(before) = &change.before {
            read_blob(active, "old", index, before)?;
        }
    }
    for (index, change) in journal.files.iter().enumerate().rev() {
        checkpoint(Phase::Rollback(index))?;
        existing_library_dirs(root, &journal.library)?;
        let target = journal.target(root, index);
        let now = current(&target)?;
        if now == change.before {
            // A previous rollback may have renamed the old file before its
            // directory sync failed. Retry that sync before dropping evidence.
            if now.is_some() || target.parent().is_some_and(Path::is_dir) {
                sync_directory(target.parent().expect("target parent"))?;
            }
            continue;
        }
        if now.as_ref() != Some(&change.after) {
            return Err(conflict(&target).into());
        }
        match &change.before {
            Some(before) => replace(active, &target, index, before, "old")?,
            None => {
                fs::remove_file(&target)?;
                sync_directory(target.parent().expect("target parent"))?;
            }
        }
    }
    let dirs = library_dirs(root, &journal.library);
    for dir in dirs[journal.existing_directories..].iter().rev() {
        match fs::remove_dir(dir) {
            Ok(()) => sync_directory(dir.parent().expect("library directory parent"))?,
            // Leave external additions in place.
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn blob(active: &Path, kind: &str, index: usize) -> PathBuf {
    active.join(format!("{kind}-{index}"))
}

fn read_blob(active: &Path, kind: &str, index: usize, expected: &Image) -> io::Result<Vec<u8>> {
    let path = blob(active, kind, index);
    require_type(&path, fs::symlink_metadata(&path)?.is_file())?;
    let bytes = fs::read(&path)?;
    if format!("{:x}", Sha256::digest(&bytes)) != expected.sha256 || expected.mode & !0o7777 != 0 {
        return Err(invalid(format!(
            "library journal blob or mode invalid: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

fn replace(
    active: &Path,
    target: &Path,
    index: usize,
    image: &Image,
    kind: &str,
) -> Result<(), StoreError> {
    let bytes = read_blob(active, kind, index, image)?;
    // Keep crash leftovers in the excluded journal directory. The rename
    // is within the project filesystem, even for newly created blocks.
    let replacement = active.join("replace.tmp");
    if let Some(meta) = metadata(&replacement)? {
        require_type(&replacement, meta.is_file())?;
        fs::remove_file(&replacement)?;
    }
    let file = write_private(&replacement, &bytes, image.mode)?;
    check_replacement_access(&file, target, image.mode, Some(&image.access))?;
    drop(file);
    fs::rename(replacement, target)?;
    sync_directory(target.parent().expect("target parent"))?;
    Ok(sync_directory(active)?)
}

fn library_dirs(root: &Path, library: &str) -> [PathBuf; 3] {
    let pous = root.join("pous");
    let lib = pous.join(LIBRARY_SLUG_PREFIX);
    let target = lib.join(library);
    [pous, lib, target]
}

fn existing_library_dirs(root: &Path, library: &str) -> Result<usize, StoreError> {
    #[cfg(unix)]
    let device = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(root)?.dev()
    };
    let dirs = library_dirs(root, library);
    for (index, dir) in dirs.iter().enumerate() {
        let Some(meta) = metadata(dir)? else {
            return Ok(index);
        };
        require_type(dir, meta.is_dir())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.dev() != device {
                return Err(invalid(format!(
                    "library directory is on a different filesystem: {}",
                    dir.display()
                ))
                .into());
            }
        }
    }
    require_exact_name(&dirs[2])?;
    Ok(dirs.len())
}

fn check_current_library_path(root: &Path, library: &str) -> Result<usize, StoreError> {
    let manifest = root.join(MANIFEST);
    let (_, bytes) = read_image(&manifest)?
        .ok_or_else(|| StoreError::NotFound(manifest.display().to_string()))?;
    check_library_path(root, library, &decode_manifest(&bytes)?, None)
}

fn check_library_path(
    root: &Path,
    library: &str,
    table: &toml::Table,
    probe: Option<&Path>,
) -> Result<usize, StoreError> {
    let count = existing_library_dirs(root, library)?;
    let registered = table
        .get("libraries")
        .map(|value| {
            value
                .as_table()
                .ok_or_else(|| invalid("[libraries] must be a table"))
        })
        .transpose()?;
    let other_names = registered
        .into_iter()
        .flat_map(|libs| libs.keys())
        .filter(|name| name.as_str() != library && validate_single_segment(name).is_ok());
    if count == 3 {
        let target = &library_dirs(root, library)[2];
        for other in other_names {
            if shares_library_directory(target, &target.with_file_name(other))? {
                return Err(library_alias(library, other));
            }
        }
    } else if let Some(probe) = probe {
        // A legacy manifest may still name a deleted library. Test name
        // resolution on the project filesystem before creating the real path.
        let target = probe.join(library);
        fs::create_dir(&target)?;
        require_exact_name(&target)?;
        for other in other_names {
            if metadata(&probe.join(other))?.is_some() {
                return Err(library_alias(library, other));
            }
        }
        fs::remove_dir(target)?;
    }
    Ok(count)
}

fn shares_library_directory(target: &Path, other: &Path) -> io::Result<bool> {
    let metadata = match fs::metadata(other) {
        Ok(metadata) if metadata.is_dir() => metadata,
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let target = fs::metadata(target)?;
        Ok((metadata.dev(), metadata.ino()) == (target.dev(), target.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        same_file::is_same_file(target, other)
    }
}

fn library_alias(library: &str, other: &str) -> StoreError {
    StoreError::LibraryPathConflict(format!(
        "library '{library}' resolves to the same directory as registered library '{other}'"
    ))
}

fn require_exact_name(path: &Path) -> Result<(), StoreError> {
    let name = path.file_name().expect("library path has a file name");
    for entry in fs::read_dir(path.parent().expect("library path has a parent"))? {
        if entry?.file_name() == name {
            return Ok(());
        }
    }
    Err(StoreError::LibraryPathConflict(format!(
        "{} resolves through a different directory entry spelling",
        path.display()
    )))
}

fn retire(root: &Path) -> io::Result<()> {
    checkpoint(Phase::BeforeCleanup)?;
    let stage = root.join(STAGE);
    if metadata(&stage)?.is_some() {
        return Err(invalid(
            "unexpected staging directory while retiring library journal",
        ));
    }
    // Once retired, even an interrupted recursive removal cannot be
    // mistaken for an incomplete live journal.
    fs::rename(root.join(ACTIVE), stage)?;
    sync_directory(root)?;
    warn_cleanup(root, cleanup_stage(root));
    Ok(())
}

fn owner_marker(directory: &Path) -> io::Result<String> {
    let root = directory
        .parent()
        .expect("transaction directory has a parent");
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(root)?;
        format!("{:x}-{:x}", meta.dev(), meta.ino())
    };
    #[cfg(not(unix))]
    let identity = format!(
        "{:x}",
        Sha256::digest(fs::canonicalize(root)?.as_os_str().as_encoded_bytes())
    );
    Ok(format!("{OWNER}-{identity}"))
}

fn verify_owner(directory: &Path) -> io::Result<String> {
    let owner = owner_marker(directory)?;
    let marker = directory.join(&owner);
    let meta = metadata(&marker)?
        .ok_or_else(|| invalid("library transaction does not belong to this project directory"))?;
    require_type(&marker, meta.is_file() && meta.len() == 0)?;
    Ok(owner)
}

fn cleanup_stage(root: &Path) -> io::Result<()> {
    let stage = root.join(STAGE);
    let Some(meta) = metadata(&stage)? else {
        return Ok(());
    };
    require_type(&stage, meta.is_dir())?;
    let entries = fs::read_dir(&stage)?.collect::<Result<Vec<_>, _>>()?;
    if !entries.is_empty() {
        let owner = verify_owner(&stage)?;
        for entry in &entries {
            let name = entry.file_name();
            let kind = entry.file_type()?;
            let known = match name.to_str() {
                Some("names") => kind.is_dir(),
                Some(PREPARED | COMMITTED | "replace.tmp") => kind.is_file(),
                Some(name) if name == owner => kind.is_file(),
                Some(name) => {
                    kind.is_file()
                        && name.split_once('-').is_some_and(|(prefix, id)| {
                            matches!(prefix, "new" | "old") && id.parse::<usize>().is_ok()
                        })
                }
                None => false,
            };
            if !known {
                return Err(invalid(format!(
                    "unexpected data in library stage: {}",
                    entry.path().display()
                )));
            }
        }
        for entry in entries {
            if entry.file_name() == owner.as_str() {
                continue;
            }
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
        // Keep the owner marker until no data remains. A crash after
        // removing the marker leaves only an empty scratch directory.
        sync_directory(&stage)?;
        fs::remove_file(stage.join(owner))?;
    }
    fs::remove_dir(stage)?;
    sync_directory(root)
}

fn warn_cleanup(root: &Path, result: io::Result<()>) {
    if let Err(error) = result {
        tracing::warn!(project = %root.display(), %error, "library import staging cleanup deferred");
    }
}

fn metadata(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn require_type(path: &Path, expected: bool) -> io::Result<()> {
    if expected {
        Ok(())
    } else {
        Err(invalid(format!(
            "unexpected file type (including symlinks): {}",
            path.display()
        )))
    }
}

fn current(path: &Path) -> Result<Option<Image>, StoreError> {
    Ok(read_image(path)?.map(|(image, _)| image))
}

fn read_image(path: &Path) -> Result<Option<(Image, Vec<u8>)>, StoreError> {
    let Some(meta) = metadata(path)? else {
        return Ok(None);
    };
    require_type(path, meta.is_file())?;
    require_exact_name(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
    let meta = file.metadata()?;
    require_type(path, meta.is_file())?;
    let access = access_fingerprint(&file, path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some((
        Image::of(&bytes, mode(&meta.permissions()), access),
        bytes,
    )))
}

fn access_fingerprint(file: &File, path: &Path) -> Result<String, StoreError> {
    library_permissions::fingerprint(file).map_err(|error| StoreError::LibraryPermissionConflict {
        path: path.display().to_string(),
        reason: error.to_string(),
    })
}

fn check_replacement_access(
    file: &File,
    target: &Path,
    expected_mode: u32,
    expected_access: Option<&str>,
) -> Result<String, StoreError> {
    let access = access_fingerprint(file, target)?;
    if mode(&file.metadata()?.permissions()) != expected_mode
        || expected_access.is_some_and(|expected| expected != access)
    {
        return Err(StoreError::LibraryPermissionConflict {
            path: target.display().to_string(),
            reason: "staged file has different access metadata".into(),
        });
    }
    Ok(access)
}

fn mode(permissions: &Permissions) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        if permissions.readonly() {
            0o400
        } else {
            0o600
        }
    }
}

fn write_private(path: &Path, bytes: &[u8], mode: u32) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let mut permissions = file.metadata()?.permissions();
        permissions.set_readonly(mode & 0o200 == 0);
        file.set_permissions(permissions)?;
    }
    file.sync_all()?;
    Ok(file)
}

fn private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return File::open(path)?.sync_all();
    // This platform's standard File API provides file sync, but no
    // portable directory sync. The journal still handles process exit.
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message.into())
}

fn conflict(path: &Path) -> io::Error {
    invalid(format!(
        "{} changed outside the library import",
        path.display()
    ))
}

fn recovery_required(error: impl std::fmt::Display) -> StoreError {
    StoreError::LibraryRecoveryRequired(error.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    BeforeStage,
    BeforeBackup(usize),
    BeforeStageContents(usize),
    Prepared,
    BeforeInstall(usize),
    AfterInstall(usize),
    BeforeCommit,
    SyncCommit,
    Rollback(usize),
    BeforeCleanup,
}

fn checkpoint(phase: Phase) -> io::Result<()> {
    #[cfg(test)]
    return tests::checkpoint(phase);
    #[cfg(not(test))]
    {
        let _ = phase;
        Ok(())
    }
}

#[cfg(test)]
#[path = "library_transaction_tests.rs"]
mod tests;
