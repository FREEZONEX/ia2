//! Static lint for a device document, checked against *itself*.
//!
//! [`crate::iomap_check`] validates mappings against devices: does the named
//! device exist, does the named channel exist on it, is the direction
//! possible. Nothing validated the device document on its own, and the two
//! defects below both fail silently — no error, no warning, no log line.
//!
//! **Duplicate channel names.** Every adapter builds its channel table as
//! `HashMap::insert(ch.name, ch)` over the config's `Vec`, so a repeated name
//! does not collide — it *overwrites*, and the earlier channel ceases to
//! exist. A mapping onto that name still validates (the surviving channel
//! answers to it) and then reads or writes the wrong register, object or node
//! for the rest of the plant's life. `iomap_check` makes it worse by
//! disagreeing: it resolves a name with `channels.iter().find(...)` — the
//! FIRST match — while the adapter keeps the LAST, so the linter can be
//! reasoning about a channel the runtime has already discarded. Both
//! `EthercatChannel` and `CanopenChannel` document the field as a "unique
//! channel name"; nothing enforced it.
//!
//! **A failsafe that can never be written.** OPC UA and CANopen carry an
//! opt-in per-channel `failsafe`, applied on shutdown or trip. Both adapters
//! filter their sweep to `access == write`, so a `failsafe` on a read channel
//! is dropped without a word. That is a safety value the author believes is
//! configured and which cannot ever be applied.
//!
//! Every finding is an error. Neither is a style question: one silently
//! discards a channel, the other silently discards a safe state.

use std::collections::HashMap;

use crate::types::{CanopenAccess, Device, OpcuaAccess, ProtocolConfig};

/// One finding, naming the device and the channel it concerns.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceIssue {
    pub device: String,
    pub channel: String,
    pub message: String,
}

/// Channel names that appear more than once, each reported once, in order of
/// first appearance so the report is stable across runs.
///
/// Shared with the adapters, which apply it to their own config at connect:
/// the edge runtime runs no project validation at all, so the adapter is the
/// only place that always sees the document.
pub fn duplicate_channel_names<'a, I>(names: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for name in names {
        if *counts.entry(name).or_insert(0) == 0 {
            order.push(name);
        }
        *counts.get_mut(name).expect("just inserted") += 1;
    }
    order
        .into_iter()
        .filter(|n| counts[n] > 1)
        .map(str::to_string)
        .collect()
}

/// Check every device document. Returns all findings; an empty Vec means the
/// documents are internally consistent.
pub fn validate_devices(devices: &[Device]) -> Vec<DeviceIssue> {
    let mut issues = Vec::new();
    for device in devices {
        let names = device.config.channel_names();
        for dup in duplicate_channel_names(names.iter().map(String::as_str)) {
            let count = names.iter().filter(|n| **n == dup).count();
            issues.push(DeviceIssue {
                device: device.name.clone(),
                channel: dup.clone(),
                message: format!(
                    "channel name '{dup}' is declared {count} times — the adapter keeps only \
                     the last one, so the others silently do not exist; give each channel \
                     its own name"
                ),
            });
        }
        for (channel, access) in unwritable_failsafes(&device.config) {
            issues.push(DeviceIssue {
                device: device.name.clone(),
                channel: channel.clone(),
                message: format!(
                    "channel '{channel}' sets a failsafe but has access = {access}, and the \
                     failsafe sweep only writes access = write channels — this safe state can \
                     never be applied"
                ),
            });
        }
    }
    issues
}

/// Channels carrying a `failsafe` the adapter's sweep will never write.
/// Modbus has no per-channel failsafe (it zeroes every writable output) and
/// EtherCAT's failsafe is the whole-image zero, so neither can produce this.
fn unwritable_failsafes(config: &ProtocolConfig) -> Vec<(String, &'static str)> {
    match config {
        ProtocolConfig::Opcua(c) => c
            .channels
            .iter()
            .filter(|ch| ch.failsafe.is_some() && ch.access != OpcuaAccess::Write)
            .map(|ch| (ch.name.clone(), "read"))
            .collect(),
        ProtocolConfig::Canopen(c) => c
            .channels
            .iter()
            .filter(|ch| ch.failsafe.is_some() && ch.access != CanopenAccess::Write)
            .map(|ch| (ch.name.clone(), "read"))
            .collect(),
        ProtocolConfig::Modbus(_) | ProtocolConfig::Ethercat(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        CanopenChannel, CanopenConfig, CanopenDataType, CanopenTransport, ModbusAccess,
        ModbusChannel, ModbusChannelKind, ModbusConfig, ModbusDataType, ModbusTcpParams,
        ModbusTransport, ModbusWordOrder, OpcuaChannel, OpcuaConfig, OpcuaDataType,
    };

    fn modbus_device(name: &str, channels: Vec<ModbusChannel>) -> Device {
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
                channels,
            }),
        }
    }

    fn reg(name: &str, address: u16) -> ModbusChannel {
        ModbusChannel {
            name: name.into(),
            kind: ModbusChannelKind::HoldingRegister,
            address,
            data_type: ModbusDataType::U16,
            word_order: ModbusWordOrder::HiLo,
            access: ModbusAccess::Read,
        }
    }

    #[test]
    fn a_clean_document_reports_nothing() {
        let d = modbus_device("plc", vec![reg("flow", 10), reg("level", 11)]);
        assert!(validate_devices(&[d]).is_empty());
    }

    /// The defect: two registers, one name. The adapter's HashMap keeps the
    /// last, and address 10 becomes unreachable while every mapping onto
    /// 'flow' silently moves to address 99.
    #[test]
    fn a_repeated_channel_name_is_an_error_naming_the_count() {
        let d = modbus_device(
            "plc",
            vec![reg("flow", 10), reg("level", 11), reg("flow", 99)],
        );
        let issues = validate_devices(&[d]);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].device, "plc");
        assert_eq!(issues[0].channel, "flow");
        assert!(
            issues[0].message.contains("declared 2 times"),
            "{}",
            issues[0].message
        );
    }

    #[test]
    fn each_repeated_name_is_reported_once_in_first_appearance_order() {
        let d = modbus_device(
            "plc",
            vec![
                reg("b", 1),
                reg("a", 2),
                reg("b", 3),
                reg("a", 4),
                reg("b", 5),
            ],
        );
        let issues = validate_devices(&[d]);
        let names: Vec<&str> = issues.iter().map(|i| i.channel.as_str()).collect();
        assert_eq!(
            names,
            vec!["b", "a"],
            "one finding per name, first-seen order"
        );
        assert!(
            issues[0].message.contains("declared 3 times"),
            "{}",
            issues[0].message
        );
    }

    /// Names are compared exactly — the adapters' HashMap does too, so
    /// 'Flow' and 'flow' really are two different channels.
    #[test]
    fn channel_names_differing_only_in_case_are_not_duplicates() {
        let d = modbus_device("plc", vec![reg("flow", 10), reg("Flow", 11)]);
        assert!(validate_devices(&[d]).is_empty());
    }

    fn opcua_device(channels: Vec<OpcuaChannel>) -> Device {
        Device {
            name: "dcs".into(),
            config: ProtocolConfig::Opcua(OpcuaConfig {
                endpoint_url: "opc.tcp://127.0.0.1:4840".into(),
                auth: Default::default(),
                poll_interval_ms: 500,
                channels,
            }),
        }
    }

    fn tag(name: &str, access: OpcuaAccess, failsafe: Option<f64>) -> OpcuaChannel {
        OpcuaChannel {
            name: name.into(),
            node_id: format!("ns=2;s={name}"),
            data_type: OpcuaDataType::F64,
            access,
            failsafe,
        }
    }

    #[test]
    fn an_opcua_failsafe_on_a_read_tag_is_an_error() {
        let d = opcua_device(vec![
            tag("sp", OpcuaAccess::Write, Some(0.0)),
            tag("pv", OpcuaAccess::Read, Some(0.0)),
        ]);
        let issues = validate_devices(&[d]);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].channel, "pv");
        assert!(
            issues[0].message.contains("can never be applied"),
            "{}",
            issues[0].message
        );
    }

    /// A failsafe of 0.0 on a write tag is the ordinary case and must stay
    /// silent — `Some(0.0)` is a configured safe state, not an absent one.
    #[test]
    fn an_opcua_failsafe_of_zero_on_a_write_tag_is_fine() {
        let d = opcua_device(vec![tag("sp", OpcuaAccess::Write, Some(0.0))]);
        assert!(validate_devices(&[d]).is_empty());
    }

    #[test]
    fn a_canopen_failsafe_on_a_read_object_is_an_error() {
        let d = Device {
            name: "drive".into(),
            config: ProtocolConfig::Canopen(CanopenConfig {
                interface: "_sim".into(),
                node_id: 1,
                bitrate: None,
                poll_interval_ms: 40,
                heartbeat_timeout_ms: 400,
                start_on_connect: true,
                channels: vec![CanopenChannel {
                    name: "statusword".into(),
                    index: 0x6041,
                    sub_index: 0,
                    data_type: CanopenDataType::U16,
                    access: CanopenAccess::Read,
                    transport: CanopenTransport::Sdo,
                    failsafe: Some(0.0),
                }],
            }),
        };
        let issues = validate_devices(&[d]);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].channel, "statusword");
    }

    #[test]
    fn findings_name_the_device_they_came_from() {
        let a = modbus_device("alpha", vec![reg("x", 1), reg("x", 2)]);
        let b = modbus_device("beta", vec![reg("y", 1), reg("y", 2)]);
        let issues = validate_devices(&[a, b]);
        let pairs: Vec<(&str, &str)> = issues
            .iter()
            .map(|i| (i.device.as_str(), i.channel.as_str()))
            .collect();
        assert_eq!(pairs, vec![("alpha", "x"), ("beta", "y")]);
    }
}
