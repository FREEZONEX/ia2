// Included in runtime::tests to share the real scan-thread fixtures.

struct ExecutionProbe {
    writes: Arc<AtomicU64>,
    failsafe: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl IoDevice for ExecutionProbe {
    fn name(&self) -> &str {
        "probe"
    }
    async fn read_channel(&mut self, _: &str) -> Result<ChannelValue, IoError> {
        Ok(ChannelValue::I32(0))
    }
    async fn write_channel(&mut self, _: &str, _: ChannelValue) -> Result<(), IoError> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    async fn enter_failsafe(&mut self) -> Result<(), IoError> {
        self.failsafe.store(true, Ordering::Relaxed);
        Ok(())
    }
    async fn shutdown(&mut self) -> Result<(), IoError> {
        assert!(self.failsafe.load(Ordering::Relaxed));
        self.shutdown.store(true, Ordering::Relaxed);
        Ok(())
    }
}

#[tokio::test]
async fn infinite_scan_faults_drains_devices_and_does_not_publish_or_persist_partial_state() {
    let writes = Arc::new(AtomicU64::new(0));
    let fs = Arc::new(AtomicBool::new(false));
    let sd = Arc::new(AtomicBool::new(false));
    let device = ExecutionProbe {
        writes: writes.clone(),
        failsafe: fs.clone(),
        shutdown: sd.clone(),
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retain.json");
    let checkpoint = b"{\"schema\":2,\"scan_count\":7,\"vars\":{\"x\":42}}";
    std::fs::write(&path, checkpoint).unwrap();
    let (c, metadata) = crate::compile_with_metadata(
        "PROGRAM main VAR RETAIN x : DINT; END_VAR
         x := 99; WHILE TRUE DO x := x + 1; END_WHILE; END_PROGRAM",
    )
    .unwrap();
    let mut u = single_unit(c, 1);
    u.retain_vars = metadata.retain_vars;
    let mapping = Mapping {
        application: "main".into(),
        variable: "x".into(),
        device: "probe".into(),
        channel: "out".into(),
        direction: Direction::Output,
        unit: None,
        min: None,
        max: None,
        description: None,
    };
    let handle = spawn_units_inner(
        vec![u],
        DeviceSource::Prebuilt(vec![Box::new(device)]),
        vec![mapping],
        Some(path.clone()),
        WriteGovernance::default(),
    );
    let mut snapshots = handle.subscribe();
    join_scan_thread(&handle)
        .await
        .expect("budget exhaustion is a fault, not a Rust panic");
    assert!(handle
        .fault()
        .unwrap()
        .contains("VM execution budget exceeded in main"));
    assert!(handle.watchdog_tripped());
    assert!(fs.load(Ordering::Relaxed) && sd.load(Ordering::Relaxed));
    assert_eq!(writes.load(Ordering::Relaxed), 0, "no partial outputs");
    assert!(snapshots.try_recv().is_err(), "no partial snapshot");
    assert_eq!(
        std::fs::read(path).unwrap(),
        checkpoint,
        "keep the last checkpoint"
    );
}

#[tokio::test]
async fn stop_interrupts_an_infinite_scan_without_reporting_a_fault() {
    // A hung scan outlasts Stop's grace: it is discarded, so it writes no
    // output and leaves the last checkpoint alone, and Stop is not a fault.
    let writes = Arc::new(AtomicU64::new(0));
    let fs = Arc::new(AtomicBool::new(false));
    let sd = Arc::new(AtomicBool::new(false));
    let device = ExecutionProbe {
        writes: writes.clone(),
        failsafe: fs.clone(),
        shutdown: sd.clone(),
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retain.json");
    let checkpoint = b"{\"schema\":2,\"scan_count\":7,\"vars\":{\"x\":42}}";
    std::fs::write(&path, checkpoint).unwrap();
    let (c, metadata) = crate::compile_with_metadata(
        "PROGRAM main VAR RETAIN x : DINT; END_VAR
         x := 99; WHILE TRUE DO x := x + 1; END_WHILE; END_PROGRAM",
    )
    .unwrap();
    let mut u = single_unit(c, 1);
    u.retain_vars = metadata.retain_vars;
    let mapping = Mapping {
        application: "main".into(),
        variable: "x".into(),
        device: "probe".into(),
        channel: "out".into(),
        direction: Direction::Output,
        unit: None,
        min: None,
        max: None,
        description: None,
    };
    let handle = spawn_units_inner(
        vec![u],
        DeviceSource::Prebuilt(vec![Box::new(device)]),
        vec![mapping],
        Some(path.clone()),
        WriteGovernance::default(),
    );
    tokio::time::sleep(Duration::from_millis(10)).await;
    handle.stop();
    join_scan_thread(&handle).await.unwrap();
    if cfg!(debug_assertions) {
        assert_eq!(handle.fault(), None);
    } else {
        // An optimized VM can exhaust the 10M-opcode ceiling (~30 ms) before
        // Stop's grace expires; that is a different, legitimate way to end.
        assert!(handle
            .fault()
            .is_none_or(|f| f.contains("VM execution budget exceeded")));
    }
    assert!(fs.load(Ordering::Relaxed) && sd.load(Ordering::Relaxed));
    assert_eq!(writes.load(Ordering::Relaxed), 0, "no partial outputs");
    assert_eq!(
        std::fs::read(path).unwrap(),
        checkpoint,
        "a discarded scan keeps the last checkpoint"
    );
}

#[tokio::test]
async fn stop_during_back_to_back_scans_still_writes_the_final_retain_checkpoint() {
    // Scans as long as their cadence leave no idle gap, so every Stop lands
    // inside a scan. The scan in flight must finish and be checkpointed:
    // RETAIN_FLUSH_INTERVAL (5 s) would otherwise be the data-loss window.
    for stop_after_ms in [300u64, 317, 331] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retain.json");
        let (c, metadata) = crate::compile_with_metadata(
            "PROGRAM main VAR RETAIN n : DINT; END_VAR VAR i : DINT; acc : DINT; END_VAR
             n := n + 1; acc := 0; FOR i := 1 TO 50000 DO acc := acc + i; END_FOR; END_PROGRAM",
        )
        .unwrap();
        let mut u = single_unit(c, 1);
        u.retain_vars = metadata.retain_vars;
        let handle = spawn_units_inner(
            vec![u],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            Some(path.clone()),
            WriteGovernance::default(),
        );
        tokio::time::sleep(Duration::from_millis(stop_after_ms)).await;
        handle.stop();
        join_scan_thread(&handle).await.unwrap();
        assert_eq!(handle.fault(), None, "a plain Stop is not a fault");
        let state = crate::retain::load(&path)
            .unwrap()
            .expect("the final flush must have written the checkpoint");
        assert!(state.vars["n"] > 0, "the counter reflects completed scans");
    }
}
