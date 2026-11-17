use super::*;
use std::fs;
use std::time::Duration;

fn unit(instance: &str, declarations: &str) -> crate::ProgramUnit {
    let (container, metadata) = crate::compile_with_metadata(&format!(
        "PROGRAM main VAR RETAIN {declarations} END_VAR END_PROGRAM"
    ))
    .unwrap();
    crate::ProgramUnit {
        instance: instance.into(),
        task_name: "task".into(),
        interval_ms: 10,
        priority: 1,
        container,
        retain_vars: metadata.retain_vars,
    }
}
fn config(path: &std::path::Path) -> RetainConfig {
    RetainConfig {
        path: Some(path.into()),
        policy: RetainPolicy {
            project_id: Some("fixture-project".into()),
            sample_interval_ms: 20,
            ..Default::default()
        },
    }
}
fn seed(path: &std::path::Path, state: &RetainState) -> Vec<u8> {
    let bytes = serde_json::to_vec_pretty(state).unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, &bytes).unwrap();
    bytes
}
fn prepare(
    path: &std::path::Path,
    policy: &RetainPolicy,
    units: &[crate::ProgramUnit],
) -> storage::Prepared {
    storage::prepare(
        &RetainConfig {
            path: Some(path.into()),
            policy: policy.clone(),
        },
        &Plan::from_units(units, policy).unwrap(),
    )
}
#[test]
fn corrupt_future_and_legacy_are_distinct_and_protected_before_any_writer() {
    for (bytes, kind) in [
        (b"not JSON".as_slice(), RecoveryKind::Corrupt),
        (
            br#"{"schema":999,"vars":{"value":900}}"#.as_slice(),
            RecoveryKind::UnsupportedVersion,
        ),
        (
            br#"{"schema":2,"vars":{"value":1000}}"#.as_slice(),
            RecoveryKind::LegacyUnverified,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retain.json");
        fs::write(&path, bytes).unwrap();
        let prepared = prepare(
            &path,
            &config(&path).policy,
            &[unit("main_inst", "value : DINT := 42;")],
        );
        assert_eq!(prepared.result.kind, kind);
        assert!(!prepared.result.admitted);
        assert!(prepared.lease.is_none());
        assert!(prepared.restored.is_empty());
        assert_eq!(
            fs::read(prepared.result.archive_path.unwrap()).unwrap(),
            bytes
        );
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}
#[test]
fn required_missing_and_policy_cold_start_are_not_silent_restores() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retain.json");
    let units = [unit("main_inst", "value : DINT := 42;")];
    let mut c = config(&path);
    let first = prepare(&path, &c.policy, &units);
    assert_eq!(first.result.kind, RecoveryKind::FirstStart);
    assert!(first.result.admitted);
    drop(first);
    c.policy.required = vec!["main_inst.value".into()];
    assert!(!prepare(&path, &c.policy, &units).result.admitted);
    fs::write(&path, b"broken").unwrap();
    c.policy.allow_cold_start.push(ColdStartCondition::Corrupt);
    assert!(!prepare(&path, &c.policy, &units).result.admitted);
    c.policy.required.clear();
    let cold = prepare(&path, &c.policy, &units);
    assert_eq!(cold.result.kind, RecoveryKind::ColdStart);
    assert_eq!(cold.result.source_kind, RecoveryKind::Corrupt);
    assert!(cold.result.admitted);
    assert!(cold.restored.is_empty());
}
#[test]
fn identity_type_and_required_layout_rejections_are_all_or_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retain.json");
    let c = config(&path);
    let plan = Plan::from_units(
        &[unit("main_inst", "value : DINT := 1000; good : DINT := 7;")],
        &c.policy,
    )
    .unwrap();
    let target = Plan::from_units(
        &[unit("main_inst", "value : REAL := 42.0; good : DINT := 0;")],
        &c.policy,
    )
    .unwrap();
    let err = compatibility(&plan.initial, &target.initial, &c.policy).unwrap_err();
    assert_eq!(err.0, RecoveryKind::LayoutMismatch);
    let mut foreign = plan.initial.clone();
    foreign.identity.instance_id = "second-controller".into();
    assert_eq!(
        compatibility(&foreign, &plan.initial, &c.policy)
            .unwrap_err()
            .0,
        RecoveryKind::IdentityMismatch
    );
    foreign.identity = plan.initial.identity.clone();
    foreign.identity.project_id = "same-name-different-project".into();
    assert!(compatibility(&foreign, &plan.initial, &c.policy).is_err());
    let mut policy = c.policy.clone();
    policy.required = vec!["main_inst.good".into()];
    let mut old = plan.initial.clone();
    old.vars.remove("main_inst.good");
    assert!(compatibility(&old, &plan.initial, &policy).is_err());
}
#[test]
fn compatible_edits_upgrade_rollback_and_instance_changes_are_explained() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retain.json");
    let c = config(&path);
    let one = Plan::from_units(&[unit("a", "value : DINT := 1000;")], &c.policy).unwrap();
    let two = Plan::from_units(
        &[
            unit("a", "value : DINT := 42; added : INT := 8;"),
            unit("b", "value : DINT := 9;"),
        ],
        &c.policy,
    )
    .unwrap();
    let values = compatibility(&one.initial, &two.initial, &c.policy).unwrap();
    assert_eq!(values.len(), 1);
    assert_eq!(values["a.value"], 1000);
    assert!(!values.contains_key("b.value"));
    assert_eq!(
        compatibility(&two.initial, &one.initial, &c.policy)
            .unwrap()
            .len(),
        1
    );
    let mut code_only = one.initial.clone();
    code_only.content_sha256 = "another-release".into();
    assert!(compatibility(&code_only, &one.initial, &c.policy).is_ok());
}
#[test]
fn explicit_numeric_migration_and_cold_start_preserve_source_and_compare_hash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retain.json");
    let c = config(&path);
    let old = Plan::from_units(&[unit("main_inst", "value : DINT := 1000;")], &c.policy).unwrap();
    let bytes = seed(&path, &old.initial);
    let new_units = [unit("main_inst", "value : REAL := 42.0;")];
    let request = ResolveRequest {
        expected_source_sha256: digest(&bytes),
        migration_source_sha256: None,
        action: ResolutionAction::Migrate,
        initialize_required: false,
        attested_source_identity: None,
        rules: vec![MigrationRule {
            from: "main_inst.value".into(),
            to: "main_inst.value".into(),
            source_layout: SlotLayout::scalar("DINT").unwrap(),
            conversion: Conversion::DintToReal,
        }],
    };
    let result = resolve_stopped(&c, &new_units, &request).unwrap();
    assert!(result.receipt.durable);
    assert_eq!(fs::read(&result.archive_path).unwrap(), bytes);
    let state: RetainState = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        state.vars["main_inst.value"].raw().unwrap(),
        1000f32.to_bits() as u64
    );
    assert!(resolve_stopped(&c, &new_units, &request)
        .unwrap_err()
        .contains("changed"));
    let original = b"broken JSON";
    fs::write(&path, original).unwrap();
    for action in [ResolutionAction::ColdStart, ResolutionAction::Discard] {
        fs::write(&path, original).unwrap();
        let req = ResolveRequest {
            action,
            expected_source_sha256: digest(original),
            rules: vec![],
            ..request.clone()
        };
        assert_eq!(
            resolve_stopped(&c, &new_units, &req).unwrap().action,
            action
        );
    }
}
#[test]
fn legacy_migration_needs_identity_and_complete_explicit_type_mapping() {
    for (schema, value) in [
        (1, serde_json::json!(-17)),
        (2, serde_json::json!(u64::MAX)),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retain.json");
        let c = config(&path);
        let units = [unit("a", "value : LINT;")];
        let bytes =
            serde_json::to_vec(&serde_json::json!({"schema":schema,"vars":{"value":value}}))
                .unwrap();
        fs::write(&path, &bytes).unwrap();
        let mut request = ResolveRequest {
            expected_source_sha256: digest(&bytes),
            migration_source_sha256: None,
            action: ResolutionAction::Migrate,
            initialize_required: false,
            attested_source_identity: None,
            rules: vec![MigrationRule {
                from: "value".into(),
                to: "a.value".into(),
                source_layout: SlotLayout::scalar("LINT").unwrap(),
                conversion: Conversion::Exact,
            }],
        };
        assert!(resolve_stopped(&c, &units, &request).is_err());
        request.attested_source_identity = Some(
            Plan::from_units(&units, &c.policy)
                .unwrap()
                .initial
                .identity,
        );
        resolve_stopped(&c, &units, &request).unwrap();
        let saved: RetainState = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            saved.vars["a.value"].raw().unwrap(),
            if schema == 1 { -17i64 as u64 } else { u64::MAX }
        );
    }
}
#[test]
fn protection_failure_writer_contention_and_external_edits_never_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retain.json");
    let c = config(&path);
    let units = [unit("a", "value : DINT;")];
    let plan = Plan::from_units(&units, &c.policy).unwrap();
    fs::write(&path, b"original").unwrap();
    fs::write(dir.path().join("retain-archive"), b"blocked").unwrap();
    let result = prepare(&path, &c.policy, &units);
    assert_eq!(result.result.kind, RecoveryKind::ProtectionFailed);
    assert_eq!(fs::read(&path).unwrap(), b"original");
    fs::remove_file(dir.path().join("retain-archive")).unwrap();
    let mut lease = storage::Lease::acquire(&path).unwrap();
    lease.read().unwrap();
    assert!(storage::Lease::acquire(&path).is_err());
    fs::write(&path, b"external").unwrap();
    assert!(lease.save(&plan.initial).unwrap_err().contains("changed"));
    assert_eq!(fs::read(&path).unwrap(), b"external");
    drop(lease);
    assert!(storage::Lease::acquire(&path).is_ok());
}
#[test]
fn unsupported_shapes_are_rejected_instead_of_restoring_pointers() {
    for declarations in [
        "value : STRING[20] := 'abc';",
        "value : ARRAY[0..2] OF DINT;",
        "value : TIME := T#1s;",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let c = config(&dir.path().join("retain.json"));
        assert!(Plan::from_units(&[unit("a", declarations)], &c.policy)
            .unwrap_err()
            .contains("unsupported"));
    }
}
#[tokio::test]
async fn real_scan_restores_lossless_64_bit_and_real_across_single_and_multiple_programs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retain.json");
    let c = config(&path);
    let first = unit("a", "wide : ULINT; real_value : REAL; signed : LINT;");
    let mut old = Plan::from_units(&[first], &c.policy).unwrap().initial;
    old.vars.get_mut("a.wide").unwrap().bits = u64::MAX.to_string();
    old.vars.get_mut("a.real_value").unwrap().bits = (12.5f32.to_bits() as u64).to_string();
    old.vars.get_mut("a.signed").unwrap().bits = (i64::MIN as u64).to_string();
    seed(&path, &old);
    for extra in [true, false] {
        let mut units = vec![unit("a", "wide : ULINT; real_value : REAL; signed : LINT;")];
        if extra {
            units.push(unit("b", "wide : ULINT := 7;"));
        }
        let handle =
            crate::spawn_units_with_retain(units, vec![], vec![], c.clone(), Default::default());
        let mut rx = handle.subscribe();
        let snapshot = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot
                .vars
                .iter()
                .find(|v| v.name.ends_with("real_value"))
                .unwrap()
                .bits,
            12.5f32.to_bits() as u64
        );
        handle.shutdown().await;
        assert_eq!(
            handle.retain().status().persistence.final_save,
            FinalSave::Durable
        );
        let saved: RetainState = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.vars["a.wide"].raw().unwrap(), u64::MAX);
        assert_eq!(saved.vars["a.signed"].raw().unwrap(), i64::MIN as u64);
        if extra {
            assert_eq!(saved.vars["b.wide"].raw().unwrap(), 7);
            assert!(
                saved.data_generation > saved.scan_count,
                "every PROGRAM scan advances persistence generation"
            );
        }
        let exported = handle.retain().checkpoint().unwrap();
        assert_eq!(
            digest(exported.checkpoint_json.as_bytes()),
            exported.receipt.sha256
        );
        assert_eq!(
            exported.checkpoint_json.as_bytes(),
            fs::read(&path).unwrap()
        );
    }
}
#[tokio::test]
async fn paused_write_advances_generation_and_is_saved_without_another_scan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retain.json");
    let mut c = config(&path);
    c.policy.sample_interval_ms = 25;
    let handle = crate::spawn_units_with_retain(
        vec![unit("a", "value : DINT := 42;")],
        vec![],
        vec![],
        c,
        Default::default(),
    );
    handle.pause();
    let control = handle.retain();
    let before = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(checkpoint) = control.checkpoint() {
                break checkpoint;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    handle.write_variable("value", 123).await.unwrap();
    let status = control.status();
    assert!(status.persistence.data_generation > before.receipt.data_generation);
    if let Some(age) = status.persistence.pending_age_ms {
        assert!(age < 100);
    }
    let after = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(checkpoint) = control.checkpoint() {
                if checkpoint.state.vars["a.value"].bits == "123" {
                    break checkpoint;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(after.receipt.scan_count, before.receipt.scan_count);
    assert!(after.receipt.data_generation > before.receipt.data_generation);
    handle.shutdown().await;
    assert_eq!(control.status().persistence.final_save, FinalSave::Durable);
}

#[tokio::test]
async fn type_rejection_latches_before_any_scan_and_stays_protected_on_stop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retain.json");
    let c = config(&path);
    let old = Plan::from_units(&[unit("a", "value : DINT := 1000;")], &c.policy).unwrap();
    let bytes = seed(&path, &old.initial);
    let handle = crate::spawn_units_with_retain(
        vec![unit("a", "value : REAL := 42.0;")],
        vec![],
        vec![],
        c,
        Default::default(),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(handle.watchdog_tripped());
    assert!(handle
        .fault()
        .unwrap()
        .contains("requires explicit migration"));
    assert!(handle.retain().checkpoint().is_none());
    handle.shutdown().await;
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[tokio::test]
async fn archived_checkpoint_can_restore_removed_keys_on_rollback_with_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retain.json");
    let c = config(&path);
    let old = Plan::from_units(
        &[unit("a", "value : DINT := 1000; removed : LINT := 77;")],
        &c.policy,
    )
    .unwrap();
    let original = seed(&path, &old.initial);
    let handle = crate::spawn_units_with_retain(
        vec![unit("a", "value : DINT;")],
        vec![],
        vec![],
        c.clone(),
        Default::default(),
    );
    let mut rx = handle.subscribe();
    tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    handle.shutdown().await;
    let current = fs::read(&path).unwrap();
    let request = ResolveRequest {
        expected_source_sha256: digest(&current),
        migration_source_sha256: Some(digest(&original)),
        action: ResolutionAction::Migrate,
        initialize_required: false,
        attested_source_identity: None,
        rules: vec![
            MigrationRule {
                from: "a.value".into(),
                to: "a.value".into(),
                source_layout: SlotLayout::scalar("DINT").unwrap(),
                conversion: Conversion::Exact,
            },
            MigrationRule {
                from: "a.removed".into(),
                to: "a.removed".into(),
                source_layout: SlotLayout::scalar("LINT").unwrap(),
                conversion: Conversion::Exact,
            },
        ],
    };
    let result =
        resolve_stopped(&c, &[unit("a", "value : DINT; removed : LINT;")], &request).unwrap();
    assert_eq!(fs::read(result.preserved_current_path).unwrap(), current);
    assert_eq!(fs::read(result.archive_path).unwrap(), original);
    let handle = crate::spawn_units_with_retain(
        vec![unit("a", "value : DINT; removed : LINT;")],
        vec![],
        vec![],
        c,
        Default::default(),
    );
    let mut rx = handle.subscribe();
    let snapshot = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        snapshot
            .vars
            .iter()
            .find(|v| v.name == "removed")
            .unwrap()
            .bits,
        77
    );
    assert_eq!(
        handle.retain().status().recovery.kind,
        RecoveryKind::Migrated
    );
    handle.shutdown().await;
    let saved: RetainState = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let origin = saved.resolution.unwrap();
    assert_eq!(origin.source_sha256, digest(&original));
    assert_eq!(origin.replaced_source_sha256, Some(digest(&current)));
}
