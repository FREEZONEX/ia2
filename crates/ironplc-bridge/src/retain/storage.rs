use super::*;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub(super) struct Lease {
    path: PathBuf,
    _lock: File,
    expected: Option<String>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self._lock.unlock();
    }
}
impl Lease {
    pub fn acquire(path: &Path) -> Result<Self, String> {
        let parent = path.parent().ok_or("RETAIN path needs a parent")?;
        create_durable_dirs(parent)?;
        let path = fs::canonicalize(parent)
            .map_err(|e| e.to_string())?
            .join(path.file_name().ok_or("RETAIN filename missing")?);
        let lock_path = path.with_extension("lock");
        reject_non_regular(&path)?;
        reject_non_regular(&lock_path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(&lock_path).map_err(|e| e.to_string())?;
        lock.try_lock()
            .map_err(|e| format!("RETAIN writer lock unavailable: {e}"))?;
        Ok(Self {
            path,
            _lock: lock,
            expected: None,
        })
    }
    pub fn read(&mut self) -> Result<Option<Vec<u8>>, String> {
        reject_non_regular(&self.path)?;
        let bytes = match fs::read(&self.path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.to_string()),
        };
        self.expected = bytes.as_ref().map(|b| digest(b));
        Ok(bytes)
    }
    pub fn archive(&self, bytes: &[u8], reason: &str) -> Result<String, String> {
        let parent = self.path.parent().unwrap();
        let dir = parent.join("retain-archive");
        if let Ok(meta) = fs::symlink_metadata(&dir) {
            if !meta.is_dir() {
                return Err("RETAIN archive is not a real directory".into());
            }
        }
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| format!("archive parent sync: {e}"))?;
        let hash = digest(bytes);
        let path = dir.join(format!("{hash}.json"));
        preserve(&path, bytes)?;
        let metadata = serde_json::to_vec_pretty(&serde_json::json!({"source_sha256": hash, "reason": reason, "source_path": self.path, "recorded_at_unix_ms": now_ms()})).map_err(|e| e.to_string())?;
        let note = dir.join(format!("{hash}.metadata.json"));
        if !note.exists() {
            preserve(&note, &metadata)?;
        }
        File::open(&dir)
            .and_then(|f| f.sync_all())
            .map_err(|e| format!("archive directory sync: {e}"))?;
        Ok(path.to_string_lossy().into_owned())
    }
    pub fn save(
        &mut self,
        state: &RetainState,
    ) -> Result<(project::AtomicWriteReceipt, String), String> {
        reject_non_regular(&self.path)?;
        let current = match fs::read(&self.path) {
            Ok(bytes) => Some(digest(&bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.to_string()),
        };
        if current != self.expected {
            return Err("RETAIN file changed outside its writer; refusing overwrite".into());
        }
        let bytes = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
        let receipt =
            project::write_atomic_receipt(&self.path, &bytes).map_err(|e| e.to_string())?;
        let hash = digest(&bytes);
        self.expected = Some(hash.clone()); // rename happened even if directory sync failed
        Ok((receipt, hash))
    }
}
fn reject_non_regular(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() => Err(format!("{} is not a regular file", path.display())),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
// Persist newly created directory entries all the way to an existing parent.
fn create_durable_dirs(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() || path.is_dir() {
        return Ok(());
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    create_durable_dirs(parent)?;
    match fs::create_dir(path) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => (),
        Err(e) => return Err(e.to_string()),
    }
    let sync_parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    File::open(sync_parent)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("state directory creation sync: {e}"))
}

fn preserve(path: &Path, bytes: &[u8]) -> Result<(), String> {
    reject_non_regular(path)?;
    if path.exists() {
        if fs::read(path).map_err(|e| e.to_string())? != bytes {
            return Err("archive name exists with different bytes; original preserved".into());
        }
        return File::open(path)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string());
    }
    // Publish only a fully synced file with create-if-absent semantics. A kill
    // during writing can leave a dot temporary, never a partial archive entry.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = path.with_file_name(format!(
        ".archive.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).map_err(|e| e.to_string())?;
    let written = (|| -> io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::hard_link(&tmp, path)?;
        Ok(())
    })();
    let cleanup = fs::remove_file(&tmp);
    written.map_err(|e| e.to_string())?;
    cleanup.map_err(|e| e.to_string())
}

pub(super) struct Prepared {
    pub result: RecoveryResult,
    pub restored: BTreeMap<String, u64>,
    pub lease: Option<Lease>,
}
pub(super) fn prepare(config: &RetainConfig, plan: &Plan) -> Prepared {
    let failure = |kind, reason| Prepared {
        result: RecoveryResult::new(kind, false, reason),
        restored: BTreeMap::new(),
        lease: None,
    };
    let Some(path) = &config.path else {
        return if config.policy.required.is_empty() {
            Prepared {
                result: RecoveryResult::new(
                    RecoveryKind::Disabled,
                    true,
                    "ephemeral run; no RETAIN storage",
                ),
                restored: BTreeMap::new(),
                lease: None,
            }
        } else {
            failure(
                RecoveryKind::ConfigurationError,
                "required RETAIN data needs persistent storage".into(),
            )
        };
    };
    if plan.entries.is_empty() {
        return Prepared {
            result: RecoveryResult::new(
                RecoveryKind::Disabled,
                true,
                "no retained scalar declarations; existing file untouched",
            ),
            restored: BTreeMap::new(),
            lease: None,
        };
    }
    if config.policy.project_id.is_none() {
        return failure(RecoveryKind::ConfigurationError, "persistent RETAIN requires an explicit project.toml [retain].project_id; legacy files are not automatically claimed".into());
    }
    let mut lease = match Lease::acquire(path) {
        Ok(l) => l,
        Err(e) => {
            let kind = if e.contains("lock unavailable") {
                RecoveryKind::WriterBusy
            } else {
                RecoveryKind::ReadFailed
            };
            return failure(kind, e);
        }
    };
    let bytes = match lease.read() {
        Ok(b) => b,
        Err(e) => return failure(RecoveryKind::ReadFailed, e),
    };
    let mut result = RecoveryResult::new(RecoveryKind::FirstStart, false, "no RETAIN checkpoint");
    let mut restored = BTreeMap::new();
    if let Some(bytes) = &bytes {
        result.source_sha256 = Some(digest(bytes));
        let decoded = decode(bytes).and_then(|s| compatibility(&s, &plan.initial, &config.policy));
        match decoded {
            Ok(values) => {
                result.kind = RecoveryKind::Restored;
                result.admitted = true;
                result.reason = "identity and scalar layouts compatible".into();
                result.restored = values.keys().cloned().collect();
                restored = values;
                let old: RetainState = serde_json::from_slice(bytes).unwrap();
                result.resolution = old.resolution.clone();
                if old.data_generation == 0 {
                    if let Some(origin) = &old.resolution {
                        result.kind = match origin.action {
                            ResolutionAction::Migrate => RecoveryKind::Migrated,
                            _ => RecoveryKind::ColdStart,
                        };
                        result.reason = format!(
                            "explicit {:?} checkpoint recovered; source {}",
                            origin.action, origin.source_sha256
                        );
                    }
                }
                result.removed = old
                    .vars
                    .keys()
                    .filter(|k| !plan.initial.vars.contains_key(*k))
                    .cloned()
                    .collect();
            }
            Err((kind, reason)) => {
                result.kind = kind;
                result.reason = reason;
            }
        }
        // Protect the complete source before ANY new checkpoint, including
        // compatible upgrades that drop variables needed by rollback.
        match lease.archive(bytes, &result.reason) {
            Ok(path) => result.archive_path = Some(path),
            Err(e) => {
                result.source_kind = result.kind;
                result.kind = RecoveryKind::ProtectionFailed;
                result.reason = e;
                result.admitted = false;
                return Prepared {
                    result,
                    restored: BTreeMap::new(),
                    lease: None,
                };
            }
        }
    }
    result.source_kind = result.kind;
    if bytes.is_none() && !config.policy.required.is_empty() {
        result.reason = format!(
            "required RETAIN data missing: {}",
            config.policy.required.join(", ")
        );
    }
    if !result.admitted
        && config.policy.required.is_empty()
        && result
            .kind
            .cold_condition()
            .is_some_and(|c| config.policy.allow_cold_start.contains(&c))
    {
        result.admitted = true;
        if result.kind != RecoveryKind::FirstStart {
            result.kind = RecoveryKind::ColdStart;
        }
    }
    result.initialized = plan
        .initial
        .vars
        .keys()
        .filter(|k| !restored.contains_key(*k))
        .cloned()
        .collect();
    result.writes_enabled = result.admitted;
    if result.admitted {
        Prepared {
            result,
            restored,
            lease: Some(lease),
        }
    } else {
        Prepared {
            result,
            restored: BTreeMap::new(),
            lease: None,
        }
    }
}
fn decode(bytes: &[u8]) -> Result<RetainState, (RecoveryKind, String)> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| (RecoveryKind::Corrupt, e.to_string()))?;
    match value.get("schema").and_then(|v| v.as_u64()) {
        Some(1 | 2) => Err((RecoveryKind::LegacyUnverified, "legacy RETAIN has no identity/type evidence; explicit migration or cold start required".into())),
        Some(3) => serde_json::from_slice(bytes).map_err(|e| (RecoveryKind::Corrupt, e.to_string())),
        Some(schema) => Err((RecoveryKind::UnsupportedVersion, format!("unsupported RETAIN schema {schema}"))),
        None => Err((RecoveryKind::Corrupt, "missing/invalid RETAIN schema".into())),
    }
}

pub(super) fn resolve(
    config: &RetainConfig,
    plan: &Plan,
    request: &ResolveRequest,
) -> Result<ResolveResult, String> {
    let path = config.path.as_ref().ok_or("no RETAIN storage")?;
    if config.policy.project_id.is_none() {
        return Err("configure project_id before resolving RETAIN".into());
    }
    let mut lease = Lease::acquire(path)?;
    let bytes = lease.read()?.unwrap_or_default();
    let hash = digest(&bytes);
    if hash != request.expected_source_sha256 {
        return Err("RETAIN source changed; re-read its SHA256 before resolving".into());
    }
    let selected = if let Some(selected_hash) = &request.migration_source_sha256 {
        if request.action != ResolutionAction::Migrate {
            return Err("migration_source_sha256 is only valid for migration".into());
        }
        if selected_hash.len() != 64 || !selected_hash.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("invalid archived source SHA256".into());
        }
        let dir = lease.path.parent().unwrap().join("retain-archive");
        if !fs::symlink_metadata(&dir)
            .map_err(|e| e.to_string())?
            .is_dir()
        {
            return Err("archive directory is not a real directory".into());
        }
        let selected_path = dir.join(format!("{}.json", selected_hash.to_lowercase()));
        reject_non_regular(&selected_path)?;
        let selected = fs::read(selected_path).map_err(|e| e.to_string())?;
        if digest(&selected) != selected_hash.to_lowercase() {
            return Err("archived source SHA256 mismatch".into());
        }
        selected
    } else {
        bytes.clone()
    };
    let mut state = match request.action {
        ResolutionAction::Migrate => migrate(&selected, request, plan, &config.policy)?,
        ResolutionAction::ColdStart | ResolutionAction::Discard => {
            if !config.policy.required.is_empty() && !request.initialize_required {
                return Err("initializing required data needs initialize_required=true".into());
            }
            if !request.rules.is_empty() {
                return Err("cold start/discard does not accept migration rules".into());
            }
            plan.initial.clone()
        }
    };
    state.resolution = Some(ResolutionEvidence {
        action: request.action,
        source_sha256: digest(&selected),
        replaced_source_sha256: Some(hash.clone()),
    });
    state.saved_at_unix_ms = now_ms();
    let preserved_current_path = lease.archive(
        &bytes,
        &format!("replaced by explicit {:?}", request.action),
    )?;
    let archive_path = lease.archive(
        &selected,
        &format!("source of explicit {:?}", request.action),
    )?;
    let (receipt, checkpoint_sha256) = lease.save(&state)?;
    Ok(ResolveResult {
        replaced_source_sha256: hash,
        preserved_current_path,
        action: request.action,
        source_sha256: digest(&selected),
        archive_path,
        checkpoint_sha256,
        receipt,
        restart_required: true,
    })
}
