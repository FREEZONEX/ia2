// Pulse ownership regressions, included in runtime::tests.

// Keep Tokio's test clock from auto-advancing while the real scan thread
// answers commands. Only the explicit advances below may expire pulses.
fn hold_pulse_clock() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    })
}

async fn pulse_value_after(handle: &ProgramHandle, ms: u64) -> i32 {
    // Start newly spawned pulse timers at the current test-clock time.
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_millis(ms)).await;
    // Let ready reset tasks enqueue their commands before the barrier.
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    handle.write_variable("barrier", 1).await.unwrap();
    let snap = handle.subscribe().recv().await.unwrap();
    snap.vars.iter().find(|v| v.name == "x").unwrap().bits as u32 as i32
}

fn pulse_test_handle() -> ProgramHandle {
    let prog = crate::compile(
        "PROGRAM main VAR x : INT; y : INT; barrier : INT; END_VAR y := x; END_PROGRAM",
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

#[tokio::test(start_paused = true)]
async fn pulse_old_reset_does_not_cut_short_a_new_pulse() {
    let clock = hold_pulse_clock();
    let handle = pulse_test_handle();
    handle.pause(); // Pausing must not suppress a still-owned pulse reset.
    crate::monitor::write_with_pulse(&handle, "x", 1, Some(100))
        .await
        .unwrap();
    assert_eq!(pulse_value_after(&handle, 50).await, 1);
    // Same slot via a qualified alias: ownership follows resolved identity.
    crate::monitor::write_with_pulse(&handle, "main.x", 1, Some(200))
        .await
        .unwrap();
    let at_old_deadline = pulse_value_after(&handle, 50).await;
    let at_new_deadline = pulse_value_after(&handle, 150).await;
    clock.abort();
    handle.shutdown().await;
    assert_eq!(
        at_old_deadline, 1,
        "old reset must not truncate the newer pulse"
    );
    assert_eq!(at_new_deadline, 0, "the current pulse must still clear");
}

#[tokio::test(start_paused = true)]
async fn pulse_old_reset_does_not_overwrite_a_plain_write() {
    let clock = hold_pulse_clock();
    let handle = pulse_test_handle();
    crate::monitor::write_with_pulse(&handle, "x", 1, Some(100))
        .await
        .unwrap();
    assert_eq!(pulse_value_after(&handle, 50).await, 1);
    handle.write_variable("main.x", 7).await.unwrap();
    let after_reset = pulse_value_after(&handle, 50).await;
    clock.abort();
    handle.shutdown().await;
    assert_eq!(after_reset, 7, "a later plain write owns the value");
}

#[tokio::test]
async fn pulse_reset_and_later_commands_follow_queue_order() {
    let handle = pulse_test_handle();
    let (_, old) = handle.start_pulse("x", 1).await.unwrap();
    // Queue both before either acknowledgement. Even writing the SAME
    // value supersedes ownership; comparing the VM's value is insufficient.
    let (write, reset) =
        tokio::join!(biased; handle.write_variable("MAIN.x", 1), handle.reset_pulse(old));
    assert_eq!(write.unwrap().value, 1);
    assert_eq!(reset.unwrap(), None);

    // The opposite queue order still permits the later write to win.
    let (_, current) = handle.start_pulse("x", 1).await.unwrap();
    let (reset, write) =
        tokio::join!(biased; handle.reset_pulse(current), handle.write_variable("x", 7));
    assert_eq!(reset.unwrap().unwrap().value, 0);
    assert_eq!(write.unwrap().value, 7);

    let (_, forced) = handle.start_pulse("x", 1).await.unwrap();
    handle.force_variable("MAIN.x", 9).await.unwrap();
    handle.unforce_variable("MAIN.x").await.unwrap();
    assert_eq!(handle.reset_pulse(forced).await.unwrap(), None);
    handle.shutdown().await;
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
    let (_, first) = handle
        .start_pulse("x", 50.0_f32.to_bits() as i32)
        .await
        .unwrap();
    let (_, second) = handle
        .start_pulse("second.x", 50.0_f32.to_bits() as i32)
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
    assert_eq!(handle.reset_pulse(first).await.unwrap().unwrap().value, 0);
    assert_eq!(handle.reset_pulse(second).await.unwrap().unwrap().value, 0);
    handle.shutdown().await;
}

#[tokio::test]
async fn pulse_reset_after_stop_cannot_write_into_a_replacement_run() {
    let old = pulse_test_handle();
    let (_, token) = old.start_pulse("x", 1).await.unwrap();
    old.shutdown().await;
    let replacement = pulse_test_handle();
    replacement.write_variable("x", 7).await.unwrap();
    assert!(matches!(
        old.reset_pulse(token).await,
        Err(RuntimeWriteError::Disconnected)
    ));
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
    let (initial, token) = handle.start_pulse("x", 1).await.unwrap();
    assert_eq!(initial.undelivered_device, None);
    handle.subscribe().recv().await.unwrap();
    assert_eq!(wire.load(Ordering::Relaxed), 1);
    healthy.store(false, Ordering::Relaxed);
    let reset = handle.reset_pulse(token).await.unwrap().unwrap();
    assert_eq!(reset.value, 0);
    assert_eq!(reset.undelivered_device.as_deref(), Some("pulse_bus"));
    let snap = handle.subscribe().recv().await.unwrap();
    assert_eq!(var_value(&snap, "x"), "0");
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

/// An HTTP server drops a handler's future when its client disconnects. A
/// pulse write already queued to the scan loop still lands then, so its reset
/// must not have been tied to the dropped caller: the value would stay
/// latched, which is what `pulse_ms` exists to prevent.
#[tokio::test]
async fn pulse_reset_survives_a_caller_dropped_while_the_write_is_queued() {
    let handle = pulse_test_handle();
    // Hold the scan loop so the write is still queued when its caller goes.
    handle.inject_scan_stall(300, 1).await.unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    let t0 = tokio::time::Instant::now();
    let dropped = tokio::time::timeout(
        Duration::from_millis(20),
        crate::monitor::write_with_pulse(&handle, "x", 1, Some(800)),
    )
    .await;
    assert!(dropped.is_err(), "the write must still be queued when dropped");

    tokio::time::sleep_until(t0 + Duration::from_millis(500)).await;
    let landed = handle.subscribe().recv().await.unwrap();
    // The write landed about 240 ms after t0 and is held 800 ms from then.
    tokio::time::sleep_until(t0 + Duration::from_millis(1500)).await;
    let after = handle.subscribe().recv().await.unwrap();
    handle.shutdown().await;
    assert_eq!(var_value(&landed, "x"), "1", "the queued write still lands");
    assert_eq!(var_value(&after, "x"), "0", "and its reset still runs");
}

/// Parks the scan thread in the input phase when armed, until released: a
/// scan gate that does not depend on how long anything sleeps.
struct ScanGateDevice {
    armed: Arc<AtomicBool>,
    parked: tokio::sync::mpsc::UnboundedSender<()>,
    release: std::sync::mpsc::Receiver<()>,
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
    async fn write_channel(&mut self, _: &str, _: ChannelValue) -> Result<(), IoError> {
        Ok(())
    }
}

struct ScanGate {
    armed: Arc<AtomicBool>,
    parked: tokio::sync::mpsc::UnboundedReceiver<()>,
    release: std::sync::mpsc::Sender<()>,
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
        })]),
        vec![project::Mapping {
            application: "main".into(),
            variable: "g".into(),
            direction: Direction::Input,
            device: "gate".into(),
            channel: "in".into(),
            unit: None,
            min: None,
            max: None,
            description: None,
        }],
        None,
        WriteGovernance::default(),
    );
    (
        handle,
        ScanGate {
            armed,
            parked: parked_rx,
            release: release_tx,
        },
    )
}

/// Poll once: `write_with_pulse` queues its write in that first poll, so
/// Pending means the write is in the scan loop's queue and not yet applied.
async fn queue_once<F: std::future::Future>(fut: &mut std::pin::Pin<&mut F>) -> bool {
    std::future::poll_fn(|cx| std::task::Poll::Ready(fut.as_mut().poll(cx).is_pending())).await
}

/// The write waits `queued_ms` behind a parked scan, then lands; from that
/// moment the value must be held for the whole pulse and then cleared.
async fn pulse_held_from_landing(pulse_ms: u64, queued_ms: u64) {
    let clock = hold_pulse_clock();
    let (handle, mut gate) = gated_pulse_handle();
    gate.park().await;
    let pulse = crate::monitor::write_with_pulse(&handle, "x", 1, Some(pulse_ms as u32));
    let mut pulse = std::pin::pin!(pulse);
    assert!(queue_once(&mut pulse).await, "the write is still queued");
    tokio::time::advance(Duration::from_millis(queued_ms)).await;
    gate.release();
    assert_eq!(pulse.await.unwrap().value, 1);
    // The pulse time starts now, when the write has landed.
    let before_deadline = pulse_value_after(&handle, pulse_ms - 1).await;
    let after_deadline = pulse_value_after(&handle, 2).await;
    clock.abort();
    handle.shutdown().await;
    assert_eq!(
        before_deadline, 1,
        "{queued_ms} ms in the queue must not shorten a {pulse_ms} ms pulse"
    );
    assert_eq!(after_deadline, 0, "the pulse clears at its deadline");
}

#[tokio::test(start_paused = true)]
async fn pulse_queued_longer_than_its_time_is_held_for_all_of_it() {
    pulse_held_from_landing(100, 300).await;
}

#[tokio::test(start_paused = true)]
async fn pulse_queued_for_part_of_its_time_is_held_for_all_of_it() {
    pulse_held_from_landing(800, 300).await;
}

/// The caller goes while its pulse write is queued: the write lands, the
/// value is held for the whole pulse from then, and the reset still runs.
#[tokio::test(start_paused = true)]
async fn pulse_dropped_while_queued_is_held_for_its_time_and_then_cleared() {
    let clock = hold_pulse_clock();
    let (handle, mut gate) = gated_pulse_handle();
    gate.park().await;
    {
        let pulse = crate::monitor::write_with_pulse(&handle, "x", 1, Some(100));
        let mut pulse = std::pin::pin!(pulse);
        assert!(queue_once(&mut pulse).await, "the write is still queued");
    } // dropped here, still queued
    tokio::time::advance(Duration::from_millis(300)).await;
    // Nobody awaits the write any more. Park again at the next input phase:
    // the scan drains its command queue before that, so once parked there the
    // write has landed, and the test clock has not moved since.
    gate.arm();
    gate.release();
    gate.parked().await;
    gate.release();
    let before_deadline = pulse_value_after(&handle, 99).await;
    // The reset task was woken by the write's ack, before the scan parked;
    // even if it ran late, its deadline is at most one pulse after the check.
    let cleared = pulse_value_after(&handle, 100).await;
    clock.abort();
    handle.shutdown().await;
    assert_eq!(before_deadline, 1, "the queue time must not shorten the pulse");
    assert_eq!(cleared, 0, "and the reset still runs");
}
