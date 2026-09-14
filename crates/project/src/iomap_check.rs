//! Static validation of the IO map against the project's devices.
//!
//! Pure function over in-memory project state — no I/O, no connections.
//! The server exposes it through `/api/project/validate` so the IDE (and
//! agents, per the API-first rule) can catch broken bindings *before*
//! deploying: a mapping that names a missing device, points at a channel
//! the device doesn't have, drives a read-only channel, or fights another
//! variable for the same output.
//!
//! Severity model:
//! - `Error` — the runtime will reject or misbehave on this mapping
//!   (unknown device/channel, direction impossible for the channel,
//!   two different variables writing one output).
//! - `Warning` — legal but suspicious (one variable fanning out to
//!   several output channels).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::types::{
    CanopenAccess, CanopenDataType, CanopenTransport, Device, Direction, EthercatDataType,
    EthercatPdoDirection, IoMap, Mapping, ModbusAccess, ModbusChannelKind, OpcuaAccess,
    OpcuaDataType, ProtocolConfig,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum IomapIssueSeverity {
    Error,
    Warning,
}

/// One finding from [`validate_iomap`]. `mapping_index` is the position
/// in `IoMap::mappings` of the offending row, so the UI can highlight it
/// and an agent can patch it by index.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct IomapIssue {
    pub severity: IomapIssueSeverity,
    pub mapping_index: usize,
    pub message: String,
}

fn error(mapping_index: usize, message: String) -> IomapIssue {
    IomapIssue {
        severity: IomapIssueSeverity::Error,
        mapping_index,
        message,
    }
}

fn warning(mapping_index: usize, message: String) -> IomapIssue {
    IomapIssue {
        severity: IomapIssueSeverity::Warning,
        mapping_index,
        message,
    }
}

/// Validate every mapping in `iomap` against `devices`. Returns all
/// findings, sorted by `mapping_index` (errors and warnings interleaved
/// in row order; an empty Vec means the map is clean).
/// This is a mapping validator, not a full connect-time device preflight:
/// unreferenced devices have no mapping row and are not checked here.
///
/// Checks, per mapping:
/// 1. the named device exists, and — for EtherCAT — its gear channel
///    names are unambiguous (an ambiguous device is unroutable, so the
///    row reports that instead of its own channel finding);
/// 2. the named channel exists on that device (per-protocol metadata);
/// 3. the mapping direction is possible for the channel:
///    - Modbus: every channel kind is readable (`Input` always fine);
///      `Output` needs a writable kind (`Coil` / `HoldingRegister`);
///    - EtherCAT: PDO directions must match; gear parameters also support
///      Input echoes, whereas gear feedback is read-only;
///    - OPC UA: `Output` needs `access = write`. `Input` is fine on both
///      accesses — the adapter mirrors *all* channels each poll cycle
///      (write tags are documented "also readable for verification").
///
/// And across mappings:
/// 4. two *different* variables both bound as `Output` to one
///    (device, channel) — last-writer-wins fights, always a bug;
/// 5. one variable bound as `Output` to several distinct channels —
///    legitimate fan-out, flagged as a warning so it's a visible choice.
///
/// Plus metadata sanity, per mapping:
/// 6. `min`/`max` engineering-range metadata must be finite and ordered
///    (`min <= max`) — errors, because the values flow verbatim into
///    generated HMI SetValue bounds and device describe exports; range
///    metadata on a boolean channel is a warning (booleans carry only
///    0/1, so a range there usually means the row targets the wrong
///    channel).
pub fn validate_iomap(iomap: &IoMap, devices: &[Device]) -> Vec<IomapIssue> {
    let mut issues = Vec::new();
    // Validate device-wide gear names once, while retaining a diagnostic
    // on each affected mapping row (the public issue contract is row-based).
    let mut gear_names: HashMap<&str, Result<(), String>> = HashMap::new();

    for (index, mapping) in iomap.mappings.iter().enumerate() {
        // Metadata sanity is device-independent — check it even when the
        // device lookup below fails, so one broken row reports both
        // problems in a single validate pass.
        check_metadata(index, mapping, &mut issues);
        let Some(device) = devices.iter().find(|d| d.name == mapping.device) else {
            issues.push(error(
                index,
                format!(
                    "mapping '{app}.{var}': unknown device '{dev}'",
                    app = mapping.application,
                    var = mapping.variable,
                    dev = mapping.device
                ),
            ));
            continue;
        };
        // Ambiguous gear names make the whole device unroutable, so every
        // row that names it fails — gear rows and plain PDO rows alike — and
        // the row's own channel check is short-circuited until it is fixed.
        // Connect-time validation rejects the same configs; surfacing them
        // here just moves the failure ahead of the first scan.
        if let ProtocolConfig::Ethercat(cfg) = &device.config {
            let validity = gear_names.entry(device.name.as_str()).or_insert_with(|| {
                let pdo_names = cfg.channels.iter().map(|c| c.name.as_str()).collect();
                crate::validate_gear_channel_names(&cfg.gear, &pdo_names)
            });
            if let Err(message) = &*validity {
                issues.push(error(
                    index,
                    format!(
                        "mapping '{app}.{var}': device '{dev}': {message}",
                        app = mapping.application,
                        var = mapping.variable,
                        dev = device.name
                    ),
                ));
                continue;
            }
        }
        check_channel(index, mapping, device, &mut issues);
    }

    check_duplicate_writers(&iomap.mappings, &mut issues);
    check_output_fanout(&iomap.mappings, &mut issues);
    check_input_only_writable_modbus(&iomap.mappings, devices, &mut issues);

    // Cross-mapping checks append out of row order; normalize so callers
    // (and snapshots in tests) see a deterministic, row-ordered report.
    issues.sort_by_key(|i| i.mapping_index);
    issues
}

/// Rule 6 (error half): `min`/`max` metadata must be finite and ordered.
/// TOML accepts `nan`/`inf` literals and swapped bounds; catching them
/// here gives the caller a `mapping_index`, instead of the bad value
/// flowing verbatim into generated HMI SetValue bounds (where the HMI
/// validator rejects it at the wrong layer) or into describe exports as
/// authoritative-looking range data.
fn check_metadata(index: usize, mapping: &Mapping, issues: &mut Vec<IomapIssue>) {
    for (bound, value) in [("min", mapping.min), ("max", mapping.max)] {
        if let Some(v) = value {
            if !v.is_finite() {
                issues.push(error(
                    index,
                    format!(
                        "mapping '{app}.{var}': {bound} must be a finite number (got {v})",
                        app = mapping.application,
                        var = mapping.variable
                    ),
                ));
            }
        }
    }
    if let (Some(min), Some(max)) = (mapping.min, mapping.max) {
        if min > max {
            issues.push(error(
                index,
                format!(
                    "mapping '{app}.{var}': min ({min}) is greater than max ({max})",
                    app = mapping.application,
                    var = mapping.variable
                ),
            ));
        }
    }
}

/// Rule 6 (warning half): range metadata on a boolean channel. Legal,
/// but the value lane only ever carries 0/1 — a min/max there almost
/// always means the row points at the wrong channel.
fn warn_range_on_bool_channel(index: usize, mapping: &Mapping, issues: &mut Vec<IomapIssue>) {
    if mapping.min.is_none() && mapping.max.is_none() {
        return;
    }
    issues.push(warning(
        index,
        format!(
            "mapping '{app}.{var}': min/max range metadata on boolean channel '{ch}' of \
             device '{dev}' — a boolean carries only 0/1, so the range cannot apply",
            app = mapping.application,
            var = mapping.variable,
            ch = mapping.channel,
            dev = mapping.device
        ),
    ));
}

/// Rules 2 + 3: channel existence and direction compatibility, dispatched
/// on the device's protocol so the message can cite the channel's actual
/// kind/direction/access. Also hosts rule 6's boolean-channel warning,
/// since this is where the channel's data type is resolved.
fn check_channel(index: usize, mapping: &Mapping, device: &Device, issues: &mut Vec<IomapIssue>) {
    let unknown_channel = |issues: &mut Vec<IomapIssue>| {
        issues.push(error(
            index,
            format!(
                "mapping '{app}.{var}': device '{dev}' has no channel '{ch}'",
                app = mapping.application,
                var = mapping.variable,
                dev = mapping.device,
                ch = mapping.channel
            ),
        ));
    };

    match &device.config {
        ProtocolConfig::Modbus(cfg) => {
            let Some(ch) = cfg.channels.iter().find(|c| c.name == mapping.channel) else {
                unknown_channel(issues);
                return;
            };
            if matches!(
                ch.kind,
                ModbusChannelKind::Coil | ModbusChannelKind::DiscreteInput
            ) {
                warn_range_on_bool_channel(index, mapping, issues);
            }
            // Every Modbus kind is readable (FC 1/2/3/4), so Input is
            // always satisfiable; only writes are kind-restricted.
            let writable = matches!(
                ch.kind,
                ModbusChannelKind::Coil | ModbusChannelKind::HoldingRegister
            );
            if mapping.direction == Direction::Output && !writable {
                issues.push(error(
                    index,
                    format!(
                        "mapping '{app}.{var}': channel '{ch}' on Modbus device '{dev}' is a \
                         read-only {kind:?}; Output mappings need a Coil or HoldingRegister",
                        app = mapping.application,
                        var = mapping.variable,
                        ch = mapping.channel,
                        dev = mapping.device,
                        kind = ch.kind
                    ),
                ));
            }
            // The register kind alone does not grant write permission:
            // access=read marks a measurement living in a writable kind
            // (mirrors the OPC UA output-needs-access=write rule).
            if mapping.direction == Direction::Output && writable && ch.access == ModbusAccess::Read
            {
                issues.push(error(
                    index,
                    format!(
                        "mapping '{app}.{var}': channel '{ch}' on Modbus device '{dev}' has \
                         access=read; Output mappings need access=write",
                        app = mapping.application,
                        var = mapping.variable,
                        ch = mapping.channel,
                        dev = mapping.device
                    ),
                ));
            }
        }
        ProtocolConfig::Ethercat(cfg) => {
            // Gear routes are virtual mailbox channels, not PDO bytes, and
            // the adapters resolve them first too. Looking here before the
            // PDO list is only unambiguous because `validate_iomap` has
            // already rejected gear/PDO name collisions on this device —
            // keep that precheck ahead of this call.
            let gear_channel = cfg
                .gear
                .iter()
                .flat_map(|g| g.routed_channels())
                .find(|(name, _)| *name == mapping.channel);
            if let Some((_, channel)) = gear_channel {
                if channel.is_bool() {
                    warn_range_on_bool_channel(index, mapping, issues);
                }
                if mapping.direction == Direction::Output && channel.parameter().is_none() {
                    issues.push(error(
                        index,
                        format!(
                            "mapping '{app}.{var}': channel '{ch}' on EtherCAT device '{dev}' is \
                             read-only gear feedback; Output mappings need a writable parameter",
                            app = mapping.application,
                            var = mapping.variable,
                            ch = mapping.channel,
                            dev = device.name,
                        ),
                    ));
                }
                return;
            }
            let Some(ch) = cfg.channels.iter().find(|c| c.name == mapping.channel) else {
                unknown_channel(issues);
                return;
            };
            if ch.data_type == EthercatDataType::Bool {
                warn_range_on_bool_channel(index, mapping, issues);
            }
            match (mapping.direction, ch.direction) {
                (Direction::Input, EthercatPdoDirection::RxPdo) => {
                    issues.push(error(
                        index,
                        format!(
                            "mapping '{app}.{var}': channel '{ch}' on EtherCAT device '{dev}' is \
                             an RxPDO (controller→device output); Input mappings need a TxPDO \
                             channel",
                            app = mapping.application,
                            var = mapping.variable,
                            ch = mapping.channel,
                            dev = mapping.device
                        ),
                    ));
                }
                (Direction::Output, EthercatPdoDirection::TxPdo) => {
                    issues.push(error(
                        index,
                        format!(
                            "mapping '{app}.{var}': channel '{ch}' on EtherCAT device '{dev}' is \
                             a TxPDO (device→controller input); Output mappings need an RxPDO \
                             channel",
                            app = mapping.application,
                            var = mapping.variable,
                            ch = mapping.channel,
                            dev = mapping.device
                        ),
                    ));
                }
                _ => {}
            }
        }
        ProtocolConfig::Opcua(cfg) => {
            let Some(ch) = cfg.channels.iter().find(|c| c.name == mapping.channel) else {
                unknown_channel(issues);
                return;
            };
            if ch.data_type == OpcuaDataType::Bool {
                warn_range_on_bool_channel(index, mapping, issues);
            }
            // Output strictly needs access=write (the adapter rejects
            // writes to read tags). Input is fine on either access: the
            // poll task mirrors every channel, write tags included.
            if mapping.direction == Direction::Output && ch.access != OpcuaAccess::Write {
                issues.push(error(
                    index,
                    format!(
                        "mapping '{app}.{var}': channel '{ch}' on OPC UA device '{dev}' has \
                         access=read; Output mappings need access=write",
                        app = mapping.application,
                        var = mapping.variable,
                        ch = mapping.channel,
                        dev = mapping.device
                    ),
                ));
            }
        }
        ProtocolConfig::Canopen(cfg) => {
            let Some(ch) = cfg.channels.iter().find(|c| c.name == mapping.channel) else {
                unknown_channel(issues);
                return;
            };
            if ch.data_type == CanopenDataType::Bool {
                warn_range_on_bool_channel(index, mapping, issues);
            }
            // Same contract as OPC UA: the adapter mirrors every channel
            // (SDO polls, TPDO frames, RPDO shadows), so Input always
            // works; Output needs access=write. Additionally a TPDO is
            // by definition device→controller — writing one is a config
            // error even if access says write.
            if mapping.direction == Direction::Output {
                if ch.access != CanopenAccess::Write {
                    issues.push(error(
                        index,
                        format!(
                            "mapping '{app}.{var}': channel '{ch}' on CANopen device '{dev}' \
                             has access=read; Output mappings need access=write",
                            app = mapping.application,
                            var = mapping.variable,
                            ch = mapping.channel,
                            dev = mapping.device
                        ),
                    ));
                } else if matches!(ch.transport, CanopenTransport::Tpdo { .. }) {
                    issues.push(error(
                        index,
                        format!(
                            "mapping '{app}.{var}': channel '{ch}' on CANopen device '{dev}' \
                             rides a TPDO (device→controller); Output mappings need SDO or \
                             an RPDO slot",
                            app = mapping.application,
                            var = mapping.variable,
                            ch = mapping.channel,
                            dev = mapping.device
                        ),
                    ));
                }
            }
        }
    }
}

/// Rule 4: distinct variables writing the same (device, channel) — the
/// scan loop would apply them in mapping order every cycle, so the last
/// writer silently wins. Every involved row gets the error so the UI can
/// highlight the whole conflict set.
/// Rule 7 (Modbus migration nudge): a writable-kind channel
/// (Coil/HoldingRegister) that is bound ONLY as Input and still carries
/// the default `access = write` is almost certainly a measurement the
/// coupler mapped into a holding register — the failsafe sweep will try
/// to zero it (Illegal-data-address on real couplers). Warn, don't
/// error: an output readback bound as Input is legitimate, but then an
/// Output binding for the same channel exists and suppresses this rule.
fn check_input_only_writable_modbus(
    mappings: &[Mapping],
    devices: &[Device],
    issues: &mut Vec<IomapIssue>,
) {
    let mut has_output: HashMap<(&str, &str), bool> = HashMap::new();
    for m in mappings {
        let e = has_output
            .entry((m.device.as_str(), m.channel.as_str()))
            .or_insert(false);
        *e |= m.direction == Direction::Output;
    }
    for (index, m) in mappings.iter().enumerate() {
        if m.direction != Direction::Input || has_output[&(m.device.as_str(), m.channel.as_str())] {
            continue;
        }
        let Some(device) = devices.iter().find(|d| d.name == m.device) else {
            continue;
        };
        let ProtocolConfig::Modbus(cfg) = &device.config else {
            continue;
        };
        let Some(ch) = cfg.channels.iter().find(|c| c.name == m.channel) else {
            continue;
        };
        if ch.access == ModbusAccess::Write
            && matches!(
                ch.kind,
                ModbusChannelKind::Coil | ModbusChannelKind::HoldingRegister
            )
        {
            issues.push(warning(
                index,
                format!(
                    "channel '{ch}' on Modbus device '{dev}' is only read (Input) but still \
                     marked access=write — the shutdown failsafe will write zeros to it; if it \
                     is a measurement register, set access = \"read\"",
                    ch = m.channel,
                    dev = m.device
                ),
            ));
        }
    }
}

fn check_duplicate_writers(mappings: &[Mapping], issues: &mut Vec<IomapIssue>) {
    let mut writers: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for (index, m) in mappings.iter().enumerate() {
        if m.direction == Direction::Output {
            writers
                .entry((m.device.as_str(), m.channel.as_str()))
                .or_default()
                .push(index);
        }
    }
    for ((device, channel), indices) in writers {
        let mut variables: Vec<String> = indices
            .iter()
            .map(|&i| format!("{}.{}", mappings[i].application, mappings[i].variable))
            .collect();
        variables.sort();
        variables.dedup();
        if variables.len() > 1 {
            let list = variables.join(", ");
            for &index in &indices {
                issues.push(error(
                    index,
                    format!(
                        "conflicting writers: {list} all output to channel '{channel}' on \
                         device '{device}' — only one variable may drive an output"
                    ),
                ));
            }
        }
    }
}

/// Rule 5: one variable fanning out to several output channels. Valid
/// (e.g. mirroring a command to two coils) but worth surfacing; warn on
/// every involved row.
fn check_output_fanout(mappings: &[Mapping], issues: &mut Vec<IomapIssue>) {
    let mut outputs_of: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for (index, m) in mappings.iter().enumerate() {
        if m.direction == Direction::Output {
            outputs_of
                .entry((m.application.as_str(), m.variable.as_str()))
                .or_default()
                .push(index);
        }
    }
    for ((application, variable), indices) in outputs_of {
        let mut targets: Vec<String> = indices
            .iter()
            .map(|&i| format!("{}/{}", mappings[i].device, mappings[i].channel))
            .collect();
        targets.sort();
        targets.dedup();
        if targets.len() > 1 {
            let list = targets.join(", ");
            for &index in &indices {
                issues.push(warning(
                    index,
                    format!(
                        "variable '{application}.{variable}' is bound as Output to \
                         {n} channels ({list}) — fan-out is legal but make sure it is intended",
                        n = targets.len()
                    ),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        EthercatChannel, EthercatConfig, EthercatDataType, EthercatDcSync, ModbusChannel,
        ModbusConfig, ModbusDataType, ModbusTcpParams, ModbusTransport, ModbusWordOrder, OpcuaAuth,
        OpcuaChannel, OpcuaDataType,
    };

    // ---- fixtures -------------------------------------------------------

    fn modbus_device(name: &str) -> Device {
        let ch = |n: &str, kind: ModbusChannelKind, address: u16| ModbusChannel {
            name: n.into(),
            kind,
            address,
            data_type: ModbusDataType::U16,
            word_order: ModbusWordOrder::HiLo,
            access: Default::default(),
        };
        // A measurement the coupler maps into a HOLDING register — the
        // shape behind the access field (writable kind, read-only truth).
        let hr_meas = ModbusChannel {
            access: ModbusAccess::Read,
            ..ch("hr_meas", ModbusChannelKind::HoldingRegister, 20)
        };
        Device {
            name: name.into(),
            config: ProtocolConfig::Modbus(ModbusConfig {
                transport: ModbusTransport::Tcp(ModbusTcpParams {
                    host: "127.0.0.1".into(),
                    port: 502,
                }),
                slave_id: 1,
                poll_interval_ms: 100,
                timeout_ms: None,
                reconnect_backoff_ms: None,
                channels: vec![
                    ch("coil_pump", ModbusChannelKind::Coil, 0),
                    ch("di_estop", ModbusChannelKind::DiscreteInput, 0),
                    ch("hr_setpoint", ModbusChannelKind::HoldingRegister, 10),
                    ch("ir_temp", ModbusChannelKind::InputRegister, 0),
                    hr_meas,
                ],
            }),
        }
    }

    fn ethercat_device(name: &str) -> Device {
        let ch = |n: &str, direction: EthercatPdoDirection| EthercatChannel {
            name: n.into(),
            slave_index: 0,
            direction,
            pdo_index: 0x6000,
            sub_index: 1,
            bit_length: 1,
            data_type: EthercatDataType::Bool,
            pdi_byte_offset: 0,
            pdi_bit_offset: 0,
        };
        Device {
            name: name.into(),
            config: ProtocolConfig::Ethercat(EthercatConfig {
                nic: "_sim".into(),
                bringup: Default::default(),
                cycle_us: 1_000,
                dc_sync: EthercatDcSync::Off,
                dc_static_sync_iterations: 0,
                slaves: vec![],
                channels: vec![
                    ch("tx_status", EthercatPdoDirection::TxPdo),
                    ch("rx_command", EthercatPdoDirection::RxPdo),
                ],
                gear: vec![],
            }),
        }
    }

    fn opcua_device(name: &str) -> Device {
        let ch = |n: &str, access: OpcuaAccess| OpcuaChannel {
            name: n.into(),
            node_id: format!("ns=2;s={n}"),
            data_type: OpcuaDataType::F32,
            access,
            failsafe: None,
        };
        Device {
            name: name.into(),
            config: ProtocolConfig::Opcua(crate::types::OpcuaConfig {
                endpoint_url: "opc.tcp://127.0.0.1:4840".into(),
                auth: OpcuaAuth::Anonymous,
                poll_interval_ms: 500,
                channels: vec![
                    ch("pv_flow", OpcuaAccess::Read),
                    ch("sp_flow", OpcuaAccess::Write),
                ],
            }),
        }
    }

    fn canopen_device(name: &str) -> Device {
        let ch = |n: &str, access: CanopenAccess, transport: CanopenTransport| {
            crate::types::CanopenChannel {
                name: n.into(),
                index: 0x6041,
                sub_index: 0,
                data_type: crate::types::CanopenDataType::U16,
                access,
                transport,
                failsafe: None,
            }
        };
        Device {
            name: name.into(),
            config: ProtocolConfig::Canopen(crate::types::CanopenConfig {
                interface: "_sim".into(),
                node_id: 34,
                bitrate: None,
                poll_interval_ms: 100,
                heartbeat_timeout_ms: 3000,
                start_on_connect: true,
                channels: vec![
                    ch(
                        "statusword",
                        CanopenAccess::Read,
                        CanopenTransport::Tpdo {
                            slot: 1,
                            byte_offset: 0,
                        },
                    ),
                    ch(
                        "controlword",
                        CanopenAccess::Write,
                        CanopenTransport::Rpdo {
                            slot: 1,
                            byte_offset: 0,
                        },
                    ),
                    ch("velocity_sp", CanopenAccess::Write, CanopenTransport::Sdo),
                    ch(
                        "wrongway",
                        CanopenAccess::Write,
                        CanopenTransport::Tpdo {
                            slot: 2,
                            byte_offset: 0,
                        },
                    ),
                ],
            }),
        }
    }

    fn mapping(variable: &str, direction: Direction, device: &str, channel: &str) -> Mapping {
        Mapping {
            application: "main".into(),
            variable: variable.into(),
            direction,
            device: device.into(),
            channel: channel.into(),
            unit: None,
            min: None,
            max: None,
            description: None,
        }
    }

    fn iomap(mappings: Vec<Mapping>) -> IoMap {
        IoMap { mappings }
    }

    fn errors(issues: &[IomapIssue]) -> Vec<&IomapIssue> {
        issues
            .iter()
            .filter(|i| i.severity == IomapIssueSeverity::Error)
            .collect()
    }

    fn warnings(issues: &[IomapIssue]) -> Vec<&IomapIssue> {
        issues
            .iter()
            .filter(|i| i.severity == IomapIssueSeverity::Warning)
            .collect()
    }

    // ---- rule 0: clean map ---------------------------------------------

    #[test]
    fn clean_map_produces_no_issues() {
        let devices = vec![
            modbus_device("plc"),
            ethercat_device("ec"),
            opcua_device("dcs"),
            canopen_device("servo"),
        ];
        let map = iomap(vec![
            mapping("estop", Direction::Input, "plc", "di_estop"),
            mapping("pump", Direction::Output, "plc", "coil_pump"),
            mapping("status", Direction::Input, "ec", "tx_status"),
            mapping("command", Direction::Output, "ec", "rx_command"),
            mapping("flow_pv", Direction::Input, "dcs", "pv_flow"),
            mapping("flow_sp", Direction::Output, "dcs", "sp_flow"),
            mapping("drive_status", Direction::Input, "servo", "statusword"),
            mapping("drive_cmd", Direction::Output, "servo", "controlword"),
            mapping("drive_sp", Direction::Output, "servo", "velocity_sp"),
        ]);
        assert!(validate_iomap(&map, &devices).is_empty());
    }

    // ---- CANopen-specific direction rules ------------------------------

    #[test]
    fn canopen_output_to_read_channel_is_an_error() {
        let devices = vec![canopen_device("servo")];
        let map = iomap(vec![mapping("x", Direction::Output, "servo", "statusword")]);
        let issues = validate_iomap(&map, &devices);
        assert_eq!(errors(&issues).len(), 1);
        assert!(issues[0].message.contains("access=read"), "{issues:?}");
    }

    #[test]
    fn canopen_output_to_tpdo_is_an_error_even_with_write_access() {
        let devices = vec![canopen_device("servo")];
        let map = iomap(vec![mapping("x", Direction::Output, "servo", "wrongway")]);
        let issues = validate_iomap(&map, &devices);
        assert_eq!(errors(&issues).len(), 1);
        assert!(issues[0].message.contains("TPDO"), "{issues:?}");
    }

    #[test]
    fn canopen_input_from_any_channel_is_fine() {
        // Mirror covers SDO polls, TPDO frames and RPDO shadows alike.
        let devices = vec![canopen_device("servo")];
        let map = iomap(vec![
            mapping("a", Direction::Input, "servo", "statusword"),
            mapping("b", Direction::Input, "servo", "controlword"),
            mapping("c", Direction::Input, "servo", "velocity_sp"),
        ]);
        assert!(validate_iomap(&map, &devices).is_empty());
    }

    #[test]
    fn canopen_unknown_channel_is_an_error() {
        let devices = vec![canopen_device("servo")];
        let map = iomap(vec![mapping("x", Direction::Input, "servo", "nope")]);
        let issues = validate_iomap(&map, &devices);
        assert_eq!(errors(&issues).len(), 1);
        assert!(issues[0].message.contains("no channel"), "{issues:?}");
    }

    // ---- rule 1: unknown device ----------------------------------------

    #[test]
    fn unknown_device_is_an_error() {
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![mapping("x", Direction::Input, "ghost", "whatever")]);
        let issues = validate_iomap(&map, &devices);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, IomapIssueSeverity::Error);
        assert_eq!(issues[0].mapping_index, 0);
        assert!(
            issues[0].message.contains("unknown device 'ghost'"),
            "{}",
            issues[0].message
        );
    }

    // ---- rule 2: unknown channel (every protocol) ------------------------

    #[test]
    fn unknown_channel_is_an_error_for_each_protocol() {
        let devices = vec![
            modbus_device("plc"),
            ethercat_device("ec"),
            opcua_device("dcs"),
        ];
        let map = iomap(vec![
            mapping("a", Direction::Input, "plc", "no_such"),
            mapping("b", Direction::Input, "ec", "no_such"),
            mapping("c", Direction::Input, "dcs", "no_such"),
        ]);
        let issues = validate_iomap(&map, &devices);
        assert_eq!(issues.len(), 3);
        for (i, issue) in issues.iter().enumerate() {
            assert_eq!(issue.severity, IomapIssueSeverity::Error);
            assert_eq!(issue.mapping_index, i);
            assert!(
                issue.message.contains("has no channel 'no_such'"),
                "{}",
                issue.message
            );
        }
    }

    // ---- rule 3: direction compatibility ---------------------------------

    #[test]
    fn modbus_output_to_read_only_kinds_is_an_error() {
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![
            mapping("a", Direction::Output, "plc", "di_estop"), // DiscreteInput
            mapping("b", Direction::Output, "plc", "ir_temp"),  // InputRegister
        ]);
        let issues = validate_iomap(&map, &devices);
        assert_eq!(issues.len(), 2);
        assert!(
            issues[0].message.contains("DiscreteInput"),
            "{}",
            issues[0].message
        );
        assert!(
            issues[1].message.contains("InputRegister"),
            "{}",
            issues[1].message
        );
    }

    #[test]
    fn modbus_input_is_fine_on_every_kind() {
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![
            mapping("a", Direction::Input, "plc", "coil_pump"),
            mapping("b", Direction::Input, "plc", "di_estop"),
            mapping("c", Direction::Input, "plc", "hr_setpoint"),
            mapping("d", Direction::Input, "plc", "ir_temp"),
        ]);
        let issues = validate_iomap(&map, &devices);
        // Readable on every kind: no errors. But the two writable-kind
        // channels bound ONLY as Input get the rule-7 failsafe nudge.
        assert!(errors(&issues).is_empty(), "{issues:?}");
        assert_eq!(warnings(&issues).len(), 2, "{issues:?}");
    }

    #[test]
    fn ethercat_direction_mismatches_are_errors() {
        let devices = vec![ethercat_device("ec")];
        let map = iomap(vec![
            mapping("a", Direction::Input, "ec", "rx_command"), // Input from RxPDO
            mapping("b", Direction::Output, "ec", "tx_status"), // Output to TxPDO
        ]);
        let issues = validate_iomap(&map, &devices);
        assert_eq!(issues.len(), 2);
        assert!(issues[0].message.contains("RxPDO"), "{}", issues[0].message);
        assert!(
            issues[0].message.contains("need a TxPDO"),
            "{}",
            issues[0].message
        );
        assert!(issues[1].message.contains("TxPDO"), "{}", issues[1].message);
        assert!(
            issues[1].message.contains("need an RxPDO"),
            "{}",
            issues[1].message
        );
    }

    #[test]
    fn opcua_output_to_read_tag_is_an_error_but_input_on_write_tag_is_fine() {
        let devices = vec![opcua_device("dcs")];
        let map = iomap(vec![
            mapping("a", Direction::Output, "dcs", "pv_flow"), // write to read tag: error
            mapping("b", Direction::Input, "dcs", "sp_flow"),  // read a write tag: mirrored, OK
        ]);
        let issues = validate_iomap(&map, &devices);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].mapping_index, 0);
        assert!(
            issues[0].message.contains("access=read"),
            "{}",
            issues[0].message
        );
    }

    // ---- rule 4: duplicate writers ---------------------------------------

    #[test]
    fn two_variables_writing_one_channel_is_an_error_on_each_row() {
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![
            mapping("pump_a", Direction::Output, "plc", "coil_pump"),
            mapping("pump_b", Direction::Output, "plc", "coil_pump"),
        ]);
        let issues = validate_iomap(&map, &devices);
        let errs = errors(&issues);
        assert_eq!(errs.len(), 2, "{issues:?}");
        assert_eq!(errs[0].mapping_index, 0);
        assert_eq!(errs[1].mapping_index, 1);
        for e in errs {
            assert!(e.message.contains("conflicting writers"), "{}", e.message);
            assert!(e.message.contains("main.pump_a"), "{}", e.message);
            assert!(e.message.contains("main.pump_b"), "{}", e.message);
        }
    }

    #[test]
    fn same_variable_twice_on_one_channel_is_not_a_writer_conflict() {
        // Redundant rows, but only one writer — no fight to flag. (It is
        // also not fan-out: a single distinct target channel.)
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![
            mapping("pump", Direction::Output, "plc", "coil_pump"),
            mapping("pump", Direction::Output, "plc", "coil_pump"),
        ]);
        assert!(validate_iomap(&map, &devices).is_empty());
    }

    #[test]
    fn two_variables_reading_one_channel_is_fine() {
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![
            mapping("t1", Direction::Input, "plc", "ir_temp"),
            mapping("t2", Direction::Input, "plc", "ir_temp"),
        ]);
        assert!(validate_iomap(&map, &devices).is_empty());
    }

    // ---- rule 5: output fan-out ------------------------------------------

    #[test]
    fn one_variable_outputting_to_two_channels_is_a_warning_on_each_row() {
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![
            mapping("cmd", Direction::Output, "plc", "coil_pump"),
            mapping("cmd", Direction::Output, "plc", "hr_setpoint"),
        ]);
        let issues = validate_iomap(&map, &devices);
        let warns = warnings(&issues);
        assert_eq!(warns.len(), 2, "{issues:?}");
        assert!(
            errors(&issues).is_empty(),
            "fan-out alone is not an error: {issues:?}"
        );
        for w in warns {
            assert!(w.message.contains("main.cmd"), "{}", w.message);
            assert!(w.message.contains("plc/coil_pump"), "{}", w.message);
            assert!(w.message.contains("plc/hr_setpoint"), "{}", w.message);
        }
    }

    #[test]
    fn same_variable_name_in_different_applications_is_not_fanout() {
        let devices = vec![modbus_device("plc")];
        let mut m1 = mapping("cmd", Direction::Output, "plc", "coil_pump");
        m1.application = "line_a".into();
        let mut m2 = mapping("cmd", Direction::Output, "plc", "hr_setpoint");
        m2.application = "line_b".into();
        let map = iomap(vec![m1, m2]);
        assert!(validate_iomap(&map, &devices).is_empty());
    }

    #[test]
    fn input_fanout_is_not_flagged() {
        // The same variable reading two channels would be odd, but rule 5
        // is explicitly about outputs (reads can't fight).
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![
            mapping("v", Direction::Input, "plc", "ir_temp"),
            mapping("v", Direction::Input, "plc", "di_estop"),
        ]);
        assert!(validate_iomap(&map, &devices).is_empty());
    }

    // ---- rule 6: engineering-range metadata -------------------------------

    fn with_range(mut m: Mapping, min: Option<f64>, max: Option<f64>) -> Mapping {
        m.min = min;
        m.max = max;
        m
    }

    #[test]
    fn metadata_min_greater_than_max_is_an_error() {
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![with_range(
            mapping("sp", Direction::Output, "plc", "hr_setpoint"),
            Some(200.0),
            Some(0.0),
        )]);
        let issues = validate_iomap(&map, &devices);
        assert_eq!(errors(&issues).len(), 1, "{issues:?}");
        assert_eq!(issues[0].mapping_index, 0);
        assert!(
            issues[0]
                .message
                .contains("min (200) is greater than max (0)"),
            "{}",
            issues[0].message
        );
    }

    #[test]
    fn metadata_non_finite_bounds_are_errors() {
        // TOML happily parses `min = nan` / `max = inf`; both bounds on
        // one row report independently.
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![with_range(
            mapping("sp", Direction::Output, "plc", "hr_setpoint"),
            Some(f64::NAN),
            Some(f64::INFINITY),
        )]);
        let issues = validate_iomap(&map, &devices);
        let errs = errors(&issues);
        assert_eq!(errs.len(), 2, "{issues:?}");
        assert!(
            errs[0].message.contains("min must be a finite number"),
            "{}",
            errs[0].message
        );
        assert!(
            errs[1].message.contains("max must be a finite number"),
            "{}",
            errs[1].message
        );
    }

    #[test]
    fn metadata_is_checked_even_when_the_device_is_unknown() {
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![with_range(
            mapping("sp", Direction::Output, "ghost", "x"),
            Some(1.0),
            Some(0.0),
        )]);
        let issues = validate_iomap(&map, &devices);
        let errs = errors(&issues);
        assert_eq!(errs.len(), 2, "{issues:?}");
        assert!(errs.iter().any(|e| e.message.contains("unknown device")));
        assert!(errs
            .iter()
            .any(|e| e.message.contains("min (1) is greater than max (0)")));
    }

    #[test]
    fn range_metadata_on_boolean_channels_is_a_warning() {
        // Modbus Coil and EtherCAT Bool PDO — the two most common
        // boolean lanes; OPC UA / CANopen Bool tags follow the same
        // helper.
        let devices = vec![modbus_device("plc"), ethercat_device("ec")];
        let map = iomap(vec![
            with_range(
                mapping("pump", Direction::Output, "plc", "coil_pump"),
                Some(0.0),
                Some(1.0),
            ),
            with_range(
                mapping("status", Direction::Input, "ec", "tx_status"),
                None,
                Some(1.0),
            ),
        ]);
        let issues = validate_iomap(&map, &devices);
        assert!(errors(&issues).is_empty(), "{issues:?}");
        let warns = warnings(&issues);
        assert_eq!(warns.len(), 2, "{issues:?}");
        for w in warns {
            assert!(w.message.contains("boolean channel"), "{}", w.message);
        }
    }

    #[test]
    fn valid_range_metadata_is_clean() {
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![with_range(
            mapping("sp", Direction::Output, "plc", "hr_setpoint"),
            Some(0.0),
            Some(100.0),
        )]);
        assert!(validate_iomap(&map, &devices).is_empty());
    }

    // ---- aggregation ------------------------------------------------------

    // ---- access field (writable kind ≠ write permission) -----------------

    #[test]
    fn output_on_access_read_holding_is_an_error() {
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![mapping("sp", Direction::Output, "plc", "hr_meas")]);
        let issues = validate_iomap(&map, &devices);
        assert_eq!(errors(&issues).len(), 1, "{issues:?}");
        assert!(issues[0].message.contains("access=read"), "{issues:?}");
    }

    #[test]
    fn input_only_writable_holding_warns_about_failsafe() {
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![mapping(
            "temp",
            Direction::Input,
            "plc",
            "hr_setpoint",
        )]);
        let issues = validate_iomap(&map, &devices);
        let w = warnings(&issues);
        assert_eq!(w.len(), 1, "{issues:?}");
        assert!(w[0].message.contains("failsafe"), "{issues:?}");
    }

    #[test]
    fn input_on_access_read_holding_is_clean() {
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![mapping("temp", Direction::Input, "plc", "hr_meas")]);
        assert!(validate_iomap(&map, &devices).is_empty());
    }

    #[test]
    fn output_readback_pair_does_not_warn() {
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![
            mapping("sp", Direction::Output, "plc", "hr_setpoint"),
            mapping("sp_fb", Direction::Input, "plc", "hr_setpoint"),
        ]);
        let issues = validate_iomap(&map, &devices);
        assert!(warnings(&issues).is_empty(), "{issues:?}");
        assert!(errors(&issues).is_empty(), "{issues:?}");
    }

    #[test]
    fn issues_come_back_sorted_by_mapping_index() {
        let devices = vec![modbus_device("plc")];
        let map = iomap(vec![
            mapping("ok", Direction::Input, "plc", "ir_temp"),
            mapping("w1", Direction::Output, "plc", "coil_pump"),
            mapping("bad", Direction::Input, "ghost", "x"),
            mapping("w2", Direction::Output, "plc", "coil_pump"),
        ]);
        let issues = validate_iomap(&map, &devices);
        let indices: Vec<usize> = issues.iter().map(|i| i.mapping_index).collect();
        let mut sorted = indices.clone();
        sorted.sort_unstable();
        assert_eq!(indices, sorted, "{issues:?}");
        // 1 unknown-device error + 2 conflicting-writer errors.
        assert_eq!(errors(&issues).len(), 3, "{issues:?}");
    }

    #[test]
    fn empty_iomap_and_no_devices_is_clean() {
        assert!(validate_iomap(&IoMap::default(), &[]).is_empty());
    }

    #[test]
    fn serializes_with_snake_case_severity() {
        let issue = error(3, "boom".into());
        let v = serde_json::to_value(&issue).unwrap();
        assert_eq!(v["severity"], "error");
        assert_eq!(v["mapping_index"], 3);
        assert_eq!(v["message"], "boom");
    }
}
