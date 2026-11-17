//! Identity- and type-checked RETAIN recovery, shared by server and edge.
//! Raw scalar slots are lossless decimal strings on disk and on the wire.
//! No filesystem operation occurs in a scan round.
mod storage;
#[cfg(test)]
mod tests;
mod worker;

use ironplc_container::debug_section::iec_type_tag as tag;
use ironplc_vm::{Vm, VmBuffers};
use project::{ColdStartCondition, RetainPolicy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use ts_rs::TS;

pub(crate) use worker::RetainWriter;
pub use worker::{
    CheckpointExport, CheckpointReceipt, FinalSave, PersistenceHealth, RetainControl, RetainStatus,
};
pub const SCHEMA: u32 = 3;

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct RetainIdentity {
    pub project_id: String,
    pub instance_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct SlotLayout {
    pub type_name: String,
    pub width_bits: u8,
}
impl SlotLayout {
    pub fn scalar(name: &str) -> Result<Self, String> {
        let name = name.to_uppercase();
        let width_bits = match name.as_str() {
            "BOOL" => 1,
            "SINT" | "USINT" | "BYTE" => 8,
            "INT" | "UINT" | "WORD" => 16,
            "DINT" | "UDINT" | "DWORD" | "REAL" => 32,
            "LINT" | "ULINT" | "LWORD" | "LREAL" => 64,
            _ => return Err(format!("unsupported RETAIN shape/type {name}; only elementary numeric/BOOL scalar slots are supported")),
        };
        Ok(Self {
            type_name: name,
            width_bits,
        })
    }
    fn validate(&self) -> Result<(), String> {
        if Self::scalar(&self.type_name)? != *self {
            return Err("inconsistent RETAIN width/type".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct RetainedValue {
    pub layout: SlotLayout,
    pub bits: String,
}
impl RetainedValue {
    fn raw(&self) -> Result<u64, String> {
        self.layout.validate()?;
        self.bits
            .parse()
            .map_err(|_| "invalid unsigned 64-bit RETAIN slot".into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct ResolutionEvidence {
    pub action: ResolutionAction,
    pub source_sha256: String,
    #[serde(default)]
    pub replaced_source_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct RetainState {
    pub schema: u32,
    pub identity: RetainIdentity,
    /// Content provenance, not an admission identity. Logic-only edits are compatible.
    pub content_sha256: String,
    #[ts(type = "number")]
    pub saved_at_unix_ms: u64,
    #[ts(type = "number")]
    pub scan_count: u64,
    #[ts(type = "number")]
    pub data_generation: u64,
    pub vars: BTreeMap<String, RetainedValue>,
    pub resolution: Option<ResolutionEvidence>,
}

#[derive(Debug, Clone)]
pub struct RetainConfig {
    pub path: Option<PathBuf>,
    pub policy: RetainPolicy,
}
impl RetainConfig {
    pub fn ephemeral() -> Self {
        Self {
            path: None,
            policy: RetainPolicy::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub unit: usize,
    pub key: String,
    pub slot: u16,
    pub layout: SlotLayout,
}
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub entries: Vec<Entry>,
    pub initial: RetainState,
}
impl Plan {
    pub fn from_units(units: &[crate::ProgramUnit], policy: &RetainPolicy) -> Result<Self, String> {
        policy.validate()?;
        let mut entries = Vec::new();
        let mut vars = BTreeMap::new();
        let mut content = Vec::new();
        for (unit_index, unit) in units.iter().enumerate() {
            unit.container
                .write_to(&mut content)
                .map_err(|e| e.to_string())?;
            content.extend_from_slice(unit.instance.as_bytes());
            let mut buffers = VmBuffers::from_container(&unit.container);
            let vm = Vm::new()
                .load(&unit.container, &mut buffers)
                .map_err(|e| format!("RETAIN initial VM: {e:?}"))?;
            let vm = vm
                .start()
                .map_err(|e| format!("RETAIN initial VM: {e:?}"))?;
            for name in &unit.retain_vars {
                let info = unit
                    .container
                    .debug_section
                    .iter()
                    .flat_map(|d| &d.var_names)
                    .find(|v| &v.name == name)
                    .ok_or_else(|| {
                        format!("RETAIN declaration {name} has no supported debug slot")
                    })?;
                if !matches!(
                    info.iec_type_tag,
                    tag::BOOL
                        | tag::SINT
                        | tag::INT
                        | tag::DINT
                        | tag::LINT
                        | tag::USINT
                        | tag::UINT
                        | tag::UDINT
                        | tag::ULINT
                        | tag::REAL
                        | tag::LREAL
                        | tag::BYTE
                        | tag::WORD
                        | tag::DWORD
                        | tag::LWORD
                ) {
                    return Err(format!(
                        "unsupported RETAIN shape {} ({name})",
                        info.type_name
                    ));
                }
                let layout = SlotLayout::scalar(&info.type_name)?;
                let key = format!("{}.{}", unit.instance.to_lowercase(), name.to_lowercase());
                let raw = vm
                    .read_variable_raw(info.var_index)
                    .map_err(|e| format!("RETAIN initial read: {e:?}"))?;
                if vars
                    .insert(
                        key.clone(),
                        RetainedValue {
                            layout: layout.clone(),
                            bits: raw.to_string(),
                        },
                    )
                    .is_some()
                {
                    return Err(format!("duplicate RETAIN key {key}"));
                }
                entries.push(Entry {
                    unit: unit_index,
                    key,
                    slot: info.var_index.raw(),
                    layout,
                });
            }
        }
        for required in &policy.required {
            if !vars.contains_key(&required.to_lowercase()) {
                return Err(format!("required RETAIN key {required} is not declared"));
            }
        }
        Ok(Self {
            entries,
            initial: RetainState {
                schema: SCHEMA,
                identity: RetainIdentity {
                    project_id: policy.project_id.clone().unwrap_or_default(),
                    instance_id: policy.instance_id.clone(),
                },
                content_sha256: digest(&content),
                saved_at_unix_ms: 0,
                scan_count: 0,
                data_generation: 0,
                vars,
                resolution: None,
            },
        })
    }
    pub fn sample(
        &self,
        raw: BTreeMap<String, u64>,
        generation: u64,
        scan_count: u64,
    ) -> RetainState {
        let mut state = self.initial.clone();
        state.vars = self
            .entries
            .iter()
            .filter_map(|e| {
                raw.get(&e.key).map(|v| {
                    (
                        e.key.clone(),
                        RetainedValue {
                            layout: e.layout.clone(),
                            bits: v.to_string(),
                        },
                    )
                })
            })
            .collect();
        state.data_generation = generation;
        state.scan_count = scan_count;
        state.saved_at_unix_ms = crate::monitor::wall_now_us() / 1000;
        state
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryKind {
    Disabled,
    Pending,
    FirstStart,
    Restored,
    Migrated,
    ColdStart,
    Corrupt,
    UnsupportedVersion,
    LegacyUnverified,
    IdentityMismatch,
    LayoutMismatch,
    ReadFailed,
    ProtectionFailed,
    WriterBusy,
    ConfigurationError,
}
impl RecoveryKind {
    fn cold_condition(self) -> Option<ColdStartCondition> {
        match self {
            Self::FirstStart => Some(ColdStartCondition::Missing),
            Self::Corrupt => Some(ColdStartCondition::Corrupt),
            Self::UnsupportedVersion => Some(ColdStartCondition::UnsupportedVersion),
            Self::LegacyUnverified => Some(ColdStartCondition::LegacyUnverified),
            Self::IdentityMismatch => Some(ColdStartCondition::IdentityMismatch),
            Self::LayoutMismatch => Some(ColdStartCondition::LayoutMismatch),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RecoveryResult {
    pub kind: RecoveryKind,
    /// Original classification survives a policy-authorized cold start.
    pub source_kind: RecoveryKind,
    pub admitted: bool,
    pub writes_enabled: bool,
    pub reason: String,
    pub source_sha256: Option<String>,
    pub resolution: Option<ResolutionEvidence>,
    pub archive_path: Option<String>,
    pub restored: Vec<String>,
    pub initialized: Vec<String>,
    pub removed: Vec<String>,
}
impl RecoveryResult {
    pub(crate) fn new(kind: RecoveryKind, admitted: bool, reason: impl Into<String>) -> Self {
        Self {
            kind,
            source_kind: kind,
            admitted,
            writes_enabled: false,
            reason: reason.into(),
            source_sha256: None,
            resolution: None,
            archive_path: None,
            restored: vec![],
            initialized: vec![],
            removed: vec![],
        }
    }
}

/// Pure compatibility decision used by recovery and deployment consumers.
/// Rejected decisions return no values: no partial restore can reach a VM.
pub fn compatibility(
    state: &RetainState,
    expected: &RetainState,
    policy: &RetainPolicy,
) -> Result<BTreeMap<String, u64>, (RecoveryKind, String)> {
    if state.schema != SCHEMA {
        return Err((
            RecoveryKind::UnsupportedVersion,
            format!("unsupported RETAIN schema {}", state.schema),
        ));
    }
    if state.identity != expected.identity {
        return Err((
            RecoveryKind::IdentityMismatch,
            "RETAIN project/instance identity mismatch".into(),
        ));
    }
    let mut restored = BTreeMap::new();
    for (key, saved) in &state.vars {
        let raw = saved
            .raw()
            .map_err(|e| (RecoveryKind::Corrupt, format!("{key}: {e}")))?;
        if let Some(target) = expected.vars.get(key) {
            if target.layout != saved.layout {
                return Err((
                    RecoveryKind::LayoutMismatch,
                    format!(
                        "RETAIN {key}: {:?} -> {:?} requires explicit migration",
                        saved.layout, target.layout
                    ),
                ));
            }
            restored.insert(key.clone(), raw);
        }
    }
    for required in &policy.required {
        if !restored.contains_key(&required.to_lowercase()) {
            return Err((
                RecoveryKind::LayoutMismatch,
                format!("required RETAIN {required} missing"),
            ));
        }
    }
    Ok(restored)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionAction {
    ColdStart,
    Discard,
    Migrate,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum Conversion {
    Exact,
    DintToReal,
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct MigrationRule {
    pub from: String,
    pub to: String,
    pub source_layout: SlotLayout,
    pub conversion: Conversion,
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct ResolveRequest {
    pub expected_source_sha256: String,
    /// Select an already preserved checkpoint for rollback; no filesystem paths.
    #[serde(default)]
    pub migration_source_sha256: Option<String>,
    pub action: ResolutionAction,
    #[serde(default)]
    pub initialize_required: bool,
    /// Required for legacy data, whose file cannot prove either identity or types.
    pub attested_source_identity: Option<RetainIdentity>,
    #[serde(default)]
    pub rules: Vec<MigrationRule>,
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ResolveResult {
    pub replaced_source_sha256: String,
    pub preserved_current_path: String,
    pub action: ResolutionAction,
    pub source_sha256: String,
    pub archive_path: String,
    pub checkpoint_sha256: String,
    pub receipt: project::AtomicWriteReceipt,
    pub restart_required: bool,
}

fn migrate(
    bytes: &[u8],
    request: &ResolveRequest,
    plan: &Plan,
    policy: &RetainPolicy,
) -> Result<RetainState, String> {
    let json: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let schema = json
        .get("schema")
        .and_then(|v| v.as_u64())
        .ok_or("missing RETAIN schema")?;
    let mut target = plan.initial.clone();
    let mut sources = BTreeMap::<String, RetainedValue>::new();
    match schema {
        1 | 2 => {
            if request.attested_source_identity.as_ref() != Some(&target.identity) {
                return Err("legacy migration needs attested_source_identity matching the destination; verify it from external evidence".into());
            }
            let vars = json
                .get("vars")
                .and_then(|v| v.as_object())
                .ok_or("legacy vars missing")?;
            for rule in &request.rules {
                let value = vars
                    .get(&rule.from)
                    .ok_or_else(|| format!("missing source {}", rule.from))?;
                let raw = if schema == 1 {
                    let i = value.as_i64().ok_or("schema 1 needs i32")?;
                    i32::try_from(i).map_err(|_| "schema 1 outside i32")? as i64 as u64
                } else {
                    value.as_u64().ok_or("schema 2 needs u64")?
                };
                sources.insert(
                    rule.from.clone(),
                    RetainedValue {
                        layout: rule.source_layout.clone(),
                        bits: raw.to_string(),
                    },
                );
            }
        }
        3 => {
            let old: RetainState = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
            if old.identity != target.identity {
                return Err(
                    "migration cannot transfer state across project/instance identities".into(),
                );
            }
            sources = old.vars;
        }
        _ => return Err(format!("schema {schema} has no migration decoder")),
    }
    let mut assigned = std::collections::HashSet::new();
    for rule in &request.rules {
        if !assigned.insert(rule.to.clone()) {
            return Err("duplicate migration target".into());
        }
        let old = sources
            .get(&rule.from)
            .ok_or_else(|| format!("missing source {}", rule.from))?;
        if old.layout != rule.source_layout {
            return Err(format!("source layout mismatch: {}", rule.from));
        }
        let new = target
            .vars
            .get_mut(&rule.to)
            .ok_or_else(|| format!("unknown target {}", rule.to))?;
        let raw = old.raw()?;
        new.bits = match rule.conversion {
            Conversion::Exact if old.layout == new.layout => raw.to_string(),
            Conversion::DintToReal
                if old.layout.type_name == "DINT" && new.layout.type_name == "REAL" =>
            {
                let value = raw as i32;
                let converted = value as f32;
                if converted as f64 != value as f64 {
                    return Err("DINT -> REAL would lose integer precision".into());
                }
                u64::from(converted.to_bits()).to_string()
            }
            _ => return Err("migration type/conversion mismatch".into()),
        };
    }
    // Explicit complete mapping avoids silently retaining old data the reviewer
    // did not authorize, and distinguishes defaults from migrated values.
    for key in target.vars.keys() {
        if !assigned.contains(key)
            && (!request.initialize_required
                || policy.required.iter().any(|r| r.eq_ignore_ascii_case(key)))
        {
            return Err(format!("migration needs an explicit rule for {key}"));
        }
    }
    target.resolution = Some(ResolutionEvidence {
        action: ResolutionAction::Migrate,
        source_sha256: digest(bytes),
        replaced_source_sha256: None,
    });
    Ok(target)
}

pub(crate) fn now_ms() -> u64 {
    crate::monitor::wall_now_us() / 1000
}

/// Offline maintenance entry. Acquires the same exclusive lease as a runtime;
/// never connects devices and never starts a scan.
pub fn resolve_stopped(
    config: &RetainConfig,
    units: &[crate::ProgramUnit],
    request: &ResolveRequest,
) -> Result<ResolveResult, String> {
    storage::resolve(config, &Plan::from_units(units, &config.policy)?, request)
}

/// Compile-time target descriptor for deployment/rollback compatibility checks.
/// Runs only VM initialization in memory, without I/O or scheduled scans.
pub fn declaration_state(
    units: &[crate::ProgramUnit],
    policy: &RetainPolicy,
) -> Result<RetainState, String> {
    Plan::from_units(units, policy).map(|plan| plan.initial)
}
