//! Connect-time validation of the configured topology against the
//! discovered bus.
//!
//! Pure functions over config + [`SlaveDiscovery`] lists so they are
//! unit-testable without hardware. Used by the real driver right after
//! the bus walk (identity + PDI ranges + channel shapes) and by the sim
//! driver against its derived extents (PDI ranges) — the goal is to fail
//! the *connect* with a precise message instead of dribbling per-cycle
//! `Transport` errors out of the PDI accessors later.

use project::{EthercatChannel, EthercatGear, EthercatPdoDirection, EthercatSlave, GearMaster};

use crate::SlaveDiscovery;

/// Reject channel *references* before either driver touches the bus: no
/// two channels may share a name (iomap entries and the PDI/value maps are
/// keyed by it, so a collision would silently alias) and every channel's
/// `slave_index` must name a declared slave. An empty slave list means the
/// topology wasn't authored and skips the index check — back-compat with
/// sim-only configs that predate the slave table. Run first by both the
/// real and sim connect paths.
pub(crate) fn validate_channel_refs(
    channels: &[EthercatChannel],
    slaves: &[EthercatSlave],
) -> Result<(), String> {
    let known_slaves: std::collections::HashSet<u16> = slaves.iter().map(|s| s.index).collect();
    let mut seen_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for ch in channels {
        if !seen_names.insert(ch.name.as_str()) {
            return Err(format!("duplicate channel name '{}'", ch.name));
        }
        if !known_slaves.is_empty() && !known_slaves.contains(&ch.slave_index) {
            return Err(format!(
                "channel '{}' references unknown slave_index={}",
                ch.name, ch.slave_index
            ));
        }
    }
    Ok(())
}

/// Compare each configured slave's identity against what the bus walk
/// found at that index. A `vendor_id`/`product_id` of 0 means "not
/// authored" and skips that comparison (back-compat with configs written
/// before identity support); a slave with both at 0 is skipped entirely.
/// Returns every mismatch joined into one message — wrong module *and*
/// wrong position usually show up together, and seeing all of it beats
/// fixing one line per restart.
pub(crate) fn validate_identities(
    configured: &[EthercatSlave],
    discovered: &[SlaveDiscovery],
) -> Result<(), String> {
    let mut problems: Vec<String> = Vec::new();
    for slave in configured {
        if slave.vendor_id == 0 && slave.product_id == 0 {
            continue;
        }
        let Some(found) = discovered.iter().find(|d| d.index == slave.index) else {
            problems.push(format!(
                "slave {idx} ('{name}'): not found on the bus ({n} subdevice(s) discovered)",
                idx = slave.index,
                name = slave.name,
                n = discovered.len()
            ));
            continue;
        };
        if slave.vendor_id != 0 && slave.vendor_id != found.vendor_id {
            problems.push(format!(
                "slave {idx} ('{name}'): vendor_id mismatch: expected {exp:#010x}, found \
                 {got:#010x} ('{got_name}')",
                idx = slave.index,
                name = slave.name,
                exp = slave.vendor_id,
                got = found.vendor_id,
                got_name = found.name
            ));
        }
        if slave.product_id != 0 && slave.product_id != found.product_id {
            problems.push(format!(
                "slave {idx} ('{name}'): product_id mismatch: expected {exp:#010x}, found \
                 {got:#010x} ('{got_name}')",
                idx = slave.index,
                name = slave.name,
                exp = slave.product_id,
                got = found.product_id,
                got_name = found.name
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

/// Verify every channel's PDI window fits inside the discovered (or, in
/// sim mode, derived) extents of its slave: a TxPDO channel must fit the
/// slave's input bytes, an RxPDO channel its output bytes. Counted in
/// bits so bit-packed digital I/O at the last byte's high bits still
/// passes while one bit past the end fails.
pub(crate) fn validate_pdi_ranges(
    channels: &[EthercatChannel],
    discovered: &[SlaveDiscovery],
) -> Result<(), String> {
    let mut problems: Vec<String> = Vec::new();
    for ch in channels {
        let Some(slave) = discovered.iter().find(|d| d.index == ch.slave_index) else {
            problems.push(format!(
                "channel '{name}': slave_index={idx} not present on the bus",
                name = ch.name,
                idx = ch.slave_index
            ));
            continue;
        };
        let (extent_bytes, region) = match ch.direction {
            EthercatPdoDirection::TxPdo => (slave.input_bytes, "input"),
            EthercatPdoDirection::RxPdo => (slave.output_bytes, "output"),
        };
        let start_bit = ch.pdi_byte_offset as u32 * 8 + ch.pdi_bit_offset as u32;
        let end_bit = start_bit + ch.bit_length as u32;
        if end_bit > extent_bytes as u32 * 8 {
            problems.push(format!(
                "channel '{name}': needs bits {start_bit}..{end_bit} of slave {idx} \
                 ('{slave_name}') {region} PDI, but it has only {extent_bytes} byte(s)",
                name = ch.name,
                idx = ch.slave_index,
                slave_name = slave.name,
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

/// Hold each `[[gear]]` axis against the discovered bus: the follower's
/// target_position (i32) must fit its output PDI, and its actual_position
/// (i32) plus statusword (u16) must fit its input PDI; an `Axis` master's
/// actual_position (i32) must fit that slave's input PDI. Returns one
/// message per offending offset (empty when clean) so the caller folds
/// them into the same post-discovery batch as the identity and PDI-range
/// checks. Real mode only — sim stores gear parameters by name and has no
/// cyclic gear PDI to overflow.
pub(crate) fn validate_gear_offsets(
    gear: &[EthercatGear],
    discovered: &[SlaveDiscovery],
) -> Vec<String> {
    let mut problems: Vec<String> = Vec::new();
    let find = |idx: u16| discovered.iter().find(|d| d.index == idx);
    for g in gear {
        match find(g.slave_index) {
            Some(d) => {
                if g.target_pos_offset as usize + 4 > d.output_bytes as usize {
                    problems.push(format!(
                        "gear follower slave {} target_pos_offset {}+4 exceeds output PDI ({} B)",
                        g.slave_index, g.target_pos_offset, d.output_bytes
                    ));
                }
                if g.actual_pos_offset as usize + 4 > d.input_bytes as usize
                    || g.status_word_offset as usize + 2 > d.input_bytes as usize
                {
                    problems.push(format!(
                        "gear follower slave {} actual/status offsets exceed input PDI ({} B)",
                        g.slave_index, d.input_bytes
                    ));
                }
            }
            None => problems.push(format!(
                "gear follower slave_index {} not on the discovered bus",
                g.slave_index
            )),
        }
        if let GearMaster::Axis {
            slave_index,
            actual_pos_offset,
        } = g.master
        {
            match find(slave_index) {
                Some(d) if (actual_pos_offset as usize + 4) <= d.input_bytes as usize => {}
                Some(d) => problems.push(format!(
                    "gear master slave {} actual_pos_offset {}+4 exceeds input PDI ({} B)",
                    slave_index, actual_pos_offset, d.input_bytes
                )),
                None => problems.push(format!(
                    "gear master slave_index {slave_index} not on the discovered bus"
                )),
            }
        }
    }
    problems
}

/// Reject channel shapes the PDI bit accessors cannot serve: zero-length
/// entries and non-byte-aligned multi-bit values (`bits.rs` supports
/// sub-byte offsets only for single-bit channels). Real mode only — sim
/// mode stores values per name and never touches the bit packers, so
/// legacy sim configs with odd shapes keep connecting.
pub(crate) fn validate_channel_shapes(channels: &[EthercatChannel]) -> Result<(), String> {
    let mut problems: Vec<String> = Vec::new();
    for ch in channels {
        if ch.bit_length == 0 {
            problems.push(format!(
                "channel '{name}': bit_length must be > 0",
                name = ch.name
            ));
            continue;
        }
        if ch.bit_length > 1 && ch.pdi_bit_offset != 0 {
            problems.push(format!(
                "channel '{name}': non-byte-aligned multi-bit entries are not supported \
                 (bit_length={len}, pdi_bit_offset={off})",
                name = ch.name,
                len = ch.bit_length,
                off = ch.pdi_bit_offset
            ));
        }
        // The lane must be able to hold the entry. `bits.rs` owns the rule so
        // connect and the accessors cannot drift apart; an entry wider than
        // its declared type would lose its high bits every cycle, silently.
        if let Err(why) = crate::bits::check_width(ch.data_type, ch.bit_length) {
            problems.push(format!("channel '{name}': {why}", name = ch.name));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

/// Reject malformed `init_sdo` entries at connect, before the bus thread
/// spawns: width must be one of the expedited-transfer sizes, and the
/// value must fit that width (as either a signed or unsigned quantity).
pub(crate) fn validate_init_sdo(slaves: &[EthercatSlave]) -> Result<(), String> {
    let mut problems: Vec<String> = Vec::new();
    for slave in slaves {
        for cmd in &slave.init_sdo {
            let fits = match cmd.bits {
                8 => (-(1 << 7)..(1i64 << 8)).contains(&cmd.value),
                16 => (-(1 << 15)..(1i64 << 16)).contains(&cmd.value),
                32 => (-(1 << 31)..(1i64 << 32)).contains(&cmd.value),
                other => {
                    problems.push(format!(
                        "slave {} init_sdo {:#06x}:{:02x}: bits must be 8, 16, or 32 (got {other})",
                        slave.index, cmd.index, cmd.sub_index
                    ));
                    continue;
                }
            };
            if !fits {
                problems.push(format!(
                    "slave {} init_sdo {:#06x}:{:02x}: value {} does not fit in {} bits",
                    slave.index, cmd.index, cmd.sub_index, cmd.value, cmd.bits
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use project::{EthercatDataType, EthercatSdoInit};

    fn slave(index: u16, name: &str, vendor_id: u32, product_id: u32) -> EthercatSlave {
        EthercatSlave {
            index,
            name: name.into(),
            vendor_id,
            product_id,
            dc_sync: None,
            init_sdo: vec![],
        }
    }

    fn slave_with_sdo(value: i64, bits: u8) -> EthercatSlave {
        EthercatSlave {
            index: 0,
            name: "sv660n".into(),
            vendor_id: 0,
            product_id: 0,
            dc_sync: None,
            init_sdo: vec![EthercatSdoInit {
                index: 0x6060,
                sub_index: 0,
                value,
                bits,
            }],
        }
    }

    fn found(
        index: u16,
        name: &str,
        vendor_id: u32,
        product_id: u32,
        input_bytes: u16,
        output_bytes: u16,
    ) -> SlaveDiscovery {
        SlaveDiscovery {
            index,
            name: name.into(),
            input_bytes,
            output_bytes,
            vendor_id,
            product_id,
        }
    }

    fn channel(
        name: &str,
        slave_index: u16,
        direction: EthercatPdoDirection,
        byte_offset: u16,
        bit_offset: u8,
        bit_length: u8,
    ) -> EthercatChannel {
        EthercatChannel {
            name: name.into(),
            slave_index,
            direction,
            pdo_index: 0x6000,
            sub_index: 1,
            bit_length,
            // Derived from the width rather than hardcoded: these fixtures
            // used to declare 16-bit entries as Bool, a shape the accessors
            // cannot serve and `validate_channel_shapes` now rejects.
            data_type: match bit_length {
                0 | 1 => EthercatDataType::Bool,
                2..=8 => EthercatDataType::U8,
                9..=16 => EthercatDataType::U16,
                _ => EthercatDataType::U32,
            },
            pdi_byte_offset: byte_offset,
            pdi_bit_offset: bit_offset,
        }
    }

    fn gear(
        slave_index: u16,
        target_pos_offset: u16,
        actual_pos_offset: u16,
        status_word_offset: u16,
        master: GearMaster,
    ) -> EthercatGear {
        EthercatGear {
            slave_index,
            target_pos_offset,
            actual_pos_offset,
            status_word_offset,
            master,
            engage_channel: "engage".into(),
            ratio_num_channel: "ratio_num".into(),
            ratio_den_channel: "ratio_den".into(),
            ratio_step_channel: "ratio_step".into(),
            phase_channel: "phase".into(),
            master_vel_channel: "master_vel".into(),
            max_travel_channel: "max_travel".into(),
            engaged_channel: "engaged".into(),
            trip_channel: "trip".into(),
            ratio_apply_channel: "ratio_apply".into(),
            ratio_ack_channel: "ratio_ack".into(),
        }
    }

    // ---- channel refs ------------------------------------------------------

    #[test]
    fn duplicate_channel_name_is_rejected() {
        let slaves = vec![slave(0, "io", 0, 0)];
        let chans = vec![
            channel("dup", 0, EthercatPdoDirection::TxPdo, 0, 0, 1),
            channel("dup", 0, EthercatPdoDirection::RxPdo, 1, 0, 1),
        ];
        let err = validate_channel_refs(&chans, &slaves).unwrap_err();
        assert!(err.contains("duplicate channel name 'dup'"), "{err}");
    }

    #[test]
    fn channel_referencing_unknown_slave_is_rejected() {
        let slaves = vec![slave(0, "io", 0, 0)];
        let chans = vec![channel("ghost", 5, EthercatPdoDirection::TxPdo, 0, 0, 1)];
        let err = validate_channel_refs(&chans, &slaves).unwrap_err();
        assert!(err.contains("references unknown slave_index=5"), "{err}");
    }

    #[test]
    fn unique_names_on_known_slaves_pass() {
        let slaves = vec![slave(0, "a", 0, 0), slave(1, "b", 0, 0)];
        let chans = vec![
            channel("in", 0, EthercatPdoDirection::TxPdo, 0, 0, 8),
            channel("out", 1, EthercatPdoDirection::RxPdo, 0, 0, 8),
        ];
        assert!(validate_channel_refs(&chans, &slaves).is_ok());
    }

    #[test]
    fn empty_slave_list_skips_the_index_check() {
        // Back-compat: sim-only configs predating the slave table carry no
        // slaves, so the slave_index check is skipped (names still checked).
        let chans = vec![channel("in", 9, EthercatPdoDirection::TxPdo, 0, 0, 8)];
        assert!(validate_channel_refs(&chans, &[]).is_ok());
    }

    // ---- identity ---------------------------------------------------------

    #[test]
    fn matching_identity_passes() {
        let cfg = vec![slave(0, "SV660N", 0x00100000, 0x000C0108)];
        let bus = vec![found(0, "SV660-Ecat", 0x00100000, 0x000C0108, 32, 32)];
        assert!(validate_identities(&cfg, &bus).is_ok());
    }

    #[test]
    fn zero_ids_skip_all_identity_checks_even_when_absent() {
        // Back-compat: configs authored before identity support carry 0/0
        // and must keep connecting — even if the slave list doesn't line
        // up with the bus (PDI validation handles actual channel use).
        let cfg = vec![slave(7, "unknown", 0, 0)];
        assert!(validate_identities(&cfg, &[]).is_ok());
    }

    #[test]
    fn vendor_mismatch_reports_expected_and_found() {
        let cfg = vec![slave(0, "EL2008", 0x2, 0x07D83052)];
        let bus = vec![found(0, "EL1008", 0x2, 0x03F03052, 1, 0)];
        let err = validate_identities(&cfg, &bus).unwrap_err();
        assert!(err.contains("product_id mismatch"), "{err}");
        assert!(err.contains("expected 0x07d83052"), "{err}");
        assert!(err.contains("found 0x03f03052"), "{err}");
        assert!(err.contains("'EL1008'"), "{err}");
    }

    #[test]
    fn only_nonzero_fields_are_compared() {
        // product authored, vendor left 0 → vendor difference tolerated.
        let cfg = vec![slave(0, "drive", 0, 0xBEEF)];
        let bus = vec![found(0, "drive", 0x1234, 0xBEEF, 8, 8)];
        assert!(validate_identities(&cfg, &bus).is_ok());
        // …and the authored product still catches a mismatch.
        let bus_wrong = vec![found(0, "other", 0x1234, 0xF00D, 8, 8)];
        let err = validate_identities(&cfg, &bus_wrong).unwrap_err();
        assert!(err.contains("product_id mismatch"), "{err}");
        assert!(!err.contains("vendor_id mismatch"), "{err}");
    }

    #[test]
    fn authored_slave_missing_from_bus_is_an_error() {
        let cfg = vec![slave(1, "EL2008", 0x2, 0x07D83052)];
        let bus = vec![found(0, "EK1100", 0x2, 0x044C2C52, 0, 0)];
        let err = validate_identities(&cfg, &bus).unwrap_err();
        assert!(err.contains("slave 1 ('EL2008'): not found"), "{err}");
        assert!(err.contains("1 subdevice(s) discovered"), "{err}");
    }

    #[test]
    fn multiple_identity_problems_are_all_reported() {
        let cfg = vec![slave(0, "a", 0x1, 0x1), slave(1, "b", 0x2, 0x2)];
        let bus = vec![found(0, "x", 0x9, 0x1, 1, 1), found(1, "y", 0x2, 0x9, 1, 1)];
        let err = validate_identities(&cfg, &bus).unwrap_err();
        assert!(err.contains("vendor_id mismatch"), "{err}");
        assert!(err.contains("product_id mismatch"), "{err}");
        assert!(err.contains("; "), "issues joined: {err}");
    }

    // ---- PDI ranges --------------------------------------------------------

    #[test]
    fn in_range_channels_pass_both_directions() {
        let bus = vec![found(0, "mod", 0, 0, 2, 4)];
        let chans = vec![
            channel("in_word", 0, EthercatPdoDirection::TxPdo, 0, 0, 16),
            channel("out_dword", 0, EthercatPdoDirection::RxPdo, 0, 0, 32),
            // last bit of the last input byte — boundary, still fine
            channel("in_flag", 0, EthercatPdoDirection::TxPdo, 1, 7, 1),
        ];
        assert!(validate_pdi_ranges(&chans, &bus).is_ok());
    }

    #[test]
    fn txpdo_overflowing_input_bytes_is_an_error() {
        let bus = vec![found(0, "EL1002", 0, 0, 1, 0)];
        let chans = vec![channel("in_word", 0, EthercatPdoDirection::TxPdo, 0, 0, 16)];
        let err = validate_pdi_ranges(&chans, &bus).unwrap_err();
        assert!(err.contains("'in_word'"), "{err}");
        assert!(err.contains("bits 0..16"), "{err}");
        assert!(err.contains("input PDI"), "{err}");
        assert!(err.contains("only 1 byte(s)"), "{err}");
    }

    #[test]
    fn rxpdo_overflowing_output_bytes_is_an_error() {
        let bus = vec![found(0, "EL2008", 0, 0, 0, 1)];
        // byte_offset 1 starts past the single output byte
        let chans = vec![channel("out9", 0, EthercatPdoDirection::RxPdo, 1, 0, 1)];
        let err = validate_pdi_ranges(&chans, &bus).unwrap_err();
        assert!(err.contains("bits 8..9"), "{err}");
        assert!(err.contains("output PDI"), "{err}");
    }

    #[test]
    fn one_bit_past_the_end_fails_while_last_bit_passes() {
        let bus = vec![found(0, "io", 0, 0, 1, 0)];
        let last = vec![channel("ok", 0, EthercatPdoDirection::TxPdo, 0, 7, 1)];
        assert!(validate_pdi_ranges(&last, &bus).is_ok());
        let past = vec![channel("bad", 0, EthercatPdoDirection::TxPdo, 1, 0, 1)];
        assert!(validate_pdi_ranges(&past, &bus).is_err());
    }

    #[test]
    fn channel_on_undiscovered_slave_is_an_error() {
        let bus = vec![found(0, "only", 0, 0, 4, 4)];
        let chans = vec![channel("ghost", 3, EthercatPdoDirection::TxPdo, 0, 0, 8)];
        let err = validate_pdi_ranges(&chans, &bus).unwrap_err();
        assert!(err.contains("slave_index=3 not present"), "{err}");
    }

    #[test]
    fn all_range_problems_are_reported_together() {
        let bus = vec![found(0, "io", 0, 0, 1, 1)];
        let chans = vec![
            channel("a", 0, EthercatPdoDirection::TxPdo, 2, 0, 8),
            channel("b", 0, EthercatPdoDirection::RxPdo, 2, 0, 8),
            channel("c", 9, EthercatPdoDirection::TxPdo, 0, 0, 1),
        ];
        let err = validate_pdi_ranges(&chans, &bus).unwrap_err();
        assert_eq!(err.matches("channel '").count(), 3, "{err}");
    }

    // ---- gear offsets -------------------------------------------------------

    #[test]
    fn gear_target_offset_overflowing_output_pdi_is_reported() {
        // output PDI = 4 B; target_pos_offset 2 needs [2..6) → overflow.
        let bus = vec![found(0, "drive", 0, 0, 8, 4)];
        let g = vec![gear(0, 2, 0, 0, GearMaster::Virtual)];
        let problems = validate_gear_offsets(&g, &bus);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("target_pos_offset 2+4 exceeds output PDI (4 B)"),
            "{problems:?}"
        );
    }

    #[test]
    fn gear_actual_offset_overflowing_input_pdi_is_reported() {
        // input PDI = 4 B; actual_pos_offset 4 needs [4..8) → overflow.
        let bus = vec![found(0, "drive", 0, 0, 4, 8)];
        let g = vec![gear(0, 0, 4, 0, GearMaster::Virtual)];
        let problems = validate_gear_offsets(&g, &bus);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("actual/status offsets exceed input PDI (4 B)"),
            "{problems:?}"
        );
    }

    #[test]
    fn gear_on_undiscovered_slave_is_reported() {
        let bus = vec![found(0, "only", 0, 0, 8, 8)];
        let g = vec![gear(3, 0, 0, 0, GearMaster::Virtual)];
        let problems = validate_gear_offsets(&g, &bus);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("gear follower slave_index 3 not on the discovered bus"),
            "{problems:?}"
        );
    }

    #[test]
    fn gear_master_axis_offset_overflowing_input_pdi_is_reported() {
        // follower fits; the Axis master's actual_pos_offset overflows.
        let bus = vec![
            found(0, "follower", 0, 0, 8, 8),
            found(1, "master", 0, 0, 4, 8),
        ];
        let g = vec![gear(
            0,
            0,
            0,
            4,
            GearMaster::Axis {
                slave_index: 1,
                actual_pos_offset: 4,
            },
        )];
        let problems = validate_gear_offsets(&g, &bus);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0]
                .contains("gear master slave 1 actual_pos_offset 4+4 exceeds input PDI (4 B)"),
            "{problems:?}"
        );
    }

    #[test]
    fn valid_gear_offsets_report_nothing() {
        // Follower output ≥ target+4, input ≥ max(actual+4, status+2); the
        // Axis master's actual_position fits its own input PDI.
        let bus = vec![
            found(0, "follower", 0, 0, 8, 8),
            found(1, "master", 0, 0, 8, 8),
        ];
        let g = vec![gear(
            0,
            0,
            0,
            4,
            GearMaster::Axis {
                slave_index: 1,
                actual_pos_offset: 0,
            },
        )];
        assert!(validate_gear_offsets(&g, &bus).is_empty());
    }

    // ---- channel shapes -----------------------------------------------------

    #[test]
    fn zero_bit_length_is_rejected() {
        let chans = vec![channel("z", 0, EthercatPdoDirection::TxPdo, 0, 0, 0)];
        let err = validate_channel_shapes(&chans).unwrap_err();
        assert!(err.contains("bit_length must be > 0"), "{err}");
    }

    #[test]
    fn misaligned_multibit_is_rejected_and_packed_bools_pass() {
        let bad = vec![channel("w", 0, EthercatPdoDirection::TxPdo, 0, 2, 16)];
        let err = validate_channel_shapes(&bad).unwrap_err();
        assert!(err.contains("non-byte-aligned"), "{err}");

        let good = vec![
            channel("bit5", 0, EthercatPdoDirection::TxPdo, 0, 5, 1),
            channel("word", 0, EthercatPdoDirection::TxPdo, 2, 0, 16),
        ];
        assert!(validate_channel_shapes(&good).is_ok());
    }

    /// The lane must be able to hold the entry. `map_data_type` takes the
    /// type from the ESI name and the length from its BitLen without
    /// cross-checking them, so a coupler whose vendor names an 8-bit entry
    /// `UDINT` reaches us as U32-at-8-bits; the reverse — a 32-bit entry on
    /// an 8-bit lane — would drop three bytes of a real measurement every
    /// cycle. Fail the connect, with the channel named.
    #[test]
    fn a_lane_too_narrow_for_the_entry_fails_the_connect() {
        let mut ch = channel("torque", 0, EthercatPdoDirection::TxPdo, 0, 0, 32);
        ch.data_type = EthercatDataType::U8;
        let err = validate_channel_shapes(&[ch]).unwrap_err();
        assert!(err.contains("torque"), "{err}");
        assert!(err.contains("dropped silently"), "{err}");

        // The wide-lane direction is legitimate and must still connect: a
        // 24-bit vendor entry read as U32 is the ESI fallback's normal output.
        let mut ok = channel("pos24", 0, EthercatPdoDirection::TxPdo, 0, 0, 24);
        ok.data_type = EthercatDataType::U32;
        assert!(validate_channel_shapes(&[ok]).is_ok());
    }

    // ---- init_sdo -----------------------------------------------------------

    #[test]
    fn init_sdo_accepts_in_range_values() {
        // 0x6060 = 8 (CSP), the canonical use.
        assert!(validate_init_sdo(&[slave_with_sdo(8, 8)]).is_ok());
        // Unsigned top of range and signed bottom both fit each width.
        assert!(validate_init_sdo(&[slave_with_sdo(255, 8)]).is_ok());
        assert!(validate_init_sdo(&[slave_with_sdo(-128, 8)]).is_ok());
        assert!(validate_init_sdo(&[slave_with_sdo(0xFFFF_FFFF, 32)]).is_ok());
        assert!(validate_init_sdo(&[slave_with_sdo(i32::MIN as i64, 32)]).is_ok());
        // No init_sdo at all is trivially fine.
        assert!(validate_init_sdo(&[slave(0, "io", 0, 0)]).is_ok());
    }

    #[test]
    fn init_sdo_rejects_bad_width_and_overflow() {
        assert!(validate_init_sdo(&[slave_with_sdo(8, 24)]).is_err());
        assert!(validate_init_sdo(&[slave_with_sdo(256, 8)]).is_err());
        assert!(validate_init_sdo(&[slave_with_sdo(-129, 8)]).is_err());
        assert!(validate_init_sdo(&[slave_with_sdo(0x1_0000_0000, 32)]).is_err());
    }
}
