//! Recorded device facts, shared by the IDE and edge. No output or parameter writes.
//!
//! An observation is evidence at its sampling time, never an assertion that an
//! axis is referenced or safe to move. Unknown devices receive only CiA 402 reads;
//! vendor objects are selected from the identity returned by the actual upload.
use crate::ProgramHandle;
use async_trait::async_trait;
use iocore::sdo::{SdoAddress, SdoError, SdoValue};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use ts_rs::TS;

const CAPACITY: usize = 64;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const BATCH_BUDGET: Duration = Duration::from_millis(2500);

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct EngineeringTarget {
    pub device: String,
    pub slave: u16,
}

impl EngineeringTarget {
    pub fn validate(&self) -> Result<(), String> {
        if self.device.trim().is_empty()
            || self.device.len() > 240
            || self.device.chars().any(char::is_control)
        {
            return Err("device must contain 1–240 bytes without control characters".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ObservedIdentity {
    pub vendor_id: u32,
    pub product_id: u32,
    pub revision: u32,
    pub serial: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EngineeringReading {
    pub bytes: Vec<u8>,
    /// Decimal strings preserve all 64 bits in JavaScript clients.
    pub unsigned: String,
    pub signed: String,
    #[ts(type = "number")]
    pub sampled_at_unix_ms: u64,
    pub identity: ObservedIdentity,
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EngineeringParameter {
    pub key: String,
    pub label: String,
    pub object: String,
    pub expected_bytes: u8,
    pub data_type: String,
    pub reading: Option<EngineeringReading>,
    pub error_kind: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EngineeringFinding {
    pub code: String,
    pub severity: String,
    pub message: String,
    pub evidence_keys: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EngineeringObservation {
    pub id: String,
    pub project: String,
    pub target: EngineeringTarget,
    pub runtime_session: String,
    #[ts(type = "number")]
    pub started_at_unix_ms: u64,
    #[ts(type = "number")]
    pub finished_at_unix_ms: u64,
    pub source: String,
    pub profile: String,
    pub complete: bool,
    pub identity_consistent: bool,
    /// Same-model devices with serial zero cannot establish cross-session identity.
    pub identity_strength: String,
    /// Existing SDO uploads lack a bus-generation token. Do not infer continuity.
    pub bus_continuity: String,
    pub retention: String,
    pub mechanical_reference: String,
    pub parameters: Vec<EngineeringParameter>,
    pub findings: Vec<EngineeringFinding>,
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EngineeringHistory {
    pub schema_version: u8,
    pub capacity: usize,
    /// Oldest to newest by commit order, independent of wall-clock changes.
    pub observations: Vec<EngineeringObservation>,
}

impl Default for EngineeringHistory {
    fn default() -> Self {
        Self {
            schema_version: 1,
            capacity: CAPACITY,
            observations: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct EngineeringRefresh {
    pub observation: EngineeringObservation,
    pub saved: bool,
    pub storage_error: Option<String>,
}

#[derive(Clone, Copy)]
struct Parameter {
    key: &'static str,
    label: &'static str,
    index: u16,
    sub: u8,
    width: u8,
}
const COMMON: &[Parameter] = &[
    Parameter {
        key: "statusword",
        label: "Drive status word",
        index: 0x6041,
        sub: 0,
        width: 2,
    },
    Parameter {
        key: "error_code",
        label: "Drive error code",
        index: 0x603f,
        sub: 0,
        width: 2,
    },
    Parameter {
        key: "position",
        label: "Reported position (drive reference units)",
        index: 0x6064,
        sub: 0,
        width: 4,
    },
    Parameter {
        key: "control_mode",
        label: "Displayed control mode",
        index: 0x6061,
        sub: 0,
        width: 1,
    },
];
const SV660N: &[Parameter] = &[
    Parameter {
        key: "absolute_mode",
        label: "Absolute system mode (H02-01)",
        index: 0x2002,
        sub: 2,
        width: 2,
    },
    Parameter {
        key: "motor_code",
        label: "Configured motor code",
        index: 0x2000,
        sub: 1,
        width: 2,
    },
    Parameter {
        key: "direction",
        label: "Rotation direction",
        index: 0x2002,
        sub: 3,
        width: 2,
    },
    Parameter {
        key: "encoder_fault",
        label: "Encoder fault information",
        index: 0x200b,
        sub: 0x1d,
        width: 2,
    },
    Parameter {
        key: "gear_numerator",
        label: "Electronic gear numerator",
        index: 0x6091,
        sub: 1,
        width: 4,
    },
    Parameter {
        key: "gear_denominator",
        label: "Electronic gear denominator",
        index: 0x6091,
        sub: 2,
        width: 4,
    },
    Parameter {
        key: "home_offset",
        label: "Home offset",
        index: 0x607c,
        sub: 0,
        width: 4,
    },
    Parameter {
        key: "linear_offset_low",
        label: "Linear origin offset, low word",
        index: 0x2005,
        sub: 0x2f,
        width: 4,
    },
    Parameter {
        key: "linear_offset_high",
        label: "Linear origin offset, high word",
        index: 0x2005,
        sub: 0x31,
        width: 4,
    },
    Parameter {
        key: "encoder_turns",
        label: "Encoder multi-turn word",
        index: 0x200b,
        sub: 0x47,
        width: 2,
    },
    Parameter {
        key: "encoder_single_turn",
        label: "Encoder single-turn position",
        index: 0x200b,
        sub: 0x48,
        width: 4,
    },
    Parameter {
        key: "encoder_absolute_low",
        label: "Encoder absolute position, low word",
        index: 0x200b,
        sub: 0x4e,
        width: 4,
    },
    Parameter {
        key: "encoder_absolute_high",
        label: "Encoder absolute position, high word",
        index: 0x200b,
        sub: 0x50,
        width: 4,
    },
];

#[async_trait]
pub trait EngineeringReader: Send + Sync {
    async fn read(&self, device: &str, address: SdoAddress) -> Result<SdoValue, SdoError>;
}
#[async_trait]
impl EngineeringReader for ProgramHandle {
    async fn read(&self, device: &str, address: SdoAddress) -> Result<SdoValue, SdoError> {
        self.sdo_upload(device, address).await
    }
}

fn now_ms() -> u64 {
    super::wall_now_us() / 1000
}

async fn read_parameter(
    reader: &dyn EngineeringReader,
    target: &EngineeringTarget,
    p: Parameter,
    deadline: tokio::time::Instant,
) -> EngineeringParameter {
    let mut row = EngineeringParameter {
        key: p.key.into(),
        label: p.label.into(),
        object: format!("0x{:04X}:{:02X}", p.index, p.sub),
        expected_bytes: p.width,
        data_type: format!(
            "{}{}",
            if matches!(
                p.key,
                "position"
                    | "control_mode"
                    | "home_offset"
                    | "linear_offset_high"
                    | "encoder_absolute_high"
            ) {
                "i"
            } else {
                "u"
            },
            p.width * 8
        ),
        reading: None,
        error_kind: None,
        error: None,
    };
    let address = SdoAddress {
        slave: target.slave,
        index: p.index,
        subindex: p.sub,
    };
    let result = if tokio::time::Instant::now() >= deadline {
        Err(SdoError::Timeout)
    } else {
        tokio::time::timeout_at(
            deadline.min(tokio::time::Instant::now() + Duration::from_millis(500)),
            reader.read(&target.device, address),
        )
        .await
        .unwrap_or(Err(SdoError::Timeout))
    };
    match result {
        Ok(value) => {
            if value.address != address || value.bytes.len() != usize::from(p.width) {
                row.error_kind = Some("invalid_response".into());
                row.error = Some(
                    "Reply address or payload width differs from the parameter definition".into(),
                );
            }
            row.reading = Some(EngineeringReading {
                bytes: value.bytes,
                unsigned: value.unsigned.to_string(),
                signed: value.signed.to_string(),
                sampled_at_unix_ms: value.sampled_at_unix_ms,
                identity: ObservedIdentity {
                    vendor_id: value.vendor_id,
                    product_id: value.product_id,
                    revision: value.revision,
                    serial: value.serial,
                },
            });
        }
        Err(e) => {
            row.error_kind = Some(
                serde_json::to_value(&e)
                    .ok()
                    .and_then(|v| v["kind"].as_str().map(str::to_owned))
                    .unwrap_or_else(|| "unknown".into()),
            );
            row.error = Some(e.to_string());
        }
    }
    row
}

fn numeric(rows: &[EngineeringParameter], key: &str) -> Option<u64> {
    rows.iter()
        .find(|r| r.key == key && r.error.is_none())?
        .reading
        .as_ref()?
        .unsigned
        .parse()
        .ok()
}

fn interpret(observation: &mut EngineeringObservation) {
    let add = |o: &mut EngineeringObservation,
               code: &str,
               severity: &str,
               message: &str,
               keys: &[&str]| {
        o.findings.push(EngineeringFinding {
            code: code.into(),
            severity: severity.into(),
            message: message.into(),
            evidence_keys: keys.iter().map(|s| (*s).into()).collect(),
        });
    };
    if observation.identity_strength == "unknown" {
        add(observation, "observation_unavailable", "warning", "No valid parameter response was obtained. Device identity and position retention remain unknown; do not substitute an earlier observation.", &[]);
        return;
    }
    if !observation.identity_consistent {
        add(
            observation,
            "identity_changed",
            "warning",
            "Device identity differs within this batch. Position interpretation is unavailable.",
            &[],
        );
        return;
    }
    if observation.profile == "inovance_sv660n" {
        match numeric(&observation.parameters, "absolute_mode") {
            Some(0) => {
                observation.retention = "disabled_incremental".into();
                add(observation, "incremental_position", "warning", "Incremental mode does not retain the reported coordinate across drive power cycles. A battery box does not enable absolute mode. Compare positions only within an established continuous coordinate interval.", &["absolute_mode"]);
                add(observation, "battery_not_assessed", "info", "Incremental mode does not establish battery health; absence of an encoder battery alarm is not a retention test.", &["absolute_mode"]);
            }
            Some(1..=3) => {
                observation.retention = "configured_absolute_unverified".into();
                add(observation, "absolute_configured", "info", "An absolute mode is configured. Battery retention, the applied mode after power-on and the machine origin still require independent verification.", &["absolute_mode"]);
                if numeric(&observation.parameters, "absolute_mode") == Some(2) {
                    add(observation, "rotary_coordinate", "info", "Rotary absolute mode reports the load's cyclic position. Cumulative travel requires a separate, validated coordinate model.", &["absolute_mode"]);
                }
            }
            Some(4) => {
                observation.retention = "single_turn_only".into();
                add(observation, "single_turn_position", "warning", "Single-turn mode does not preserve a multi-turn machine coordinate.", &["absolute_mode"]);
            }
            _ => add(observation, "position_mode_unknown", "warning", "The position mode could not be interpreted. Do not infer retention from the motor model or position value.", &["absolute_mode"]),
        }
        if numeric(&observation.parameters, "encoder_fault").is_some_and(|n| n != 0) {
            observation.retention = "encoder_fault_present".into();
            add(observation, "encoder_fault_present", "warning", "The encoder reports fault information. Preserve the evidence and diagnose it before using an absolute reference; do not automatically reset multi-turn data.", &["encoder_fault"]);
        }
        if numeric(&observation.parameters, "motor_code") == Some(14101)
            && numeric(&observation.parameters, "gear_numerator") == Some(1)
            && numeric(&observation.parameters, "gear_denominator") == Some(1)
        {
            add(observation, "position_width", "info", "At 8,388,608 counts per motor revolution and 1:1 electronic gearing, a signed 32-bit coordinate spans approximately -256 to +256 motor revolutions. Encoder multi-turn range is a separate limit.", &["motor_code", "gear_numerator", "gear_denominator"]);
        }
    } else {
        add(observation, "unsupported_position_profile", "info", "No supported vendor position profile was identified. Standard drive values do not establish encoder type or power-off retention.", &[]);
    }
    if observation.identity_strength != "reported_serial" {
        add(observation, "weak_device_identity", "info", "A zero serial number identifies only the observed slot and model; it cannot prove the same physical device across reconnects.", &[]);
    }
    add(observation, "non_atomic_sample", "info", "Parameters are sequential samples, not one atomic position snapshot. Do not combine separate high/low words into a trusted moving coordinate. No bus-generation continuity is available.", &[]);
    add(observation, "machine_reference_unverified", "info", "These reads do not establish a machine origin, travel limits, physical safety or permission to move. Runtime reconnection alone is not evidence that mechanics changed.", &[]);
    if !observation.complete {
        add(observation, "partial_observation", "warning", "Some reads failed or were not completed within the batch budget. Their values remain unknown; earlier observations are historical evidence.", &[]);
    }
}

fn identity_changed(rows: &[EngineeringParameter]) -> bool {
    let mut identities = rows
        .iter()
        .filter_map(|r| r.reading.as_ref().map(|v| &v.identity));
    identities
        .next()
        .is_some_and(|first| identities.any(|i| i != first))
}

pub async fn collect(
    reader: &dyn EngineeringReader,
    target: EngineeringTarget,
    project: String,
    session: String,
    id: String,
) -> EngineeringObservation {
    let started = now_ms();
    let deadline = tokio::time::Instant::now() + BATCH_BUDGET;
    let mut rows = Vec::new();
    for p in COMMON {
        rows.push(read_parameter(reader, &target, *p, deadline).await);
        if identity_changed(&rows) {
            break;
        }
    }
    let identity = rows
        .iter()
        .find(|r| r.error.is_none())
        .and_then(|r| r.reading.as_ref())
        .map(|r| r.identity.clone());
    let supported = identity
        .as_ref()
        .is_some_and(|i| i.vendor_id == 0x0010_0000 && i.product_id == 0x000c_010d);
    if supported && !identity_changed(&rows) {
        for p in SV660N {
            rows.push(read_parameter(reader, &target, *p, deadline).await);
            if identity_changed(&rows) {
                break;
            }
        }
    }
    let consistent = identity.as_ref().is_some_and(|i| {
        rows.iter()
            .filter_map(|r| r.reading.as_ref())
            .all(|r| &r.identity == i)
    });
    let mut result = EngineeringObservation {
        id,
        project,
        target,
        runtime_session: session,
        started_at_unix_ms: started,
        finished_at_unix_ms: now_ms(),
        source: "device_sdo_upload".into(),
        profile: if supported {
            "inovance_sv660n"
        } else {
            "generic_cia402"
        }
        .into(),
        complete: !identity_changed(&rows)
            && rows
                .iter()
                .all(|r| r.reading.is_some() && r.error.is_none()),
        identity_consistent: consistent,
        identity_strength: match identity {
            Some(i) if i.serial != 0 => "reported_serial",
            Some(_) => "slot_and_model_only",
            None => "unknown",
        }
        .into(),
        bus_continuity: "not_verified".into(),
        retention: "unknown".into(),
        mechanical_reference: "not_verified".into(),
        parameters: rows,
        findings: Vec::new(),
    };
    interpret(&mut result);
    result
}

/// Serialize refreshes before they reach the scan-command queue. Disk work stays
/// on blocking workers, outside the real-time scan; the on-disk lock covers other processes.
#[derive(Default)]
pub struct EngineeringService {
    gate: Arc<tokio::sync::Mutex<()>>,
}

struct Archive {
    directory: PathBuf,
    lock: fs::File,
    history: EngineeringHistory,
}
impl Drop for Archive {
    fn drop(&mut self) {
        let _ = self.lock.unlock();
    }
}

fn load(directory: &Path) -> Result<EngineeringHistory, String> {
    let path = directory.join("observations.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(EngineeringHistory::default())
        }
        Err(e) => return Err(e.to_string()),
    };
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err("Engineering archive is not a bounded regular file".into());
    }
    let h: EngineeringHistory = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| format!("Engineering archive is invalid; preserved without overwrite: {e}"))?;
    if h.schema_version != 1 || h.capacity != CAPACITY || h.observations.len() > CAPACITY {
        return Err("Unsupported engineering archive schema or capacity".into());
    }
    Ok(h)
}

impl Archive {
    fn open(directory: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        if !fs::symlink_metadata(&directory)
            .map_err(|e| e.to_string())?
            .is_dir()
        {
            return Err("Engineering directory must not be a symlink".into());
        }
        let path = directory.join("archive.lock");
        if fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file()) {
            return Err("Engineering lock must be a regular file".into());
        }
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| e.to_string())?;
        lock.try_lock()
            .map_err(|e| format!("Engineering archive is busy or unavailable: {e}"))?;
        let history = load(&directory)?;
        Ok(Self {
            directory,
            lock,
            history,
        })
    }
    fn save(&mut self, observation: EngineeringObservation) -> Result<(), String> {
        self.history.observations.push(observation);
        if self.history.observations.len() > CAPACITY {
            self.history.observations.remove(0);
        }
        let bytes = serde_json::to_vec_pretty(&self.history).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err("Engineering archive capacity exceeded".into());
        }
        let pending = self.directory.join(format!(
            "pending-{}-{}.json",
            std::process::id(),
            super::wall_now_us()
        ));
        let outcome = (|| -> std::io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&pending)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&pending, self.directory.join("observations.json"))?;
            fs::File::open(&self.directory)?.sync_all()
        })();
        if outcome.is_err() {
            let _ = fs::remove_file(pending);
        }
        outcome.map_err(|e| e.to_string())
    }
}

impl EngineeringService {
    pub async fn history(&self, directory: PathBuf) -> Result<EngineeringHistory, String> {
        tokio::task::spawn_blocking(move || load(&directory))
            .await
            .map_err(|e| e.to_string())?
    }
    pub async fn refresh(
        &self,
        directory: PathBuf,
        project: String,
        target: EngineeringTarget,
        handle: ProgramHandle,
    ) -> Result<EngineeringRefresh, (u16, String)> {
        let session = handle.observation_session().to_owned();
        self.refresh_reader(directory, project, target, &handle, session)
            .await
    }
    async fn refresh_reader(
        &self,
        directory: PathBuf,
        project: String,
        target: EngineeringTarget,
        reader: &dyn EngineeringReader,
        session: String,
    ) -> Result<EngineeringRefresh, (u16, String)> {
        target.validate().map_err(|e| (400, e))?;
        let permit = self.gate.clone().try_lock_owned().map_err(|_| {
            (
                409,
                "An engineering observation is already in progress".into(),
            )
        })?;
        let mut archive = tokio::task::spawn_blocking(move || Archive::open(directory))
            .await
            .map_err(|e| (500, e.to_string()))?
            .map_err(|e| (503, e))?;
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = format!(
            "{}-{}-{}",
            session,
            super::wall_now_us(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let observation = collect(reader, target, project, session, id).await;
        let saved_observation = observation.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let result = archive.save(saved_observation);
            drop(archive);
            drop(permit);
            result
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
        Ok(EngineeringRefresh {
            observation,
            saved: outcome.is_ok(),
            storage_error: outcome.err(),
        })
    }
}

#[cfg(test)]
mod tests;
