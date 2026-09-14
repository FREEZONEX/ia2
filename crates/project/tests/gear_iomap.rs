//! Gear channels are virtual adapter routes, not fabricated PDO entries.
use project::{
    validate_iomap, Device, Direction, EthercatConfig, EthercatGear, IoMap, IomapIssueSeverity,
    Mapping, ProtocolConfig,
};

// Deliberately independent of the implementation's channel catalog.
const PARAMETERS: [&str; 7] = [
    "gear_engage",
    "ratio_num",
    "ratio_den",
    "ratio_step",
    "phase_ofs",
    "master_vel",
    "gear_max_travel",
];
const FEEDBACK: [&str; 2] = ["gear_engaged", "gear_trip"];

fn gear() -> EthercatGear {
    toml::from_str(
        r#"
        slave_index = 0
        target_pos_offset = 0
        actual_pos_offset = 0
        status_word_offset = 4
        master = { kind = "virtual" }
        "#,
    )
    .unwrap()
}

fn device(gears: Vec<EthercatGear>) -> Device {
    Device {
        name: "ec".into(),
        config: ProtocolConfig::Ethercat(EthercatConfig {
            nic: "_sim".into(),
            cycle_us: 2_000,
            bringup: Default::default(),
            dc_sync: Default::default(),
            dc_static_sync_iterations: 0,
            slaves: vec![],
            channels: vec![],
            gear: gears,
        }),
    }
}

fn mapping(channel: &str, direction: Direction) -> Mapping {
    Mapping {
        application: "main".into(),
        variable: channel.into(),
        device: "ec".into(),
        channel: channel.into(),
        direction,
        unit: None,
        min: None,
        max: None,
        description: None,
    }
}

fn check(device: Device, mappings: Vec<Mapping>) -> Vec<project::IomapIssue> {
    validate_iomap(&IoMap { mappings }, &[device])
}

/// A real PDO entry to sit beside the gear routes. Only name and direction
/// reach the assertions here; the type stays non-Bool so the boolean range
/// warning never fires by accident.
fn pdo(name: &str, direction: project::EthercatPdoDirection) -> project::EthercatChannel {
    project::EthercatChannel {
        name: name.into(),
        slave_index: 0,
        direction,
        pdo_index: 0x6041,
        sub_index: 0,
        bit_length: 16,
        data_type: project::EthercatDataType::U16,
        pdi_byte_offset: 0,
        pdi_bit_offset: 0,
    }
}

/// Mutable access to a device built by [`device`].
fn ethercat(device: &mut Device) -> &mut EthercatConfig {
    let ProtocolConfig::Ethercat(cfg) = &mut device.config else {
        unreachable!()
    };
    cfg
}

#[test]
fn all_nine_default_gear_routes_validate_without_pdo_entries() {
    let mappings = PARAMETERS
        .iter()
        .map(|n| mapping(n, Direction::Output))
        .chain(FEEDBACK.iter().map(|n| mapping(n, Direction::Input)))
        .collect();
    assert!(check(device(vec![gear()]), mappings).is_empty());
}

#[test]
fn every_parameter_echo_can_be_read() {
    let mappings = PARAMETERS
        .iter()
        .map(|n| mapping(n, Direction::Input))
        .collect();
    assert!(check(device(vec![gear()]), mappings).is_empty());
}

#[test]
fn feedback_outputs_are_rejected_as_read_only_not_unknown() {
    for name in FEEDBACK {
        let issues = check(device(vec![gear()]), vec![mapping(name, Direction::Output)]);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].severity, IomapIssueSeverity::Error);
        assert!(issues[0].message.contains("read-only gear"), "{issues:?}");
    }
}

#[test]
fn custom_name_replaces_default_and_stays_device_scoped() {
    let mut custom = gear();
    custom.ratio_num_channel = "ratio_custom".into();
    let ec = device(vec![custom]);
    assert!(check(ec.clone(), vec![mapping("ratio_custom", Direction::Output)]).is_empty());
    assert_eq!(
        check(ec, vec![mapping("ratio_num", Direction::Output)]).len(),
        1
    );
    assert_eq!(
        check(
            device(vec![]),
            vec![mapping("ratio_custom", Direction::Output)]
        )
        .len(),
        1
    );
}

#[test]
fn unknown_and_unrouted_ratio_apply_names_still_fail() {
    for name in ["typo", "gear_ratio_apply", "gear_ratio_ack"] {
        let issues = check(device(vec![gear()]), vec![mapping(name, Direction::Input)]);
        assert_eq!(issues.len(), 1, "{name}: {issues:?}");
        assert!(issues[0].message.contains("no channel"), "{issues:?}");
    }
}

#[test]
fn boolean_gear_metadata_warns_but_numeric_metadata_is_allowed() {
    for name in ["gear_engage", "gear_engaged", "gear_trip", "ratio_num"] {
        let mut m = mapping(name, Direction::Input);
        m.min = Some(0.0);
        m.max = Some(1.0);
        let issues = check(device(vec![gear()]), vec![m]);
        if name == "ratio_num" {
            assert!(issues.is_empty(), "{issues:?}");
        } else {
            assert_eq!(issues.len(), 1, "{issues:?}");
            assert_eq!(issues[0].severity, IomapIssueSeverity::Warning);
        }
    }
}

#[test]
fn duplicate_writers_remain_an_error() {
    let first = mapping("ratio_num", Direction::Output);
    let mut second = first.clone();
    second.variable = "other_writer".into();
    let issues = check(device(vec![gear()]), vec![first, second]);
    // Existing behavior highlights both writers, not just the second one.
    assert_eq!(issues.len(), 2, "{issues:?}");
    for issue in issues {
        assert_eq!(issue.severity, IomapIssueSeverity::Error);
        assert!(issue.message.contains("conflicting writers"));
    }
}

#[test]
fn duplicate_gear_names_fail_even_when_the_mapping_itself_is_unambiguous() {
    let mut second = gear();
    second.ratio_num_channel = "second_ratio".into();
    let issues = check(
        device(vec![gear(), second]),
        vec![mapping("second_ratio", Direction::Output)],
    );
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(
        issues[0].message.contains("more than one gear"),
        "{issues:?}"
    );
}

fn rename_channels(gear: &mut EthercatGear, prefix: &str) {
    for name in [
        &mut gear.engage_channel,
        &mut gear.ratio_num_channel,
        &mut gear.ratio_den_channel,
        &mut gear.ratio_step_channel,
        &mut gear.phase_channel,
        &mut gear.master_vel_channel,
        &mut gear.max_travel_channel,
        &mut gear.engaged_channel,
        &mut gear.trip_channel,
        &mut gear.ratio_apply_channel,
        &mut gear.ratio_ack_channel,
    ] {
        *name = format!("{prefix}{name}");
    }
}

#[test]
fn independently_named_gears_validate_together() {
    let mut second = gear();
    rename_channels(&mut second, "second_");
    let ec = device(vec![gear(), second]);
    let mappings = vec![
        mapping("ratio_num", Direction::Output),
        mapping("second_ratio_num", Direction::Output),
        mapping("second_gear_trip", Direction::Input),
    ];
    assert!(check(ec, mappings).is_empty());
}

#[test]
fn same_gear_names_on_different_devices_do_not_collide() {
    let mut second = device(vec![gear()]);
    second.name = "other_ec".into();
    let first_mapping = mapping("ratio_num", Direction::Output);
    let mut second_mapping = first_mapping.clone();
    second_mapping.device = "other_ec".into();
    second_mapping.variable = "second_ratio".into();
    let issues = validate_iomap(
        &IoMap {
            mappings: vec![first_mapping, second_mapping],
        },
        &[device(vec![gear()]), second],
    );
    assert!(issues.is_empty(), "{issues:?}");
}

#[test]
fn a_gear_name_cannot_shadow_a_pdo() {
    let mut ec = device(vec![gear()]);
    ethercat(&mut ec)
        .channels
        .push(pdo("ratio_num", project::EthercatPdoDirection::RxPdo));
    let issues = check(ec, vec![mapping("ratio_num", Direction::Output)]);
    assert_eq!(issues.len(), 1);
    assert!(
        issues[0].message.contains("collides with a PDO"),
        "{issues:?}"
    );
}

#[test]
fn intra_gear_and_reserved_name_collisions_are_rejected() {
    for reserved in [false, true] {
        let mut g = gear();
        if reserved {
            g.ratio_apply_channel = "ratio_num".into();
        } else {
            g.trip_channel = "ratio_num".into();
        }
        let issues = check(
            device(vec![g]),
            vec![mapping("ratio_num", Direction::Output)],
        );
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].message.contains("more than one gear"),
            "{issues:?}"
        );
    }
}

#[test]
fn a_parameter_writer_and_a_separate_echo_reader_are_compatible() {
    let writer = mapping("ratio_num", Direction::Output);
    let mut reader = mapping("ratio_num", Direction::Input);
    reader.variable = "ratio_echo".into();
    assert!(check(device(vec![gear()]), vec![writer, reader]).is_empty());
}

#[test]
fn gear_routes_do_not_hide_pdo_direction_checks() {
    let mut ec = device(vec![gear()]);
    ethercat(&mut ec)
        .channels
        .push(pdo("statusword", project::EthercatPdoDirection::TxPdo));
    assert!(check(ec.clone(), vec![mapping("statusword", Direction::Input)]).is_empty());
    let issues = check(ec, vec![mapping("statusword", Direction::Output)]);
    assert_eq!(issues.len(), 1);
    assert!(issues[0].message.contains("TxPDO"), "{issues:?}");
}

#[test]
fn a_device_collision_highlights_each_affected_mapping_row() {
    let mut g = gear();
    g.trip_channel = "ratio_num".into();
    let issues = check(
        device(vec![g]),
        vec![
            mapping("ratio_num", Direction::Output),
            mapping("gear_engaged", Direction::Input),
        ],
    );
    assert_eq!(issues.len(), 2);
    for (index, issue) in issues.iter().enumerate() {
        assert_eq!(issue.mapping_index, index);
        assert_eq!(issue.severity, IomapIssueSeverity::Error);
        assert!(issue.message.contains("more than one gear"));
    }
    assert_ne!(issues[0].message, issues[1].message);
}

#[test]
fn a_gear_collision_also_fails_a_plain_pdo_row_on_that_device() {
    let mut ec = device(vec![gear()]);
    let cfg = ethercat(&mut ec);
    cfg.gear[0].trip_channel = "ratio_num".into();
    cfg.channels
        .push(pdo("statusword", project::EthercatPdoDirection::TxPdo));
    // The device is unroutable as configured, so a row that never touches a
    // gear channel fails too — and reports the collision, not a channel error.
    let issues = check(ec, vec![mapping("statusword", Direction::Input)]);
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(
        issues[0].message.contains("more than one gear"),
        "{issues:?}"
    );
}

#[test]
fn a_broken_device_does_not_poison_a_clean_one() {
    let mut broken = device(vec![gear()]);
    broken.name = "broken_ec".into();
    ethercat(&mut broken).gear[0].trip_channel = "ratio_num".into();

    let mut broken_row = mapping("ratio_num", Direction::Output);
    broken_row.device = "broken_ec".into();
    broken_row.variable = "broken_ratio".into();

    let issues = validate_iomap(
        &IoMap {
            mappings: vec![broken_row, mapping("gear_engaged", Direction::Input)],
        },
        &[broken, device(vec![gear()])],
    );
    // The per-device cache must not spread one device's verdict to the other.
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].mapping_index, 0);
    assert!(
        issues[0].message.contains("more than one gear"),
        "{issues:?}"
    );
}

#[test]
fn unreferenced_devices_have_no_iomap_row_to_diagnose() {
    let mut g = gear();
    g.trip_channel = "ratio_num".into();
    // The separate device-name check rejects this config at connect time.
    assert!(
        project::validate_gear_channel_names(std::slice::from_ref(&g), &Default::default())
            .is_err()
    );
    // Do not invent mapping #0 (or an out-of-bounds index) for a device
    // that the map does not reference. Full device preflight is separate.
    assert!(check(device(vec![g]), vec![]).is_empty());
}
