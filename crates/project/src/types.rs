use serde::{Deserialize, Serialize};
use ts_rs::TS;

// ---------------- Manifest (project.toml) ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectManifest {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    /// Write governance for the monitor `/write` path. Absent = `open`,
    /// which is exactly the pre-governance behaviour (any declared
    /// variable writable, any value).
    #[serde(default, skip_serializing_if = "WriteGovernance::is_default")]
    pub governance: WriteGovernance,
}

fn default_version() -> String {
    "0.1".into()
}

// ---------------- Write governance (project.toml [governance]) ----------------
//
// The monitor write path (`/write`, HMI actions, MQTT northbound writes)
// accepts any value on any variable unless the project says otherwise.
// Governance is declared in project.toml — not a new config file:
//
//   [governance]
//   write_mode = "allowlist"
//
//   [[governance.rules]]
//   variable = "level_sp"        # monitor name: bare or instance.variable
//   min = 0.0
//   max = 100.0
//
// `open` (default) applies no checks. `allowlist` rejects writes to
// variables with no matching rule and clamps out-of-range values to the
// rule's min/max (the response echoes the clamped value — nothing is
// silent). `/force` deliberately bypasses governance: it is the pure
// debug override and is never exported on any northbound surface
// (ADR-0002).

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    /// Legacy behaviour: every declared variable is writable, no clamps.
    #[default]
    Open,
    /// Only variables named by a `[[governance.rules]]` entry are
    /// writable; min/max on the rule clamp the written value.
    Allowlist,
}

// `deny_unknown_fields` on both governance structs: a typo'd key
// (`writemode`, `mim`) must be a loud parse error, not a silently-open
// policy / silently-unenforced bound. The table is new, so no legacy
// project.toml can carry extra keys here. `ProjectManifest` itself stays
// tolerant of unknown top-level tables (e.g. `[libraries]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct WriteRule {
    /// Monitor variable name — bare (`level_sp`) or instance-qualified
    /// (`main.level_sp`). Matching is case-insensitive, like the IEC.
    pub variable: String,
    /// Inclusive lower bound; values below are clamped up to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub min: Option<f64>,
    /// Inclusive upper bound; values above are clamped down to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub max: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct WriteGovernance {
    #[serde(default)]
    pub write_mode: WriteMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<WriteRule>,
}

impl WriteGovernance {
    /// True when governance carries no behaviour — used to keep
    /// untouched project.tomls round-tripping byte-identical.
    pub fn is_default(&self) -> bool {
        self.write_mode == WriteMode::Open && self.rules.is_empty()
    }

    /// Sanity-check the governance table. TOML 1.0 happily parses
    /// `min = nan`, `min = inf`, swapped bounds, and empty or repeated
    /// variable names — all of which would make enforcement misbehave
    /// on the scan thread. `ProjectStore::open` calls this wherever the
    /// manifest is parsed so a bad table is a loud load error, never a
    /// silently wrong policy. Every problem found is reported (joined
    /// with "; "), each naming the offending rule.
    pub fn validate(&self) -> Result<(), String> {
        use std::collections::HashMap;

        let mut errors: Vec<String> = Vec::new();
        // Lowercased variable → 1-based index of the first rule naming
        // it. Rule matching is case-insensitive, so duplicates are too.
        let mut first_use: HashMap<String, usize> = HashMap::new();

        for (i, rule) in self.rules.iter().enumerate() {
            let n = i + 1;
            let label = if rule.variable.trim().is_empty() {
                format!("governance rule #{n}")
            } else {
                format!("governance rule #{n} ('{}')", rule.variable)
            };

            if rule.variable.trim().is_empty() {
                errors.push(format!("{label}: variable must not be empty"));
            }
            for (bound, value) in [("min", rule.min), ("max", rule.max)] {
                if let Some(v) = value {
                    if !v.is_finite() {
                        errors.push(format!(
                            "{label}: {bound} must be a finite number (got {v})"
                        ));
                    }
                }
            }
            if let (Some(min), Some(max)) = (rule.min, rule.max) {
                if min > max {
                    errors.push(format!("{label}: min ({min}) is greater than max ({max})"));
                }
            }

            let key = rule.variable.to_lowercase();
            if let Some(&first) = first_use.get(&key) {
                errors.push(format!(
                    "{label}: duplicate of rule #{first} — each variable may appear in one rule only"
                ));
            } else {
                first_use.insert(key, n);
            }
        }

        // A bare rule matches its variable in EVERY program instance, so
        // it always overlaps a qualified rule for the same leaf name.
        // Enforcement would take whichever rule is listed first — an
        // ambiguity, not a precedence feature — so reject the pair.
        for i in 0..self.rules.len() {
            for j in (i + 1)..self.rules.len() {
                let a = self.rules[i].variable.to_lowercase();
                let b = self.rules[j].variable.to_lowercase();
                if a == b {
                    continue; // already reported as an exact duplicate
                }
                let overlap = match (a.split_once('.'), b.split_once('.')) {
                    (None, Some((_, leaf))) => leaf == a,
                    (Some((_, leaf)), None) => leaf == b,
                    _ => false,
                };
                if overlap {
                    errors.push(format!(
                        "governance rule #{} ('{}') and rule #{} ('{}') govern the same \
                         variable (a bare name matches it in every program instance) — keep one",
                        i + 1,
                        self.rules[i].variable,
                        j + 1,
                        self.rules[j].variable
                    ));
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

// ---------------- POUs (Program Organization Units) ----------------
//
// A POU is one IEC declaration (a single `PROGRAM`, `FUNCTION_BLOCK`,
// or `FUNCTION` block). The file is just the storage medium — one
// `.st` file may declare multiple POUs side by side.
//
// The tree shows POU declarations; the editor reads/writes by file
// path. `PouFile` carries both: the on-disk identifier + the parser-
// derived declaration list inside it.

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum PouType {
    Program,
    FunctionBlock,
    Function,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum PouLanguage {
    /// Structured Text — the textual source of truth; the graphical
    /// languages below transpile to it at compile time.
    St,
    Ld,
    Fbd,
    Sfc,
}

/// One IEC POU declaration parsed out of a `.st` file. Surfaced in the
/// tree as a separate node — a multi-POU file (e.g. a FB next to a
/// PROGRAM) renders as multiple sibling nodes sharing a file path.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PouDecl {
    /// IEC identifier — what `PROGRAM <inst> WITH <task> : <name>`
    /// references in tasks.toml.
    pub name: String,
    #[serde(rename = "type")]
    pub type_: PouType,
    pub language: PouLanguage,
}

/// A `.st` file on disk and the POU declarations parsed from it.
/// The tree section's children are `PouFile`s; the editor opens by
/// `path`.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct PouFile {
    /// Project-relative slash-path under `pous/`, without `.st`
    /// extension (same identifier system the old `Application.name`
    /// used — keeps the URL pattern `/api/pous/{path}` predictable).
    pub path: String,
    /// Declarations in source order. Empty when the file fails to
    /// parse — the tree still shows the file so the user can fix it.
    pub declarations: Vec<PouDecl>,
}

/// Full read response: same as `PouFile` plus the raw source.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct Pou {
    pub path: String,
    pub source: String,
    pub declarations: Vec<PouDecl>,
}

/// Intermediate type used by `ProjectStore::tree_skeleton` — carries
/// each `.st` file's raw source. The server's `/api/project` handler
/// pairs this with `ironplc_bridge::extract_pou_declarations` to build
/// the final `ProjectTree` with parsed POU declarations. Not exported
/// to TS because the frontend never sees this shape directly.
#[derive(Debug, Clone, Serialize)]
pub struct PouFileSource {
    pub path: String,
    pub source: String,
    /// Detected from the file extension — `.st` → `St`, `.ld.json` →
    /// `Ld`. Lets the bridge route parsing/transpilation per file
    /// without re-probing the filesystem.
    pub language: PouLanguage,
}

/// As `ProjectTree`, but with each POU file represented as raw source
/// rather than parsed declarations. The server fills in declarations
/// at the API layer.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectTreeSkeleton {
    pub name: String,
    pub path: String,
    pub pous: Vec<PouFileSource>,
    pub pou_folders: Vec<String>,
    pub devices: Vec<Device>,
    pub device_folders: Vec<String>,
    pub edges: Vec<Edge>,
    pub edge_folders: Vec<String>,
    pub iomap: IoMap,
    pub tasks: Tasks,
}

// ---------------- Devices ----------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Modbus,
    Ethercat,
    Opcua,
    Canopen,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "protocol", rename_all = "snake_case")]
pub enum ProtocolConfig {
    Modbus(ModbusConfig),
    Ethercat(EthercatConfig),
    Opcua(OpcuaConfig),
    Canopen(CanopenConfig),
}

impl ProtocolConfig {
    pub fn protocol(&self) -> Protocol {
        match self {
            ProtocolConfig::Modbus(_) => Protocol::Modbus,
            ProtocolConfig::Ethercat(_) => Protocol::Ethercat,
            ProtocolConfig::Opcua(_) => Protocol::Opcua,
            ProtocolConfig::Canopen(_) => Protocol::Canopen,
        }
    }

    pub fn channel_names(&self) -> Vec<String> {
        match self {
            ProtocolConfig::Modbus(c) => c.channels.iter().map(|c| c.name.clone()).collect(),
            ProtocolConfig::Ethercat(c) => c.channels.iter().map(|c| c.name.clone()).collect(),
            ProtocolConfig::Opcua(c) => c.channels.iter().map(|c| c.name.clone()).collect(),
            ProtocolConfig::Canopen(c) => c.channels.iter().map(|c| c.name.clone()).collect(),
        }
    }
}

/// Modbus device config. The transport is either TCP (host + port) or
/// RTU (a serial port + line settings).
///
/// **Back-compat note** — projects saved before RTU support landed
/// had `host` + `port` as top-level fields rather than wrapped in a
/// `transport` variant. The custom `Deserialize` below accepts both
/// shapes: a config with `transport` deserialises directly, and one
/// with just `host` + `port` is upgraded in-place to a Tcp variant.
/// On the next write the new shape is persisted, so old projects
/// migrate silently on first save.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ModbusConfig {
    pub transport: ModbusTransport,
    pub slave_id: u8,
    /// Polling interval in milliseconds (u32 so it round-trips through JSON
    /// as a number rather than a TS bigint).
    pub poll_interval_ms: u32,
    /// Per-request timeout in milliseconds, applied to the TCP connect and
    /// to every Modbus request (reads, writes, failsafe). `None` = adapter
    /// default (1000 ms). Optional + skip-if-none so existing device tomls
    /// round-trip byte-identical until the user sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u32>,
    /// Initial reconnect backoff in milliseconds after a transport failure
    /// (the adapter doubles it per failed attempt up to a fixed 10 s cap).
    /// `None` = adapter default (1000 ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnect_backoff_ms: Option<u32>,
    #[serde(default)]
    pub channels: Vec<ModbusChannel>,
}

// Custom deserialize so projects authored against the old flat
// `{host, port, …}` shape keep loading. `untagged` tries variants in
// order — the new shape wins because it has a `transport` field;
// the old shape matches when `transport` is missing.
impl<'de> Deserialize<'de> for ModbusConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Compat {
            New {
                transport: ModbusTransport,
                slave_id: u8,
                #[serde(default = "default_poll_interval_ms")]
                poll_interval_ms: u32,
                #[serde(default)]
                timeout_ms: Option<u32>,
                #[serde(default)]
                reconnect_backoff_ms: Option<u32>,
                #[serde(default)]
                channels: Vec<ModbusChannel>,
            },
            // Legacy: TCP fields at the top level, no `transport`.
            // Auto-upgrades to ModbusTransport::Tcp on read.
            Old {
                host: String,
                port: u16,
                slave_id: u8,
                #[serde(default = "default_poll_interval_ms")]
                poll_interval_ms: u32,
                #[serde(default)]
                timeout_ms: Option<u32>,
                #[serde(default)]
                reconnect_backoff_ms: Option<u32>,
                #[serde(default)]
                channels: Vec<ModbusChannel>,
            },
        }
        let v = Compat::deserialize(deserializer)?;
        Ok(match v {
            Compat::New {
                transport,
                slave_id,
                poll_interval_ms,
                timeout_ms,
                reconnect_backoff_ms,
                channels,
            } => Self {
                transport,
                slave_id,
                poll_interval_ms,
                timeout_ms,
                reconnect_backoff_ms,
                channels,
            },
            Compat::Old {
                host,
                port,
                slave_id,
                poll_interval_ms,
                timeout_ms,
                reconnect_backoff_ms,
                channels,
            } => Self {
                transport: ModbusTransport::Tcp(ModbusTcpParams { host, port }),
                slave_id,
                poll_interval_ms,
                timeout_ms,
                reconnect_backoff_ms,
                channels,
            },
        })
    }
}

fn default_poll_interval_ms() -> u32 {
    100
}

/// Either Modbus TCP (open a socket) or Modbus RTU (open a serial
/// port). The wire payloads are byte-identical; only the transport
/// differs. JSON tag is `"kind"`.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModbusTransport {
    Tcp(ModbusTcpParams),
    Rtu(ModbusRtuParams),
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ModbusTcpParams {
    pub host: String,
    pub port: u16,
}

/// Modbus RTU serial-line parameters. The defaults match the most
/// common configuration in the wild (9600-8-N-1) so a minimal
/// `{"kind":"rtu","serial_device":"/dev/ttyUSB0","baud_rate":9600}`
/// JSON is valid input.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ModbusRtuParams {
    /// OS device path. macOS: `/dev/cu.usbserial-*` (or
    /// `/dev/tty.usbserial-*` for synchronous open). Linux:
    /// `/dev/ttyUSB0` / `/dev/ttyS0`. Windows: `COM3`.
    pub serial_device: String,
    /// Bits per second. Modbus RTU is typically one of
    /// 1200 / 2400 / 4800 / 9600 / 19200 / 38400 / 57600 / 115200.
    pub baud_rate: u32,
    /// 5 / 6 / 7 / 8 data bits. Modbus RTU is 8 in practice; we
    /// expose the choice so non-standard slaves still work.
    #[serde(default)]
    pub data_bits: ModbusDataBits,
    /// 1 / 2 stop bits. Spec: 2 when parity is None, 1 when parity
    /// is Even/Odd. We let the user set it explicitly — vendor
    /// variations are common.
    #[serde(default)]
    pub stop_bits: ModbusStopBits,
    /// None / Even / Odd. Even is the spec default; None is what
    /// Modicon / many Chinese inverters actually ship.
    #[serde(default)]
    pub parity: ModbusParity,
    /// RS485 half-duplex direction control. `None` (default) opens the
    /// port in plain serial mode — correct for USB adapters with automatic
    /// (auto-direction) transceivers. `Some` enables the kernel RS485 mode
    /// (Linux `TIOCSRS485`) so the driver toggles RTS/DE around each frame;
    /// required by adapters whose transmitter is *RTS-gated* — without it
    /// the master never actually drives the bus and every request times out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rs485: Option<ModbusRs485>,
}

/// Linux RS485 (`TIOCSRS485`) direction-control settings for an RTU port.
/// Only applied on Linux; ignored (with a warning) elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ModbusRs485 {
    /// RTS/DE is asserted *high* while transmitting (`SER_RS485_RTS_ON_SEND`)
    /// — the common case. Set false for adapters that drive on RTS *low*
    /// (uses `SER_RS485_RTS_AFTER_SEND` instead).
    #[serde(default = "default_true")]
    pub rts_on_send: bool,
    /// Keep the receiver enabled during transmit (`SER_RS485_RX_DURING_TX`).
    /// 2-wire adapters echo the master's own bytes; enable so the stack can
    /// see/skip the echo rather than the kernel muting RX.
    #[serde(default)]
    pub rx_during_tx: bool,
    /// RTS settle delay *before* the frame, in milliseconds (driver enable).
    #[serde(default)]
    pub delay_rts_before_send_ms: u32,
    /// RTS settle delay *after* the frame, in milliseconds (turnaround to RX).
    #[serde(default)]
    pub delay_rts_after_send_ms: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq, Default)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ModbusParity {
    #[default]
    None,
    Even,
    Odd,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq, Default)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ModbusDataBits {
    Five,
    Six,
    Seven,
    #[default]
    Eight,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq, Default)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ModbusStopBits {
    #[default]
    One,
    Two,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ModbusChannel {
    pub name: String,
    pub kind: ModbusChannelKind,
    pub address: u16,
    /// Register interpretation (register kinds only; coils/discretes
    /// ignore it). 32-bit types span TWO consecutive registers starting
    /// at `address` — the norm for instrument floats and totalizers.
    #[serde(default)]
    pub data_type: ModbusDataType,
    /// Word order for 32-bit types: which register holds the high word.
    /// `hi_lo` = big-endian word order ("ABCD", the Modbus-spec default);
    /// `lo_hi` = swapped ("CDAB", common on Chinese instruments).
    #[serde(default)]
    pub word_order: ModbusWordOrder,
    /// Whether this register may be WRITTEN. Couplers routinely map
    /// read-only measurements into HOLDING registers (the NX6 maps DI
    /// and AI there); writing those draws an Illegal-data-address
    /// exception. `read` excludes the channel from output bindings AND
    /// from the shutdown/trip failsafe sweep.
    #[serde(default)]
    pub access: ModbusAccess,
}

/// Modbus write permission. The default is `write` — the OPPOSITE of
/// the OPC UA/CANopen default — for two deliberate reasons: on a field
/// bus IA2 owns the outputs, so the failsafe sweep zeroing every
/// writable channel is the safe default (a valve must close on stop);
/// and existing device files predate this field, so the default must
/// preserve their failsafe coverage. Mark measurement registers
/// `access = "read"` — `validate_iomap` nudges you where it can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ModbusAccess {
    #[default]
    Write,
    Read,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ModbusDataType {
    #[default]
    U16,
    I16,
    U32,
    I32,
    F32,
}

impl ModbusDataType {
    /// How many 16-bit registers this type occupies.
    pub fn register_len(self) -> u16 {
        match self {
            ModbusDataType::U16 | ModbusDataType::I16 => 1,
            ModbusDataType::U32 | ModbusDataType::I32 | ModbusDataType::F32 => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ModbusWordOrder {
    #[default]
    HiLo,
    LoHi,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ModbusChannelKind {
    Coil,
    DiscreteInput,
    HoldingRegister,
    InputRegister,
}

#[cfg(test)]
mod modbus_access_compat_tests {
    use super::*;

    /// Device files predate the access field: absent must mean `write`
    /// so their failsafe coverage is unchanged.
    #[test]
    fn legacy_channel_without_access_defaults_to_write() {
        let ch: ModbusChannel =
            toml::from_str("name = \"ao1\"\nkind = \"holding_register\"\naddress = 193\n").unwrap();
        assert_eq!(ch.access, ModbusAccess::Write);
    }

    #[test]
    fn access_read_parses() {
        let ch: ModbusChannel = toml::from_str(
            "name = \"ai1\"\nkind = \"holding_register\"\naddress = 64\naccess = \"read\"\n",
        )
        .unwrap();
        assert_eq!(ch.access, ModbusAccess::Read);
    }
}

#[cfg(test)]
mod gear_config_compat_tests {
    use super::*;

    /// A legacy [[gear]] block authored before the dynamic-ratio channels
    /// existed must still deserialize: the two new channel names default,
    /// so old projects load unchanged.
    #[test]
    fn legacy_gear_block_gets_default_ratio_apply_channels() {
        let toml = r#"
            slave_index = 0
            target_pos_offset = 2
            actual_pos_offset = 4
            status_word_offset = 2
            master = { kind = "virtual" }
        "#;
        let g: EthercatGear = toml::from_str(toml).unwrap();
        assert_eq!(g.ratio_apply_channel, "gear_ratio_apply");
        assert_eq!(g.ratio_ack_channel, "gear_ratio_ack");
        // and the pre-existing defaults still apply
        assert_eq!(g.engage_channel, "gear_engage");
        assert_eq!(g.trip_channel, "gear_trip");
    }

    /// Explicit channel names survive a round-trip.
    #[test]
    fn ratio_apply_channels_roundtrip_when_set() {
        let toml = r#"
            slave_index = 1
            target_pos_offset = 2
            actual_pos_offset = 4
            status_word_offset = 2
            master = { kind = "axis", slave_index = 0, actual_pos_offset = 4 }
            ratio_apply_channel = "apply_now"
            ratio_ack_channel = "apply_done"
        "#;
        let g: EthercatGear = toml::from_str(toml).unwrap();
        assert_eq!(g.ratio_apply_channel, "apply_now");
        assert_eq!(g.ratio_ack_channel, "apply_done");
        let back = toml::to_string(&g).unwrap();
        let g2: EthercatGear = toml::from_str(&back).unwrap();
        assert_eq!(g2.ratio_apply_channel, "apply_now");
        assert_eq!(g2.ratio_ack_channel, "apply_done");
    }
}

#[cfg(test)]
mod modbus_config_compat_tests {
    use super::*;

    /// Old-shape JSON without `transport` upgrades to a Tcp variant.
    /// Same shape the IDE / on-disk project files have been writing
    /// since v0.
    #[test]
    fn deserializes_legacy_flat_shape_as_tcp() {
        let json = serde_json::json!({
            "host": "192.168.1.50",
            "port": 502,
            "slave_id": 1,
            "poll_interval_ms": 100,
            "channels": [],
        });
        let cfg: ModbusConfig = serde_json::from_value(json).unwrap();
        match &cfg.transport {
            ModbusTransport::Tcp(p) => {
                assert_eq!(p.host, "192.168.1.50");
                assert_eq!(p.port, 502);
            }
            other => panic!("expected Tcp, got {other:?}"),
        }
        // Fields added after v0 default to None on legacy input…
        assert_eq!(cfg.timeout_ms, None);
        assert_eq!(cfg.reconnect_backoff_ms, None);
        // …and stay absent on re-serialize, so untouched device tomls
        // keep round-tripping byte-identical.
        let back = serde_json::to_value(&cfg).unwrap();
        assert!(back.get("timeout_ms").is_none());
        assert!(back.get("reconnect_backoff_ms").is_none());
    }

    /// Explicit timeout / backoff values survive a round-trip in both
    /// the new and the legacy top-level shape.
    #[test]
    fn timeout_and_backoff_roundtrip_when_set() {
        let json = serde_json::json!({
            "transport": { "kind": "tcp", "host": "127.0.0.1", "port": 5502 },
            "slave_id": 1,
            "poll_interval_ms": 100,
            "timeout_ms": 250,
            "reconnect_backoff_ms": 500,
            "channels": [],
        });
        let cfg: ModbusConfig = serde_json::from_value(json).unwrap();
        assert_eq!(cfg.timeout_ms, Some(250));
        assert_eq!(cfg.reconnect_backoff_ms, Some(500));
        let back = serde_json::to_value(&cfg).unwrap();
        assert_eq!(back["timeout_ms"], 250);
        assert_eq!(back["reconnect_backoff_ms"], 500);

        // Legacy flat shape with the new fields also picks them up.
        let legacy = serde_json::json!({
            "host": "10.0.0.9",
            "port": 502,
            "slave_id": 3,
            "timeout_ms": 750,
        });
        let cfg: ModbusConfig = serde_json::from_value(legacy).unwrap();
        assert_eq!(cfg.timeout_ms, Some(750));
        assert_eq!(cfg.reconnect_backoff_ms, None);
    }

    /// New-shape JSON with `transport.kind = "tcp"` round-trips.
    #[test]
    fn roundtrips_new_tcp_shape() {
        let json = serde_json::json!({
            "transport": { "kind": "tcp", "host": "127.0.0.1", "port": 5502 },
            "slave_id": 7,
            "poll_interval_ms": 50,
            "channels": [],
        });
        let cfg: ModbusConfig = serde_json::from_value(json.clone()).unwrap();
        let back = serde_json::to_value(&cfg).unwrap();
        assert_eq!(back["transport"]["kind"], "tcp");
        assert_eq!(back["slave_id"], 7);
    }

    /// RTU shape with minimal fields uses sensible defaults
    /// (8-N-1 — the universal Modbus RTU default).
    #[test]
    fn parses_rtu_with_defaults_filled_in() {
        let json = serde_json::json!({
            "transport": {
                "kind": "rtu",
                "serial_device": "/dev/ttyUSB0",
                "baud_rate": 19200,
            },
            "slave_id": 2,
            "poll_interval_ms": 100,
            "channels": [],
        });
        let cfg: ModbusConfig = serde_json::from_value(json).unwrap();
        match cfg.transport {
            ModbusTransport::Rtu(p) => {
                assert_eq!(p.serial_device, "/dev/ttyUSB0");
                assert_eq!(p.baud_rate, 19200);
                assert_eq!(p.data_bits, ModbusDataBits::Eight);
                assert_eq!(p.parity, ModbusParity::None);
                assert_eq!(p.stop_bits, ModbusStopBits::One);
            }
            other => panic!("expected Rtu, got {other:?}"),
        }
    }
}

/// Distributed-clock (DC) mode for an EtherCAT bus.
///
/// Most servo drives (e.g. Inovance SV660N) **require** DC SYNC0 to reach
/// the OP state — without it the SAFE-OP→OP transition times out. Simple
/// IO couplers, by contrast, run fine free-running and some can't be DC-
/// configured at all. So DC is opt-in per device: `Off` by default (safe
/// for IO), `Sync0` for drives that need it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum EthercatDcSync {
    /// Free-running — no distributed clocks. Default.
    #[default]
    Off,
    /// Enable the SYNC0 pulse on every SubDevice (period = `cycle_us`).
    Sync0,
}

/// How the EtherCAT layer brings a device's bus to OP and discovers its
/// process data.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum EthercatBringup {
    /// Read the device's runtime CoE PDO-assignment objects
    /// (`0x1C12`/`0x1C13`) to discover the process image. The default;
    /// works for fixed-PDO servos and slices that publish their mapping
    /// over CoE.
    #[default]
    Auto,
    /// ESI-driven modular bring-up: build the process image from the
    /// device's ESI (.xml) file + the modules it reports at `0xF050`,
    /// programming SyncManagers/FMMUs directly. For modular couplers whose
    /// assembled module PDOs never appear over runtime CoE (`0x1C12`
    /// read-only, `0xF030` absent), so auto-discovery has nothing to read.
    EsiModular {
        /// Project-relative path to the device's ESI (.xml) file.
        esi_path: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EthercatConfig {
    /// Network interface bound to the MainDevice (e.g. "eth0", "en0").
    /// Persisted so the IDE can preserve the user's intent even on hosts
    /// where the NIC isn't currently up.
    pub nic: String,
    /// Bring-up strategy. `Auto` (CoE PDO-assignment discovery) by default;
    /// `EsiModular` for modular couplers that need ESI-driven assembly.
    #[serde(default)]
    pub bringup: EthercatBringup,
    /// SYNC0 cycle time in microseconds (also the free-run scan period when
    /// `dc_sync` is `Off`). Defaults to 1 ms (1 kHz).
    #[serde(default = "default_cycle_us")]
    pub cycle_us: u32,
    /// Distributed-clock mode. `Off` (free-run) by default; set to `sync0`
    /// for servo drives that need DC to reach OP. Individual SubDevices can
    /// override this via `EthercatSlave.dc_sync` — a mixed bus (servo +
    /// plain IO coupler) sets `sync0` here and `off` on the coupler, or
    /// `off` here and `sync0` on the drive.
    #[serde(default)]
    pub dc_sync: EthercatDcSync,
    /// Iterations of ethercrab's static drift compensation during init — a
    /// burst of FRMW frames that converges SubDevice clocks before SYNC0
    /// starts. 0 (default) skips it: short buses come up fine without it,
    /// and on a non-RT host one lost frame mid-burst aborts init with
    /// Timeout(Pdu). Raise it (1000–10000) on longer DC buses where clock
    /// convergence at OP-entry matters.
    #[serde(default)]
    pub dc_static_sync_iterations: u32,
    /// Bus topology — describes the SubDevices the MainDevice expects to
    /// find on the ring. Order matters: the `index` here is the auto-
    /// incremented 0-based position on the bus, matching how ethercrab
    /// numbers slaves after the discovery walk.
    #[serde(default)]
    pub slaves: Vec<EthercatSlave>,
    /// PDO channels exposed to IO mapping. Kept flat (rather than nested
    /// under each slave) so the iomap layer treats them identically to
    /// Modbus channels — referenced by a unique name string, resolved
    /// via this list.
    #[serde(default)]
    pub channels: Vec<EthercatChannel>,
    /// In-cycle electronic gear axes (B-tier motion). Each entry makes
    /// the cyclic loop generate the follower axis's target_position every
    /// bus cycle, strictly cycle-aligned; the PLC scan plane only feeds
    /// slow parameters (ratio / engage / …) through the named channels
    /// below, which the device routes into a lock-free shared struct
    /// instead of PDI bytes. The loop OWNS the follower's target_position
    /// bytes — PLC writes to that PDO field are overwritten every cycle.
    #[serde(default)]
    pub gear: Vec<EthercatGear>,
}

fn default_cycle_us() -> u32 {
    1_000
}

/// One in-cycle electronic gear axis: a follower SubDevice whose
/// target_position is generated inside the cyclic loop from a master
/// source (virtual accumulator or another axis's actual_position).
///
/// Safety model (mirrors the field-hardened ST-tier gear): while the
/// follower drive is not in CiA402 Operation Enabled the engine shadows
/// its actual_position (no jump at enable); engagement requires
/// `max_travel` > 0 and latches ratio/phase at the engage edge (mid-run
/// parameter edits are inert until re-engage); overtravel trips to a
/// position hold until the engage channel is dropped.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EthercatGear {
    /// Bus position of the follower (controlled) axis.
    pub slave_index: u16,
    /// Byte offset of target_position (0x607A, i32) in the follower's
    /// output PDI.
    pub target_pos_offset: u16,
    /// Byte offset of actual_position (0x6064, i32) in the follower's
    /// input PDI — read for the not-enabled shadow and engage latch.
    pub actual_pos_offset: u16,
    /// Byte offset of statusword (0x6041, u16) in the follower's input
    /// PDI — the engine gates motion on CiA402 Operation Enabled itself
    /// rather than trusting the slow plane.
    pub status_word_offset: u16,
    /// Master position source.
    pub master: GearMaster,
    /// Slow-plane parameter channels, routed by `write_channel` into the
    /// gear's lock-free shared params instead of PDI bytes. Names must
    /// not collide with PDO channel names on the same device.
    #[serde(default = "default_gear_engage_channel")]
    pub engage_channel: String,
    #[serde(default = "default_gear_ratio_num_channel")]
    pub ratio_num_channel: String,
    #[serde(default = "default_gear_ratio_den_channel")]
    pub ratio_den_channel: String,
    #[serde(default = "default_gear_ratio_step_channel")]
    pub ratio_step_channel: String,
    #[serde(default = "default_gear_phase_channel")]
    pub phase_channel: String,
    #[serde(default = "default_gear_master_vel_channel")]
    pub master_vel_channel: String,
    /// Travel limit in counts from the engage origin; engagement is
    /// refused while the routed value is <= 0 (locked-by-default).
    #[serde(default = "default_gear_max_travel_channel")]
    pub max_travel_channel: String,
    /// Read-only feedback channels (engine → PLC/monitor).
    #[serde(default = "default_gear_engaged_channel")]
    pub engaged_channel: String,
    #[serde(default = "default_gear_trip_channel")]
    pub trip_channel: String,
    /// Rising edge = apply the ratio currently in the ratio channels to the
    /// running gear, phase-continuously (no position step): the engine
    /// re-references its law origins and ramps from the OLD ratio to the new
    /// one at `ratio_step` per cycle. Distinct from re-engage: it does not
    /// reset the soft-engage ramp, clear a trip, or bypass the re-arm rule.
    #[serde(default = "default_gear_ratio_apply_channel")]
    pub ratio_apply_channel: String,
    /// Read-only apply counter (engine → PLC/monitor): increments once per
    /// accepted apply edge, so the slow plane can handshake the change.
    #[serde(default = "default_gear_ratio_ack_channel")]
    pub ratio_ack_channel: String,
}

/// Master position source for an in-cycle gear axis.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GearMaster {
    /// Software master: an accumulator advanced by `master_vel` counts
    /// per bus cycle inside the loop.
    Virtual,
    /// Another axis on the same bus: its actual_position (0x6064, i32)
    /// read from the input PDI each cycle.
    Axis {
        slave_index: u16,
        actual_pos_offset: u16,
    },
}

fn default_gear_engage_channel() -> String {
    "gear_engage".into()
}
fn default_gear_ratio_num_channel() -> String {
    "ratio_num".into()
}
fn default_gear_ratio_den_channel() -> String {
    "ratio_den".into()
}
fn default_gear_ratio_step_channel() -> String {
    "ratio_step".into()
}
fn default_gear_phase_channel() -> String {
    "phase_ofs".into()
}
fn default_gear_master_vel_channel() -> String {
    "master_vel".into()
}
fn default_gear_max_travel_channel() -> String {
    "gear_max_travel".into()
}
fn default_gear_engaged_channel() -> String {
    "gear_engaged".into()
}
fn default_gear_trip_channel() -> String {
    "gear_trip".into()
}
fn default_gear_ratio_apply_channel() -> String {
    "gear_ratio_apply".into()
}
fn default_gear_ratio_ack_channel() -> String {
    "gear_ratio_ack".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EthercatSlave {
    /// 0-based position on the ring. Mirrors the order ethercrab discovers
    /// SubDevices in; written here so configuration is preserved even if
    /// the bus isn't currently up.
    pub index: u16,
    /// Friendly name; surfaced in the device editor and helpful in logs.
    pub name: String,
    /// 32-bit vendor ID from ESI/SII (e.g. 0x00000002 for Beckhoff). 0
    /// when unknown — the runtime treats 0 as "skip identity check".
    #[serde(default)]
    pub vendor_id: u32,
    /// 32-bit product code from ESI/SII. 0 when unknown.
    #[serde(default)]
    pub product_id: u32,
    /// Per-SubDevice DC override. `None` inherits the device-level
    /// `dc_sync`. The bus runs the DC path whenever any SubDevice ends up
    /// `sync0`; SubDevices left at `off` free-run inside that DC bus.
    #[serde(default)]
    pub dc_sync: Option<EthercatDcSync>,
    /// CoE SDO writes applied in PRE-OP on every connect, in listed order,
    /// before PDO mapping is read and before the SAFE-OP transition.
    /// This is how drives whose setup doesn't persist in EEPROM get
    /// configured — e.g. the SV660N needs 0x6060 = 8 (CSP) written at
    /// each power-up, and PDO remapping (0x1C12/0x1C13) also goes here.
    #[serde(default)]
    pub init_sdo: Vec<EthercatSdoInit>,
}

/// One CoE SDO write executed during PRE-OP at connect. Expedited
/// transfer only (≤ 4 bytes), which covers mode selection, PDO
/// remapping, and the rest of the usual startup list.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EthercatSdoInit {
    /// CoE object index (e.g. 0x6060 for Modes of Operation).
    pub index: u16,
    /// Sub-index within the object.
    #[serde(default)]
    pub sub_index: u8,
    /// Value, written little-endian at `bits` width. Negative values are
    /// two's-complement; unsigned values up to the width's max also fit.
    pub value: i64,
    /// Width of the write on the wire: 8, 16, or 32.
    pub bits: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EthercatChannel {
    /// Unique channel name — what iomap entries reference. Uniqueness is
    /// enforced by `validate_devices`; a repeat would silently discard the
    /// earlier PDO entry, since the channel table is keyed by name.
    pub name: String,
    /// Which SubDevice on the bus this PDO entry lives on (matches
    /// `EthercatSlave.index`).
    pub slave_index: u16,
    pub direction: EthercatPdoDirection,
    /// CoE object dictionary index (e.g. 0x6000 for the first TxPDO entry
    /// on a typical digital input slave). Stored as a plain u16 — the UI
    /// renders it as hex. Informational in real mode (the cyclic exchange
    /// uses `pdi_byte_offset` + `pdi_bit_offset` for fast lookup), but
    /// kept on the channel so users can document the source PDO entry.
    pub pdo_index: u16,
    /// Sub-index inside the PDO object.
    pub sub_index: u8,
    /// Bit length of the PDO entry. Usually 1, 8, 16, or 32.
    pub bit_length: u8,
    pub data_type: EthercatDataType,
    /// Byte offset of this PDO entry inside the SubDevice's input (TxPDO)
    /// or output (RxPDO) PDI region. Required in real mode — the cyclic
    /// task pulls bytes from `[pdi_byte_offset .. pdi_byte_offset +
    /// ceil(bit_length / 8)]` of the SubDevice's PDI buffer. Defaults to
    /// 0 for back-compat with existing sim-only configs (those ignore it).
    #[serde(default)]
    pub pdi_byte_offset: u16,
    /// Bit offset *within* the byte at `pdi_byte_offset`. 0 is the LSB.
    /// Only meaningful for `bit_length < 8` channels (e.g. digital I/O
    /// where 8 channels share one byte). Defaults to 0.
    #[serde(default)]
    pub pdi_bit_offset: u8,
}

/// PDO direction from the MainDevice's perspective.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum EthercatPdoDirection {
    /// TxPDO — SubDevice → MainDevice (controller reads this each cycle).
    TxPdo,
    /// RxPDO — MainDevice → SubDevice (controller writes this each cycle).
    RxPdo,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum EthercatDataType {
    Bool,
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    Real,
}

/// OPC UA client device — southbound integration with an existing DCS /
/// PLC / gateway that exposes its tags over OPC UA. IA2 acts as the
/// supervisory layer: it reads PV tags and writes SP/command tags, while
/// the underlying DCS keeps base regulatory control and safety.
///
/// Classic OPC DA (COM/DCOM, Windows-only) is reached through a DA→UA
/// gateway (KEPServerEX, Matrikon UA Proxy, …) — IA2 itself speaks UA
/// only, which is the standard bridge architecture on Linux edges.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct OpcuaConfig {
    /// `opc.tcp://host:4840/path` endpoint of the UA server.
    pub endpoint_url: String,
    /// Session authentication.
    #[serde(default)]
    pub auth: OpcuaAuth,
    /// Cyclic read period for the tag mirror, in milliseconds. All
    /// readable channels are fetched in ONE bulk Read service call per
    /// cycle, so this scales to hundreds of tags.
    #[serde(default = "default_opcua_poll_ms")]
    pub poll_interval_ms: u32,
    pub channels: Vec<OpcuaChannel>,
}

fn default_opcua_poll_ms() -> u32 {
    500
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OpcuaAuth {
    #[default]
    Anonymous,
    UserPassword {
        username: String,
        password: String,
    },
}

/// One mapped tag. `node_id` is the full OPC UA NodeId string, e.g.
/// `ns=2;s=Channel1.Device1.FT0202_PV` (string ids) or `ns=3;i=1042`
/// (numeric ids) — exactly what UaExpert shows.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct OpcuaChannel {
    pub name: String,
    pub node_id: String,
    pub data_type: OpcuaDataType,
    /// `read` tags feed iomap inputs; `write` tags accept iomap outputs
    /// (and are also readable for verification).
    #[serde(default)]
    pub access: OpcuaAccess,
    /// Optional failsafe value written when the runtime shuts down or
    /// trips. Default: leave the tag UNTOUCHED — on a supervisory layer
    /// the DCS below keeps authority, and blind zero-writes to DCS tags
    /// are more dangerous than holding last value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failsafe: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum OpcuaAccess {
    #[default]
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum OpcuaDataType {
    Bool,
    I16,
    U16,
    I32,
    U32,
    F32,
    /// Server-side Double; carried as f32 in the channel lane (PLC REAL).
    F64,
}

// ---------------- CANopen ----------------

/// CANopen device config (CiA 301). The runtime is the bus *master*
/// side of a point-to-point conversation with one node: SDO for
/// configuration-rate channels, PDO (predefined connection set) for
/// process-rate ones, heartbeat consumption for health.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CanopenConfig {
    /// CAN interface — a SocketCAN name (`can0`, Linux edge) or `_sim`
    /// for the in-memory simulated bus (dev machines, tests). Same
    /// convention as EtherCAT's `nic`.
    pub interface: String,
    /// The remote node's CANopen node-id (1–127).
    pub node_id: u8,
    /// Informational: the bus bitrate ops configured via `ip link`
    /// (SocketCAN sets bitrate outside the process). Shown in the UI
    /// so the project records what the wiring expects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<u32>,
    /// Cyclic SDO poll period for `sdo`-transport channels, in ms.
    #[serde(default = "default_canopen_poll_ms")]
    pub poll_interval_ms: u32,
    /// Heartbeat watchdog: unhealthy when no heartbeat arrives for this
    /// long. 0 disables monitoring (node doesn't produce heartbeats).
    #[serde(default = "default_canopen_heartbeat_timeout_ms")]
    pub heartbeat_timeout_ms: u32,
    /// Send NMT Start Remote Node on connect so a node sitting in
    /// pre-operational enters Operational (PDOs only run there).
    #[serde(default = "default_true")]
    pub start_on_connect: bool,
    #[serde(default)]
    pub channels: Vec<CanopenChannel>,
}

fn default_canopen_poll_ms() -> u32 {
    100
}

fn default_canopen_heartbeat_timeout_ms() -> u32 {
    3000
}

/// One object-dictionary entry the iomap can bind.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CanopenChannel {
    /// Unique channel name — what iomap entries reference. Uniqueness is
    /// enforced: `validate_devices` reports a repeat, and the adapter refuses
    /// to connect (its channel table is keyed by name, so a repeat would
    /// silently discard the earlier object).
    pub name: String,
    /// Object dictionary index (hex in the UI, e.g. 0x6041 statusword).
    pub index: u16,
    #[serde(default)]
    pub sub_index: u8,
    pub data_type: CanopenDataType,
    #[serde(default)]
    pub access: CanopenAccess,
    /// How the value moves on the wire. SDO = request/response at
    /// `poll_interval_ms`; PDO = process data at the node's event/sync
    /// rate using the CiA 301 predefined COB-IDs.
    #[serde(default)]
    pub transport: CanopenTransport,
    /// Optional failsafe written on shutdown/trip (same opt-in contract
    /// as OPC UA: absent = leave the object untouched).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failsafe: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum CanopenAccess {
    #[default]
    Read,
    Write,
}

/// Wire transport for one channel. PDO slots use the CiA 301
/// predefined connection set (TPDO1..4 = 0x180/0x280/0x380/0x480 +
/// node-id, RPDO1..4 = 0x200/0x300/0x400/0x500 + node-id) with the
/// node's existing PDO mapping; `byte_offset` locates the object
/// inside the ≤8-byte frame. Devices needing PDO *re*-mapping get it
/// configured out-of-band (vendor tool / SDO init sequence) for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CanopenTransport {
    #[default]
    Sdo,
    Tpdo {
        /// PDO number 1–4.
        slot: u8,
        /// Byte offset of this object inside the PDO payload.
        byte_offset: u8,
    },
    Rpdo {
        slot: u8,
        byte_offset: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum CanopenDataType {
    Bool,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Device {
    pub name: String,
    #[serde(flatten)]
    pub config: ProtocolConfig,
}

// ---------------- IO Mapping ----------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Input,  // bus → variable
    Output, // variable → bus
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Mapping {
    pub application: String,
    pub variable: String,
    pub direction: Direction,
    pub device: String,
    pub channel: String,
    /// Engineering unit of the mapped variable (e.g. "°C", "%", "mbar").
    /// Optional metadata for agent/operator reference files — never
    /// interpreted by the runtime.
    ///
    /// `#[ts(optional)]` on these four fields keeps the generated TS
    /// honest about the wire format: serialization OMITS the keys when
    /// `None` (skip_serializing_if), so a JSON consumer sees `undefined`
    /// — `unit?: string`, not `unit: string | null` — and pre-metadata
    /// `Mapping` literals keep typechecking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub unit: Option<String>,
    /// Low end of the variable's meaningful engineering range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub min: Option<f64>,
    /// High end of the variable's meaningful engineering range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub max: Option<f64>,
    /// Natural-language description of what this variable means in the
    /// plant ("reactor level setpoint"). Feeds generated reference files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct IoMap {
    #[serde(default)]
    pub mappings: Vec<Mapping>,
}

// ---------------- Edges (deploy targets) ----------------

/// A Linux box where the compiled program is meant to actually run. Sister
/// concept to `Device`: Device = thing the program talks to over a bus,
/// Edge = thing the program runs on.
///
/// On purpose, we do **not** store SSH credentials in this struct. The IDE
/// runs `ssh <host>` and lets the OS resolve the host via `~/.ssh/config`
/// (keys, agent, jump hosts, all of that). The user lists hosts here only
/// so the project records *which* edges this project deploys to.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Edge {
    pub name: String,
    /// SSH hostname or `~/.ssh/config` alias (preferred).
    pub host: String,
    /// SSH port. Defaults to 22; here so the user can override per-edge
    /// without polluting their global SSH config.
    #[serde(default = "default_ssh_port")]
    pub ssh_port: u16,
    /// SSH user. Empty string means "let ssh decide" — typically resolved
    /// via `~/.ssh/config` or the current user name.
    #[serde(default)]
    pub ssh_user: String,
    /// Absolute path on the edge box where the runtime + project versions
    /// live. Convention: `/opt/ia2`. The deploy script lays out
    /// `versions/<timestamp>/` and atomically swaps `current -> …`.
    #[serde(default = "default_install_dir")]
    pub install_dir: String,
    /// Local TCP port the runtime's monitor server binds to on the edge.
    /// Always `127.0.0.1:<port>` — remote access is via SSH port-forward,
    /// never direct exposure.
    #[serde(default = "default_runtime_port")]
    pub runtime_port: u16,
    /// Free-form notes — "production line 1", "test bench", whatever.
    #[serde(default)]
    pub notes: String,
}

fn default_ssh_port() -> u16 {
    22
}

fn default_install_dir() -> String {
    "/opt/ia2".to_string()
}

fn default_runtime_port() -> u16 {
    13001
}

// ---------------- Tasks (project-level scheduling) ----------------

/// One scheduling task — periodic only in V1. Maps directly to IEC's
/// `TASK <name>(INTERVAL := T#<ms>ms, PRIORITY := <priority>);`.
///
/// Event-triggered (`SINGLE :=`) tasks are deferred to a later iteration.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Task {
    pub name: String,
    /// Period in milliseconds; emitted as `T#<ms>ms` in the synthesized
    /// CONFIGURATION block.
    pub interval_ms: u32,
    /// IEC priority — lower numbers are higher priority on most runtimes.
    pub priority: i32,
}

/// One `PROGRAM <instance> WITH <task> : <program_type>;` binding. Only
/// PROGRAM-kind POUs can be instantiated here (FBs and FUNCTIONs are
/// used from inside PROGRAMs); the UI enforces this on the dropdown.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ProgramInstance {
    /// Instance name, unique within the configuration.
    pub instance: String,
    /// PROGRAM-kind POU this instance is of (matches a name in
    /// `applications/`).
    pub program: String,
    /// Task name this instance is scheduled on.
    pub task: String,
}

/// Project-level scheduling — replaces the per-POU inline CONFIGURATION
/// blocks. Lives in `tasks.toml`. The runtime synthesizes the IEC
/// CONFIGURATION block from this at compile time.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Tasks {
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub programs: Vec<ProgramInstance>,
}

// ---------------- Northbound (edge → platform publishing) ----------------

/// `alarms.toml` — declarative alarm definitions, evaluated by the
/// runtime-monitor layer on every sampled snapshot (IDE server and edge
/// runtime alike). Deliberately NOT a general expression language: one
/// variable, one condition, one limit — same non-Turing philosophy as
/// HMI bindings. Agents author this file like any other project config
/// (`cs get alarms` / `cs set alarms --from -`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AlarmConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alarms: Vec<AlarmDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AlarmDef {
    /// Unique id — the ack/journal key (`level_high`).
    pub id: String,
    /// Snapshot variable the condition reads.
    pub variable: String,
    pub condition: AlarmCondition,
    /// Threshold for the numeric conditions; ignored (and omitted) for
    /// `is_true` / `is_false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    /// Hysteresis on CLEARING a numeric alarm: `gt 90` with deadband 2
    /// raises above 90 and clears only below 88 — stops limit-riding
    /// values from chattering the alarm. Default 0.
    #[serde(default)]
    pub deadband: f64,
    /// The condition must hold this long before the alarm raises
    /// (debounce for spikes). Default 0 = raise on first sample.
    #[serde(default)]
    pub delay_ms: u32,
    #[serde(default)]
    pub severity: AlarmSeverity,
    /// Operator-facing text. Keep it actionable ("Tank level high —
    /// check outlet valve"), not a restatement of the condition.
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum AlarmCondition {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
    Ne,
    IsTrue,
    IsFalse,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, TS,
)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum AlarmSeverity {
    Info,
    #[default]
    Warn,
    High,
    Critical,
}

/// `northbound.toml` — how the *edge runtime* publishes live data up to
/// the plant platform (supOS / Tier0). MQTT only by design: it's the
/// integration protocol the platform side ingests natively. Southbound
/// (driving instruments/valves) is the device layer's job (OPC UA /
/// EtherCAT / Modbus) — never this.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct NorthboundConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mqtt: Option<MqttNorthbound>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MqttNorthbound {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub broker_host: String,
    #[serde(default = "default_mqtt_port")]
    pub broker_port: u16,
    /// Defaults to `ia2-<project>` when empty.
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// Topic root; defaults to `ia2/<project>` when empty. Topics:
    /// `<prefix>/status` (retained online/offline + LWT),
    /// `<prefix>/snapshot` (periodic values JSON),
    /// `<prefix>/write` (subscribed when `allow_write`).
    #[serde(default)]
    pub topic_prefix: String,
    #[serde(default = "default_publish_ms")]
    pub publish_interval_ms: u32,
    /// 0 or 1 (QoS 2 is overkill for cyclic data).
    #[serde(default)]
    pub qos: u8,
    /// Accept variable writes from `<prefix>/write` (payload
    /// `{"name": ..., "value": ...}`). Off by default — turning the
    /// northbound link into a control path is an explicit decision.
    #[serde(default)]
    pub allow_write: bool,
}

fn default_true() -> bool {
    true
}
fn default_mqtt_port() -> u16 {
    1883
}
fn default_publish_ms() -> u32 {
    1000
}

// ---------------- Project tree (frontend response) ----------------

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ProjectTree {
    pub name: String,
    pub path: String,
    /// Every `.st` file under `pous/` with its parsed declarations.
    /// The tree renders each declaration as a node; multi-POU files
    /// (FB + PROGRAM in one .st) show up as sibling nodes that share
    /// a `path`.
    pub pous: Vec<PouFile>,
    /// All folder paths under `pous/` (slash-separated, relative).
    /// Includes empty folders.
    pub pou_folders: Vec<String>,
    /// Full Device records (config inline) so the IO Mapping UI can
    /// resolve channel kind/address without per-device fetches.
    pub devices: Vec<Device>,
    /// Same as `app_folders` but rooted at `devices/`.
    pub device_folders: Vec<String>,
    pub iomap: IoMap,
    /// Deploy targets — Linux boxes where the program is meant to run.
    pub edges: Vec<Edge>,
    /// Folders under `edges/`, including empty ones.
    pub edge_folders: Vec<String>,
    /// Project-level scheduling. May be empty for a fresh project; the
    /// migration step populates it from inline CONFIGURATION blocks the
    /// first time an old project is opened.
    pub tasks: Tasks,
}

// ---------------- Project list entry ----------------

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ProjectListing {
    pub name: String,
    pub path: String,
    pub is_last_opened: bool,
}

#[cfg(test)]
mod mapping_metadata_compat_tests {
    use super::*;

    /// A v0 iomap.toml without any metadata fields still parses, every
    /// metadata field defaults to None, and re-serializing keeps the
    /// keys absent — untouched project files round-trip byte-identical.
    #[test]
    fn legacy_mapping_without_metadata_roundtrips_clean() {
        let toml_src = r#"
[[mappings]]
application = "main"
variable = "level"
direction = "input"
device = "plc1"
channel = "ch0"
"#;
        let map: IoMap = toml::from_str(toml_src).unwrap();
        assert_eq!(map.mappings.len(), 1);
        let m = &map.mappings[0];
        assert_eq!(m.unit, None);
        assert_eq!(m.min, None);
        assert_eq!(m.max, None);
        assert_eq!(m.description, None);
        let back = toml::to_string_pretty(&map).unwrap();
        assert!(!back.contains("unit"));
        assert!(!back.contains("min"));
        assert!(!back.contains("max"));
        assert!(!back.contains("description"));
    }

    /// Explicit metadata survives a TOML round-trip.
    #[test]
    fn metadata_roundtrips_when_set() {
        let toml_src = r#"
[[mappings]]
application = "main"
variable = "level_sp"
direction = "output"
device = "plc1"
channel = "ch1"
unit = "%"
min = 0.0
max = 100.0
description = "Tank level setpoint"
"#;
        let map: IoMap = toml::from_str(toml_src).unwrap();
        let m = &map.mappings[0];
        assert_eq!(m.unit.as_deref(), Some("%"));
        assert_eq!(m.min, Some(0.0));
        assert_eq!(m.max, Some(100.0));
        assert_eq!(m.description.as_deref(), Some("Tank level setpoint"));
        let back: IoMap = toml::from_str(&toml::to_string_pretty(&map).unwrap()).unwrap();
        assert_eq!(back.mappings[0].max, Some(100.0));
    }
}

#[cfg(test)]
mod governance_compat_tests {
    use super::*;

    /// A v0 project.toml without [governance] parses fully open, and
    /// re-serializing keeps the section absent — untouched manifests
    /// round-trip byte-identical.
    #[test]
    fn legacy_manifest_without_governance_roundtrips_clean() {
        let m: ProjectManifest = toml::from_str("name = \"p\"\n").unwrap();
        assert_eq!(m.version, "0.1");
        assert!(m.governance.is_default());
        let back = toml::to_string_pretty(&m).unwrap();
        assert!(!back.contains("governance"));
    }

    /// The documented TOML shape parses: mode + rules with min/max.
    #[test]
    fn governance_section_parses() {
        let src = r#"
name = "p"

[governance]
write_mode = "allowlist"

[[governance.rules]]
variable = "level_sp"
min = 0.0
max = 100.0

[[governance.rules]]
variable = "main.valve_cmd"
"#;
        let m: ProjectManifest = toml::from_str(src).unwrap();
        assert_eq!(m.governance.write_mode, WriteMode::Allowlist);
        assert_eq!(m.governance.rules.len(), 2);
        assert_eq!(m.governance.rules[0].variable, "level_sp");
        assert_eq!(m.governance.rules[0].min, Some(0.0));
        assert_eq!(m.governance.rules[0].max, Some(100.0));
        assert_eq!(m.governance.rules[1].min, None);
        m.governance.validate().expect("documented shape is valid");
    }
}

#[cfg(test)]
mod governance_validation_tests {
    use super::*;

    fn rule(variable: &str, min: Option<f64>, max: Option<f64>) -> WriteRule {
        WriteRule {
            variable: variable.into(),
            min,
            max,
        }
    }

    fn allowlist(rules: Vec<WriteRule>) -> WriteGovernance {
        WriteGovernance {
            write_mode: WriteMode::Allowlist,
            rules,
        }
    }

    // ---- unknown keys are parse errors (deny_unknown_fields) -----------

    /// `writemode` (typo'd `write_mode`) must not parse as a
    /// default-Open policy — governance silently OFF is exactly the
    /// failure mode the deny is there to prevent.
    #[test]
    fn typoed_governance_key_fails_parse() {
        let src = "name = \"p\"\n\n[governance]\nwritemode = \"allowlist\"\n";
        let err = toml::from_str::<ProjectManifest>(src)
            .unwrap_err()
            .to_string();
        assert!(err.contains("writemode"), "{err}");
        assert!(err.contains("unknown field"), "{err}");
    }

    /// `mim` (typo'd `min`) must not parse as a rule with no lower
    /// bound — the intended clamp would be silently unenforced.
    #[test]
    fn typoed_rule_key_fails_parse() {
        let src = "name = \"p\"\n\n[governance]\nwrite_mode = \"allowlist\"\n\n\
                   [[governance.rules]]\nvariable = \"sp\"\nmim = 0.0\nmax = 100.0\n";
        let err = toml::from_str::<ProjectManifest>(src)
            .unwrap_err()
            .to_string();
        assert!(err.contains("mim"), "{err}");
        assert!(err.contains("unknown field"), "{err}");
    }

    /// Manifest-level tolerance is unchanged: unknown TOP-LEVEL tables
    /// (e.g. `[libraries]`) still parse — only the governance structs
    /// deny unknown keys.
    #[test]
    fn unknown_top_level_table_still_parses() {
        let src = "name = \"p\"\n\n[libraries]\nfb_util = \"1.0\"\n";
        let m: ProjectManifest = toml::from_str(src).unwrap();
        assert!(m.governance.is_default());
    }

    // ---- validate(): bounds ---------------------------------------------

    /// TOML 1.0 accepts `nan` / `inf` float literals; validate() must
    /// reject them, naming the offending rule.
    #[test]
    fn validate_rejects_non_finite_bounds() {
        let src = "name = \"p\"\n\n[governance]\nwrite_mode = \"allowlist\"\n\n\
                   [[governance.rules]]\nvariable = \"sp\"\nmin = nan\n";
        let m: ProjectManifest = toml::from_str(src).expect("nan parses as TOML");
        let err = m.governance.validate().unwrap_err();
        assert!(err.contains("rule #1 ('sp')"), "{err}");
        assert!(err.contains("min must be a finite number"), "{err}");

        let err = allowlist(vec![rule("sp", None, Some(f64::INFINITY))])
            .validate()
            .unwrap_err();
        assert!(err.contains("max must be a finite number"), "{err}");
    }

    #[test]
    fn validate_rejects_min_greater_than_max() {
        let err = allowlist(vec![rule("sp", Some(2.0), Some(1.0))])
            .validate()
            .unwrap_err();
        assert!(err.contains("rule #1 ('sp')"), "{err}");
        assert!(err.contains("min (2) is greater than max (1)"), "{err}");
    }

    #[test]
    fn validate_accepts_equal_bounds_and_half_open_rules() {
        // min == max pins the variable to one value — legal.
        allowlist(vec![
            rule("pin", Some(5.0), Some(5.0)),
            rule("floor_only", Some(0.0), None),
            rule("ceiling_only", None, Some(10.0)),
            rule("bare_allow", None, None),
        ])
        .validate()
        .expect("degenerate-but-ordered bounds are valid");
    }

    // ---- validate(): variable names --------------------------------------

    #[test]
    fn validate_rejects_empty_variable() {
        let err = allowlist(vec![rule("", Some(0.0), Some(1.0))])
            .validate()
            .unwrap_err();
        assert!(err.contains("governance rule #1"), "{err}");
        assert!(err.contains("variable must not be empty"), "{err}");
        // Whitespace-only is just as dead a rule.
        let err = allowlist(vec![rule("  ", None, None)])
            .validate()
            .unwrap_err();
        assert!(err.contains("variable must not be empty"), "{err}");
    }

    /// Duplicates are case-insensitive, because rule matching is.
    #[test]
    fn validate_rejects_duplicate_variables() {
        let err = allowlist(vec![rule("sp", None, None), rule("SP", Some(0.0), None)])
            .validate()
            .unwrap_err();
        assert!(err.contains("rule #2 ('SP')"), "{err}");
        assert!(err.contains("duplicate of rule #1"), "{err}");
    }

    /// A bare rule matches the variable in every instance, so bare `sp`
    /// and qualified `main.sp` govern the same variable — first-match
    /// precedence would silently decide which bounds apply.
    #[test]
    fn validate_rejects_bare_and_qualified_rules_for_same_variable() {
        let err = allowlist(vec![
            rule("sp", Some(0.0), Some(10.0)),
            rule("main.sp", Some(0.0), Some(100.0)),
        ])
        .validate()
        .unwrap_err();
        assert!(err.contains("rule #1 ('sp')"), "{err}");
        assert!(err.contains("rule #2 ('main.sp')"), "{err}");
        assert!(err.contains("govern the same variable"), "{err}");
    }

    /// Qualified rules for the same leaf under DIFFERENT instances are
    /// distinct variables — no conflict.
    #[test]
    fn validate_accepts_same_leaf_under_different_instances() {
        allowlist(vec![
            rule("line_a.sp", Some(0.0), Some(10.0)),
            rule("line_b.sp", Some(0.0), Some(20.0)),
        ])
        .validate()
        .expect("distinct qualified variables are fine");
    }

    /// Every problem is reported in one pass, each naming its rule.
    #[test]
    fn validate_reports_all_errors_at_once() {
        let err = allowlist(vec![
            rule("a", Some(5.0), Some(1.0)),
            rule("", Some(f64::NAN), None),
            rule("A", None, None),
        ])
        .validate()
        .unwrap_err();
        assert!(err.contains("min (5) is greater than max (1)"), "{err}");
        assert!(err.contains("variable must not be empty"), "{err}");
        assert!(err.contains("min must be a finite number"), "{err}");
        assert!(err.contains("duplicate of rule #1"), "{err}");
    }

    /// The default (open, no rules) table is trivially valid.
    #[test]
    fn validate_accepts_default() {
        WriteGovernance::default()
            .validate()
            .expect("default is valid");
    }
}
