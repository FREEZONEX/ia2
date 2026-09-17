//! Real `ethercrab::MainDevice` driver. Selected when `EthercatConfig.nic`
//! is anything other than `"_sim"`.
//!
//! Architecture:
//!
//! - `connect` spawns a **dedicated OS thread** that owns ethercrab. We
//!   need our own thread because ethercrab's `tx_rx_task` is built on
//!   `async-io`, not tokio, and conflicting reactors in the same thread
//!   would deadlock. The thread runs `smol::block_on`, which drives
//!   `async-io` natively.
//!
//! - The thread:
//!   1. `Box::leak`s a `PduStorage` (gives `&'static`; required by
//!      `try_split`). One leaked storage per `connect` — fine, devices
//!      don't churn at runtime.
//!   2. Builds the `MainDevice`, spawns `tx_rx_task` as a detached smol
//!      task, walks the bus with `init_single_group`, transitions to OP.
//!   3. Reports back through a `tokio::sync::oneshot` so the connect()
//!      future awaits an "actually live" signal (or an init error).
//!   4. Enters the cyclic loop, exiting when the shutdown flag flips.
//!
//! - **PDI mirror** (`Arc<Mutex<PdiMirror>>`): the cyclic task is the
//!   sole owner of the `SubDeviceGroup`. Reader/writer paths
//!   (`read_channel` / `write_channel`) never touch ethercrab — they
//!   only lock the mirror briefly. Each cycle:
//!     - Pre-cycle: copy `mirror.outputs[slave_index]` → group's per-slave
//!       output PDI bytes.
//!     - `group.tx_rx(...).await` — actual fieldbus exchange.
//!     - Post-cycle: copy group's per-slave input PDI bytes →
//!       `mirror.inputs[slave_index]`.
//!
//!   The result: the program's scan loop sees consistent inputs and the
//!   bus sees consistent outputs each round, without ethercrab ever
//!   being touched from a tokio task.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ethercrab::std::ethercat_now;
use ethercrab::subdevice_group::DcConfiguration;
use ethercrab::{DcSync, MainDevice, MainDeviceConfig, PduStorage, Timeouts};
use iocore::{ChannelValue, HealthTracker, HealthTransition, IoDevice, IoError};
use project::{
    EthercatChannel, EthercatConfig, EthercatDcSync, EthercatPdoDirection, EthercatSlave,
};

use crate::bits;
use crate::validate;
use crate::SlaveDiscovery;

/// Consecutive cyclic `tx_rx` failures before the device is flagged
/// unhealthy (one ERROR log per outage, not one per cycle — at a 1 ms
/// cycle that distinction matters).
const UNHEALTHY_AFTER_TX_ERRORS: u32 = 10;

// Storage sizing — picked to comfortably cover a typical edge configuration
// (an EK1100-class coupler + EL modules). Sized for plant-scale buses,
// not just demo benches: a 1000-point project (AI230/AO54/DI480/DO270)
// needs ~660 B of PDI and tens of modular subdevices, so 128 subdevices
// / 4096 B PDI leaves comfortable headroom. The cost is static memory
// only (PduStorage is Box::leaked per connect): 64 frames x ~1100 B is
// about 70 KB — irrelevant on an edge box — and one 4 KiB PDI cycle
// splits across ~4 PDUs per direction, which 64 in-flight frames cover
// several times over. MAX_SUBDEVICES must be a power of 2 > 1;
// MAX_PDU_DATA at ~1100 tracks the Ethernet frame limit.
const MAX_SUBDEVICES: usize = 128;
const MAX_PDU_DATA: usize = PduStorage::element_size(1100);
const MAX_FRAMES: usize = 64;
const PDI_LEN: usize = 4096;

type Storage = PduStorage<MAX_FRAMES, MAX_PDU_DATA>;

// These three macros share the per-cycle / discovery logic between the DC
// (`Sync0`) and free-run (`Off`) paths in `smol_main`. They're macros, not
// generic fns, to sidestep the `SubDeviceGroup` typestate generics (HasDc
// vs NoDc are distinct types) without duplicating ~40 lines twice.

/// Walk the OP group, size the PDI mirror, log + return the discovered
/// SubDevices.
macro_rules! capture_discovery {
    ($group:expr, $maindevice:expr, $pdi:expr) => {{
        let mut discovered: Vec<SlaveDiscovery> = Vec::new();
        {
            let mut mirror = $pdi.lock().expect("pdi mirror poisoned");
            // Insert-only maps would keep stale keys across a re-walk of
            // a shrunken bus, silently serving frozen inputs for vanished
            // positions. Clear + re-insert under the one lock: a vanished
            // slave now degrades to an explicit per-access Transport
            // error instead. (First walk: clearing empty maps is a no-op.)
            mirror.inputs.clear();
            mirror.outputs.clear();
            for (offset_idx, sd) in $group.iter(&$maindevice).enumerate() {
                let io = sd.io_raw();
                let in_len = io.inputs().len();
                let out_len = io.outputs().len();
                let idx = offset_idx as u16;
                mirror.inputs.insert(idx, vec![0u8; in_len]);
                mirror.outputs.insert(idx, vec![0u8; out_len]);
                discovered.push(SlaveDiscovery {
                    index: idx,
                    name: sd.name().to_string(),
                    input_bytes: in_len as u16,
                    output_bytes: out_len as u16,
                    vendor_id: sd.identity().vendor_id,
                    product_id: sd.identity().product_id,
                });
            }
        }
        for sd in &discovered {
            tracing::info!(
                slave = sd.index,
                sd_name = %sd.name,
                vendor = format!("{:#010x}", sd.vendor_id),
                product = format!("{:#010x}", sd.product_id),
                in_bytes = sd.input_bytes,
                out_bytes = sd.output_bytes,
                "discovered subdevice"
            );
        }
        discovered
    }};
}

/// Pre-cycle: copy our owned output bytes onto the bus surface.
macro_rules! copy_outputs_to_bus {
    ($group:expr, $maindevice:expr, $pdi:expr) => {{
        let mirror = $pdi.lock().expect("pdi mirror poisoned");
        for (offset_idx, sd) in $group.iter(&$maindevice).enumerate() {
            let idx = offset_idx as u16;
            if let Some(src) = mirror.outputs.get(&idx) {
                let mut out = sd.outputs_raw_mut();
                let n = out.len().min(src.len());
                out[..n].copy_from_slice(&src[..n]);
            }
        }
    }};
}

/// In-cycle gear tick: for every configured gear axis, read the follower's
/// statusword + actual (and the master axis's actual) from the *input*
/// surface of the previous exchange, run the engine, and overwrite the
/// follower's target_position bytes on the *output* surface. Runs after
/// `copy_outputs_to_bus!` so the engine — not the PLC mirror — owns those
/// bytes on the wire.
macro_rules! gear_tick {
    ($group:expr, $maindevice:expr, $engines:expr, $bus_ok:expr) => {{
        for eng in $engines.iter_mut() {
            let mut master_actual: Option<i32> = None;
            if let crate::gear::MasterSrc::Axis {
                slave_index,
                offset,
            } = eng.master
            {
                for (i, sd) in $group.iter(&$maindevice).enumerate() {
                    if i as u16 == slave_index {
                        master_actual = crate::gear::read_i32(&sd.inputs_raw()[..], offset);
                        break;
                    }
                }
            }
            for (i, sd) in $group.iter(&$maindevice).enumerate() {
                if i as u16 == eng.follower_index {
                    let (sw, actual) = {
                        let inp = sd.inputs_raw();
                        (
                            crate::gear::read_u16(&inp[..], eng.status_off).unwrap_or(0),
                            crate::gear::read_i32(&inp[..], eng.actual_off).unwrap_or(0),
                        )
                    };
                    let target = eng.tick(sw, actual, master_actual, $bus_ok);
                    let mut out = sd.outputs_raw_mut();
                    crate::gear::write_i32(&mut out[..], eng.target_off, target);
                    break;
                }
            }
        }
    }};
}

/// Post-cycle: snapshot inputs back into our mirror.
macro_rules! copy_inputs_from_bus {
    ($group:expr, $maindevice:expr, $pdi:expr, $changes:expr) => {{
        let mut mirror = $pdi.lock().expect("pdi mirror poisoned");
        let mut changed = false;
        for (offset_idx, sd) in $group.iter(&$maindevice).enumerate() {
            let idx = offset_idx as u16;
            let inputs = sd.inputs_raw();
            if let Some(dst) = mirror.inputs.get_mut(&idx) {
                let n = inputs.len().min(dst.len());
                if dst[..n] != inputs[..n] {
                    changed = true;
                }
                dst[..n].copy_from_slice(&inputs[..n]);
            }
        }
        if changed {
            $changes.fetch_add(1, Ordering::Relaxed);
        }
    }};
}

/// Per-slave byte mirrors of the PDI. Indexed by `EthercatSlave.index`
/// (which matches the auto-incremented bus position ethercrab assigns).
///
/// Inputs are written by the cyclic task post-tx_rx; reads from
/// `read_channel` extract bits out of these buffers.
///
/// Outputs are written by `write_channel`; the cyclic task copies them
/// onto the bus pre-tx_rx. Reads of RxPDO channels echo back from this
/// same buffer (matches sim mode and is useful for "did my write take?"
/// debugging in the UI).
#[derive(Default, Debug)]
struct PdiMirror {
    inputs: HashMap<u16, Vec<u8>>,
    outputs: HashMap<u16, Vec<u8>>,
}

/// Bounded wait for the cyclic worker thread to stop on graceful
/// shutdown. The loop observes `shutdown` within ~one cycle (plus, at
/// worst, one ethercrab PDU timeout on a wedged bus), then flushes a
/// final zeroed frame and exits — comfortably inside this budget. Capped
/// so a hung bus can't push the whole shutdown past systemd's kill
/// timeout: if we blow it, we abandon the thread and let process
/// teardown reap it.
const WORKER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

pub struct RealEthercat {
    name: String,
    channels: HashMap<String, EthercatChannel>,
    /// Slow-plane routing for in-cycle gear parameter channels; consulted
    /// by read/write_channel before the PDI channel lookup.
    gear_routing: crate::gear::GearRouting,
    pdi: Arc<Mutex<PdiMirror>>,
    shutdown: Arc<AtomicBool>,
    /// Flipped by the worker (via a drop guard) on every `smol_main`
    /// exit path, so `shutdown` can join with a bound instead of risking
    /// a wait on a thread that's already gone.
    stopped: Arc<AtomicBool>,
    /// Mirrored from the cyclic worker's `HealthTracker`: `false` after
    /// `UNHEALTHY_AFTER_TX_ERRORS` consecutive `tx_rx` failures, `true`
    /// again on the first successful exchange.
    healthy: Arc<AtomicBool>,
    /// Subdevices found during the bus walk at connect (for `/discover`).
    discovered: Vec<SlaveDiscovery>,
    // The cyclic worker. Joined by `shutdown` on a clean stop (so the
    // final zeroed frame is guaranteed on the wire); on `Drop` it's only
    // signalled, not joined — drop can run mid-teardown and must not block.
    _thread: Option<thread::JoinHandle<()>>,
}

/// The `Arc` handles `connect` shares with the cyclic worker thread,
/// bundled so `smol_main` keeps a readable signature.
struct WorkerShared {
    pdi: Arc<Mutex<PdiMirror>>,
    shutdown: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    healthy: Arc<AtomicBool>,
    /// RTSO-HOLD-0731 instrumentation: bumped once per cyclic loop
    /// iteration (Ok or Err). A parked or dead worker stops advancing
    /// this; the watchdog thread turns that into a journal line.
    cycles: Arc<AtomicU64>,
    /// Bumped whenever a post-cycle input snapshot differed from the
    /// mirror's previous contents — "the bus is delivering genuinely
    /// fresh bytes", as opposed to byte-identical echoes.
    input_changes: Arc<AtomicU64>,
    /// TRUE while the worker is re-walking the bus to OP after a
    /// demotion (RTSO-HOLD-0731 fix). The cycle watchdog reports a
    /// paused counter as "re-walking", not as a stall.
    reinitializing: Arc<AtomicBool>,
    /// Count of bus re-walks started since connect (0 = never demoted).
    reinits: Arc<AtomicU64>,
    /// In-cycle gear engines (one per configured [[gear]] axis), owned by
    /// the cyclic worker and ticked between the mirror copy and tx_rx.
    engines: Vec<crate::gear::GearEngine>,
}

/// Wait up to `timeout` for the worker to flag `stopped`, then join it.
/// Returns `true` if the thread was joined, `false` if it didn't stop in
/// time (caller abandons the handle; the process exit reaps the thread).
/// Sync + bounded so it can run on a blocking pool without ever holding
/// the shutdown past the service supervisor's deadline.
fn join_worker(
    stopped: &AtomicBool,
    handle: Option<thread::JoinHandle<()>>,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while !stopped.load(Ordering::Relaxed) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(2));
    }
    if let Some(handle) = handle {
        let _ = handle.join();
    }
    true
}

/// Whether the bus-side init succeeded. Sent back over the oneshot.
#[derive(Debug)]
enum InitResult {
    Ok { discovered: Vec<SlaveDiscovery> },
    Err(String),
}

impl RealEthercat {
    pub async fn connect(name: String, config: &EthercatConfig) -> Result<Self, IoError> {
        // ESI-modular real-bus cyclic bring-up (master-programmed
        // SyncManager/FMMU + logical-RW exchange, bypassing the auto
        // PDO-assignment path) is validated against the physical coupler
        // before it ships — we don't run un-hardware-verified EtherCAT
        // cyclic I/O on a live control bus. The ESI parse/assembly and the
        // offline channel authoring (`cs device esi-assemble`) work today;
        // author + verify the program in sim (`nic: "_sim"`) meanwhile.
        if matches!(config.bringup, project::EthercatBringup::EsiModular { .. }) {
            return Err(IoError::Connect(
                "ESI-modular real-bus bring-up is not yet wired (issue #11): assemble channels \
                 with `cs device esi-assemble` and validate the program in sim (nic \"_sim\") — \
                 the real-bus SM/FMMU/LRW path lands after per-coupler hardware validation"
                    .into(),
            ));
        }

        // Validate channel references up front (unique names + known slave
        // indices), same shape as sim.
        validate::validate_channel_refs(&config.channels, &config.slaves)
            .map_err(IoError::Connect)?;
        // Rebuilt for the gear checks below: declared slave indices and the
        // set of PDO channel names gear params must not shadow.
        let known_slaves: std::collections::HashSet<u16> =
            config.slaves.iter().map(|s| s.index).collect();
        let pdo_names: std::collections::HashSet<&str> =
            config.channels.iter().map(|c| c.name.as_str()).collect();

        // Shape problems (zero-length / misaligned entries) are knowable
        // before touching the bus — reject them before spawning anything.
        validate::validate_channel_shapes(&config.channels).map_err(IoError::Connect)?;
        validate::validate_init_sdo(&config.slaves).map_err(IoError::Connect)?;

        // Gear axes: channel names must be unique (across gears and within
        // each gear) and must not shadow PDO channels — the routing check in
        // read/write_channel runs first and would silently eat a collision.
        crate::gear::validate_channels(&config.gear, &pdo_names).map_err(IoError::Connect)?;
        // Every referenced slave must exist in the declared topology.
        for g in &config.gear {
            if !known_slaves.is_empty() && !known_slaves.contains(&g.slave_index) {
                return Err(IoError::Connect(format!(
                    "gear follower references unknown slave_index={}",
                    g.slave_index
                )));
            }
            if let project::GearMaster::Axis { slave_index, .. } = g.master {
                if !known_slaves.is_empty() && !known_slaves.contains(&slave_index) {
                    return Err(IoError::Connect(format!(
                        "gear master references unknown slave_index={slave_index}"
                    )));
                }
            }
        }
        let (engines, gear_routing) = crate::gear::build(&config.gear);

        let channels: HashMap<String, EthercatChannel> = config
            .channels
            .iter()
            .map(|c| (c.name.clone(), c.clone()))
            .collect();
        let pdi = Arc::new(Mutex::new(PdiMirror::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let healthy = Arc::new(AtomicBool::new(true));
        let cycles = Arc::new(AtomicU64::new(0));
        let input_changes = Arc::new(AtomicU64::new(0));
        let reinitializing = Arc::new(AtomicBool::new(false));
        let reinits = Arc::new(AtomicU64::new(0));
        let (init_tx, init_rx) = mpsc::sync_channel::<InitResult>(1);

        let nic = config.nic.clone();
        let cycle_us = config.cycle_us.max(100); // hard floor: don't melt the CPU
        let dc_sync = config.dc_sync;
        let dc_static_sync_iterations = config.dc_static_sync_iterations;
        let slaves = config.slaves.clone();
        let shared = WorkerShared {
            pdi: pdi.clone(),
            shutdown: shutdown.clone(),
            stopped: stopped.clone(),
            healthy: healthy.clone(),
            cycles: cycles.clone(),
            input_changes: input_changes.clone(),
            reinitializing: reinitializing.clone(),
            reinits: reinits.clone(),
            engines,
        };
        let thread_name = format!("ec-{name}");

        let thread = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                smol_main(
                    &nic,
                    cycle_us,
                    dc_sync,
                    dc_static_sync_iterations,
                    &slaves,
                    shared,
                    init_tx,
                )
            })
            .map_err(|e| IoError::Connect(format!("spawn ethercat thread: {e}")))?;

        // Wait for the worker to report success or failure. The init walk
        // is bounded (timeouts inside ethercrab); a generous wait here is
        // OK because connect() runs at startup, not in the hot path.
        let init = tokio::task::spawn_blocking(move || {
            init_rx
                .recv_timeout(Duration::from_secs(15))
                .map_err(|e| format!("init handshake timed out: {e}"))
        })
        .await
        .map_err(|e| IoError::Connect(format!("init join: {e}")))?
        .map_err(IoError::Connect)?;

        match init {
            InitResult::Ok { discovered } => {
                // The bus is walked and the cyclic worker is live — now
                // hold the discovered topology against the configuration.
                // Identity first (wrong/missing module), then PDI ranges
                // (channel windows that the real PDI cannot serve), then the
                // gear axes' target/actual/status offsets. Fail the connect
                // with everything found at once, instead of letting each
                // problem surface later as per-cycle Transport errors or,
                // worse, bits driven on the wrong module.
                let mut problems: Vec<String> = Vec::new();
                if let Err(m) = validate::validate_identities(&config.slaves, &discovered) {
                    problems.push(m);
                }
                if let Err(m) = validate::validate_pdi_ranges(&config.channels, &discovered) {
                    problems.push(m);
                }
                problems.extend(validate::validate_gear_offsets(&config.gear, &discovered));
                if !problems.is_empty() {
                    // A failed connect must not leak a live cyclic thread
                    // driving the bus: signal it down and join (bounded).
                    shutdown.store(true, Ordering::Relaxed);
                    let stopped_for_join = stopped.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        join_worker(&stopped_for_join, Some(thread), WORKER_JOIN_TIMEOUT)
                    })
                    .await;
                    return Err(IoError::Connect(format!(
                        "ethercat bus validation failed: {}",
                        problems.join("; ")
                    )));
                }

                tracing::info!(
                    name = %name,
                    nic = %config.nic,
                    cycle_us,
                    discovered = discovered.len(),
                    "ethercat device live"
                );
                spawn_cycle_watchdog(
                    name.clone(),
                    cycles,
                    input_changes,
                    stopped.clone(),
                    healthy.clone(),
                    shutdown.clone(),
                    reinitializing,
                    reinits,
                );
                Ok(Self {
                    name,
                    channels,
                    gear_routing,
                    pdi,
                    shutdown,
                    stopped,
                    healthy,
                    discovered,
                    _thread: Some(thread),
                })
            }
            InitResult::Err(e) => Err(IoError::Connect(format!("ethercat init: {e}"))),
        }
    }

    /// Subdevices found during the bus walk at connect.
    pub fn discovered(&self) -> Vec<SlaveDiscovery> {
        self.discovered.clone()
    }
}

impl Drop for RealEthercat {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Don't join here — drop can run mid-teardown and must never block
        // the runtime. The worker exits on the next cycle tick. A clean
        // stop goes through `shutdown()` instead, which DOES join (after
        // failsafe) so the zeroed frame is guaranteed on the wire.
    }
}

#[async_trait]
impl IoDevice for RealEthercat {
    fn name(&self) -> &str {
        &self.name
    }

    /// `false` once the cyclic worker has seen `UNHEALTHY_AFTER_TX_ERRORS`
    /// consecutive `tx_rx` failures (cable pulled, slave dropped off,
    /// watchdog tripped); `true` again after the first good exchange.
    /// While unhealthy the PDI mirror serves last-known inputs.
    fn is_healthy(&self) -> bool {
        // RTSO-HOLD-0731: a dead worker (DoneGuard flips `stopped` on every
        // exit path, panic included) used to freeze `healthy` at its last
        // value forever — only `record_failure` could clear it, and a dead
        // thread never calls it. Reported observable only; nothing gates
        // control flow on this.
        self.healthy.load(Ordering::Relaxed) && !self.stopped.load(Ordering::Relaxed)
    }

    async fn read_channel(&mut self, channel: &str) -> Result<ChannelValue, IoError> {
        // Gear parameter / feedback channels live in the lock-free shared
        // params, not the PDI.
        if let Some(v) = self.gear_routing.read(channel) {
            return Ok(v);
        }
        let meta = self
            .channels
            .get(channel)
            .ok_or_else(|| IoError::UnknownChannel(channel.into()))?
            .clone();
        let pdi = self.pdi.lock().expect("pdi mirror poisoned");
        let map = match meta.direction {
            EthercatPdoDirection::TxPdo => &pdi.inputs,
            EthercatPdoDirection::RxPdo => &pdi.outputs,
        };
        let buffer = map.get(&meta.slave_index).ok_or_else(|| {
            IoError::Transport(format!(
                "slave_index={} not present in discovered bus",
                meta.slave_index
            ))
        })?;
        bits::read_value(
            buffer,
            meta.pdi_byte_offset as usize,
            meta.pdi_bit_offset,
            meta.bit_length,
            meta.data_type,
        )
    }

    async fn write_channel(&mut self, channel: &str, value: ChannelValue) -> Result<(), IoError> {
        // Gear parameter channels route into the lock-free shared params
        // the cyclic loop reads each tick — never into PDI bytes.
        if let Some(res) = self.gear_routing.write(channel, &value) {
            return res;
        }
        let meta = self
            .channels
            .get(channel)
            .ok_or_else(|| IoError::UnknownChannel(channel.into()))?
            .clone();
        if meta.direction == EthercatPdoDirection::TxPdo {
            return Err(IoError::TypeMismatch {
                channel: channel.into(),
                value,
            });
        }
        let mut pdi = self.pdi.lock().expect("pdi mirror poisoned");
        let buffer = pdi.outputs.get_mut(&meta.slave_index).ok_or_else(|| {
            IoError::Transport(format!(
                "slave_index={} not present in discovered bus",
                meta.slave_index
            ))
        })?;
        bits::write_value(
            buffer,
            meta.pdi_byte_offset as usize,
            meta.pdi_bit_offset,
            meta.bit_length,
            meta.data_type,
            value,
        )
    }

    /// Zero the entire output PDI mirror for every discovered slave.
    /// The next cyclic tick on the worker thread copies these zeros
    /// onto the bus surface and `tx_rx`'s them out — at default cycle
    /// times that's a millisecond or two. Wait a couple of cycle
    /// periods after returning if you need to guarantee propagation
    /// before exiting (the bridge does this).
    ///
    /// Returns `Err` when the cyclic worker has already stopped: the mirror
    /// is still zeroed, but nothing will transmit it, so `Ok` would be a
    /// false report of a safe state having been commanded.
    async fn enter_failsafe(&mut self) -> Result<(), IoError> {
        // Gear axes first: drop every engage request so the engines fall
        // back to shadow/hold instead of re-driving targets over the
        // zeroed mirror below.
        self.gear_routing.disengage_all();
        {
            let mut pdi = self.pdi.lock().expect("pdi mirror poisoned");
            for buf in pdi.outputs.values_mut() {
                buf.fill(0);
            }
        }
        // Zeroing the mirror is not the failsafe — the worker's next tick
        // putting it on the wire is. A dead worker (DoneGuard flips
        // `stopped` on every exit path, panic included) means no tick will
        // ever come, so reporting Ok here would tell the caller the outputs
        // were commanded safe when nothing was sent. The SubDevices do still
        // de-energize, via their own SyncManager watchdogs once the master
        // stops transmitting — a different mechanism, on a different
        // timescale, and the operator should be told which one they got.
        // Every other adapter already reports a failsafe it could not
        // deliver; this one always said Ok.
        if self.stopped.load(Ordering::Relaxed) {
            tracing::error!(
                device = %self.name,
                "ethercat failsafe: output PDI zeroed but the cyclic worker is not running"
            );
            return Err(IoError::Transport(format!(
                "ethercat '{}': output PDI zeroed, but the cyclic worker has stopped, so no \
                 frame will carry it — the SubDevices de-energize via their own SyncManager \
                 watchdogs instead",
                self.name
            )));
        }
        tracing::info!(device = %self.name, "ethercat output PDI zeroed for failsafe");
        Ok(())
    }

    /// Graceful teardown: signal the cyclic worker to stop and JOIN it.
    /// By this point `enter_failsafe` has zeroed the output mirror, so the
    /// worker's final flush (one last `tx_rx`) puts controlword = 0 on the
    /// wire before it exits. Joining guarantees that frame was sent before
    /// the master goes away — the whole point of the in-runtime failsafe.
    /// Bounded so a wedged bus can't stall the process past its kill
    /// timeout. The join runs on a blocking thread so we don't park the
    /// async executor on a std thread join.
    async fn shutdown(&mut self) -> Result<(), IoError> {
        self.shutdown.store(true, Ordering::Relaxed);
        let handle = self._thread.take();
        let stopped = self.stopped.clone();
        let joined =
            tokio::task::spawn_blocking(move || join_worker(&stopped, handle, WORKER_JOIN_TIMEOUT))
                .await
                .unwrap_or(false);
        if joined {
            tracing::info!(device = %self.name, "ethercat cyclic worker joined (final failsafe frame flushed)");
        } else {
            tracing::warn!(
                device = %self.name,
                timeout_s = WORKER_JOIN_TIMEOUT.as_secs(),
                "ethercat cyclic worker did not stop in time; abandoning thread (process teardown will reap it)"
            );
        }
        Ok(())
    }
}

/// RTSO-HOLD-0731 instrumentation: one step of the cycle-watchdog latch.
/// Pure so the stall/recover edge logic is unit-testable. Returns the new
/// `stalled` state and whether an edge (Some(true)=stalled, Some(false)=
/// recovered) should be logged this step.
fn watchdog_step(last: u64, now: u64, was_stalled: bool) -> (bool, Option<bool>) {
    if now == last && !was_stalled {
        (true, Some(true))
    } else if now != last && was_stalled {
        (false, Some(false))
    } else {
        (was_stalled, None)
    }
}

/// RTSO-HOLD-0731 instrumentation: an independent std-thread watchdog over
/// the cyclic worker's cycle counter. Deliberately uses only
/// `thread::sleep` and relaxed atomics — it must stay alive through the
/// exact failure modes it instruments (async-io reactor stall, smol
/// executor wedge, poisoned PDI mutex), so it never locks anything and
/// never touches the async runtime. At a 2 ms cycle, 1 s of zero advance
/// is 500 missed cycles — unambiguous. The 60 s heartbeat writes a
/// bus-side timeline into the journal (the 1 Hz historian only sees the
/// VM side).
#[allow(clippy::too_many_arguments)]
fn spawn_cycle_watchdog(
    name: String,
    cycles: Arc<AtomicU64>,
    input_changes: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
    healthy: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    reinitializing: Arc<AtomicBool>,
    reinits: Arc<AtomicU64>,
) {
    let _ = thread::Builder::new()
        .name(format!("ecwd-{name}"))
        .spawn(move || {
            let mut last = cycles.load(Ordering::Relaxed);
            let mut stalled = false;
            let mut beat: u64 = 0;
            while !shutdown.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_secs(1));
                let now = cycles.load(Ordering::Relaxed);
                let (next, edge) = watchdog_step(last, now, stalled);
                stalled = next;
                match edge {
                    // A paused counter during a bus re-walk is the fix at
                    // work, not a stall — report it as such (INFO).
                    Some(true) if reinitializing.load(Ordering::Relaxed) => tracing::warn!(
                        device = %name,
                        cycles = now,
                        reinits = reinits.load(Ordering::Relaxed),
                        "ethercat cyclic exchange paused: bus re-walk in progress"
                    ),
                    Some(true) => tracing::error!(
                        device = %name,
                        cycles = now,
                        worker_stopped = stopped.load(Ordering::Relaxed),
                        healthy = healthy.load(Ordering::Relaxed),
                        input_changes = input_changes.load(Ordering::Relaxed),
                        "ethercat cyclic worker made ZERO cycles in 1s \
                         (worker_stopped=true => thread dead; false => parked \
                         in reactor/timer — capture a stack before restarting)"
                    ),
                    Some(false) => tracing::info!(
                        device = %name,
                        cycles = now,
                        "ethercat cyclic worker advancing again"
                    ),
                    None => {}
                }
                beat += 1;
                if beat.is_multiple_of(60) {
                    tracing::info!(
                        device = %name,
                        cycles = now,
                        input_changes = input_changes.load(Ordering::Relaxed),
                        healthy = healthy.load(Ordering::Relaxed),
                        reinits = reinits.load(Ordering::Relaxed),
                        "ethercat cyclic heartbeat"
                    );
                }
                last = now;
            }
        });
}

/// RTSO-HOLD-0731 fix: build (or rebuild) the EtherCAT transport — a
/// freshly leaked PduStorage, a MainDevice over it, and the detached
/// TX/RX socket pump. The pump flips `pump_dead` on ANY exit, return or
/// panic; the supervise loop rebuilds the whole transport when it sees
/// that flag, because a PduStorage can only be split once and a dead
/// pump makes every subsequent exchange time out forever (so retrying
/// the walk without rebuilding could never converge). One PduStorage
/// leaks per pump death; pump deaths are rare, hard failures.
fn build_transport(
    nic: &str,
    dc_static_sync_iterations: u32,
    pump_dead: Arc<AtomicBool>,
) -> Result<Arc<MainDevice<'static>>, String> {
    let storage: &'static Storage = Box::leak(Box::new(Storage::new()));
    let (tx, rx, pdu_loop) = storage
        .try_split()
        .map_err(|_| "PduStorage split failed on a freshly leaked storage".to_string())?;
    let maindevice = Arc::new(MainDevice::new(
        pdu_loop,
        Timeouts {
            wait_loop_delay: Duration::from_millis(2),
            mailbox_response: Duration::from_millis(1000),
            ..Default::default()
        },
        // dc_static_sync_iterations is configurable because the right
        // value depends on the bus: 0 (our default) skips the init-time
        // FRMW burst entirely — on a short bus / non-RT host any one of
        // those frames timing out aborts init with Timeout(Pdu). Longer
        // DC buses that care about clock convergence at OP-entry can
        // raise it from the device config.
        MainDeviceConfig {
            dc_static_sync_iterations,
            ..MainDeviceConfig::default()
        },
    ));
    let tx_rx = ethercrab::std::tx_rx_task(nic, tx, rx)
        .map_err(|e| format!("tx_rx_task on {nic}: {e} (need CAP_NET_RAW + real NIC)"))?;
    smol::spawn(async move {
        // The guard fires on unwind too, closing the silent panic exit;
        // the flag is what turns a dead pump from a permanent wedge into
        // a transport rebuild.
        struct PumpGuard(Arc<AtomicBool>);
        impl Drop for PumpGuard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
                tracing::error!(
                    "ethercat tx_rx socket pump EXITED (return or panic) — \
                     the supervise loop will rebuild the transport"
                );
            }
        }
        let _g = PumpGuard(pump_dead);
        if let Err(e) = tx_rx.await {
            tracing::error!(?e, "ethercat tx_rx task exited");
        }
    })
    .detach();
    Ok(maindevice)
}

/// RTSO-HOLD-0731 fix: backoff before the Nth consecutive failed bus
/// re-walk. Attempt 0 (the walk right after a demotion) is immediate —
/// the common case is a bus that is back and just needs the ESM walk.
fn reinit_backoff(attempt: u64) -> Duration {
    match attempt {
        0 => Duration::ZERO,
        1 => Duration::from_secs(1),
        2 => Duration::from_secs(2),
        3 => Duration::from_secs(4),
        _ => Duration::from_secs(5),
    }
}

/// RTSO-HOLD-0731 fix: shared failure handling for every step of the bus
/// walk. First-init failures abort connect exactly as before (caller must
/// `return`); re-walk failures log and retry (caller must `continue`).
/// Returns TRUE when the caller should abort.
fn walk_fail(
    first_init: bool,
    init_tx: &mpsc::SyncSender<InitResult>,
    attempt: &mut u64,
    msg: String,
) -> bool {
    if first_init {
        let _ = init_tx.send(InitResult::Err(msg));
        return true;
    }
    *attempt += 1;
    tracing::warn!(
        attempt = *attempt,
        error = %msg,
        "ethercat bus re-walk step failed; backing off before retry"
    );
    false
}

/// Health bookkeeping for one failed cyclic exchange. Exactly one ERROR
/// per outage (when the threshold is crossed); WARN before that, DEBUG
/// for the repeats while already unhealthy (a dead bus at a 1 ms cycle
/// would otherwise log a thousand lines a second).
fn note_txrx_failure<E: std::fmt::Debug>(health: &mut HealthTracker, error: &E) {
    match health.record_failure() {
        HealthTransition::BecameUnhealthy => {
            tracing::error!(
                ?error,
                consecutive_failures = health.consecutive_failures(),
                "ethercat unhealthy: cyclic tx_rx failing; inputs frozen at last-known values"
            );
        }
        _ if health.is_healthy() => tracing::warn!(?error, "ethercat tx_rx failed"),
        _ => tracing::debug!(?error, "ethercat tx_rx still failing"),
    }
}

/// RTSO-HOLD-0731 instrumentation: edge-triggered AL-state/WKC logging.
/// The per-cycle `TxRxResponse` already carries every subdevice's AL state
/// and the group working counter; steady state was silently discarding
/// them. Logs one INFO baseline, then one ERROR per change (including the
/// recovery back). Flap suppressor: first 10 edges verbatim, then every
/// 1000th, with the running edge count so nothing is lost statistically.
/// Returns true when this call logged an edge (callers may fetch AL codes).
fn note_bus_shape(
    prev: &mut Option<(Vec<ethercrab::SubDeviceState>, u16)>,
    edges: &mut u64,
    states: &[ethercrab::SubDeviceState],
    wkc: u16,
) -> bool {
    match prev {
        None => {
            tracing::info!(
                ?states,
                wkc,
                "ethercat bus shape baseline (per-slave AL state + WKC)"
            );
            *prev = Some((states.to_vec(), wkc));
            false
        }
        Some((ps, pw)) if ps.as_slice() != states || *pw != wkc => {
            *edges += 1;
            let log_it = *edges <= 10 || edges.is_multiple_of(1000);
            if log_it {
                tracing::error!(
                    prev_states = ?ps,
                    new_states = ?states,
                    prev_wkc = *pw,
                    new_wkc = wkc,
                    edge = *edges,
                    "ethercat bus shape CHANGED mid-run (AL demotion/promotion or WKC step)"
                );
            }
            *prev = Some((states.to_vec(), wkc));
            log_it
        }
        _ => false,
    }
}

/// The entire ethercrab session lives inside this function — bus walk,
/// state-machine transition to OP, and the cyclic exchange loop. Runs on
/// its own thread under `smol::block_on` to drive `async-io`.
#[allow(clippy::too_many_arguments)]
fn smol_main(
    nic: &str,
    cycle_us: u32,
    dc_sync: EthercatDcSync,
    dc_static_sync_iterations: u32,
    slaves: &[EthercatSlave],
    shared: WorkerShared,
    init_tx: mpsc::SyncSender<InitResult>,
) {
    let WorkerShared {
        pdi,
        shutdown,
        stopped,
        healthy,
        cycles,
        input_changes,
        reinitializing,
        reinits,
        mut engines,
    } = shared;
    // Flip `stopped` on EVERY exit path (init failure or loop end) via a
    // drop guard, so a bounded join never waits on a thread that's already
    // gone. Created first thing so even the early `try_split` error returns
    // flag completion.
    struct DoneGuard(Arc<AtomicBool>);
    impl Drop for DoneGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let _done = DoneGuard(stopped);

    let nic_owned = nic.to_string();
    smol::block_on(async move {
        // Transport = leaked PduStorage + MainDevice + detached TX/RX
        // pump. Built once here; rebuilt from a fresh leak if the pump
        // ever dies (see build_transport).
        let pump_dead = Arc::new(AtomicBool::new(false));
        let mut maindevice =
            match build_transport(&nic_owned, dc_static_sync_iterations, pump_dead.clone()) {
                Ok(m) => m,
                Err(msg) => {
                    let _ = init_tx.send(InitResult::Err(msg));
                    return;
                }
            };

        // ===== RTSO-HOLD-0731 fix: supervised re-walk loop =====
        // Everything from bus enumeration through the cyclic exchange sits
        // inside this loop. The first iteration keeps the original connect
        // semantics (InitResult handshake, abort on failure). When the
        // cyclic loop detects any subdevice out of OP it breaks back here
        // and the whole walk re-runs — bus enumeration, init SDO writes
        // (0x6060 = 8 and any PDO remap, which a power-cycled slave has
        // lost), PDO dump, DC config, OP transition — exactly what a
        // process restart used to be needed for. ethercrab's init calls
        // reset_subdevices(), which forces every slave to INIT first, so
        // a SAFE-OP+ERROR latch is cleared by the walk itself.
        let mut first_init = true;
        let mut reinit_attempt: u64 = 0;
        // Set after every successful walk; re-walks must match it (F6:
        // ethercrab's init returns Ok even on an EMPTY bus, and re-walk
        // iterations bypass connect-time validation entirely).
        let mut expected_subdevices: Option<usize> = None;
        let mut health = HealthTracker::with_flag(UNHEALTHY_AFTER_TX_ERRORS, healthy.clone());
        'supervise: loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            if !first_init {
                reinitializing.store(true, Ordering::Relaxed);
                reinits.fetch_add(1, Ordering::Relaxed);
                let delay = reinit_backoff(reinit_attempt);
                if !delay.is_zero() {
                    tracing::info!(
                        attempt = reinit_attempt,
                        delay_ms = delay.as_millis() as u64,
                        "ethercat re-walk backoff"
                    );
                    let mut waited = Duration::ZERO;
                    while waited < delay {
                        if shutdown.load(Ordering::Relaxed) {
                            break 'supervise;
                        }
                        smol::Timer::after(Duration::from_millis(100)).await;
                        waited += Duration::from_millis(100);
                    }
                }
            }

            if pump_dead.load(Ordering::Relaxed) {
                tracing::error!(
                    "rebuilding ethercat transport after pump death \
                     (fresh PduStorage + MainDevice + pump)"
                );
                match build_transport(&nic_owned, dc_static_sync_iterations, pump_dead.clone()) {
                    Ok(m) => {
                        maindevice = m;
                        pump_dead.store(false, Ordering::Relaxed);
                    }
                    Err(msg) => {
                        if walk_fail(first_init, &init_tx, &mut reinit_attempt, msg) {
                            return;
                        }
                        continue 'supervise;
                    }
                }
            }

            // Walk the bus and assign each SubDevice an auto-increment address.
            let mut group = match maindevice
                .init_single_group::<MAX_SUBDEVICES, PDI_LEN>(ethercat_now)
                .await
            {
                Ok(g) => g,
                Err(e) => {
                    if walk_fail(
                        first_init,
                        &init_tx,
                        &mut reinit_attempt,
                        format!("init_single_group: {e:?}"),
                    ) {
                        return;
                    }
                    continue 'supervise;
                }
            };

            // Early bus census: log every SubDevice's identity *now*, in PRE-OP,
            // before any init_sdo / PDO / OP step that a non-matching device can
            // abort (e.g. a coupler with no 0x6060, or one that rejects the CoE
            // 0x1600 PDO-assign). This makes `cs get edges/<n>/scan` work as a pure
            // discovery probe against unknown hardware: you always see what's on
            // the wire, even when the configured device can't reach OP.
            for (pos, sd) in group.iter(&maindevice).enumerate() {
                let id = sd.identity();
                tracing::info!(
                    slave = pos,
                    sd_name = %sd.name(),
                    vendor = format!("{:#010x}", id.vendor_id),
                    product = format!("{:#010x}", id.product_id),
                    revision = format!("{:#010x}", id.revision),
                    serial = format!("{:#010x}", id.serial),
                    "bus census (PRE-OP)"
                );
            }

            // RTSO-HOLD-0731 fix: a re-walk must never accept a bus that
            // does not match the one connect() validated. ethercrab's
            // init returns Ok on an EMPTY bus (BRD WKC 0), and re-walk
            // iterations bypass connect-time validation entirely —
            // without this guard a vanished or swapped bus would
            // "succeed" straight back into the silent-lie failure mode
            // this loop exists to kill. A changed bus keeps retrying
            // until the expected composition returns (or the process is
            // restarted against a new config).
            let found_subdevices = group.iter(&maindevice).count();
            if let Some(expected) = expected_subdevices {
                if found_subdevices != expected {
                    if walk_fail(
                        first_init,
                        &init_tx,
                        &mut reinit_attempt,
                        format!(
                            "re-walked bus has {found_subdevices} subdevices, expected {expected}"
                        ),
                    ) {
                        return;
                    }
                    continue 'supervise;
                }
                let mut identity_mismatch: Option<String> = None;
                for (pos, sd) in group.iter(&maindevice).enumerate() {
                    if let Some(cfg) = slaves.iter().find(|s| s.index == pos as u16) {
                        let id = sd.identity();
                        if id.vendor_id != cfg.vendor_id || id.product_id != cfg.product_id {
                            identity_mismatch = Some(format!(
                                "slave {pos} identity changed across re-walk: \
                                 found {:#010x}/{:#010x}, configured {:#010x}/{:#010x}",
                                id.vendor_id, id.product_id, cfg.vendor_id, cfg.product_id
                            ));
                            break;
                        }
                    }
                }
                if let Some(msg) = identity_mismatch {
                    if walk_fail(first_init, &init_tx, &mut reinit_attempt, msg) {
                        return;
                    }
                    continue 'supervise;
                }
            }

            // Per-SubDevice startup SDO writes (PRE-OP, mailboxes are up).
            // Runs before the PDO-mapping dump below so the logged layout
            // reflects any remapping done here. A failed write aborts init:
            // these are things like 0x6060 = 8 (CSP) — silently running a
            // drive in the wrong mode is worse than not starting.
            for (pos, sd) in group.iter(&maindevice).enumerate() {
                let Some(cfg) = slaves.iter().find(|s| s.index == pos as u16) else {
                    continue;
                };
                for cmd in &cfg.init_sdo {
                    let res = match cmd.bits {
                        8 => {
                            sd.sdo_write(cmd.index, cmd.sub_index, cmd.value as u8)
                                .await
                        }
                        16 => {
                            sd.sdo_write(cmd.index, cmd.sub_index, cmd.value as u16)
                                .await
                        }
                        // validate_init_sdo limited bits to {8, 16, 32}.
                        _ => {
                            sd.sdo_write(cmd.index, cmd.sub_index, cmd.value as u32)
                                .await
                        }
                    };
                    match res {
                        Ok(()) => tracing::info!(
                            slave = pos,
                            obj = format!("{:#06x}:{:02x}", cmd.index, cmd.sub_index),
                            value = cmd.value,
                            bits = cmd.bits,
                            "init sdo write"
                        ),
                        Err(e) => {
                            if walk_fail(
                            first_init,
                            &init_tx,
                            &mut reinit_attempt,
                            format!(
                                "init sdo write {:#06x}:{:02x} = {} ({} bits) on slave {pos}: {e:?}",
                                cmd.index, cmd.sub_index, cmd.value, cmd.bits
                            ),
                        ) {
                            return;
                        }
                            continue 'supervise;
                        }
                    }
                }
            }

            // One-time: read + log the CoE PDO mapping (0x1C12 RxPDO-assign /
            // 0x1C13 TxPDO-assign -> 0x16xx / 0x1Axx entries). Surfaces the exact
            // byte offset of controlword / target_velocity / statusword / etc. in
            // the logs, so iomap channels are configured off the real layout
            // rather than guessed. Reads happen in PRE-OP where CoE is available.
            for sd in group.iter(&maindevice) {
                for (assign, dir) in [(0x1C12u16, "out/rxpdo"), (0x1C13u16, "in/txpdo")] {
                    let count: u8 = sd.sdo_read(assign, 0u8).await.unwrap_or(0);
                    let mut bit_off: u32 = 0;
                    for i in 1..=count {
                        let pdo: u16 = match sd.sdo_read(assign, i).await {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let entries: u8 = sd.sdo_read(pdo, 0u8).await.unwrap_or(0);
                        for j in 1..=entries {
                            let entry: u32 = match sd.sdo_read(pdo, j).await {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            let obj = (entry >> 16) as u16;
                            let sub = ((entry >> 8) & 0xff) as u8;
                            let bits = (entry & 0xff) as u8;
                            tracing::info!(
                                dir,
                                pdo = format!("{pdo:#06x}"),
                                obj = format!("{obj:#06x}:{sub:02x}"),
                                bits,
                                byte = bit_off / 8,
                                "pdo entry"
                            );
                            bit_off += bits as u32;
                        }
                    }
                }
            }

            let sync0 = Duration::from_micros(cycle_us as u64);

            // Effective DC mode per SubDevice: per-slave override if listed in
            // the config, else the device-level default. The bus takes the DC
            // path when any SubDevice ends up Sync0 — SubDevices left Off
            // aren't flagged, and ethercrab's configure_dc_sync skips unflagged
            // ones, so plain IO couplers free-run inside a DC bus.
            let effective_dc = |pos: u16| {
                slaves
                    .iter()
                    .find(|s| s.index == pos)
                    .and_then(|s| s.dc_sync)
                    .unwrap_or(dc_sync)
            };
            let subdevice_count = group.iter(&maindevice).count() as u16;
            let bus_dc = if (0..subdevice_count).any(|p| effective_dc(p) == EthercatDcSync::Sync0) {
                EthercatDcSync::Sync0
            } else {
                EthercatDcSync::Off
            };

            match bus_dc {
                EthercatDcSync::Sync0 => {
                    // Servo drives (e.g. Inovance SV660N) need DC SYNC0 to
                    // reach OP. Flag SYNC0 on the SubDevices that want it,
                    // configure the group DC, then *request* OP and cycle
                    // tx_rx_dc until all OP — a blocking into_op() doesn't pump
                    // PDI, so the drive's SyncManager watchdog would trip
                    // during SAFE-OP -> OP.
                    for (pos, mut subdevice) in group.iter_mut(&maindevice).enumerate() {
                        if effective_dc(pos as u16) == EthercatDcSync::Sync0 {
                            subdevice.set_dc_sync(DcSync::Sync0);
                        }
                    }
                    let group = match group.into_pre_op_pdi(&maindevice).await {
                        Ok(g) => g,
                        Err(e) => {
                            if walk_fail(
                                first_init,
                                &init_tx,
                                &mut reinit_attempt,
                                format!("into_pre_op_pdi (PRE-OP+PDI): {e:?}"),
                            ) {
                                return;
                            }
                            continue 'supervise;
                        }
                    };
                    let group = match group
                        .configure_dc_sync(
                            &maindevice,
                            DcConfiguration {
                                // Start SYNC0 100ms out; period = the cycle; send
                                // data half-way through the cycle.
                                start_delay: Duration::from_millis(100),
                                sync0_period: sync0,
                                sync0_shift: sync0 / 2,
                            },
                        )
                        .await
                    {
                        Ok(g) => g,
                        Err(e) => {
                            if walk_fail(
                                first_init,
                                &init_tx,
                                &mut reinit_attempt,
                                format!("configure_dc_sync: {e:?}"),
                            ) {
                                return;
                            }
                            continue 'supervise;
                        }
                    };
                    let group = match group.request_into_op(&maindevice).await {
                        Ok(g) => g,
                        Err(e) => {
                            if walk_fail(
                                first_init,
                                &init_tx,
                                &mut reinit_attempt,
                                format!("request_into_op (-> request OP): {e:?}"),
                            ) {
                                return;
                            }
                            continue 'supervise;
                        }
                    };

                    // Capture discovery before confirming OP so the topology is
                    // visible even if OP never settles.
                    let discovered = capture_discovery!(group, maindevice, pdi);

                    // Pump tx_rx_dc until every SubDevice reaches OP (zero
                    // outputs / controlword 0 — nothing moves). Bounded.
                    {
                        let deadline = std::time::Instant::now() + Duration::from_secs(10);
                        let mut reached_op = false;
                        while !shutdown.load(Ordering::Relaxed)
                            && std::time::Instant::now() < deadline
                        {
                            match group.tx_rx_dc(&maindevice).await {
                                Ok(resp) => {
                                    if resp.all_op() {
                                        reached_op = true;
                                        break;
                                    }
                                }
                                Err(e) => tracing::warn!(?e, "tx_rx_dc while waiting for OP"),
                            }
                            smol::Timer::after(sync0).await;
                        }
                        if !reached_op {
                            if walk_fail(
                            first_init,
                            &init_tx,
                            &mut reinit_attempt,
                            "SubDevices did not reach OP within 10s (SyncManager watchdog / DC?)"
                                .into(),
                        ) {
                            return;
                        }
                            continue 'supervise;
                        }
                        tracing::info!("all subdevices reached OP (dc=sync0)");
                    }

                    if first_init {
                        let _ = init_tx.send(InitResult::Ok { discovered });
                        first_init = false;
                    } else {
                        tracing::info!(
                            attempt = reinit_attempt,
                            reinits = reinits.load(Ordering::Relaxed),
                            "ethercat bus re-walked to OP; cyclic exchange resuming"
                        );
                    }
                    reinit_attempt = 0;
                    expected_subdevices = Some(found_subdevices);
                    reinitializing.store(false, Ordering::Relaxed);

                    // DC cyclic loop: tx_rx_dc keeps the reference clock synced
                    // and its CycleInfo tells us when to send the next frame
                    // (stays aligned to SYNC0). The health tracker is hoisted
                    // above the supervise loop (with_flag resets the shared
                    // flag to healthy, which must not happen mid-re-walk).
                    let mut demoted = false;
                    // Tracks whether the *previous* exchange succeeded — the gear
                    // engines read inputs captured by that exchange, so on a
                    // failed cycle they freeze (no master advance / target held)
                    // and the bus recovers without a one-cycle catch-up step.
                    let mut bus_ok = true;
                    let mut prev_shape: Option<(Vec<ethercrab::SubDeviceState>, u16)> = None;
                    let mut shape_edges: u64 = 0;
                    while !shutdown.load(Ordering::Relaxed) {
                        cycles.fetch_add(1, Ordering::Relaxed);
                        let cycle_start = std::time::Instant::now();
                        copy_outputs_to_bus!(group, maindevice, pdi);
                        gear_tick!(group, maindevice, engines, bus_ok);
                        let next_wait = match group.tx_rx_dc(&maindevice).await {
                            Ok(resp) => {
                                let edge = note_bus_shape(
                                    &mut prev_shape,
                                    &mut shape_edges,
                                    &resp.subdevice_states,
                                    resp.working_counter,
                                );
                                if edge {
                                    // Why did it demote? Fetch the AL status code
                                    // for every non-OP subdevice. Edge cycles only
                                    // (1-2 extra PDUs, can push this one cycle past
                                    // its 2 ms slot — accepted: the edge IS the
                                    // diagnostic moment).
                                    for (i, st) in resp.subdevice_states.iter().enumerate() {
                                        if *st != ethercrab::SubDeviceState::Op {
                                            let sd = group.subdevice(&maindevice, i);
                                            match sd {
                                                Ok(sd) => match sd.status().await {
                                                    Ok((state, code)) => tracing::error!(
                                                        slave = i,
                                                        ?state,
                                                        ?code,
                                                        "AL status code for demoted subdevice"
                                                    ),
                                                    // A demoted slave with its AL error
                                                    // flag set surfaces the code through
                                                    // state()'s Err path (ethercrab wraps
                                                    // it in Error::SubDevice) — that IS
                                                    // the answer, not a read failure.
                                                    // Seen live 2026-08-27: SafeOp slaves
                                                    // reported SyncManagerWatchdog /
                                                    // SynchronizationError this way.
                                                    Err(ethercrab::error::Error::SubDevice(
                                                        code,
                                                    )) => {
                                                        tracing::error!(
                                                        slave = i,
                                                        ?code,
                                                        "AL status code for demoted subdevice (AL error flag set)"
                                                    )
                                                    }
                                                    Err(e) => tracing::warn!(
                                                        slave = i,
                                                        ?e,
                                                        "AL status code read failed"
                                                    ),
                                                },
                                                Err(e) => tracing::warn!(
                                                    slave = i,
                                                    ?e,
                                                    "subdevice ref for AL code failed"
                                                ),
                                            }
                                        }
                                    }
                                }
                                // Capture the last valid inputs first —
                                // SAFE-OP still serves them — then decide.
                                copy_inputs_from_bus!(group, maindevice, pdi, input_changes);
                                if resp
                                    .subdevice_states
                                    .iter()
                                    .any(|s| *s != ethercrab::SubDeviceState::Op)
                                {
                                    // The RTSO-HOLD-0731 mechanism: exchange
                                    // still succeeds but the slave no longer
                                    // applies outputs. Leave the cyclic loop
                                    // and re-walk instead of exchanging with
                                    // a bus that silently ignores us.
                                    healthy.store(false, Ordering::Relaxed);
                                    demoted = true;
                                    break;
                                }
                                if health.record_success() == HealthTransition::Recovered {
                                    tracing::info!(
                                        "ethercat recovered; cyclic exchange running again"
                                    );
                                }
                                bus_ok = true;
                                resp.extra.next_cycle_wait
                            }
                            Err(e) => {
                                note_txrx_failure(&mut health, &e);
                                if pump_dead.load(Ordering::Relaxed) {
                                    // A dead pump cannot recover by retrying
                                    // tx_rx — leave for a transport rebuild.
                                    healthy.store(false, Ordering::Relaxed);
                                    demoted = true;
                                    break;
                                }
                                bus_ok = false;
                                sync0
                            }
                        };
                        smol::Timer::at(cycle_start + next_wait).await;
                    }
                    if demoted && !shutdown.load(Ordering::Relaxed) {
                        tracing::error!(
                            "subdevice(s) out of OP mid-run — re-walking the bus \
                         (previously this wedged until a process restart)"
                        );
                    } else {
                        // Final flush before teardown: failsafe has zeroed the
                        // output mirror; push it out once more so the drive latches
                        // controlword = 0 (Disable Voltage) before the thread stops,
                        // instead of de-energizing only via its own SyncManager
                        // watchdog once the master goes away.
                        copy_outputs_to_bus!(group, maindevice, pdi);
                        let _ = group.tx_rx_dc(&maindevice).await;
                        tracing::info!("ethercat cyclic loop exiting (shutdown signalled)");
                    }
                }

                EthercatDcSync::Off => {
                    // Free-run (no DC): a blocking into_op works for IO couplers
                    // / SubDevices that don't need (or can't do) DC. Then a
                    // fixed-interval tx_rx loop.
                    let group = match group.into_op(&maindevice).await {
                        Ok(g) => g,
                        Err(e) => {
                            if walk_fail(
                                first_init,
                                &init_tx,
                                &mut reinit_attempt,
                                format!("into_op (PRE-OP -> OP): {e:?}"),
                            ) {
                                return;
                            }
                            continue 'supervise;
                        }
                    };

                    let discovered = capture_discovery!(group, maindevice, pdi);
                    if first_init {
                        let _ = init_tx.send(InitResult::Ok { discovered });
                        first_init = false;
                    } else {
                        tracing::info!(
                            attempt = reinit_attempt,
                            reinits = reinits.load(Ordering::Relaxed),
                            "ethercat bus re-walked to OP; cyclic exchange resuming"
                        );
                    }
                    reinit_attempt = 0;
                    expected_subdevices = Some(found_subdevices);
                    reinitializing.store(false, Ordering::Relaxed);

                    let mut tick = smol::Timer::interval(sync0);
                    use smol::stream::StreamExt;
                    // Health tracker hoisted above the supervise loop.
                    let mut demoted = false;
                    let mut bus_ok = true;
                    let mut prev_shape: Option<(Vec<ethercrab::SubDeviceState>, u16)> = None;
                    let mut shape_edges: u64 = 0;
                    while !shutdown.load(Ordering::Relaxed) {
                        cycles.fetch_add(1, Ordering::Relaxed);
                        copy_outputs_to_bus!(group, maindevice, pdi);
                        gear_tick!(group, maindevice, engines, bus_ok);
                        match group.tx_rx(&maindevice).await {
                            Ok(resp) => {
                                note_bus_shape(
                                    &mut prev_shape,
                                    &mut shape_edges,
                                    &resp.subdevice_states,
                                    resp.working_counter,
                                );
                                if resp
                                    .subdevice_states
                                    .iter()
                                    .any(|s| *s != ethercrab::SubDeviceState::Op)
                                {
                                    // Break is deferred until after the input
                                    // copy below — the last snapshot is still
                                    // worth capturing.
                                    healthy.store(false, Ordering::Relaxed);
                                    demoted = true;
                                } else if health.record_success() == HealthTransition::Recovered {
                                    tracing::info!(
                                        "ethercat recovered; cyclic exchange running again"
                                    );
                                }
                                bus_ok = true;
                            }
                            Err(e) => {
                                note_txrx_failure(&mut health, &e);
                                if pump_dead.load(Ordering::Relaxed) {
                                    healthy.store(false, Ordering::Relaxed);
                                    demoted = true;
                                }
                                bus_ok = false;
                            }
                        }
                        copy_inputs_from_bus!(group, maindevice, pdi, input_changes);
                        if demoted {
                            break;
                        }
                        tick.next().await;
                    }
                    if demoted && !shutdown.load(Ordering::Relaxed) {
                        tracing::error!(
                            "subdevice(s) out of OP mid-run — re-walking the bus \
                         (previously this wedged until a process restart)"
                        );
                    } else {
                        // Final flush before teardown: failsafe has zeroed the
                        // output mirror; push it out once more so the SubDevices
                        // latch their safe (zero) outputs before the thread stops.
                        copy_outputs_to_bus!(group, maindevice, pdi);
                        let _ = group.tx_rx(&maindevice).await;
                        tracing::info!("ethercat cyclic loop exiting (shutdown signalled)");
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // The bus-side paths need a real NIC + CAP_NET_RAW, so these exercise
    // the bounded-join logic in isolation — that's the part that has to
    // hold the line on shutdown latency regardless of bus health.

    fn device_with_outputs(stopped_flag: bool) -> RealEthercat {
        let mut outputs = HashMap::new();
        outputs.insert(0u16, vec![0xffu8; 4]);
        RealEthercat {
            name: "axis".into(),
            channels: HashMap::new(),
            gear_routing: crate::gear::GearRouting {
                writes: HashMap::new(),
                reads: HashMap::new(),
            },
            pdi: Arc::new(Mutex::new(PdiMirror {
                inputs: HashMap::new(),
                outputs,
            })),
            shutdown: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(stopped_flag)),
            healthy: Arc::new(AtomicBool::new(true)),
            discovered: Vec::new(),
            _thread: None,
        }
    }

    /// Zeroing the mirror is not the failsafe — the worker's next tick
    /// putting it on the wire is. With the worker alive that tick is coming,
    /// so `Ok` is the truth.
    #[tokio::test]
    async fn failsafe_zeroes_the_output_mirror_and_reports_ok_while_the_worker_runs() {
        let mut dev = device_with_outputs(false);
        dev.enter_failsafe().await.expect("worker alive => Ok");
        assert_eq!(
            dev.pdi.lock().expect("mirror").outputs[&0],
            vec![0u8; 4],
            "the output image must be zeroed regardless"
        );
    }

    /// With the worker stopped no frame will ever carry those zeros. The
    /// mirror is still zeroed — if anything revives, the safe image is what
    /// it finds — but reporting Ok would tell the caller a safe state was
    /// commanded when nothing was transmitted.
    #[tokio::test]
    async fn failsafe_reports_failure_when_the_cyclic_worker_has_stopped() {
        let mut dev = device_with_outputs(true);
        let err = match dev.enter_failsafe().await {
            Err(e) => e.to_string(),
            Ok(()) => panic!("a dead worker cannot deliver a failsafe"),
        };
        assert!(err.contains("cyclic worker has stopped"), "{err}");
        assert!(
            err.contains("SyncManager"),
            "the fallback mechanism is named: {err}"
        );
        assert_eq!(
            dev.pdi.lock().expect("mirror").outputs[&0],
            vec![0u8; 4],
            "the mirror is zeroed even when it cannot be sent"
        );
    }

    #[test]
    fn reinit_backoff_is_immediate_then_capped() {
        // The walk right after a demotion must be immediate: the common
        // case is a bus that is back and just needs the ESM walk.
        assert_eq!(reinit_backoff(0), Duration::ZERO);
        assert_eq!(reinit_backoff(1), Duration::from_secs(1));
        assert_eq!(reinit_backoff(2), Duration::from_secs(2));
        assert_eq!(reinit_backoff(3), Duration::from_secs(4));
        // Cap: an unplugged cable must not grow the delay unboundedly.
        assert_eq!(reinit_backoff(4), Duration::from_secs(5));
        assert_eq!(reinit_backoff(1000), Duration::from_secs(5));
    }

    #[test]
    fn walk_fail_aborts_first_init_and_retries_reinit() {
        let (tx, rx) = mpsc::sync_channel::<InitResult>(1);
        let mut attempt = 0u64;
        // First init: abort, and the error reaches the connect handshake.
        assert!(walk_fail(true, &tx, &mut attempt, "boom".into()));
        assert!(matches!(rx.try_recv(), Ok(InitResult::Err(m)) if m == "boom"));
        assert_eq!(attempt, 0, "first-init failure is not a retry");
        // Re-walk: no abort, no handshake message, attempt counted.
        assert!(!walk_fail(false, &tx, &mut attempt, "boom again".into()));
        assert!(
            rx.try_recv().is_err(),
            "re-walk failures never touch init_tx"
        );
        assert_eq!(attempt, 1);
        assert!(!walk_fail(false, &tx, &mut attempt, "boom x3".into()));
        assert_eq!(attempt, 2);
    }

    #[test]
    fn watchdog_step_latches_stall_and_recovery_edges() {
        // Advancing counter, not stalled: no edge, stays clear.
        assert_eq!(watchdog_step(10, 11, false), (false, None));
        // Counter froze: stall edge fires exactly once...
        assert_eq!(watchdog_step(11, 11, false), (true, Some(true)));
        // ...and repeats of the frozen state are silent.
        assert_eq!(watchdog_step(11, 11, true), (true, None));
        // Counter moves again: recovery edge fires exactly once...
        assert_eq!(watchdog_step(11, 12, true), (false, Some(false)));
        // ...and steady advance stays silent again.
        assert_eq!(watchdog_step(12, 13, false), (false, None));
    }

    #[test]
    fn bus_shape_logs_baseline_then_edges_only() {
        use ethercrab::SubDeviceState as S;
        let mut prev = None;
        let mut edges = 0u64;
        let base = [S::Op, S::Op, S::Op];
        // First call: baseline, not an edge.
        assert!(!note_bus_shape(&mut prev, &mut edges, &base, 9));
        assert_eq!(edges, 0);
        // Unchanged shape: silent.
        assert!(!note_bus_shape(&mut prev, &mut edges, &base, 9));
        // AL demotion: edge.
        let demoted = [S::Op, S::Op, S::SafeOp];
        assert!(note_bus_shape(&mut prev, &mut edges, &demoted, 9));
        assert_eq!(edges, 1);
        // Same demoted shape again: silent (edge-triggered, not per-cycle).
        assert!(!note_bus_shape(&mut prev, &mut edges, &demoted, 9));
        // WKC step alone is also an edge.
        assert!(note_bus_shape(&mut prev, &mut edges, &demoted, 8));
        assert_eq!(edges, 2);
        // Recovery back to baseline is an edge too.
        assert!(note_bus_shape(&mut prev, &mut edges, &base, 9));
        assert_eq!(edges, 3);
    }

    #[test]
    fn bus_shape_flap_suppressor_caps_logging() {
        use ethercrab::SubDeviceState as S;
        let mut prev = None;
        let mut edges = 0u64;
        let a = [S::Op];
        let b = [S::SafeOp];
        assert!(!note_bus_shape(&mut prev, &mut edges, &a, 1)); // baseline
        let mut logged = 0;
        for i in 0..2000 {
            let shape: &[S] = if i % 2 == 0 { &b } else { &a };
            if note_bus_shape(&mut prev, &mut edges, shape, 1) {
                logged += 1;
            }
        }
        assert_eq!(edges, 2000);
        // First 10 verbatim + every 1000th thereafter: 10 + 2 = 12.
        assert_eq!(logged, 12);
    }

    #[test]
    fn join_worker_joins_a_thread_that_stops() {
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            flag.store(true, Ordering::Relaxed);
        });

        let start = Instant::now();
        assert!(
            join_worker(&stopped, Some(worker), WORKER_JOIN_TIMEOUT),
            "should report a successful join once the worker stops"
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "join should return promptly after the worker stops"
        );
    }

    #[test]
    fn join_worker_returns_immediately_if_already_stopped() {
        let stopped = Arc::new(AtomicBool::new(true));
        let start = Instant::now();
        // No handle: the common Drop-already-ran case must not block.
        assert!(join_worker(&stopped, None, WORKER_JOIN_TIMEOUT));
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn join_worker_abandons_a_thread_that_never_stops() {
        let stopped = Arc::new(AtomicBool::new(false));
        // A worker that outlives the (short) timeout and never flags stop.
        let worker = thread::spawn(|| thread::sleep(Duration::from_millis(500)));

        let start = Instant::now();
        assert!(
            !join_worker(&stopped, Some(worker), Duration::from_millis(40)),
            "should give up rather than block on a worker that won't stop"
        );
        assert!(
            start.elapsed() < Duration::from_millis(300),
            "must return shortly after the timeout, not wait out the worker"
        );
    }
}
