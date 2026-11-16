use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fake {
    mode: u64,
    vendor: u32,
    fault: u64,
    calls: AtomicUsize,
    fail_mode: bool,
    wrong_width: bool,
    changed: bool,
    unavailable: bool,
    delay: bool,
}
impl Default for Fake {
    fn default() -> Self {
        Self {
            mode: 0,
            vendor: 0x10_0000,
            fault: 0,
            calls: AtomicUsize::new(0),
            fail_mode: false,
            wrong_width: false,
            changed: false,
            unavailable: false,
            delay: false,
        }
    }
}
#[async_trait]
impl EngineeringReader for Fake {
    async fn read(&self, _: &str, address: SdoAddress) -> Result<SdoValue, SdoError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.delay {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        if self.unavailable {
            return Err(SdoError::Unsupported);
        }
        let parameter = COMMON
            .iter()
            .chain(SV660N)
            .find(|p| p.index == address.index && p.sub == address.subindex)
            .unwrap();
        if self.fail_mode && parameter.key == "absolute_mode" {
            return Err(SdoError::Timeout);
        }
        let unsigned = match parameter.key {
            "absolute_mode" => self.mode,
            "encoder_fault" => self.fault,
            _ => 0,
        };
        let width = if self.wrong_width && parameter.key == "absolute_mode" {
            1
        } else {
            parameter.width as usize
        };
        Ok(SdoValue {
            address,
            bytes: unsigned.to_le_bytes()[..width].to_vec(),
            unsigned,
            signed: unsigned as i64,
            vendor_id: self.vendor,
            product_id: 0xc010d,
            revision: 1,
            serial: u32::from(self.changed && call > 3),
            sampled_at_unix_ms: 123,
        })
    }
}
fn target() -> EngineeringTarget {
    EngineeringTarget {
        device: "motion/bus".into(),
        slave: 1,
    }
}
async fn observe(fake: &Fake) -> EngineeringObservation {
    collect(
        fake,
        target(),
        "example".into(),
        "session".into(),
        "id".into(),
    )
    .await
}
fn has(o: &EngineeringObservation, code: &str) -> bool {
    o.findings.iter().any(|f| f.code == code)
}

#[tokio::test]
async fn position_modes_never_grant_machine_reference_or_retention_acceptance() {
    for (mode, expected) in [
        (0, "disabled_incremental"),
        (1, "configured_absolute_unverified"),
        (2, "configured_absolute_unverified"),
        (3, "configured_absolute_unverified"),
        (4, "single_turn_only"),
        (99, "unknown"),
    ] {
        let observation = observe(&Fake {
            mode,
            ..Fake::default()
        })
        .await;
        assert_eq!(observation.retention, expected);
        assert_eq!(observation.mechanical_reference, "not_verified");
        assert_eq!(observation.bus_continuity, "not_verified");
        assert!(observation.complete);
        assert!(has(&observation, "weak_device_identity"));
        if mode == 0 {
            assert!(has(&observation, "battery_not_assessed"));
        }
        if mode == 2 {
            assert!(has(&observation, "rotary_coordinate"));
        }
    }
}
#[tokio::test]
async fn wrong_vendor_gets_no_vendor_objects() {
    let fake = Fake {
        vendor: 55,
        ..Fake::default()
    };
    let o = observe(&fake).await;
    assert_eq!(fake.calls.load(Ordering::SeqCst), COMMON.len());
    assert_eq!(o.retention, "unknown");
    assert_eq!(o.profile, "generic_cia402");
}
#[tokio::test]
async fn partial_and_malformed_reads_preserve_unknowns() {
    for fake in [
        Fake {
            fail_mode: true,
            ..Fake::default()
        },
        Fake {
            wrong_width: true,
            ..Fake::default()
        },
    ] {
        let o = observe(&fake).await;
        assert!(!o.complete);
        assert_eq!(o.retention, "unknown");
        assert!(has(&o, "partial_observation"));
        assert!(o
            .parameters
            .iter()
            .find(|p| p.key == "absolute_mode")
            .unwrap()
            .error
            .is_some());
    }
}
#[tokio::test]
async fn identity_change_and_missing_device_do_not_look_like_incremental_zero() {
    let reader = Fake {
        changed: true,
        ..Fake::default()
    };
    let changed = observe(&reader).await;
    assert_eq!(
        reader.calls.load(Ordering::SeqCst),
        5,
        "stop the batch at the first identity mismatch"
    );
    assert!(!changed.complete);
    assert_eq!(changed.retention, "unknown");
    assert!(!changed.identity_consistent);
    assert!(has(&changed, "identity_changed"));
    let absent = observe(&Fake {
        unavailable: true,
        ..Fake::default()
    })
    .await;
    assert_eq!(absent.retention, "unknown");
    assert_eq!(absent.identity_strength, "unknown");
    assert!(has(&absent, "observation_unavailable"));
    assert!(!has(&absent, "identity_changed"));
}
#[tokio::test]
async fn encoder_fault_blocks_absolute_interpretation() {
    let o = observe(&Fake {
        mode: 1,
        fault: 128,
        ..Fake::default()
    })
    .await;
    assert_eq!(o.retention, "encoder_fault_present");
    assert_eq!(o.mechanical_reference, "not_verified");
}
#[tokio::test(start_paused = true)]
async fn timed_out_uploads_have_bounded_total_latency_and_remain_unknown() {
    let start = tokio::time::Instant::now();
    let o = observe(&Fake {
        delay: true,
        ..Fake::default()
    })
    .await;
    assert!(start.elapsed() <= BATCH_BUDGET);
    assert!(o
        .parameters
        .iter()
        .all(|p| p.error_kind.as_deref() == Some("timeout")));
    assert_eq!(o.retention, "unknown");
}
#[tokio::test]
async fn persisted_history_survives_restart_and_keeps_failed_latest_observation() {
    let temp = tempfile::tempdir().unwrap();
    let service = EngineeringService::default();
    let good = service
        .refresh_reader(
            temp.path().into(),
            "p".into(),
            target(),
            &Fake::default(),
            "a".into(),
        )
        .await
        .unwrap();
    assert!(good.saved);
    let bad = service
        .refresh_reader(
            temp.path().into(),
            "p".into(),
            target(),
            &Fake {
                unavailable: true,
                ..Fake::default()
            },
            "b".into(),
        )
        .await
        .unwrap();
    assert!(bad.saved);
    let restored = EngineeringService::default()
        .history(temp.path().into())
        .await
        .unwrap();
    assert_eq!(restored.observations.len(), 2);
    assert_eq!(restored.observations[0].retention, "disabled_incremental");
    assert_eq!(restored.observations[1].retention, "unknown");
    assert_ne!(restored.observations[0].id, restored.observations[1].id);
}
#[tokio::test]
async fn corruption_and_concurrent_refresh_are_rejected_before_bus_access() {
    let temp = tempfile::tempdir().unwrap();
    let service = EngineeringService::default();
    let fake = Fake::default();
    let guard = service.gate.lock().await;
    let busy = service
        .refresh_reader(temp.path().into(), "p".into(), target(), &fake, "s".into())
        .await
        .unwrap_err();
    assert_eq!(busy.0, 409);
    drop(guard);
    fs::write(temp.path().join("observations.json"), "broken").unwrap();
    let bad = service
        .refresh_reader(temp.path().into(), "p".into(), target(), &fake, "s".into())
        .await
        .unwrap_err();
    assert_eq!(bad.0, 503);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        fs::read_to_string(temp.path().join("observations.json")).unwrap(),
        "broken"
    );
}
#[tokio::test]
async fn archive_is_bounded_by_commit_order_not_wall_clock() {
    let temp = tempfile::tempdir().unwrap();
    let mut archive = Archive::open(temp.path().into()).unwrap();
    assert!(Archive::open(temp.path().into()).is_err());
    let mut o = observe(&Fake::default()).await;
    for sequence in 0..70 {
        o.id = sequence.to_string();
        o.started_at_unix_ms = 70 - sequence;
        archive.save(o.clone()).unwrap();
    }
    let h = load(temp.path()).unwrap();
    assert_eq!(h.observations.len(), CAPACITY);
    assert_eq!(h.observations[0].id, "6");
    assert_eq!(h.observations.last().unwrap().id, "69");
    assert_eq!(h.observations.last().unwrap().started_at_unix_ms, 1);
}
#[tokio::test]
async fn save_failure_returns_fresh_result_without_claiming_persistence() {
    struct BreakSave {
        fake: Fake,
        directory: PathBuf,
    }
    #[async_trait]
    impl EngineeringReader for BreakSave {
        async fn read(&self, device: &str, address: SdoAddress) -> Result<SdoValue, SdoError> {
            // Inject failure after the preflight archive check, before commit.
            fs::create_dir_all(self.directory.join("observations.json")).unwrap();
            self.fake.read(device, address).await
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let reader = BreakSave {
        fake: Fake::default(),
        directory: temp.path().into(),
    };
    let service = EngineeringService::default();
    let result = service
        .refresh_reader(
            temp.path().into(),
            "p".into(),
            target(),
            &reader,
            "s".into(),
        )
        .await
        .unwrap();
    assert!(!result.saved);
    assert!(result.storage_error.is_some());
    assert_eq!(result.observation.retention, "disabled_incremental");
}
