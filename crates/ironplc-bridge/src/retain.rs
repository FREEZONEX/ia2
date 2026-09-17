//! Read / write the RETAIN variable state file.
//!
//! The file is a single JSON object the runtime writes atomically
//! (tmp + rename) every few seconds and on graceful shutdown:
//!
//! ```json
//! { "schema": 2,
//!   "saved_at_us": 1700000000000000,
//!   "scan_count": 12345,
//!   "vars": { "setpoint": 1078523331, "total_m3": 4631166901565532406 } }
//! ```
//!
//! Schema 2: each value is the variable's **raw 64-bit VM slot**
//! (`read_variable_raw` / `write_variable_raw`), stored verbatim. That
//! round-trips every IEC type losslessly — including LREAL / LINT /
//! ULINT / LWORD, which schema 1's i32 encoding truncated.
//!
//! Schema 1 files (i32 values) are migrated on load: the VM's
//! `write_variable(i32)` sign-extends (`v as i64 as u64`), so applying
//! the same widening reproduces exactly the slot the old restore path
//! would have produced.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct RetainState {
    /// Schema version — bump when the value encoding changes
    /// incompatibly. Loading a higher schema than this binary
    /// supports falls back to "no restored state".
    pub schema: u32,
    /// Microseconds since the program started when this file was
    /// last written. Useful for "stale state" diagnostics; not used
    /// for correctness.
    #[serde(default)]
    pub saved_at_us: u64,
    /// Scan count at flush time. Same role as `saved_at_us` —
    /// surfaces "when did this snapshot come from".
    #[serde(default)]
    pub scan_count: u64,
    /// Variable name → last-known raw 64-bit VM slot value. Names
    /// match `VarDebugInfo.name` (lower-cased; see
    /// `extract_retain_vars` in lib.rs).
    pub vars: HashMap<String, u64>,
}

/// Current schema version. Increment when the on-disk shape changes
/// in a way older versions can't read.
const SCHEMA: u32 = 2;

/// Schema-1 shape, kept only for migration. Values are the i32 the
/// old `write_variable` restore path accepted. (`schema` itself is
/// re-checked via the probe in `load`, so it's not declared here.)
#[derive(Debug, Deserialize)]
struct RetainStateV1 {
    #[serde(default)]
    saved_at_us: u64,
    #[serde(default)]
    scan_count: u64,
    vars: HashMap<String, i32>,
}

/// Read `path` and return parsed state, or `Ok(None)` if the file
/// doesn't exist (first run / fresh deploy). Schema-1 files migrate
/// in memory (sign-extended, matching the old restore semantics); the
/// next flush rewrites them as schema 2. Returns `Err` on I/O or
/// parse errors so the caller can log loudly without aborting startup.
pub fn load(path: &Path) -> std::io::Result<Option<RetainState>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    // Peek the schema first — v1 stored i32 values which won't parse
    // into the u64 map (negatives), so dispatch before full decode.
    #[derive(Deserialize)]
    struct SchemaOnly {
        #[serde(default)]
        schema: u32,
    }
    let probe: SchemaOnly = serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if probe.schema > SCHEMA {
        tracing::warn!(
            ?path,
            file_schema = probe.schema,
            binary_schema = SCHEMA,
            "retain state file has higher schema than this binary supports; ignoring"
        );
        return Ok(None);
    }
    if probe.schema <= 1 {
        let v1: RetainStateV1 = serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        tracing::info!(?path, "migrating retain state file schema 1 → 2");
        return Ok(Some(RetainState {
            schema: SCHEMA,
            saved_at_us: v1.saved_at_us,
            scan_count: v1.scan_count,
            // Same widening `write_variable(i32)` applied on restore.
            vars: v1
                .vars
                .into_iter()
                .map(|(k, v)| (k, v as i64 as u64))
                .collect(),
        }));
    }
    let state: RetainState = serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(state))
}

/// Write atomically through [`project::write_atomic`] — the same writer
/// every project file uses: temporary → fsync → rename → directory fsync,
/// so a crash or power cut leaves either the previous good file or the new
/// one, never a partial write. The parent directory is created if missing
/// so callers don't have to pre-mkdir `state/`.
pub fn save(path: &Path, state: &RetainState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    project::write_atomic(path, bytes)
}

/// Builder helper so the runtime doesn't have to know the schema
/// version or field layout.
pub fn build(vars: HashMap<String, u64>, saved_at_us: u64, scan_count: u64) -> RetainState {
    RetainState {
        schema: SCHEMA,
        saved_at_us,
        scan_count,
        vars,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_returns_none_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert!(load(&path).unwrap().is_none());
    }

    #[test]
    fn save_then_load_roundtrip_preserves_vars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("retain.json");
        let mut vars = HashMap::new();
        vars.insert("setpoint".into(), 42u64);
        // Full 64-bit patterns survive: an LREAL bit pattern and a
        // sign-extended negative DINT slot.
        vars.insert("total_m3".into(), 1234.5678f64.to_bits());
        vars.insert("counter".into(), (-17i32) as i64 as u64);
        save(&path, &build(vars.clone(), 12345, 100)).unwrap();
        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.vars, vars);
        assert_eq!(loaded.scan_count, 100);
        assert_eq!(loaded.schema, SCHEMA);
        assert_eq!(
            f64::from_bits(loaded.vars["total_m3"]),
            1234.5678,
            "LREAL slot bits round-trip losslessly"
        );
    }

    #[test]
    fn load_migrates_schema1_with_sign_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retain.json");
        let v1 = serde_json::json!({
            "schema": 1,
            "saved_at_us": 7,
            "scan_count": 9,
            "vars": { "setpoint": 42, "counter": -17 }
        });
        std::fs::write(&path, v1.to_string()).unwrap();
        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.schema, SCHEMA);
        assert_eq!(loaded.saved_at_us, 7);
        assert_eq!(loaded.vars["setpoint"], 42u64);
        // -17 widens exactly as the old write_variable(i32) restore did.
        assert_eq!(loaded.vars["counter"], (-17i32) as i64 as u64);
    }

    #[test]
    fn load_rejects_corrupt_file_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retain.json");
        std::fs::write(&path, b"not json at all").unwrap();
        let err = load(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn load_ignores_future_schema_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retain.json");
        let future = serde_json::json!({
            "schema": SCHEMA + 5,
            "vars": { "x": 1 }
        });
        std::fs::write(&path, future.to_string()).unwrap();
        assert!(load(&path).unwrap().is_none());
    }
}
