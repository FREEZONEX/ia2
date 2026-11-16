// Pulse ownership regressions, included in runtime::tests.

async fn wait_for_pulse_bits(rx: &mut broadcast::Receiver<VarSnapshot>, name: &str, bits: u64) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let snapshot = rx.recv().await.unwrap();
            if snapshot.vars.iter().any(|var| var.name == name && var.bits == bits) {
                break;
            }
        }
    })
    .await
    .expect("pulse state observed in a snapshot");
}

fn pulse_test_handle() -> ProgramHandle {
    let prog = crate::compile(
        "PROGRAM main VAR x : INT; y : INT; held : INT; END_VAR y := x; END_PROGRAM",
    )
    .unwrap();
    spawn_units_inner(
        vec![single_unit(prog, 10)],
        DeviceSource::Prebuilt(Vec::new()),
        Vec::new(),
        None,
        WriteGovernance::default(),
    )
}

#[tokio::test]
async fn pulse_old_reset_does_not_cut_short_a_new_pulse() {
    let handle = pulse_test_handle();
    handle.pause(); // Pausing must not suppress a still-owned pulse reset.
    crate::monitor::write_with_pulse(&handle, "x", 1, Some(200))
        .await
        .unwrap();
    // Same slot via a qualified alias: ownership follows resolved identity.
    crate::monitor::write_with_pulse(&handle, "main.x", 2, Some(750))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let after_old_deadline = handle.subscribe().recv().await.unwrap();
    wait_for_pulse_bits(&mut handle.subscribe(), "x", 0).await;
    handle.shutdown().await;
    assert_eq!(
        var_value(&after_old_deadline, "x"),
        "2",
        "old reset must not truncate the newer pulse"
    );
}

#[tokio::test]
async fn pulse_old_reset_does_not_overwrite_an_accepted_write_or_force() {
    let handle = pulse_test_handle();
    handle.write_pulse("x", 1, 200).await.unwrap();
    // An accepted write of the same value also supersedes the pulse.
    assert_eq!(handle.write_variable("MAIN.x", 1).await.unwrap().value, 1);
    handle.write_pulse("held", 1, 200).await.unwrap();
    handle.force_variable("main.held", 9).await.unwrap();
    wait_for_pulse_bits(&mut handle.subscribe(), "held", 9).await;
    handle.unforce_variable("main.held").await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let after = handle.subscribe().recv().await.unwrap();
    handle.shutdown().await;
    assert_eq!(var_value(&after, "x"), "1");
    assert_eq!(var_value(&after, "held"), "9");
}

#[tokio::test]
async fn pulse_ownership_is_per_unit_and_rejected_writes_do_not_cancel_it() {
    let prog = || {
        crate::compile("PROGRAM main VAR x : REAL; y : REAL; END_VAR y := x; END_PROGRAM")
            .unwrap()
    };
    let handle = spawn_units_inner(
        vec![unit("first", prog(), 10, 1), unit("second", prog(), 10, 1)],
        DeviceSource::Prebuilt(Vec::new()),
        Vec::new(),
        None,
        governance(
            WriteMode::Allowlist,
            vec![project::WriteRule {
                variable: "x".into(),
                min: Some(1.0),
                max: Some(100.0),
            }],
        ),
    );
    handle
        .write_pulse("x", 50.0_f32.to_bits() as i32, 200)
        .await
        .unwrap();
    handle
        .write_pulse("second.x", 50.0_f32.to_bits() as i32, 400)
        .await
        .unwrap();
    assert!(matches!(
        handle
            .write_variable("first.x", f32::NAN.to_bits() as i32)
            .await,
        Err(RuntimeWriteError::GovernanceDenied(_))
    ));
    // Neither the denied write nor the other unit's pulse cancels this
    // reset. Both resets bypass min=1 and must really write zero.
    let mut snapshots = handle.subscribe();
    wait_for_pulse_bits(&mut snapshots, "first.x", 0).await;
    wait_for_pulse_bits(&mut snapshots, "second.x", 0).await;
    handle.shutdown().await;
}

#[tokio::test]
async fn pulse_reset_after_stop_cannot_write_into_a_replacement_run() {
    let old = pulse_test_handle();
    old.write_pulse("x", 1, 200).await.unwrap();
    old.shutdown().await;
    let replacement = pulse_test_handle();
    replacement.write_variable("x", 7).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let snap = replacement.subscribe().recv().await.unwrap();
    replacement.shutdown().await;
    assert_eq!(var_value(&snap, "x"), "7");
}

struct PulseOutputDevice {
    healthy: Arc<AtomicBool>,
    wire: Arc<std::sync::atomic::AtomicI32>,
}

#[async_trait::async_trait]
impl IoDevice for PulseOutputDevice {
    fn name(&self) -> &str {
        "pulse_bus"
    }
    fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }
    async fn read_channel(&mut self, _: &str) -> Result<ChannelValue, IoError> {
        Ok(ChannelValue::I32(0))
    }
    async fn write_channel(&mut self, _: &str, value: ChannelValue) -> Result<(), IoError> {
        if self.is_healthy() {
            if let ChannelValue::I32(value) = value {
                self.wire.store(value, Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

#[tokio::test]
async fn pulse_reset_reports_undelivered_and_zero_flushes_after_reconnect() {
    let healthy = Arc::new(AtomicBool::new(true));
    let wire = Arc::new(std::sync::atomic::AtomicI32::new(-1));
    let handle = spawn_units_inner(
        vec![single_unit(
            crate::compile("PROGRAM main VAR x : INT; y : INT; END_VAR y := x; END_PROGRAM")
                .unwrap(),
            10,
        )],
        DeviceSource::Prebuilt(vec![Box::new(PulseOutputDevice {
            healthy: healthy.clone(),
            wire: wire.clone(),
        })]),
        vec![project::Mapping {
            application: "main".into(),
            variable: "x".into(),
            direction: Direction::Output,
            device: "pulse_bus".into(),
            channel: "coil".into(),
            unit: None,
            min: None,
            max: None,
            description: None,
        }],
        None,
        WriteGovernance::default(),
    );
    let mut snapshots = handle.subscribe();
    let initial = handle.write_pulse("x", 1, 400).await.unwrap();
    assert_eq!(initial.undelivered_device, None);
    wait_for_pulse_bits(&mut snapshots, "x", 1).await;
    assert_eq!(wire.load(Ordering::Relaxed), 1);
    healthy.store(false, Ordering::Relaxed);
    wait_for_pulse_bits(&mut snapshots, "x", 0).await;
    assert_eq!(
        wire.load(Ordering::Relaxed),
        1,
        "no claim of delivery during the outage"
    );
    healthy.store(true, Ordering::Relaxed);
    handle.subscribe().recv().await.unwrap();
    assert_eq!(
        wire.load(Ordering::Relaxed),
        0,
        "normal recovery flushes the current zero"
    );
    handle.shutdown().await;
}

/// Parks the scan thread in the input phase when armed, until released: a
/// scan gate that does not depend on how long anything sleeps.
struct ScanGateDevice {
    armed: Arc<AtomicBool>,
    parked: tokio::sync::mpsc::UnboundedSender<()>,
    release: std::sync::mpsc::Receiver<()>,
    wire: Arc<std::sync::atomic::AtomicI32>,
}

#[async_trait::async_trait]
impl IoDevice for ScanGateDevice {
    fn name(&self) -> &str {
        "gate"
    }
    fn is_healthy(&self) -> bool {
        true
    }
    async fn read_channel(&mut self, _: &str) -> Result<ChannelValue, IoError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            let _ = self.parked.send(());
            let _ = self.release.recv();
        }
        Ok(ChannelValue::I32(0))
    }
    async fn write_channel(&mut self, _: &str, value: ChannelValue) -> Result<(), IoError> {
        self.wire.store(value.to_i32(), Ordering::Relaxed);
        Ok(())
    }
}

struct ScanGate {
    armed: Arc<AtomicBool>,
    parked: tokio::sync::mpsc::UnboundedReceiver<()>,
    release: std::sync::mpsc::Sender<()>,
    wire: Arc<std::sync::atomic::AtomicI32>,
}

impl ScanGate {
    /// Returns once the scan thread is parked: no command is applied until
    /// `release`.
    async fn park(&mut self) {
        self.arm();
        self.parked().await;
    }
    /// Park at the next input phase.
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
    async fn parked(&mut self) {
        self.parked.recv().await.unwrap();
    }
    fn release(&self) {
        self.release.send(()).unwrap();
    }
}

fn gated_pulse_handle() -> (ProgramHandle, ScanGate) {
    let armed = Arc::new(AtomicBool::new(false));
    let wire = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let (parked_tx, parked_rx) = tokio::sync::mpsc::unbounded_channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let handle = spawn_units_inner(
        vec![single_unit(
            crate::compile(
                "PROGRAM main VAR x : INT; y : INT; barrier : INT; g : INT; END_VAR y := x; END_PROGRAM",
            )
            .unwrap(),
            10,
        )],
        DeviceSource::Prebuilt(vec![Box::new(ScanGateDevice {
            armed: armed.clone(),
            parked: parked_tx,
            release: release_rx,
            wire: wire.clone(),
        })]),
        vec![
            project::Mapping {
                application: "main".into(),
                variable: "g".into(),
                direction: Direction::Input,
                device: "gate".into(),
                channel: "in".into(),
                unit: None,
                min: None,
                max: None,
                description: None,
            },
            project::Mapping {
                application: "main".into(),
                variable: "x".into(),
                direction: Direction::Output,
                device: "gate".into(),
                channel: "out".into(),
                unit: None,
                min: None,
                max: None,
                description: None,
            },
        ],
        None,
        WriteGovernance::default(),
    );
    (
        handle,
        ScanGate {
            armed,
            parked: parked_rx,
            release: release_tx,
            wire,
        },
    )
}

/// Poll once: `write_with_pulse` queues its write in that first poll, so
/// Pending means the write is in the scan loop's queue and not yet applied.
async fn queue_once<F: std::future::Future>(fut: &mut std::pin::Pin<&mut F>) -> bool {
    std::future::poll_fn(|cx| std::task::Poll::Ready(fut.as_mut().poll(cx).is_pending())).await
}

/// The caller goes while its pulse write is queued: the write lands, the
/// value is held for the whole pulse from then, and the reset still runs.
#[tokio::test]
async fn pulse_dropped_while_queued_is_held_for_its_time_and_then_cleared() {
    const PULSE_MS: u32 = 400;
    let (handle, mut gate) = gated_pulse_handle();
    gate.park().await;
    {
        let pulse = crate::monitor::write_with_pulse(&handle, "x", 1, Some(PULSE_MS));
        let mut pulse = std::pin::pin!(pulse);
        assert!(queue_once(&mut pulse).await, "the write is still queued");
    } // dropped here, still queued
    // Queue for longer than the pulse itself, then observe the full pulse
    // on the VM/output path despite the missing caller.
    tokio::time::sleep(Duration::from_millis(u64::from(PULSE_MS) + 50)).await;
    let mut snapshots = handle.subscribe();
    gate.release();
    wait_for_pulse_bits(&mut snapshots, "x", 1).await;
    assert_eq!(gate.wire.load(Ordering::Relaxed), 1);
    wait_for_pulse_bits(&mut snapshots, "x", 0).await;
    assert_eq!(gate.wire.load(Ordering::Relaxed), 0);
    handle.shutdown().await;
}

#[tokio::test]
async fn expired_pulse_reaches_the_output_while_the_ordinary_queue_is_backlogged() {
    const PULSE_MS: u32 = 400;
    let (handle, mut gate) = gated_pulse_handle();
    let mut snapshots = handle.subscribe();
    handle.write_pulse("x", 1, PULSE_MS).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(u64::from(PULSE_MS));
    wait_for_pulse_bits(&mut snapshots, "x", 1).await;
    gate.park().await;
    assert_eq!(gate.wire.load(Ordering::Relaxed), 1);

    let replies: Vec<_> = (0..COMMAND_QUEUE_CAPACITY)
        .map(|value| handle.queue_write("barrier", value as i32, None).unwrap())
        .collect();
    tokio::time::sleep_until(deadline).await;
    // Let the parked scan finish, then let one complete scan service the
    // due reset. Stop in the next input phase before the queue can drain.
    gate.arm();
    gate.release();
    gate.parked().await;
    gate.arm();
    gate.release();
    gate.parked().await;
    assert_eq!(gate.wire.load(Ordering::Relaxed), 0);
    assert!(handle.cmd_tx.capacity() < COMMAND_QUEUE_CAPACITY);
    gate.release();
    for reply in replies {
        reply.await.unwrap();
    }
    handle.shutdown().await;
}

#[tokio::test]
async fn a_full_command_queue_cannot_park_the_scan_or_delay_output_inhibition() {
    let (handle, mut gate) = gated_pulse_handle();
    gate.park().await;
    let mut replies = Vec::new();
    for value in 0..COMMAND_QUEUE_CAPACITY {
        replies.push(handle.queue_write("x", value as i32, None).unwrap());
    }
    assert!(matches!(handle.queue_write("x", 0, None), Err(RuntimeWriteError::Busy)));
    // Park the next input too. It must be reached before the backlog is empty.
    gate.arm();
    gate.release();
    tokio::time::timeout(Duration::from_secs(1), gate.parked()).await.unwrap();
    let remaining = handle.cmd_tx.max_capacity() - handle.cmd_tx.capacity();
    assert!(remaining >= COMMAND_QUEUE_CAPACITY - COMMANDS_PER_ROUND);
    for _ in remaining..COMMAND_QUEUE_CAPACITY {
        replies.push(handle.queue_write("x", 7, None).unwrap());
    }

    let latch = handle.latch_outputs_off("priority request");
    let mut latch = std::pin::pin!(latch);
    assert!(queue_once(&mut latch).await);
    gate.release();
    assert!(tokio::time::timeout(Duration::from_secs(1), latch).await.unwrap().unwrap());
    assert!(handle.watchdog_tripped());
    assert!(handle.cmd_tx.capacity() < COMMAND_QUEUE_CAPACITY);
    for reply in replies {
        reply.await.unwrap();
    }
    handle.shutdown().await;
}
