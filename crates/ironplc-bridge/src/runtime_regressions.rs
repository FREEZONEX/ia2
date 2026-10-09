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

fn retry_spec(addr: std::net::SocketAddr) -> DeviceSpec {
    DeviceSpec {
        name: "retry".into(),
        config: ProtocolConfig::Modbus(project::ModbusConfig {
            transport: project::ModbusTransport::Tcp(project::ModbusTcpParams {
                host: addr.ip().to_string(),
                port: addr.port(),
            }),
            slave_id: 1,
            poll_interval_ms: 20,
            timeout_ms: Some(100),
            reconnect_backoff_ms: None,
            channels: Vec::new(),
        }),
    }
}

#[tokio::test]
async fn scan_panic_joins_the_reconnect_worker_without_an_operator_stop() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let spec = retry_spec(listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let (d, fs, sd) = panic_device("probe", &armed, false);
    let handle = spawn_units_inner(
        vec![single_unit(trivial_container(), 1)],
        DeviceSource::PrebuiltWithRetries(vec![Box::new(d)], vec![spec]),
        Vec::new(),
        None,
        WriteGovernance::default(),
    );
    let mut rx = handle.subscribe();
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    armed.store(true, Ordering::Relaxed);
    assert!(
        join_scan_thread(&handle).await.is_err(),
        "original scan panic is preserved"
    );
    assert!(
        !handle.stop.load(Ordering::Relaxed),
        "no operator Stop masks a leak"
    );
    assert!(handle.fault().unwrap().contains("injected scan-loop panic"));
    assert!(fs.load(Ordering::Relaxed) && sd.load(Ordering::Relaxed));
    // Crossing the worker's original 1s backoff catches the old detached
    // worker even if the scan thread's join alone appeared successful.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn closing_reconnect_handoff_retains_queued_devices_and_rejects_late_delivery() {
    let state = ReconnectState::default();
    let (a, _) = MockDevice::named("queued");
    assert!(state.deliver(Box::new(a)).is_ok());
    let queued = state.finish();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].name(), "queued");
    let (b, _) = MockDevice::named("late");
    assert!(state.deliver(Box::new(b)).is_err());
    assert!(state.take_devices().is_empty());
}

#[tokio::test]
async fn reconnect_runtime_keeps_delivered_poll_tasks_alive_until_drained() {
    // A real loopback adapter is delivered while another spec still fails.
    // Closing the handoff must stop retries but keep its poll task alive for
    // the subsequent failsafe write, until the scan signals `drained`.
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = reserved.local_addr().unwrap();
    drop(reserved);
    let server = tokio::spawn(iomap_modbus::run_demo_slave(
        addr,
        iomap_modbus::DemoSlave::new(),
    ));
    let state = Arc::new(ReconnectState::default());
    let worker_state = state.clone();
    let absent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let absent_addr = absent.local_addr().unwrap();
    drop(absent);
    let worker = std::thread::spawn(move || {
        reconnect_worker(
            vec![retry_spec(addr), retry_spec(absent_addr)],
            Arc::new(std::sync::Mutex::new(Vec::new())),
            Arc::new(AtomicBool::new(false)),
            worker_state,
        )
    });
    let mut delivered = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let devices = state.take_devices();
            if !devices.is_empty() {
                break devices;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(state.finish().is_empty());
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(!worker.is_finished(), "I/O runtime must outlive failsafe");
    delivered[0].enter_failsafe().await.unwrap();
    delivered[0].shutdown().await.unwrap();
    state.drained.store(true, Ordering::Release);
    let outcome = join_within(worker, Duration::from_secs(2))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome,
        ReconnectOutcome::default(),
        "a worker that only delivered devices has no late cleanup to report"
    );
    server.abort();
}

#[tokio::test]
async fn cancelled_shutdown_keeps_the_thread_joinable() {
    let handle = spawn_units_inner(
        vec![single_unit(trivial_container(), 1)],
        DeviceSource::Prebuilt(Vec::new()),
        Vec::new(),
        None,
        WriteGovernance::default(),
    );
    let cancelled = tokio::time::timeout(Duration::from_millis(1), handle.shutdown()).await;
    assert!(
        cancelled.is_err(),
        "the 50ms failsafe grace is still in flight"
    );
    assert!(
        handle.thread.lock().unwrap().is_some(),
        "cancellation must not consume the join handle"
    );
    tokio::time::timeout(Duration::from_secs(2), handle.shutdown())
        .await
        .unwrap();
    assert!(handle.thread.lock().unwrap().is_none());
}

// --- reconnect worker: bounded wait and truthful outcome -------------------

#[tokio::test]
async fn collecting_a_clean_reconnect_worker_reports_nothing() {
    let thread = std::thread::spawn(ReconnectOutcome::default);
    let end = collect_reconnect_worker(thread, Duration::from_secs(2)).await;
    assert_eq!(end, ReconnectEnd::default());
}

#[tokio::test]
async fn collecting_a_worker_counts_its_late_cleanup_failures() {
    let thread = std::thread::spawn(|| ReconnectOutcome {
        late_failsafe_failed: 1,
        late_shutdown_failed: 2,
    });
    let end = collect_reconnect_worker(thread, Duration::from_secs(2)).await;
    assert_eq!(
        end,
        ReconnectEnd {
            failed: 3,
            unconfirmed: false
        }
    );
}

#[tokio::test]
async fn collecting_a_panicked_worker_counts_it_as_a_failure() {
    let thread = std::thread::spawn(|| -> ReconnectOutcome { panic!("injected worker panic") });
    let end = collect_reconnect_worker(thread, Duration::from_secs(2)).await;
    assert_eq!(
        end,
        ReconnectEnd {
            failed: 1,
            unconfirmed: false
        }
    );
}

#[tokio::test]
async fn a_worker_slower_than_the_grace_is_left_running_and_reported_unconfirmed() {
    let release = Arc::new(AtomicBool::new(false));
    let hold = release.clone();
    let thread = std::thread::spawn(move || {
        while !hold.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
        ReconnectOutcome::default()
    });
    let started = Instant::now();
    let end = collect_reconnect_worker(thread, Duration::from_millis(60)).await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the wait is bounded by the grace, not by the worker"
    );
    assert_eq!(
        end,
        ReconnectEnd {
            failed: 0,
            unconfirmed: true
        }
    );
    release.store(true, Ordering::Relaxed); // let the detached thread end
}

/// Accepts a TCP connection, signals `accepted`, holds it for `delay`, then
/// serves exactly ONE Modbus request (the adapter's connect-time seed poll)
/// through to `backend` and drops the connection. The adapter's connect is
/// therefore in flight for `delay` after the signal, and the device is gone
/// again by the time anything else is asked of it.
async fn delayed_one_shot_modbus_proxy(
    listener: tokio::net::TcpListener,
    backend: std::net::SocketAddr,
    delay: Duration,
    accepted: Arc<AtomicBool>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let Ok((mut client, _)) = listener.accept().await else {
            return;
        };
        accepted.store(true, Ordering::Relaxed);
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let Ok(mut upstream) = tokio::net::TcpStream::connect(backend).await else {
                return;
            };
            let mut header = [0u8; 6];
            if client.read_exact(&mut header).await.is_err() {
                return;
            }
            let mut body = vec![0u8; u16::from_be_bytes([header[4], header[5]]) as usize];
            if client.read_exact(&mut body).await.is_err() {
                return;
            }
            let _ = upstream.write_all(&header).await;
            let _ = upstream.write_all(&body).await;
            let mut reply_header = [0u8; 6];
            if upstream.read_exact(&mut reply_header).await.is_err() {
                return;
            }
            let mut reply =
                vec![0u8; u16::from_be_bytes([reply_header[4], reply_header[5]]) as usize];
            if upstream.read_exact(&mut reply).await.is_err() {
                return;
            }
            let _ = client.write_all(&reply_header).await;
            let _ = client.write_all(&reply).await;
        });
    }
}

/// A retry spec (one writable coil, so failsafe has something to zero) whose
/// connect goes through `delayed_one_shot_modbus_proxy`. Returns the spec and
/// the flag that rises once the adapter's TCP connect has been accepted.
async fn delayed_connect_spec(
    delay: Duration,
    timeout_ms: u32,
) -> (DeviceSpec, Arc<AtomicBool>, [tokio::task::AbortHandle; 2]) {
    let backend_probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let backend = backend_probe.local_addr().unwrap();
    drop(backend_probe);
    let slave = tokio::spawn(iomap_modbus::run_demo_slave(
        backend,
        iomap_modbus::DemoSlave::new(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let front = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicBool::new(false));
    let proxy = tokio::spawn(delayed_one_shot_modbus_proxy(
        listener,
        backend,
        delay,
        accepted.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(100)).await; // demo slave is up
    (
        coil_spec(front, timeout_ms),
        accepted,
        [slave.abort_handle(), proxy.abort_handle()],
    )
}

/// A retry spec with one writable coil, so the connect-time seed read has
/// something to ask for and failsafe something to zero.
fn coil_spec(front: std::net::SocketAddr, timeout_ms: u32) -> DeviceSpec {
    let mut spec = retry_spec(front);
    let ProtocolConfig::Modbus(config) = &mut spec.config else {
        unreachable!("retry_spec builds a Modbus device");
    };
    config.timeout_ms = Some(timeout_ms);
    config.channels = vec![project::ModbusChannel {
        name: "c0".into(),
        kind: project::ModbusChannelKind::Coil,
        address: 0,
        data_type: Default::default(),
        word_order: Default::default(),
        access: Default::default(),
    }];
    spec
}

async fn wait_until(flag: &AtomicBool, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !flag.load(Ordering::Relaxed) {
        assert!(Instant::now() < deadline, "condition not reached in time");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn a_late_adapter_whose_cleanup_fails_is_reported_not_swallowed() {
    // The handoff closes while the worker is inside connect_one. The adapter
    // arrives late, is refused, failsafed on the worker's runtime — and that
    // fails, because the device dropped off right after connecting. The scan
    // thread must learn about it.
    let (spec, accepted, tasks) = delayed_connect_spec(Duration::from_millis(600), 5_000).await;
    let state = Arc::new(ReconnectState::default());
    let worker_state = state.clone();
    let worker = std::thread::spawn(move || {
        reconnect_worker(
            vec![spec],
            Arc::new(std::sync::Mutex::new(Vec::new())),
            Arc::new(AtomicBool::new(false)),
            worker_state,
        )
    });
    wait_until(&accepted, Duration::from_secs(5)).await; // first attempt after the 1 s backoff
    assert!(state.finish().is_empty(), "nothing delivered yet");
    let end = collect_reconnect_worker(worker, Duration::from_secs(5)).await;
    assert!(end.failed >= 1, "late cleanup failure lost: {end:?}");
    assert!(!end.unconfirmed, "the worker finished within the grace");
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn stop_during_an_inflight_reconnect_connect_does_not_hold_shutdown_for_the_connect() {
    // The adapter's connect takes ~3 s once the worker is inside it. Shutdown
    // may wait the grace for the worker, but not for the connect itself.
    let (spec, accepted, tasks) = delayed_connect_spec(Duration::from_secs(3), 6_000).await;
    let handle = spawn_units_with_grace(
        vec![single_unit(trivial_container(), 5)],
        DeviceSource::PrebuiltWithRetries(Vec::new(), vec![spec]),
        Vec::new(),
        None,
        WriteGovernance::default(),
        Duration::from_millis(300),
    );
    wait_until(&accepted, Duration::from_secs(5)).await;
    let id = scan_thread_id(&handle);
    let stopped_at = Instant::now();
    handle.stop();
    handle.shutdown().await;
    assert!(
        stopped_at.elapsed() < Duration::from_millis(1500),
        "shutdown() waited {:?} for an in-flight connect",
        stopped_at.elapsed()
    );
    // A worker still connecting after the grace is not a clean exit.
    assert_eq!(scan_end_of(id), ScanEnd::ReconnectUnconfirmed);
    for task in tasks {
        task.abort();
    }
}

// --- scan-loop and teardown behaviors that had no test of their own --------

fn snapshot_dint(snap: &VarSnapshot, name: &str) -> i64 {
    snap.vars
        .iter()
        .find(|v| v.name == name)
        .unwrap_or_else(|| panic!("no variable {name}"))
        .value
        .parse()
        .unwrap()
}

#[tokio::test]
async fn a_disabled_task_does_not_run_its_program_body() {
    // Same PROGRAM twice: enabled, its counter advances; with the task's enable
    // flag cleared the unit's clock still runs and snapshots still flow, but the
    // body never executes.
    for (enabled, label) in [(true, "enabled"), (false, "disabled")] {
        let mut c =
            crate::compile("PROGRAM main VAR x : DINT; END_VAR x := x + 1; END_PROGRAM").unwrap();
        if !enabled {
            for task in &mut c.task_table.tasks {
                task.flags &= !1;
            }
        }
        let handle = spawn_units_inner(
            vec![single_unit(c, 5)],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            None,
            WriteGovernance::default(),
        );
        let mut rx = handle.subscribe();
        let snap = last_snapshot_within(&mut rx, Duration::from_millis(400)).await;
        let x = snapshot_dint(&snap, "x");
        handle.shutdown().await;
        if enabled {
            assert!(x > 0, "{label}: the counter never advanced");
        } else {
            assert_eq!(x, 0, "{label}: the body ran");
        }
    }
}

#[tokio::test]
async fn a_container_with_two_program_instances_is_refused_and_driven_safe() {
    // The interruptible driver runs instance 0 only, so a container that holds
    // more is refused instead of silently running one of them.
    let mut c = trivial_container();
    let second = c.task_table.programs[0].clone();
    c.task_table.programs.push(second);
    let (d, fs, sd) = panic_device("probe", &Arc::new(AtomicBool::new(false)), false);
    let handle = spawn_units_inner(
        vec![single_unit(c, 5)],
        DeviceSource::Prebuilt(vec![Box::new(d)]),
        Vec::new(),
        None,
        WriteGovernance::default(),
    );
    join_scan_thread(&handle).await.unwrap();
    let fault = handle.fault().expect("a refused container is a fault");
    assert!(
        fault.contains("exactly one program instance in main"),
        "{fault}"
    );
    assert!(fs.load(Ordering::Relaxed) && sd.load(Ordering::Relaxed));
}

/// An adapter that connected late, with each teardown call set to succeed,
/// fail, or panic.
#[derive(Clone, Copy, Debug)]
enum Call {
    Ok,
    Err,
    Panic,
}

struct LateAdapter {
    failsafe: Call,
    shutdown: Call,
    shutdown_called: Arc<AtomicBool>,
}

fn run_call(mode: Call) -> Result<(), IoError> {
    match mode {
        Call::Ok => Ok(()),
        Call::Err => Err(IoError::Transport("injected".into())),
        Call::Panic => panic!("injected adapter panic"),
    }
}

#[async_trait::async_trait]
impl IoDevice for LateAdapter {
    fn name(&self) -> &str {
        "late"
    }
    async fn read_channel(&mut self, _: &str) -> Result<ChannelValue, IoError> {
        Ok(ChannelValue::I32(0))
    }
    async fn write_channel(&mut self, _: &str, _: ChannelValue) -> Result<(), IoError> {
        Ok(())
    }
    async fn enter_failsafe(&mut self) -> Result<(), IoError> {
        run_call(self.failsafe)
    }
    async fn shutdown(&mut self) -> Result<(), IoError> {
        self.shutdown_called.store(true, Ordering::Relaxed);
        run_call(self.shutdown)
    }
}

#[tokio::test]
async fn late_adapter_cleanup_counts_each_failed_call_and_never_skips_the_second() {
    for (failsafe, shutdown, want_failsafe, want_shutdown) in [
        (Call::Ok, Call::Ok, 0, 0),
        (Call::Err, Call::Ok, 1, 0),
        (Call::Ok, Call::Err, 0, 1),
        (Call::Err, Call::Err, 1, 1),
        (Call::Panic, Call::Ok, 1, 0),
        (Call::Ok, Call::Panic, 0, 1),
        (Call::Panic, Call::Panic, 1, 1),
    ] {
        let called = Arc::new(AtomicBool::new(false));
        let adapter = LateAdapter {
            failsafe,
            shutdown,
            shutdown_called: called.clone(),
        };
        let mut outcome = ReconnectOutcome::default();
        clean_up_late_adapter("late", Box::new(adapter), &mut outcome).await;
        assert_eq!(
            (outcome.late_failsafe_failed, outcome.late_shutdown_failed),
            (want_failsafe, want_shutdown),
            "failsafe {failsafe:?}, shutdown {shutdown:?}"
        );
        assert!(
            called.load(Ordering::Relaxed),
            "shutdown was skipped after failsafe {failsafe:?}"
        );
    }
}

/// How the scan thread of `handle` concluded, once it has ended.
fn scan_end_of(thread: std::thread::ThreadId) -> ScanEnd {
    SCAN_ENDS
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|(id, _)| *id == thread)
        .map(|(_, end)| *end)
        .expect("the scan thread classified its exit")
}

fn scan_thread_id(handle: &ProgramHandle) -> std::thread::ThreadId {
    handle
        .thread
        .lock()
        .unwrap()
        .as_ref()
        .expect("the scan thread handle is still held")
        .thread()
        .id()
}

#[tokio::test]
async fn a_plain_stop_is_classified_clean() {
    let (device, _) = MockDevice::named("probe");
    let handle = spawn_units_inner(
        vec![single_unit(trivial_container(), 5)],
        DeviceSource::Prebuilt(vec![Box::new(device)]),
        Vec::new(),
        None,
        WriteGovernance::default(),
    );
    tokio::time::sleep(Duration::from_millis(60)).await;
    let id = scan_thread_id(&handle);
    handle.stop();
    handle.shutdown().await;
    assert_eq!(scan_end_of(id), ScanEnd::Clean);
}

#[tokio::test]
async fn a_late_adapter_that_cannot_be_driven_safe_makes_the_scan_end_report_a_device_failure() {
    // The worker is inside its connect when Stop arrives; the adapter then
    // arrives late and its failsafe fails. The scan thread, which waits for the
    // worker, has to conclude that a device failed, not that it exited cleanly.
    let (spec, accepted, tasks) = delayed_connect_spec(Duration::from_millis(600), 5_000).await;
    let handle = spawn_units_with_grace(
        vec![single_unit(trivial_container(), 5)],
        DeviceSource::PrebuiltWithRetries(Vec::new(), vec![spec]),
        Vec::new(),
        None,
        WriteGovernance::default(),
        Duration::from_secs(5),
    );
    wait_until(&accepted, Duration::from_secs(5)).await;
    let id = scan_thread_id(&handle);
    handle.stop();
    handle.shutdown().await;
    assert_eq!(scan_end_of(id), ScanEnd::StoppedButDevicesFailed);
    for task in tasks {
        task.abort();
    }
}

/// Accepts connections, raises `accepted`, holds each one for `delay`, then
/// connects it straight through to `backend` for good.
async fn delayed_transparent_proxy(
    listener: tokio::net::TcpListener,
    backend: std::net::SocketAddr,
    delay: Duration,
    accepted: Arc<AtomicBool>,
) {
    loop {
        let Ok((mut client, _)) = listener.accept().await else {
            return;
        };
        accepted.store(true, Ordering::Relaxed);
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let Ok(mut upstream) = tokio::net::TcpStream::connect(backend).await else {
                return;
            };
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        });
    }
}

/// A device whose failsafe takes a while.
struct SlowSafe(Duration);

#[async_trait::async_trait]
impl IoDevice for SlowSafe {
    fn name(&self) -> &str {
        "slow"
    }
    async fn read_channel(&mut self, _: &str) -> Result<ChannelValue, IoError> {
        Ok(ChannelValue::I32(0))
    }
    async fn write_channel(&mut self, _: &str, _: ChannelValue) -> Result<(), IoError> {
        Ok(())
    }
    async fn enter_failsafe(&mut self) -> Result<(), IoError> {
        tokio::time::sleep(self.0).await;
        Ok(())
    }
    async fn shutdown(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

#[tokio::test]
async fn an_adapter_still_queued_when_the_scan_ends_is_driven_safe_while_its_runtime_lives() {
    // A real Modbus adapter reaches a demo slave through a proxy that holds the
    // connect for 400 ms, with coil 0 already ON. The scan loop is parked inside
    // one tick (a stall) while the adapter arrives, and Stop comes before the loop
    // is back at the top, where adapters are adopted: it is still QUEUED when the
    // scan ends. Another device's slow failsafe runs ahead of it, so the adapter's
    // own failsafe comes later than the worker's 200 ms release poll: it only
    // reaches the slave if the worker's runtime was kept alive until the whole
    // pass had finished. Coil 0 going OFF proves both that the queued adapter was
    // driven safe and that its runtime outlived the pass.
    let backend_probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let backend = backend_probe.local_addr().unwrap();
    drop(backend_probe);
    let slave_state = iomap_modbus::DemoSlave::new();
    slave_state.coils().lock().unwrap()[0] = true;
    let slave = tokio::spawn(iomap_modbus::run_demo_slave(backend, slave_state.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let front = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicBool::new(false));
    let proxy = tokio::spawn(delayed_transparent_proxy(
        listener,
        backend,
        Duration::from_millis(400),
        accepted.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(100)).await; // demo slave is up

    let handle = spawn_units_inner(
        vec![single_unit(trivial_container(), 5)],
        DeviceSource::PrebuiltWithRetries(
            vec![Box::new(SlowSafe(Duration::from_millis(600)))],
            vec![coil_spec(front, 5_000)],
        ),
        Vec::new(),
        None,
        WriteGovernance::default(),
    );
    wait_until(&accepted, Duration::from_secs(5)).await; // the worker is inside its connect
    handle.inject_scan_stall(1_000, 1).await.unwrap();
    tokio::time::sleep(Duration::from_millis(650)).await; // the adapter arrived at ~400 ms
    handle.stop();
    join_scan_thread(&handle).await.unwrap();

    assert_eq!(handle.fault(), None);
    assert!(
        !slave_state.coils().lock().unwrap()[0],
        "the queued adapter was not driven safe before its runtime went away"
    );
    proxy.abort();
    slave.abort();
}
