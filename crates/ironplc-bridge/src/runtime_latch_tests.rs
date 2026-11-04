// Included in runtime::tests: exercise the production command and scan paths
// using local adapters only, without a transport or a physical output.

struct LatchProbeState {
    calls: AtomicU64,
    writes: AtomicU64,
    output: std::sync::atomic::AtomicI32,
    reject: AtomicBool,
    panic: AtomicBool,
}

impl LatchProbeState {
    fn new(reject: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            output: std::sync::atomic::AtomicI32::new(123),
            reject: AtomicBool::new(reject),
            panic: AtomicBool::new(false),
        })
    }
}

struct LatchProbeDevice {
    name: String,
    state: Arc<LatchProbeState>,
    first_attempt_gate: Option<Arc<tokio::sync::Notify>>,
}

#[async_trait::async_trait]
impl IoDevice for LatchProbeDevice {
    fn name(&self) -> &str {
        &self.name
    }

    async fn read_channel(&mut self, _: &str) -> Result<ChannelValue, IoError> {
        Ok(ChannelValue::I32(0))
    }

    async fn write_channel(&mut self, _: &str, value: ChannelValue) -> Result<(), IoError> {
        self.state.output.store(value.to_i32(), Ordering::Relaxed);
        self.state.writes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn enter_failsafe(&mut self) -> Result<(), IoError> {
        let previous = self.state.calls.fetch_add(1, Ordering::Relaxed);
        if previous == 0 {
            if let Some(gate) = &self.first_attempt_gate {
                gate.notified().await;
            }
        }
        assert!(
            !self.state.panic.load(Ordering::Relaxed),
            "injected adapter panic"
        );
        if self.state.reject.load(Ordering::Relaxed) {
            return Err(IoError::Protocol("safe-value write rejected".into()));
        }
        // A configured safe value need not be zero.
        self.state.output.store(-7, Ordering::Relaxed);
        Ok(())
    }
}

fn latch_probe(name: &str, state: &Arc<LatchProbeState>) -> Box<dyn IoDevice> {
    Box::new(LatchProbeDevice {
        name: name.to_string(),
        state: state.clone(),
        first_attempt_gate: None,
    })
}

fn latch_test_handle(devices: Vec<Box<dyn IoDevice>>) -> ProgramHandle {
    let mappings = devices
        .iter()
        .map(|device| Mapping {
            application: "main".into(),
            variable: "out".into(),
            device: device.name().into(),
            channel: "out".into(),
            direction: Direction::Output,
            unit: None,
            min: None,
            max: None,
            description: None,
        })
        .collect();
    let container =
        crate::compile("PROGRAM main VAR out : INT := 123; END_VAR out := 123; END_PROGRAM")
            .unwrap();
    spawn_units_inner(
        vec![single_unit(container, 10)],
        DeviceSource::Prebuilt(devices),
        mappings,
        None,
        WriteGovernance::default(),
    )
}

async fn wait_latch_condition(condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("scan condition reached");
}

#[tokio::test]
async fn latch_failure_is_unconfirmed_inhibits_writes_and_explicit_retry_only_retries_failures() {
    let rejected = LatchProbeState::new(true);
    let accepted = LatchProbeState::new(false);
    let handle = latch_test_handle(vec![
        latch_probe("rejected", &rejected),
        latch_probe("accepted", &accepted),
    ]);
    wait_latch_condition(|| rejected.writes.load(Ordering::Relaxed) > 0).await;
    assert_eq!(handle.failsafe_status(), None);
    let result = handle.latch_outputs_off("first cause").await;
    let errors = match result {
        Err(OutputLatchError::FailsafeUnconfirmed { errors }) => errors,
        other => panic!("protocol rejection must reach the caller: {other:?}"),
    };
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].device, "rejected");
    assert!(errors[0]
        .error
        .contains("protocol: safe-value write rejected"));
    assert_eq!(rejected.output.load(Ordering::Relaxed), 123);
    assert!(handle.watchdog_tripped());
    assert_eq!(handle.latch_reason().as_deref(), Some("first cause"));
    assert_eq!(
        handle.failsafe_status().unwrap(),
        FailsafeStatus {
            pending: Vec::new(),
            acknowledged: vec!["accepted".into()],
            unconfirmed: errors,
        }
    );

    let writes = rejected.writes.load(Ordering::Relaxed);
    handle.force_variable("out", 456).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(rejected.writes.load(Ordering::Relaxed), writes);
    assert_eq!(rejected.output.load(Ordering::Relaxed), 123);
    assert_eq!(
        rejected.calls.load(Ordering::Relaxed),
        1,
        "no automatic retry"
    );

    rejected.reject.store(false, Ordering::Relaxed);
    assert!(!handle.latch_outputs_off("later cause").await.unwrap());
    assert_eq!(accepted.calls.load(Ordering::Relaxed), 1);
    assert_eq!(rejected.calls.load(Ordering::Relaxed), 2);
    assert_eq!(rejected.output.load(Ordering::Relaxed), -7);
    assert_eq!(handle.latch_reason().as_deref(), Some("first cause"));
    let status = handle.failsafe_status().unwrap();
    assert!(status.pending.is_empty() && status.unconfirmed.is_empty());
    assert_eq!(status.acknowledged, vec!["accepted", "rejected"]);
    assert!(!handle.latch_outputs_off("third cause").await.unwrap());
    assert_eq!(accepted.calls.load(Ordering::Relaxed), 1);
    assert_eq!(rejected.calls.load(Ordering::Relaxed), 2);
    assert!(handle.watchdog_tripped());
    handle.shutdown().await;
}

#[tokio::test]
async fn latch_isolates_adapter_panic_and_attempts_remaining_devices() {
    let panicking = LatchProbeState::new(false);
    panicking.panic.store(true, Ordering::Relaxed);
    let accepted = LatchProbeState::new(false);
    let handle = latch_test_handle(vec![
        latch_probe("panicking", &panicking),
        latch_probe("accepted", &accepted),
    ]);
    let errors = match handle.latch_outputs_off("panic cause").await {
        Err(OutputLatchError::FailsafeUnconfirmed { errors }) => errors,
        other => panic!("adapter panic must not disappear: {other:?}"),
    };
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].device, "panicking");
    assert!(errors[0]
        .error
        .contains("adapter panicked: injected adapter panic"));
    assert_eq!(accepted.output.load(Ordering::Relaxed), -7);
    assert_eq!(accepted.calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        handle.failsafe_status().unwrap().acknowledged,
        vec!["accepted"]
    );
    assert!(handle.watchdog_tripped());
    assert_eq!(
        handle.fault(),
        None,
        "isolated adapter panic does not kill the scan"
    );
    panicking.panic.store(false, Ordering::Relaxed);
    handle.shutdown().await;
    assert!(handle.failsafe_status().unwrap().unconfirmed.is_empty());
}

#[tokio::test]
async fn latch_publishes_all_pending_devices_and_inhibit_before_blocked_adapter_attempt() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let blocked = LatchProbeState::new(false);
    let accepted = LatchProbeState::new(false);
    let handle = latch_test_handle(vec![
        Box::new(LatchProbeDevice {
            name: "blocked".into(),
            state: blocked,
            first_attempt_gate: Some(gate.clone()),
        }),
        latch_probe("accepted", &accepted),
    ]);
    let request = tokio::spawn({
        let handle = handle.clone();
        async move { handle.latch_outputs_off("blocked cause").await }
    });
    wait_latch_condition(|| handle.watchdog_tripped()).await;
    assert!(!request.is_finished(), "adapter response is still pending");
    assert_eq!(handle.latch_reason().as_deref(), Some("blocked cause"));
    assert_eq!(
        handle.failsafe_status().unwrap(),
        FailsafeStatus {
            pending: vec!["blocked".into(), "accepted".into()],
            acknowledged: Vec::new(),
            unconfirmed: Vec::new(),
        }
    );
    assert_eq!(accepted.calls.load(Ordering::Relaxed), 0);
    gate.notify_one();
    assert!(request.await.unwrap().unwrap());
    assert!(handle.failsafe_status().unwrap().pending.is_empty());
    handle.shutdown().await;
}

#[tokio::test]
async fn watchdog_failure_stays_visible_until_an_explicit_retry_succeeds() {
    let rejected = LatchProbeState::new(true);
    let handle = latch_test_handle(vec![latch_probe("rejected", &rejected)]);
    handle
        .inject_scan_stall(25, WATCHDOG_OVERRUN_THRESHOLD)
        .await
        .unwrap();
    wait_latch_condition(|| {
        handle
            .failsafe_status()
            .is_some_and(|status| !status.unconfirmed.is_empty())
    })
    .await;
    assert!(handle.watchdog_tripped());
    let reason = handle.latch_reason().unwrap();
    assert!(reason.starts_with("scan watchdog:"));
    let writes = rejected.writes.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(rejected.calls.load(Ordering::Relaxed), 1);
    assert_eq!(rejected.writes.load(Ordering::Relaxed), writes);
    rejected.reject.store(false, Ordering::Relaxed);
    assert!(!handle.latch_outputs_off("explicit retry").await.unwrap());
    assert_eq!(handle.latch_reason().as_deref(), Some(reason.as_str()));
    assert!(handle.failsafe_status().unwrap().unconfirmed.is_empty());
    handle.shutdown().await;
}

#[tokio::test]
async fn one_device_attempt_preserves_other_devices_failures_and_keeps_failed_retry_visible() {
    let shared = Arc::new(std::sync::Mutex::new(Some(FailsafeStatus {
        pending: Vec::new(),
        acknowledged: vec!["accepted".into()],
        unconfirmed: vec![DeviceFailsafeError {
            device: "rejected".into(),
            error: "original rejection".into(),
        }],
    })));
    begin_failsafe_attempts(&shared, ["rejected", "late"]);
    let pending = shared.lock().unwrap().clone().unwrap();
    assert_eq!(pending.unconfirmed[0].error, "original rejection");
    assert_eq!(pending.pending, vec!["rejected", "late"]);
    let late = LatchProbeState::new(false);
    let mut device = latch_probe("late", &late);
    attempt_device_failsafe(device.as_mut(), &shared)
        .await
        .unwrap();
    let status = shared.lock().unwrap().clone().unwrap();
    assert_eq!(status.pending, vec!["rejected"]);
    assert_eq!(status.unconfirmed, pending.unconfirmed);
    assert_eq!(status.acknowledged, vec!["accepted", "late"]);
}

#[tokio::test]
async fn shutdown_retains_unsuccessful_safe_value_results() {
    let rejected = LatchProbeState::new(true);
    let handle = latch_test_handle(vec![latch_probe("rejected", &rejected)]);
    assert!(matches!(
        handle.latch_outputs_off("stop cause").await,
        Err(OutputLatchError::FailsafeUnconfirmed { .. })
    ));
    handle.shutdown().await;
    let status = handle.failsafe_status().unwrap();
    assert!(status.pending.is_empty() && status.acknowledged.is_empty());
    assert_eq!(status.unconfirmed[0].device, "rejected");
    assert_eq!(
        rejected.calls.load(Ordering::Relaxed),
        2,
        "cleanup retries once"
    );
    assert_eq!(rejected.output.load(Ordering::Relaxed), 123);
    assert!(handle.watchdog_tripped());
}

#[tokio::test]
async fn execution_budget_teardown_reports_pending_before_safe_value_completion() {
    let rejected = LatchProbeState::new(true);
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut container = crate::compile(
        "PROGRAM main VAR x : DINT; END_VAR WHILE TRUE DO x := x + 1; END_WHILE; END_PROGRAM",
    )
    .unwrap();
    for task in &mut container.task_table.tasks {
        task.watchdog_us = 5_000;
    }
    let handle = spawn_units_inner(
        vec![single_unit(container, 1)],
        DeviceSource::Prebuilt(vec![Box::new(LatchProbeDevice {
            name: "rejected".into(),
            state: rejected.clone(),
            first_attempt_gate: Some(gate.clone()),
        })]),
        Vec::new(),
        None,
        WriteGovernance::default(),
    );
    wait_latch_condition(|| rejected.calls.load(Ordering::Relaxed) == 1).await;
    assert!(handle.watchdog_tripped());
    assert!(handle
        .latch_reason()
        .unwrap()
        .starts_with("VM execution budget exceeded"));
    assert_eq!(handle.failsafe_status().unwrap().pending, vec!["rejected"]);
    gate.notify_one();
    join_scan_thread(&handle).await.unwrap();
    let status = handle.failsafe_status().unwrap();
    assert!(status.pending.is_empty());
    assert_eq!(status.unconfirmed[0].device, "rejected");
    assert_eq!(rejected.writes.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn explicit_latch_without_connected_devices_records_an_empty_completed_batch() {
    let handle = latch_test_handle(Vec::new());
    assert_eq!(handle.failsafe_status(), None);
    assert!(handle.latch_outputs_off("empty batch").await.unwrap());
    assert_eq!(handle.failsafe_status(), Some(FailsafeStatus::default()));
    assert!(!handle.latch_outputs_off("later batch").await.unwrap());
    assert_eq!(handle.latch_reason().as_deref(), Some("empty batch"));
    handle.shutdown().await;
}
