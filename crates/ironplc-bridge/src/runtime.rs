//! Bridges the ironplc VM scan loop to:
//!  - `tokio::sync::broadcast` for streaming `VarSnapshot`s to subscribers.
//!  - `iocore::IoDevice` adapters for reading inputs before `run_round` and
//!    writing outputs after.
//!
//! The scan thread is a dedicated `std::thread` that hosts a single-thread
//! tokio runtime; everything bus-related runs inside it. ironplc's
//! `VmRunning::run_round` itself is sync.
//!
//! Multi-PROGRAM execution (ADR-0001): the one scan thread hosts N
//! `VmRunning` instances ("units"), one per scheduled PROGRAM instance,
//! each compiled into its own `Container`. Every unit keeps its own
//! cadence anchor from its task's interval; units due on the same tick
//! run in task-priority order (then tasks.toml declaration order).
//! Devices stay owned by the thread and are shared across units
//! sequentially — no concurrency on the bus.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::retain;
use iocore::{ChannelValue, IoDevice};
use ironplc_container::debug_format::{build_var_debug_map, format_variable_value, VarDebugInfo};
use ironplc_container::debug_section::iec_type_tag;
use ironplc_container::Container;
use ironplc_container::VarIndex;
use ironplc_container::STRING_HEADER_BYTES;
use ironplc_vm::{Vm, VmBuffers, VmRunning};
use project::{Direction, Mapping, ProtocolConfig, WriteGovernance, WriteMode};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct VarValue {
    pub name: String,
    pub type_name: String,
    /// Display-formatted value (IDE-facing): `12.5`, `16#1637`,
    /// `TRUE`, `'text'`.
    pub value: String,
    /// Raw 64-bit VM slot the display string was formatted from.
    /// Decode with `monitor::typed_value` (REAL = IEEE-754 bits in the
    /// low 32, LREAL = full 64). STRING values don't live in the slot —
    /// their bits are 0 and `value` is authoritative.
    ///
    /// NOTE for JS consumers: serialized as a JSON number, exact only
    /// up to 2^53 — decode bits in Rust (northbound/alarms do), not in
    /// the browser.
    #[ts(type = "number")]
    pub bits: u64,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct VarSnapshot {
    pub timestamp_us: u64,
    pub scan_count: u64,
    pub vars: Vec<VarValue>,
}

/// One subdevice on an EtherCAT bus, as reported by `/discover`.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct DiscoveredSlave {
    pub index: u16,
    pub name: String,
    pub vendor_id: u32,
    pub product_id: u32,
    pub input_bytes: u16,
    pub output_bytes: u16,
}

/// Per-device connect outcome plus (for EtherCAT) the discovered bus
/// topology. Surfaced by the runtime's `/discover` endpoint so the IDE
/// can see which devices actually connected, why a connect failed, and
/// what's on the bus — the truth that otherwise only hits the logs.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct DeviceReport {
    pub name: String,
    /// `"modbus"` | `"ethercat"`.
    pub protocol: String,
    pub connected: bool,
    /// Connect error (first line) when `connected` is false.
    pub error: Option<String>,
    /// EtherCAT subdevices (empty for Modbus / failed connects).
    pub slaves: Vec<DiscoveredSlave>,
}

/// Live transport health of one configured device, refreshed by the scan
/// loop every round from [`IoDevice::is_healthy`]. This is the monitor-layer
/// consumer that flag was designed for: while a device's background link is
/// down its inputs keep serving the last-known mirror, and nothing in the
/// variable values themselves reveals that — only this flag does.
///
/// Devices that failed the initial connect appear here too, permanently
/// `healthy: false`, so a project configured for a device that never came
/// up is just as visible as one that dropped mid-run.
/// `Deserialize` as well as `Serialize`: the IDE server parses this back
/// out of a *remote* edge runtime's `/health` when probing it, so the same
/// type describes local and edge fieldbus health.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DeviceHealth {
    pub name: String,
    /// `true` = transport believed good, variable values are live.
    /// `false` = connect failed or the background link is down; input
    /// variables are frozen at last-known values.
    pub healthy: bool,
}

#[derive(Clone, Debug)]
pub struct DeviceSpec {
    pub name: String,
    pub config: ProtocolConfig,
}

/// An applied write, plus whether it can currently reach the field.
///
/// `value` always landed in the VM. A refusal would be the wrong answer
/// here: the variable may be a Stop, and a Stop that reaches the plant
/// late when the link comes back beats one that was never commanded.
/// What must not happen is reporting a bare success — the operator would
/// read "sent" as "stopped".
///
/// `undelivered_device` names the device whose transport is down, when the
/// variable has an Output mapping onto one. `None` means either the value
/// is reaching the field or the variable is internal to the program (no
/// Output mapping — nothing to deliver).
///
/// This is transport health only. A healthy link does NOT prove the value
/// took effect: a slave can sit in SAFEOP, mask its own outputs, or hold
/// the failsafe zeros after a watchdog trip. The HMI's Stop is not a
/// safety function under any of these outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOutcome {
    pub value: i32,
    pub undelivered_device: Option<String>,
}

/// Reasons a variable write can't be honoured.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeWriteError {
    #[error("unknown variable '{0}' — not in the program's debug map")]
    UnknownVariable(String),
    #[error("scan loop has stopped — no writes will reach the VM")]
    Disconnected,
    #[error("vm trap during write: {0}")]
    Vm(String),
    /// Denied by the project's `[governance]` table. The payload is the
    /// variable name followed by the specific reason in parentheses —
    /// the HTTP layers wrap it verbatim, so the caller always learns
    /// WHY (no rule, degenerate rule bounds, unclampable value, …).
    #[error("write to '{0}' rejected by project governance")]
    GovernanceDenied(String),
}

/// Out-of-band commands the scan loop drains between rounds. Used so
/// HTTP handlers (in the server's tokio runtime) can poke the VM (which
/// lives on a dedicated std::thread + current_thread runtime).
// The "Variable" postfix on every variant is intentional — these are
// commands that target a named runtime variable, and the postfix makes
// `RuntimeCommand::WriteVariable` self-documenting at the call site.
#[allow(clippy::enum_variant_names)]
enum RuntimeCommand {
    /// One-shot variable write — applied once, may be overwritten by
    /// the program in subsequent scans. Use `ForceVariable` to keep a
    /// value pinned. `governed: false` marks the runtime's OWN internal
    /// writes (the monitor's pulse reset), which bypass the project's
    /// write governance — see `ProgramHandle::write_variable_ungoverned`.
    WriteVariable {
        name: String,
        value: i32,
        governed: bool,
        ack: tokio::sync::oneshot::Sender<Result<WriteOutcome, RuntimeWriteError>>,
    },
    /// Pin a variable's value: every scan begins by writing `value`
    /// into the VM after the input phase but before `run_round`, so the
    /// program sees the forced value and field outputs reflect it.
    /// Calling Force again with the same name updates the value.
    ForceVariable {
        name: String,
        value: i32,
        ack: tokio::sync::oneshot::Sender<Result<i32, RuntimeWriteError>>,
    },
    /// Stop pinning a variable. The variable resumes normal program-
    /// driven behaviour from the next scan onwards. No-op if the name
    /// wasn't currently forced.
    UnforceVariable {
        name: String,
        ack: tokio::sync::oneshot::Sender<Result<(), RuntimeWriteError>>,
    },
    /// Fault injection: stall each of the next `scans` **ticks** by
    /// `stall_ms` of real wall-clock time. The stall happens INSIDE the
    /// measured scan window, so the cadence check at the bottom of the
    /// scan sees a genuinely late scan and the overrun watchdog trips
    /// through its production code path — no test-only shortcut. This is
    /// the only way a `cs sim` scenario can drive a timing fault
    /// deterministically; a CPU-burn program depends on host speed and
    /// proves nothing on a fast machine.
    InjectScanStall {
        stall_ms: u64,
        scans: u32,
        ack: tokio::sync::oneshot::Sender<()>,
    },
}

/// Scan-loop execution mode. Mutated by the HTTP API / CLI via
/// `ProgramHandle::pause / resume / step`; the scan loop reads it at
/// the top of every round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeMode {
    /// Default — run continuously at the configured cycle rate.
    Running,
    /// Halt scan execution. IO inputs are NOT read, outputs are NOT
    /// written, the program does NOT advance. Variable writes /
    /// force commands still take effect (they're delivered between
    /// rounds, not inside a round), so an operator can manually
    /// stage a state while paused. This is "freeze the plant"
    /// semantics — safer than "freeze the program but keep the bus
    /// running".
    Paused,
    /// Run `remaining` scan cycles then automatically transition to
    /// `Paused`. Decremented atomically at the bottom of each cycle.
    Step { remaining: u32 },
}

/// Cheap-to-clone handle to a running scan loop. Multiple clones share the
/// same `stop` flag, snapshot fan-out, and command queue. Drop the last
/// clone to let resources go; explicit `.stop()` is preferred for clean
/// shutdown.
#[derive(Clone)]
pub struct ProgramHandle {
    stop: Arc<AtomicBool>,
    snapshot_tx: broadcast::Sender<VarSnapshot>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<RuntimeCommand>,
    /// Shared mutable mode. `Arc<Mutex>` because the scan loop reads
    /// it once per cycle and the HTTP layer writes to it from an
    /// arbitrary tokio thread — a Mutex is cheap relative to a scan
    /// round, and atomicity over the `Step { remaining }` payload
    /// matters.
    mode: Arc<std::sync::Mutex<RuntimeMode>>,
    /// Currently-forced variables: name → pinned i32 value. The scan
    /// loop applies these in order on every cycle (after input read,
    /// before run_round). Mirrored here so the HTTP layer can return
    /// the active force set without a round-trip through the cmd
    /// queue.
    forces: Arc<std::sync::Mutex<HashMap<String, i32>>>,
    /// Scan deadlines missed since this program started, summed over units,
    /// and the longest streak any unit is currently on.
    ///
    /// The log says this ONCE per unit and then suppresses itself, so after
    /// the first line scrolls away nothing answers "is it still overrunning?".
    /// `watchdog_tripped` only answers the terminal case. These two cover the
    /// middle: a total that climbs between polls means deadlines are being
    /// missed right now, and the streak is the distance to
    /// [`WATCHDOG_OVERRUN_THRESHOLD`].
    ///
    /// They describe the ACHIEVED cadence, which `scan_period_ms` — a
    /// configured number — cannot.
    scan_overruns: Arc<AtomicU64>,
    consecutive_scan_overruns: Arc<AtomicU32>,
    /// The fastest unit's task interval, in ms, normalised exactly as
    /// `UnitClock::interval` is. Fixed for the life of a run, so a plain
    /// value rather than a shared cell.
    ///
    /// This is the cadence at which a snapshot's `scan_count` can advance —
    /// `scan_count` is the max across units, so the fastest task sets it.
    /// Clients that judge "is this live data current?" need it: `interval_ms`
    /// has no upper bound, and on a 5 s-cycle project a 4 s-old value is the
    /// newest one that exists. Without this they have to guess from observed
    /// gaps, which a stalled stream inflates.
    scan_period_ms: u32,
    /// Per-device connect reports (connected/failed + EtherCAT topology),
    /// set once after the initial connect pass. Shared so the HTTP layer
    /// can serve /discover without a scan-loop round-trip.
    device_reports: Arc<std::sync::Mutex<Vec<DeviceReport>>>,
    /// Live per-device transport health, seeded after the connect pass and
    /// refreshed by the scan loop each round. Shared so the HTTP layer can
    /// serve health without a scan-loop round-trip.
    device_health: Arc<std::sync::Mutex<Vec<DeviceHealth>>>,
    /// Latched `true` once the scan watchdog has tripped: outputs are held
    /// at the zeros `enter_failsafe` wrote and the output phase no longer
    /// runs. Monotonic — only a restart clears it.
    ///
    /// Shared for the same reason `device_health` is: after a trip the VM
    /// keeps computing, `scan_count` keeps advancing and every variable
    /// looks normal, so nothing in a snapshot reveals that the plant has
    /// stopped being driven. Without this flag a UI cannot tell a healthy
    /// run from a latched one.
    watchdog_tripped: Arc<AtomicBool>,
    /// Why the scan loop died, if it died: set exactly once — to the trap
    /// message when a VM trap stops the plant, or to the panic payload if
    /// the loop panicked — and never on a clean stop. A `watch` channel
    /// rather than a plain slot because the handle itself keeps the
    /// snapshot broadcast alive, so "stream closed" can never signal a
    /// fault — watchers must be woken by the fault WRITE itself. The
    /// sender lives on the scan thread; it drops (closing the watch) on
    /// clean exit.
    fault: tokio::sync::watch::Receiver<Option<String>>,
    /// The scan thread's join handle, shared so `shutdown` can join it
    /// from any clone. `Some` until the first `shutdown` takes it. Joining
    /// is how a caller waits for the always-run failsafe pass + per-device
    /// teardown (EtherCAT thread join) to actually finish.
    thread: Arc<std::sync::Mutex<Option<std::thread::JoinHandle<()>>>>,
}

impl ProgramHandle {
    /// Cooperative stop. The scan loop checks the flag at the top of each
    /// round; expect a few extra rounds before it actually exits.
    ///
    /// Fire-and-forget: returns immediately, doesn't wait for the failsafe
    /// pass. Use `shutdown` when you need the plant guaranteed safe before
    /// proceeding (clean process exit).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Graceful, awaitable stop for a clean process exit. Requests stop,
    /// then JOINS the scan thread — which runs its always-on failsafe pass
    /// (zero every device's outputs) and each device's `shutdown` (joining
    /// the EtherCAT cyclic worker so the zeroed controlword is on the wire)
    /// before it returns. When this completes, outputs are in failsafe.
    ///
    /// Unlike `stop`, this waits for completion, so the runtime can drive
    /// the plant safe before exiting rather than racing the service
    /// supervisor's kill timeout. The join runs on a blocking thread so
    /// this is safe to call from any async runtime. Idempotent: the first
    /// caller joins; later calls are no-ops.
    pub async fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let handle = self.thread.lock().ok().and_then(|mut g| g.take());
        let Some(handle) = handle else { return };
        match tokio::task::spawn_blocking(move || handle.join()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                tracing::error!(
                    "scan thread panicked during shutdown (outputs were failsafed first)"
                )
            }
            Err(e) => tracing::error!(%e, "failed to join scan thread on shutdown"),
        }
    }

    /// Subscribe to the per-cycle VarSnapshot stream.
    pub fn subscribe(&self) -> broadcast::Receiver<VarSnapshot> {
        self.snapshot_tx.subscribe()
    }

    /// Poke a variable while the program is running. Used by debug agents
    /// to force a state (toggle a setpoint, simulate an event flag, etc.).
    ///
    /// The write is applied between scan rounds, so it's seen by the next
    /// cycle's logic. Returns the value that was written; an error if the
    /// name doesn't resolve to a known variable or the VM traps.
    ///
    /// Name resolution (multi-PROGRAM runs): a bare name targets the
    /// first unit (tasks.toml declaration order) that declares it;
    /// `instance.variable` targets that PROGRAM instance explicitly
    /// (instance match is case-insensitive).
    pub async fn write_variable(
        &self,
        name: &str,
        value: i32,
    ) -> Result<WriteOutcome, RuntimeWriteError> {
        self.send_write(name, value, true).await
    }

    /// One-shot write that BYPASSES the project's write governance.
    /// For the runtime's OWN internal semantics only — today that is
    /// the monitor's pulse reset, whose write-0-back is the safety half
    /// of the `pulse_ms` contract and must land even when a rule's
    /// `min` would clamp 0 away. Same rationale as the force bypass
    /// (ADR-0002): a runtime-internal action is not an external write.
    /// Deliberately `pub(crate)` — never wired to any HTTP or
    /// northbound surface.
    pub(crate) async fn write_variable_ungoverned(
        &self,
        name: &str,
        value: i32,
    ) -> Result<WriteOutcome, RuntimeWriteError> {
        self.send_write(name, value, false).await
    }

    async fn send_write(
        &self,
        name: &str,
        value: i32,
        governed: bool,
    ) -> Result<WriteOutcome, RuntimeWriteError> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(RuntimeCommand::WriteVariable {
                name: name.to_string(),
                value,
                governed,
                ack: ack_tx,
            })
            .map_err(|_| RuntimeWriteError::Disconnected)?;
        ack_rx.await.map_err(|_| RuntimeWriteError::Disconnected)?
    }

    /// Pin a variable to a fixed value. Until `unforce_variable` is
    /// called with the same name, the scan loop will write `value`
    /// back into the VM at the start of every cycle — so program
    /// writes get overridden each round and field inputs can't push
    /// through. Forces survive across scan cycles, unlike one-shot
    /// `write_variable`. Returns the value that was applied.
    ///
    /// Names resolve like `write_variable`: bare → first unit that has
    /// the variable, `instance.variable` → that unit explicitly.
    pub async fn force_variable(&self, name: &str, value: i32) -> Result<i32, RuntimeWriteError> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(RuntimeCommand::ForceVariable {
                name: name.to_string(),
                value,
                ack: ack_tx,
            })
            .map_err(|_| RuntimeWriteError::Disconnected)?;
        ack_rx.await.map_err(|_| RuntimeWriteError::Disconnected)?
    }

    /// Inject an artificial stall into the next `scans` **ticks**,
    /// `stall_ms` each. Counted in ticks, not unit-scans: the watchdog
    /// counts consecutive overruns *per unit*, and one stall makes every
    /// unit due in that tick late, so a per-unit-scan budget burned N x
    /// too fast on an N-unit project and never reached the threshold. With `stall_ms` comfortably over the task interval and
    /// `scans` at least `WATCHDOG_OVERRUN_THRESHOLD + 1`, the scan
    /// watchdog trips through its real code path a few hundred
    /// milliseconds later. Backs `POST /api/runtime/inject-scan-stall`
    /// and the `inject` scenario step — a test primitive; on a live
    /// plant this deliberately drives the runtime into failsafe.
    pub async fn inject_scan_stall(
        &self,
        stall_ms: u64,
        scans: u32,
    ) -> Result<(), RuntimeWriteError> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(RuntimeCommand::InjectScanStall {
                stall_ms,
                scans,
                ack: ack_tx,
            })
            .map_err(|_| RuntimeWriteError::Disconnected)?;
        ack_rx.await.map_err(|_| RuntimeWriteError::Disconnected)
    }

    /// Release a forced variable. The variable resumes program-driven
    /// behaviour next scan. No-op if the variable wasn't forced.
    pub async fn unforce_variable(&self, name: &str) -> Result<(), RuntimeWriteError> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(RuntimeCommand::UnforceVariable {
                name: name.to_string(),
                ack: ack_tx,
            })
            .map_err(|_| RuntimeWriteError::Disconnected)?;
        ack_rx.await.map_err(|_| RuntimeWriteError::Disconnected)?
    }

    /// List currently-forced (name, value) pairs. Cheap — reads from
    /// the shared map, no scan-loop round-trip.
    pub fn forces(&self) -> Vec<(String, i32)> {
        self.forces
            .lock()
            .ok()
            .map(|m| {
                let mut out: Vec<(String, i32)> = m.iter().map(|(k, &v)| (k.clone(), v)).collect();
                out.sort_by(|a, b| a.0.cmp(&b.0));
                out
            })
            .unwrap_or_default()
    }

    /// Read the current execution mode (Running / Paused / Step{N}).
    pub fn mode(&self) -> RuntimeMode {
        self.mode.lock().map(|m| *m).unwrap_or(RuntimeMode::Running)
    }

    /// Halt the scan loop. The current round finishes first; subsequent
    /// rounds are skipped until `resume` or `step` is called. IO is
    /// frozen (no inputs read, no outputs written) — `Paused` semantics
    /// match a "freeze the plant" debug stop, not "VM still ticking,
    /// outputs still flapping".
    pub fn pause(&self) {
        if let Ok(mut m) = self.mode.lock() {
            *m = RuntimeMode::Paused;
        }
    }

    /// Resume normal continuous scanning. No-op if already running.
    pub fn resume(&self) {
        if let Ok(mut m) = self.mode.lock() {
            *m = RuntimeMode::Running;
        }
    }

    /// Run `cycles` scan rounds then auto-pause. Calling `step` while
    /// in Step mode resets the remaining count.
    pub fn step(&self, cycles: u32) {
        if let Ok(mut m) = self.mode.lock() {
            *m = if cycles == 0 {
                RuntimeMode::Paused
            } else {
                RuntimeMode::Step { remaining: cycles }
            };
        }
    }

    /// Snapshot of per-device connect reports (connected/failed + EtherCAT
    /// topology). Empty until the initial connect pass completes.
    pub fn device_reports(&self) -> Vec<DeviceReport> {
        self.device_reports
            .lock()
            .map(|r| r.clone())
            .unwrap_or_default()
    }

    /// Live per-device transport health (see [`DeviceHealth`]). Empty until
    /// the initial connect pass completes. A device that failed to connect
    /// is reported permanently unhealthy; a connected device reflects its
    /// adapter's [`IoDevice::is_healthy`] as of the last scan round.
    pub fn device_health(&self) -> Vec<DeviceHealth> {
        self.device_health
            .lock()
            .map(|h| h.clone())
            .unwrap_or_default()
    }

    /// `true` once the scan watchdog has tripped and latched the outputs
    /// off. Stays `true` until the program is restarted — a caller seeing
    /// this must not read live variable values as plant state, because the
    /// bus is holding zeros while the VM keeps computing.
    /// The fastest task interval in ms — the cadence at which `scan_count`
    /// can advance. See the field docs for why a client needs it.
    pub fn scan_period_ms(&self) -> u32 {
        self.scan_period_ms
    }

    /// Scan deadlines missed since start, summed over units. Climbing between
    /// two reads means the runtime is missing them now.
    pub fn scan_overruns(&self) -> u64 {
        self.scan_overruns.load(Ordering::Relaxed)
    }

    /// Longest overrun streak any unit is currently on; 0 when every unit made
    /// its last deadline. Reaching [`WATCHDOG_OVERRUN_THRESHOLD`] trips.
    pub fn consecutive_scan_overruns(&self) -> u32 {
        self.consecutive_scan_overruns.load(Ordering::Relaxed)
    }

    pub fn watchdog_tripped(&self) -> bool {
        self.watchdog_tripped.load(Ordering::Relaxed)
    }

    /// Why the scan loop died, if it died: `Some(message)` after a VM trap
    /// or a scan-thread panic, `None` while running or after a clean stop.
    /// Callers watching the snapshot stream use this at stream close to
    /// tell fault from stop.
    pub fn fault(&self) -> Option<String> {
        self.fault.borrow().clone()
    }

    /// Watch half of [`ProgramHandle::fault`], for callers that must be
    /// WOKEN on fault (the server's snapshot forwarder): `changed()`
    /// fires when the fault is written, and errors when the scan thread
    /// exits cleanly (sender dropped) — both are "stop watching" edges.
    pub fn fault_watch(&self) -> tokio::sync::watch::Receiver<Option<String>> {
        self.fault.clone()
    }

    /// True when `other` is a clone of this same spawned run — not merely a
    /// handle to an identically-shaped one. The server's stream forwarder
    /// uses this to release the program slot only if the slot still holds
    /// the run it was watching (a newer run may have replaced it).
    pub fn same_run(&self, other: &ProgramHandle) -> bool {
        Arc::ptr_eq(&self.stop, &other.stop)
    }
}

/// Default scan period when the caller doesn't request one. 100 ms
/// matches what Codesys-class tools default to and is plenty for
/// every demo we ship. Anything below ~10 ms starts taxing the
/// snapshot fan-out without giving the user faster perception.
pub const DEFAULT_SCAN_INTERVAL_MS: u64 = 100;

/// How often to flush retained variable values to disk during normal
/// operation. Power loss between flushes loses up to this much state.
/// 5 s strikes a balance between disk churn and worst-case data loss
/// for typical setpoints / counters / accumulators.
pub const RETAIN_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// One scheduled PROGRAM instance: its own compiled `Container` plus the
/// scheduling facts the scan thread needs to run it as an independent
/// "unit". Built by `ironplc_bridge::compile_project_units` (one per
/// `tasks.toml` program entry) or hand-rolled for ad-hoc single runs.
#[derive(Debug)]
pub struct ProgramUnit {
    /// PROGRAM instance name from tasks.toml (`PROGRAM <instance> WITH
    /// <task> : <program>;`). Used to route `Mapping.application`, to
    /// prefix colliding snapshot/retain names, and for the
    /// `instance.variable` write/force syntax.
    pub instance: String,
    /// Task this instance is bound to — informational (logs).
    pub task_name: String,
    /// Cycle period in milliseconds. `0` falls back to
    /// `DEFAULT_SCAN_INTERVAL_MS`.
    pub interval_ms: u64,
    /// IEC task priority — lower runs first when several units are due
    /// on the same tick.
    pub priority: i32,
    /// Bytecode compiled from this instance's PROGRAM + the project's
    /// FUNCTION_BLOCK / FUNCTION POUs + a synthesized single-task
    /// CONFIGURATION.
    pub container: Container,
    /// `VAR RETAIN` names declared by this unit's source.
    pub retain_vars: Vec<String>,
}

/// Start the scan thread hosting one `VmRunning` per unit.
///
/// Each unit is throttled to its own `interval_ms` regardless of what
/// (if anything) its compiled CONFIGURATION requested. Why the bridge
/// owns the cadence rather than the VM scheduler: as of the currently-
/// vendored ironplc, codegen does NOT populate `container.task_table`,
/// so the VM sees zero cyclic tasks and `next_due_us()` returns `None`.
/// Until upstream wires CONFIGURATION → task_table, the per-unit anchor
/// here is the source of truth for "the scan period the user asked for
/// in tasks.toml" (and `run_round` executes the unit's single PROGRAM
/// unconditionally each call).
///
/// RETAIN variables across all units persist into one state file when
/// `state_path` is set; keys are `instance.variable` when there is more
/// than one unit, bare names otherwise.
pub fn spawn_units(
    units: Vec<ProgramUnit>,
    device_specs: Vec<DeviceSpec>,
    mappings: Vec<Mapping>,
    state_path: Option<PathBuf>,
    governance: WriteGovernance,
) -> ProgramHandle {
    spawn_units_inner(
        units,
        DeviceSource::Specs(device_specs),
        mappings,
        state_path,
        governance,
    )
}

/// Where the scan thread gets its `IoDevice`s. Production always connects
/// from `DeviceSpec`s; tests inject pre-built devices so the failsafe /
/// shutdown sequencing can be asserted without real hardware.
enum DeviceSource {
    Specs(Vec<DeviceSpec>),
    #[cfg(test)]
    Prebuilt(Vec<Box<dyn IoDevice>>),
}

async fn acquire_devices(
    source: DeviceSource,
) -> (Vec<Box<dyn IoDevice>>, Vec<DeviceReport>, Vec<DeviceSpec>) {
    match source {
        DeviceSource::Specs(specs) => connect_devices(specs).await,
        #[cfg(test)]
        DeviceSource::Prebuilt(devices) => {
            let reports = devices
                .iter()
                .map(|d| DeviceReport {
                    name: d.name().to_string(),
                    protocol: "mock".into(),
                    connected: true,
                    error: None,
                    slaves: Vec::new(),
                })
                .collect();
            (devices, reports, Vec::new())
        }
    }
}

fn spawn_units_inner(
    units: Vec<ProgramUnit>,
    device_source: DeviceSource,
    mappings: Vec<Mapping>,
    state_path: Option<PathBuf>,
    governance: WriteGovernance,
) -> ProgramHandle {
    let stop = Arc::new(AtomicBool::new(false));
    // Normalised exactly as `UnitClock::interval` below, and MIN across
    // units because `scan_count` is the max across units — the fastest task
    // is the one that makes the counter move.
    let scan_period_ms = units
        .iter()
        .map(|u| {
            if u.interval_ms == 0 {
                DEFAULT_SCAN_INTERVAL_MS
            } else {
                u.interval_ms
            }
            .max(1)
            .min(u32::MAX as u64) as u32
        })
        .min()
        .unwrap_or(DEFAULT_SCAN_INTERVAL_MS as u32);
    let scan_overruns = Arc::new(AtomicU64::new(0));
    let consecutive_scan_overruns = Arc::new(AtomicU32::new(0));
    let (snapshot_tx, _) = broadcast::channel(64);
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let mode = Arc::new(std::sync::Mutex::new(RuntimeMode::Running));
    let forces = Arc::new(std::sync::Mutex::new(HashMap::<String, i32>::new()));
    let device_reports = Arc::new(std::sync::Mutex::new(Vec::<DeviceReport>::new()));
    let device_health = Arc::new(std::sync::Mutex::new(Vec::<DeviceHealth>::new()));
    let watchdog_tripped = Arc::new(AtomicBool::new(false));
    let (fault_tx, fault_rx) = tokio::sync::watch::channel(None::<String>);

    let stop_clone = stop.clone();
    let stop_reconnect = stop.clone();
    let snapshot_tx_clone = snapshot_tx.clone();
    let mode_clone = mode.clone();
    let forces_clone = forces.clone();
    let device_reports_clone = device_reports.clone();
    let device_health_clone = device_health.clone();
    let watchdog_tripped_clone = watchdog_tripped.clone();
    let scan_overruns_clone = scan_overruns.clone();
    let consecutive_overruns_clone = consecutive_scan_overruns.clone();
    let device_reports_reconnect = device_reports.clone();

    let join_handle = std::thread::spawn(move || {
        // Southbound adapters get their own I/O runtime, and devices are
        // connected HERE — at the top of the thread, before the scan
        // runtime exists — so every background task an adapter spawns
        // during connect (the Modbus poll task, the CANopen bus task)
        // lands on the "ia2-io" worker and NEVER shares the scan
        // thread. Measured cost of sharing: a 9600-baud Modbus refresh
        // interleaving its serial round-trips between 2 ms scan rounds
        // lost ~20% cadence on a mixed servo+coupler bench (issue #30).
        // The runtime lives on this thread's stack below the scan
        // block_on, so it outlives the failsafe + teardown writes that
        // flow through those poll tasks.
        let io_rt = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("ia2-io")
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!(%e, "failed to create io runtime");
                return;
            }
        };
        // Connect devices OUTSIDE the panic-guarded run_loop so that,
        // no matter how the loop exits (clean stop, VM trap, or panic),
        // we still own the `devices` vec and can drive every output to
        // its safe state.
        let (mut devices, reports, failed_specs) = io_rt.block_on(acquire_devices(device_source));

        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!(%e, "failed to create scan-thread runtime");
                return;
            }
        };
        rt.block_on(async move {
            // Seed health from the connect outcomes: configured devices
            // that failed to connect start unhealthy (the per-round
            // refresh doesn't touch them until a background reconnect
            // brings them into `devices`); connected ones start from
            // their adapter's flag.
            if let Ok(mut slot) = device_health_clone.lock() {
                *slot = reports
                    .iter()
                    .map(|r| DeviceHealth {
                        name: r.name.clone(),
                        healthy: r.connected
                            && devices
                                .iter()
                                .find(|d| d.name() == r.name)
                                .map(|d| d.is_healthy())
                                .unwrap_or(false),
                    })
                    .collect();
            }
            if let Ok(mut slot) = device_reports_clone.lock() {
                *slot = reports;
            }

            // Devices that failed the initial connect are retried in the
            // background on a dedicated thread (own runtime, so a slow
            // EtherCAT bring-up can never stall scan rounds). Reconnected
            // adapters are handed to the scan loop over this channel; the
            // loop binds their parked mappings and the per-round health
            // refresh flips them healthy. If nothing failed, the sender
            // drops here and try_recv just reports Disconnected forever.
            let (reconnect_tx, reconnect_rx) =
                std::sync::mpsc::channel::<Box<dyn IoDevice>>();
            // Set once the failsafe + per-device shutdown pass below has
            // finished — the reconnect worker keeps its runtime (and the
            // adapters' background tasks) alive until then.
            let drained = Arc::new(AtomicBool::new(false));
            if !failed_specs.is_empty() {
                let drained_flag = drained.clone();
                let spawned = std::thread::Builder::new()
                    .name("ia2-reconnect".into())
                    .spawn(move || {
                        reconnect_worker(
                            failed_specs,
                            reconnect_tx,
                            device_reports_reconnect,
                            stop_reconnect,
                            drained_flag,
                        )
                    });
                if let Err(e) = spawned {
                    tracing::error!(%e, "failed to spawn reconnect thread; unconnected devices stay down");
                }
            }

            // Wrap the scan loop in catch_unwind so a panic in the VM
            // glue / iomap / snapshot fan-out doesn't skip failsafe.
            // `AssertUnwindSafe` is needed because `&mut Vec<...>` is
            // not auto-UnwindSafe; we accept that risk because the
            // only failure mode here is "panic in async code", and
            // we're about to discard the VMs anyway.
            use futures_util::FutureExt;
            use std::panic::AssertUnwindSafe;
            let result = AssertUnwindSafe(run_loop_async(
                units,
                &mut devices,
                mappings,
                governance,
                stop_clone,
                snapshot_tx_clone,
                cmd_rx,
                mode_clone,
                forces_clone,
                device_health_clone,
                watchdog_tripped_clone,
                scan_overruns_clone,
                consecutive_overruns_clone,
                &fault_tx,
                reconnect_rx,
                state_path,
            ))
            .catch_unwind()
            .await;
            // A panic is a fault too: record it so the HTTP layer can
            // surface WHY the run died, not just that snapshots stopped.
            // (The failsafe + teardown below still run either way.)
            if let Err(panic) = &result {
                let msg = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "scan loop panicked".into());
                fault_tx.send_if_modified(|f| {
                    if f.is_none() {
                        *f = Some(format!("scan loop panicked: {msg}"));
                        true
                    } else {
                        false
                    }
                });
            }

            // Always-run failsafe before the thread dies. Drive every
            // device's outputs to zero so a hung / panicked / stopped
            // program doesn't leave actuators energized.
            let dev_count = devices.len();
            for dev in devices.iter_mut() {
                if let Err(e) = dev.enter_failsafe().await {
                    tracing::warn!(device = %dev.name(), %e, "failsafe call failed");
                }
            }
            // Give an async-flush device (real EtherCAT) a cycle or two to
            // push the zeros onto the bus. Conservative 50 ms covers
            // cycle_us up to 25 ms; paid once, on shutdown.
            tokio::time::sleep(Duration::from_millis(50)).await;
            // Graceful per-device teardown: signal + JOIN any background
            // I/O thread (the EtherCAT cyclic worker) so the zeroed
            // controlword is guaranteed on the wire before we exit — not
            // left to the drive's own watchdog after the master is gone.
            // Runs on both the clean and panicked paths (before re-panic).
            for dev in devices.iter_mut() {
                if let Err(e) = dev.shutdown().await {
                    tracing::warn!(device = %dev.name(), %e, "device shutdown failed");
                }
            }
            // Failsafe + teardown done: release the reconnect worker (it
            // holds the runtime that late-connected adapters' background
            // tasks live on, and must outlive the writes above).
            drained.store(true, Ordering::Relaxed);
            match &result {
                Ok(()) => tracing::info!(
                    devices = dev_count,
                    "scan loop exited cleanly; failsafe applied"
                ),
                Err(_) => tracing::error!(
                    devices = dev_count,
                    "scan loop PANICKED; failsafe applied before re-panic"
                ),
            }
            if let Err(panic) = result {
                // Re-raise so the thread dies with a useful backtrace
                // in tests / logs. Outputs are already safe.
                std::panic::resume_unwind(panic);
            }
        });
    });

    ProgramHandle {
        stop,
        snapshot_tx,
        cmd_tx,
        mode,
        forces,
        scan_overruns,
        consecutive_scan_overruns,
        scan_period_ms,
        device_reports,
        device_health,
        watchdog_tripped,
        fault: fault_rx,
        thread: Arc::new(std::sync::Mutex::new(Some(join_handle))),
    }
}

/// Connect every `DeviceSpec` into a live `IoDevice` adapter. A
/// connect failure for one device is logged and the device is skipped
/// rather than aborting the whole scan — partial bus connectivity is
/// a common operational state and we'd rather run the rest of the
/// program than refuse to start. The bridge's `enter_failsafe` pass
/// at shutdown only touches devices that DID connect.
async fn connect_devices(
    device_specs: Vec<DeviceSpec>,
) -> (Vec<Box<dyn IoDevice>>, Vec<DeviceReport>, Vec<DeviceSpec>) {
    let mut devices: Vec<Box<dyn IoDevice>> = Vec::with_capacity(device_specs.len());
    let mut reports: Vec<DeviceReport> = Vec::with_capacity(device_specs.len());
    let mut failed: Vec<DeviceSpec> = Vec::new();
    for spec in device_specs {
        let (device, report) = connect_one(&spec).await;
        match device {
            Some(d) => devices.push(d),
            None => failed.push(spec),
        }
        reports.push(report);
    }
    (devices, reports, failed)
}

/// Connect a single `DeviceSpec` into a live `IoDevice` adapter plus its
/// connect report. Shared by the initial connect pass and the background
/// reconnect worker so both paths produce identical devices and reports.
async fn connect_one(spec: &DeviceSpec) -> (Option<Box<dyn IoDevice>>, DeviceReport) {
    match &spec.config {
        ProtocolConfig::Modbus(cfg) => {
            match iomap_modbus::ModbusDevice::connect(spec.name.clone(), cfg).await {
                Ok(d) => {
                    // Log the transport-relevant detail so the
                    // operator sees "tcp 192.168.x.y:502" vs
                    // "rtu /dev/ttyUSB0 @ 9600" — same line
                    // pattern, transport-specific payload.
                    match &cfg.transport {
                        project::ModbusTransport::Tcp(p) => {
                            tracing::info!(name = %spec.name, transport = "tcp", host = %p.host, port = p.port, "modbus connected");
                        }
                        project::ModbusTransport::Rtu(p) => {
                            tracing::info!(
                                name = %spec.name,
                                transport = "rtu",
                                device = %p.serial_device,
                                baud = p.baud_rate,
                                "modbus connected"
                            );
                        }
                    }
                    (
                        Some(Box::new(d) as Box<dyn IoDevice>),
                        DeviceReport {
                            name: spec.name.clone(),
                            protocol: "modbus".into(),
                            connected: true,
                            error: None,
                            slaves: Vec::new(),
                        },
                    )
                }
                Err(e) => {
                    tracing::warn!(name = %spec.name, %e, "modbus connect failed");
                    (
                        None,
                        DeviceReport {
                            name: spec.name.clone(),
                            protocol: "modbus".into(),
                            connected: false,
                            error: Some(e.to_string()),
                            slaves: Vec::new(),
                        },
                    )
                }
            }
        }
        ProtocolConfig::Ethercat(cfg) => {
            // The EtherCAT bring-up (init_single_group / DC sync) can
            // lose the first PDU right after a fresh link/bind and fail
            // with a transient Timeout(Pdu). A failed connect() exits its
            // worker thread cleanly, so retry a few times with a short
            // backoff — otherwise one transient timeout leaves the bus
            // (and the motor) dead until a manual restart.
            const MAX_ATTEMPTS: u32 = 3;
            let mut attempt: u32 = 1;
            let connected = loop {
                match iomap_ethercat::EthercatDevice::connect(spec.name.clone(), cfg).await {
                    Ok(d) => break Ok(d),
                    Err(e) if attempt < MAX_ATTEMPTS => {
                        tracing::warn!(
                            name = %spec.name, attempt, max = MAX_ATTEMPTS, %e,
                            "ethercat connect failed; retrying after backoff"
                        );
                        tokio::time::sleep(Duration::from_millis(800)).await;
                        attempt += 1;
                    }
                    Err(e) => break Err(e),
                }
            };
            match connected {
                Ok(d) => {
                    tracing::info!(name = %spec.name, nic = %cfg.nic, "ethercat connected");
                    // Pull the discovered topology before boxing the
                    // device into the trait object.
                    let slaves = d
                        .discovered()
                        .into_iter()
                        .map(|s| DiscoveredSlave {
                            index: s.index,
                            name: s.name,
                            vendor_id: s.vendor_id,
                            product_id: s.product_id,
                            input_bytes: s.input_bytes,
                            output_bytes: s.output_bytes,
                        })
                        .collect();
                    (
                        Some(Box::new(d) as Box<dyn IoDevice>),
                        DeviceReport {
                            name: spec.name.clone(),
                            protocol: "ethercat".into(),
                            connected: true,
                            error: None,
                            slaves,
                        },
                    )
                }
                Err(e) => {
                    tracing::warn!(name = %spec.name, %e, "ethercat connect failed");
                    (
                        None,
                        DeviceReport {
                            name: spec.name.clone(),
                            protocol: "ethercat".into(),
                            connected: false,
                            error: Some(e.to_string()),
                            slaves: Vec::new(),
                        },
                    )
                }
            }
        }
        ProtocolConfig::Opcua(cfg) => {
            match iomap_opcua::OpcuaDevice::connect(spec.name.clone(), cfg).await {
                Ok(d) => {
                    tracing::info!(
                        name = %spec.name,
                        endpoint = %cfg.endpoint_url,
                        tags = cfg.channels.len(),
                        "opcua connected"
                    );
                    (
                        Some(Box::new(d) as Box<dyn IoDevice>),
                        DeviceReport {
                            name: spec.name.clone(),
                            protocol: "opcua".into(),
                            connected: true,
                            error: None,
                            slaves: Vec::new(),
                        },
                    )
                }
                Err(e) => {
                    tracing::warn!(name = %spec.name, %e, "opcua connect failed");
                    (
                        None,
                        DeviceReport {
                            name: spec.name.clone(),
                            protocol: "opcua".into(),
                            connected: false,
                            error: Some(e.to_string()),
                            slaves: Vec::new(),
                        },
                    )
                }
            }
        }
        ProtocolConfig::Canopen(cfg) => {
            match iomap_canopen::CanopenDevice::connect(spec.name.clone(), cfg).await {
                Ok(d) => {
                    tracing::info!(
                        name = %spec.name,
                        interface = %cfg.interface,
                        node = cfg.node_id,
                        channels = cfg.channels.len(),
                        "canopen connected"
                    );
                    (
                        Some(Box::new(d) as Box<dyn IoDevice>),
                        DeviceReport {
                            name: spec.name.clone(),
                            protocol: "canopen".into(),
                            connected: true,
                            error: None,
                            slaves: Vec::new(),
                        },
                    )
                }
                Err(e) => {
                    tracing::warn!(name = %spec.name, %e, "canopen connect failed");
                    (
                        None,
                        DeviceReport {
                            name: spec.name.clone(),
                            protocol: "canopen".into(),
                            connected: false,
                            error: Some(e.to_string()),
                            slaves: Vec::new(),
                        },
                    )
                }
            }
        }
    }
}

/// Background retry loop for devices that failed the initial connect.
///
/// Runs on its own OS thread with a dedicated single-thread runtime so a
/// slow bus bring-up (EtherCAT walk, DC sync, TCP timeouts) can never
/// stall scan rounds. Retries every pending spec with a shared doubling
/// backoff (1 s → 30 s cap); each success rewrites that device's
/// /discover report in place and hands the live adapter to the scan
/// loop, which binds the mappings it parked at startup. Exits when every
/// spec has connected or the program stops (checked in ≤200 ms slices so
/// shutdown isn't held up by a long backoff).
fn reconnect_worker(
    mut pending: Vec<DeviceSpec>,
    tx: std::sync::mpsc::Sender<Box<dyn IoDevice>>,
    reports: Arc<std::sync::Mutex<Vec<DeviceReport>>>,
    stop: Arc<AtomicBool>,
    drained: Arc<AtomicBool>,
) {
    const BACKOFF_START: Duration = Duration::from_secs(1);
    const BACKOFF_CAP: Duration = Duration::from_secs(30);
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(%e, "failed to create reconnect runtime; unconnected devices stay down");
            return;
        }
    };
    // Everything runs inside one block_on: adapters connected here spawn
    // their background tasks (Modbus poll loop, OPC-UA session) onto THIS
    // runtime, and a current-thread runtime only makes progress while it's
    // being driven — so the worker must keep awaiting for as long as any
    // device it delivered is in service.
    rt.block_on(async move {
        tracing::warn!(
            devices = pending.len(),
            "entering background reconnect for devices that failed to connect"
        );
        let mut delivered_any = false;
        let mut backoff = BACKOFF_START;
        while !pending.is_empty() && !stop.load(Ordering::Relaxed) {
            let deadline = Instant::now() + backoff;
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                tokio::time::sleep(remaining.min(Duration::from_millis(200))).await;
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let mut still_pending = Vec::with_capacity(pending.len());
            for spec in pending {
                let (device, report) = connect_one(&spec).await;
                match device {
                    Some(d) => {
                        tracing::info!(device = %spec.name, "background reconnect succeeded");
                        if let Ok(mut slot) = reports.lock() {
                            if let Some(r) = slot.iter_mut().find(|r| r.name == report.name) {
                                *r = report;
                            }
                        }
                        // Scan loop gone ⇒ nobody left to adopt the device.
                        if tx.send(d).is_err() {
                            return;
                        }
                        delivered_any = true;
                    }
                    None => still_pending.push(spec),
                }
            }
            pending = still_pending;
            backoff = (backoff * 2).min(BACKOFF_CAP);
        }
        if pending.is_empty() {
            tracing::info!("background reconnect complete; all configured devices connected");
        }
        if !delivered_any {
            return; // nothing spawned onto this runtime; safe to wind down
        }
        // Keep driving the delivered adapters' background tasks until the
        // scan thread reports its failsafe/shutdown pass is DONE (`drained`).
        // Exiting on `stop` alone would race that pass: dropping this
        // runtime kills the poll tasks the failsafe writes go through.
        while !drained.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
}

/// Trip the watchdog after this many consecutive scan deadline overruns
/// on any single unit. Each overrun means that unit's scan body didn't
/// finish within its interval. 5 in a row → the simulation has lost
/// real-time guarantees; engage failsafe and don't re-arm until the
/// program is restarted.
pub const WATCHDOG_OVERRUN_THRESHOLD: u32 = 5;

/// Upper bounds on fault injection. A stall is a deliberate denial of
/// service against the scan loop; unbounded, one typo (or a hostile
/// caller) parks the runtime for hours with no way back but a restart.
/// 1 s is already ~500x the 2 ms motion cadence — far past anything a
/// test needs. Values above the cap are clamped and warned, not
/// rejected: a scenario that asked for too much still gets its fault.
pub const MAX_INJECT_STALL_MS: u64 = 1_000;
/// Ditto for the scan budget. THRESHOLD + 1 is the useful value; this
/// only stops a runaway.
pub const MAX_INJECT_SCANS: u32 = 1_000;

/// An injected stall is slept in slices this long so a stop request is
/// observed promptly. Sleeping the full stall in one call made
/// `shutdown` wait it out.
const INJECT_STALL_SLICE: Duration = Duration::from_millis(20);

/// Snapshot fan-out cadence (and the cap on idle sleeps, so stop /
/// command latency stays bounded even when every task interval is long).
const SNAPSHOT_PERIOD: Duration = Duration::from_millis(100);

/// A bus channel ↔ VM variable pair, resolved to indices and routed to
/// one unit.
struct ResolvedMapping {
    device_index: usize,
    channel: String,
    var_index: u16,
    type_tag: u8,
    /// REAL var — channel values cross the VM boundary as IEEE-754
    /// bits instead of numeric i32 (see `ChannelValue::to_vm_bits`).
    is_real: bool,
    /// LREAL var — crosses as a full 64-bit double via
    /// `write_variable_raw` (the i32 lane would truncate).
    is_lreal: bool,
}

/// Resolve one iomap `Mapping` against a connected device: route it to a
/// PROGRAM unit and produce the index-level `ResolvedMapping` the scan
/// loop consumes. Shared by the startup resolution pass and the mid-run
/// bind that happens when a device joins after a background reconnect.
///
/// Routing: by PROGRAM instance name (`Mapping.application`,
/// case-insensitive). An empty/unknown application falls back to the
/// first unit that declares the variable — the exact pre-multi-program
/// resolution, so legacy iomaps keep working; warns only when there's
/// real ambiguity (>1 unit). Returns `None` (with a warn) when the
/// variable can't be routed at all.
fn resolve_mapping(
    m: &Mapping,
    device_index: usize,
    instances: &[String],
    var_index_by_name: &[HashMap<String, u16>],
    debug_maps: &[HashMap<u16, VarDebugInfo>],
) -> Option<(usize, Direction, ResolvedMapping)> {
    let n_units = instances.len();
    let unit_index = match instances
        .iter()
        .position(|i| i.eq_ignore_ascii_case(&m.application))
    {
        Some(i) => {
            if !var_index_by_name[i].contains_key(&m.variable) {
                tracing::warn!(
                    application = %m.application,
                    var = %m.variable,
                    "mapping's variable not declared by its routed instance, skipping"
                );
                return None;
            }
            i
        }
        None => {
            let Some(i) = (0..n_units).find(|&i| var_index_by_name[i].contains_key(&m.variable))
            else {
                tracing::warn!(var = %m.variable, "mapping references unknown variable, skipping");
                return None;
            };
            if n_units > 1 {
                tracing::warn!(
                    application = %m.application,
                    var = %m.variable,
                    routed_to = %instances[i],
                    "mapping's application doesn't name a PROGRAM instance; \
                     falling back to the first unit that declares the variable"
                );
            }
            i
        }
    };
    let var_index = var_index_by_name[unit_index][&m.variable];
    let type_tag = debug_maps[unit_index]
        .get(&var_index)
        .map(|d| d.iec_type_tag)
        .unwrap_or(0);
    Some((
        unit_index,
        m.direction,
        ResolvedMapping {
            device_index,
            channel: m.channel.clone(),
            var_index,
            type_tag,
            is_real: type_tag == iec_type_tag::REAL,
            is_lreal: type_tag == iec_type_tag::LREAL,
        },
    ))
}

/// Per-unit scheduling state. Lives in a Vec parallel to `runnings`
/// (the `VmRunning`s borrow their containers, so unit bookkeeping stays
/// in plain owned data).
struct UnitClock {
    /// When this unit's next scan is due. Anchored, not drifting:
    /// advances by `interval` after each run; slides to `now +
    /// interval` on overrun so we don't burn CPU catching up.
    next_due: Instant,
    interval: Duration,
    scan_count: u64,
    consecutive_overruns: u32,
    warned_overrun: bool,
}

/// Resolve a runtime variable reference against all units. A bare name
/// matches the first unit (tasks.toml declaration order) that declares
/// it — which is also exactly the pre-multi-program behaviour when
/// there's one unit. `instance.variable` (instance match case-
/// insensitive) targets that unit explicitly. Bare lookup runs first so
/// a literal debug name containing a dot keeps resolving as it did
/// before multi-program support.
fn resolve_var(
    name: &str,
    instances: &[String],
    var_index_by_name: &[HashMap<String, u16>],
) -> Option<(usize, u16)> {
    for (i, m) in var_index_by_name.iter().enumerate() {
        if let Some(&idx) = m.get(name) {
            return Some((i, idx));
        }
    }
    let (prefix, rest) = name.split_once('.')?;
    let i = instances
        .iter()
        .position(|inst| inst.eq_ignore_ascii_case(prefix))?;
    var_index_by_name[i].get(rest).map(|&idx| (i, idx))
}

/// Apply the project's write governance to a resolved `/write`. Returns
/// the value to actually write — clamped to the matching rule's
/// min/max when declared — or a governance denial.
///
/// Enforcement never panics: this runs on the scan thread, where a
/// panic kills the whole run, so a rule whose bounds don't form a valid
/// range (min > max, or a NaN bound that slipped past load validation)
/// DENIES the write instead — `f64::clamp` would panic on exactly those
/// bounds and is never used here. Clamping happens in the written
/// value's own domain so the applied value is always within [min, max]:
/// the integer lane clamps to ceil(min)..=floor(max) (denying when that
/// range is empty), the REAL lane steps an f32 bound one ULP inward
/// when f64→f32 rounding would escape it.
///
/// Force/unforce deliberately never come through here: force is the
/// pure debug override and stays ungoverned (ADR-0002 records the
/// decision; a governed force would no longer be a debug override, and
/// an ungoverned exported force would be a hole — so force is simply
/// never exported on any northbound surface). The monitor's pulse
/// reset bypasses too, via `write_variable_ungoverned`: it is the
/// runtime's own safety action, not an external write.
fn enforce_write_governance(
    governance: &WriteGovernance,
    name: &str,
    unit: usize,
    var_index: u16,
    value: i32,
    instances: &[String],
    debug_maps: &[HashMap<u16, VarDebugInfo>],
) -> Result<i32, RuntimeWriteError> {
    if governance.write_mode == WriteMode::Open {
        return Ok(value);
    }
    // Candidate rule keys: the qualified `instance.variable` form and
    // the bare debug name — matching is case-insensitive, like the IEC.
    let bare = debug_maps[unit]
        .get(&var_index)
        .map(|d| d.name.to_lowercase())
        .unwrap_or_default();
    let qualified = format!("{}.{}", instances[unit].to_lowercase(), bare);
    let rule = governance.rules.iter().find(|r| {
        let key = r.variable.to_lowercase();
        key == bare || key == qualified
    });
    let Some(rule) = rule else {
        return Err(RuntimeWriteError::GovernanceDenied(format!(
            "{name} (write_mode = allowlist, no rule allows it)"
        )));
    };
    if rule.min.is_none() && rule.max.is_none() {
        return Ok(value);
    }
    let lo = rule.min.unwrap_or(f64::NEG_INFINITY);
    let hi = rule.max.unwrap_or(f64::INFINITY);
    // Degenerate bounds should be rejected at project load; if a bad
    // rule slips through anyway, deny rather than guess a range the
    // author never declared.
    if lo > hi || lo.is_nan() || hi.is_nan() {
        return Err(RuntimeWriteError::GovernanceDenied(format!(
            "{name} (rule bounds min {lo} / max {hi} do not form a valid range — fix project.toml)"
        )));
    }
    // REAL is IEEE-754 bits in the i32 slot. Everything else — LREAL
    // included — compares as a plain integer: every writer that can
    // reach governance (HTTP i32 body, northbound numeric lane)
    // encodes LREAL as a plain number, nothing produces f32 bits for
    // it, and the 64-bit engineering path (iomap inputs) uses
    // `write_variable_raw` and never comes through here. Clamping the
    // value the writer actually sent keeps governed and ungoverned
    // LREAL writes consistent.
    let is_real = debug_maps[unit]
        .get(&var_index)
        .map(|d| d.type_name.to_ascii_uppercase().starts_with("REAL"))
        .unwrap_or(false);
    if is_real {
        let current = f32::from_bits(value as u32);
        if current.is_nan() {
            // NaN is unorderable: it cannot be clamped, and writing it
            // through would void the min/max envelope while the log
            // claimed "clamped". Deny, honestly.
            return Err(RuntimeWriteError::GovernanceDenied(format!(
                "{name} (value NaN cannot be checked against the rule's min/max)"
            )));
        }
        // Convert the f64 bounds to f32 INWARD, so round-to-nearest
        // can never let the written f32 escape the declared [min, max].
        let mut lo_f = lo as f32;
        if (lo_f as f64) < lo {
            lo_f = lo_f.next_up();
        }
        let mut hi_f = hi as f32;
        if (hi_f as f64) > hi {
            hi_f = hi_f.next_down();
        }
        // A finite bound past f32 range saturates to ±inf on the far
        // side (inward stepping can't reach it): the rule admits no
        // representable REAL, and "clamping" would write literal
        // infinity into the scan. Deny, like the integer lane.
        if (lo.is_finite() && lo_f == f32::INFINITY)
            || (hi.is_finite() && hi_f == f32::NEG_INFINITY)
            || lo_f > hi_f
        {
            return Err(RuntimeWriteError::GovernanceDenied(format!(
                "{name} (rule range [{lo}, {hi}] contains no representable REAL value)"
            )));
        }
        let clamped = current.max(lo_f).min(hi_f);
        if clamped == current {
            return Ok(value);
        }
        warn_clamped(name, current as f64, clamped as f64);
        Ok(clamped.to_bits() as i32)
    } else {
        // Integer lane: clamp to the integers inside [min, max] —
        // rounding the clamped f64 could otherwise step OUTSIDE a
        // fractional bound (e.g. max = 10.6 rounding up to 11).
        let lo_i = lo.ceil().max(i32::MIN as f64);
        let hi_i = hi.floor().min(i32::MAX as f64);
        if lo_i > hi_i {
            return Err(RuntimeWriteError::GovernanceDenied(format!(
                "{name} (rule range [{lo}, {hi}] contains no writable integer value)"
            )));
        }
        let current = value as f64;
        let clamped = current.max(lo_i).min(hi_i);
        if clamped == current {
            return Ok(value);
        }
        warn_clamped(name, current, clamped);
        Ok(clamped as i32)
    }
}

/// Clamps are loud, not silent: the log says what was asked vs written,
/// and the write's ack echoes the clamped value.
fn warn_clamped(name: &str, requested: f64, written: f64) {
    tracing::warn!(
        variable = %name,
        requested,
        written,
        "write clamped to the governance rule's range"
    );
}

#[allow(clippy::too_many_arguments)]
async fn run_loop_async(
    units: Vec<ProgramUnit>,
    // Devices are borrowed so the outer wrapper retains ownership for
    // its always-run failsafe pass (see `spawn_units_inner`'s async
    // block).
    devices: &mut Vec<Box<dyn IoDevice>>,
    mappings: Vec<Mapping>,
    governance: WriteGovernance,
    stop: Arc<AtomicBool>,
    snapshot_tx: broadcast::Sender<VarSnapshot>,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<RuntimeCommand>,
    mode: Arc<std::sync::Mutex<RuntimeMode>>,
    forces: Arc<std::sync::Mutex<HashMap<String, i32>>>,
    device_health: Arc<std::sync::Mutex<Vec<DeviceHealth>>>,
    // The watchdog latch. Shared rather than local so the HTTP layer can
    // tell a latched run from a healthy one — see the field docs on
    // `ProgramHandle::watchdog_tripped`.
    watchdog_tripped: Arc<AtomicBool>,
    // Achieved-cadence counters for /status: deadlines missed since start and
    // the longest current streak. The overrun log line fires once per unit and
    // then suppresses itself, so it cannot answer "still overrunning?".
    scan_overruns: Arc<AtomicU64>,
    consecutive_scan_overruns: Arc<AtomicU32>,
    // Borrowed: the wrapper keeps ownership so its panic path can also
    // record into the same channel (and so the sender drops — closing
    // the watch — only when the scan thread exits).
    fault: &tokio::sync::watch::Sender<Option<String>>,
    reconnect_rx: std::sync::mpsc::Receiver<Box<dyn IoDevice>>,
    state_path: Option<PathBuf>,
) {
    if units.is_empty() {
        tracing::error!("no program units to run — scan loop not started");
        return;
    }
    let n_units = units.len();

    // ---- Start one VM per unit ----
    // `VmRunning` borrows its container and its buffers, so both live
    // in Vecs that outlive `runnings` and are never structurally
    // touched again (`iter_mut` hands out disjoint element borrows).
    let mut bufs: Vec<VmBuffers> = units
        .iter()
        .map(|u| VmBuffers::from_container(&u.container))
        .collect();
    let mut runnings: Vec<VmRunning<'_>> = Vec::with_capacity(n_units);
    for (unit, buf) in units.iter().zip(bufs.iter_mut()) {
        match Vm::new().load(&unit.container, buf).start() {
            Ok(r) => runnings.push(r),
            Err(ctx) => {
                tracing::error!(instance = %unit.instance, ?ctx.trap, "vm failed to start");
                return;
            }
        }
    }

    let debug_maps: Vec<HashMap<u16, VarDebugInfo>> = units
        .iter()
        .map(|u| build_var_debug_map(&u.container))
        .collect();
    // Parallel to debug_maps: per-unit `var_index -> data_offset` for STRING
    // vars, whose bytes live in the VM data region rather than the slot.
    let string_layouts: Vec<HashMap<u16, u32>> = units
        .iter()
        .map(|u| build_string_layout_map(&u.container))
        .collect();
    let var_index_by_name: Vec<HashMap<String, u16>> = debug_maps
        .iter()
        .map(|dm| {
            dm.iter()
                .map(|(idx, info)| (info.name.clone(), *idx))
                .collect()
        })
        .collect();
    let instances: Vec<String> = units.iter().map(|u| u.instance.clone()).collect();

    // Variable names declared by more than one unit get the
    // `instance.` prefix in snapshots (and retain keys) so they stay
    // distinguishable; unique names stay bare — zero churn for
    // single-program projects.
    let shared_names: std::collections::HashSet<String> = if n_units > 1 {
        let mut counts: HashMap<&String, u32> = HashMap::new();
        for m in &var_index_by_name {
            for name in m.keys() {
                *counts.entry(name).or_insert(0) += 1;
            }
        }
        counts
            .into_iter()
            .filter(|&(_, c)| c > 1)
            .map(|(n, _)| n.clone())
            .collect()
    } else {
        Default::default()
    };

    // ---- Resolve mappings into index pairs, routed per unit ----
    let device_index_by_name: HashMap<String, usize> = devices
        .iter()
        .enumerate()
        .map(|(i, d)| (d.name().to_string(), i))
        .collect();

    let mut unit_inputs: Vec<Vec<ResolvedMapping>> = (0..n_units).map(|_| Vec::new()).collect();
    let mut unit_outputs: Vec<Vec<ResolvedMapping>> = (0..n_units).map(|_| Vec::new()).collect();
    // Mappings whose device isn't connected are parked, not dropped: the
    // background reconnect worker may still deliver that device mid-run,
    // at which point they bind exactly like the startup ones below.
    let mut pending_mappings: Vec<Mapping> = Vec::new();
    for m in mappings {
        let Some(&device_index) = device_index_by_name.get(&m.device) else {
            tracing::warn!(
                device = %m.device,
                "mapping references a device that isn't connected; parked until it reconnects"
            );
            pending_mappings.push(m);
            continue;
        };
        if let Some((unit_index, direction, rm)) = resolve_mapping(
            &m,
            device_index,
            &instances,
            &var_index_by_name,
            &debug_maps,
        ) {
            match direction {
                Direction::Input => unit_inputs[unit_index].push(rm),
                Direction::Output => unit_outputs[unit_index].push(rm),
            }
        }
    }
    for (i, unit) in units.iter().enumerate() {
        tracing::info!(
            instance = %unit.instance,
            task = %unit.task_name,
            interval_ms = unit.interval_ms,
            priority = unit.priority,
            inputs = unit_inputs[i].len(),
            outputs = unit_outputs[i].len(),
            retain_vars = unit.retain_vars.len(),
            "unit scheduled"
        );
    }
    tracing::info!(
        units = n_units,
        devices = devices.len(),
        state_path = ?state_path,
        "scan loop ready"
    );

    // ---- Resolve RETAIN names → (unit, state-file key, var index) ----
    // Keys are `instance.variable` when several units run (so same-
    // named retain vars in different units don't collide on disk) and
    // bare names for the single-unit case — the historical format.
    let mut retain_entries: Vec<(usize, String, u16)> = Vec::new();
    for (i, unit) in units.iter().enumerate() {
        for name in &unit.retain_vars {
            match var_index_by_name[i].get(name) {
                Some(&idx) => {
                    let key = if n_units > 1 {
                        format!("{}.{}", unit.instance, name)
                    } else {
                        name.clone()
                    };
                    retain_entries.push((i, key, idx));
                }
                None => {
                    // Possible if the user removed a RETAIN var from
                    // source between runs but the state file still
                    // references it. The next save rewrites the file
                    // without the stale entry.
                    tracing::warn!(instance = %unit.instance, var = %name, "retain var not in debug map; skipping");
                }
            }
        }
    }

    // ---- Restore RETAIN values from disk before scanning starts ----
    if !retain_entries.is_empty() {
        if let Some(path) = state_path.as_ref() {
            match retain::load(path) {
                Ok(Some(state)) => {
                    let mut restored = 0;
                    for (i, key, idx) in &retain_entries {
                        // Prefer the canonical key; accept the bare name
                        // as migration for state files written before
                        // the project became multi-PROGRAM.
                        let value = state.vars.get(key).copied().or_else(|| {
                            key.split_once('.')
                                .and_then(|(_, bare)| state.vars.get(bare).copied())
                        });
                        if let Some(value) = value {
                            // Raw 64-bit slot write — lossless for all
                            // IEC types (schema 2; v1 files arrive here
                            // pre-widened by retain::load's migration).
                            if runnings[*i]
                                .write_variable_raw(VarIndex::new(*idx), value)
                                .is_ok()
                            {
                                restored += 1;
                            } else {
                                tracing::warn!(var = %key, "restore write trapped; skipping");
                            }
                        }
                    }
                    tracing::info!(
                        restored,
                        total = retain_entries.len(),
                        saved_at_us = state.saved_at_us,
                        "restored retain variables from state file"
                    );
                }
                Ok(None) => {
                    tracing::info!(?path, "no retain state file yet; starting fresh");
                }
                Err(e) => {
                    // Don't refuse to start on a corrupt state file —
                    // log and continue with defaults. Operator can
                    // inspect or delete the file out-of-band.
                    tracing::warn!(?path, %e, "failed to read retain state file; using defaults");
                }
            }
        }
    }

    // ---- Scan loop ----
    let start = Instant::now();
    let mut last_snapshot = Instant::now() - Duration::from_secs(1);
    let mut last_retain_flush = Instant::now();
    // Every unit is due immediately on the first tick (matches the old
    // single-unit loop, which ran its first scan right away).
    let mut clocks: Vec<UnitClock> = units
        .iter()
        .map(|u| UnitClock {
            next_due: start,
            interval: Duration::from_millis(
                if u.interval_ms == 0 {
                    DEFAULT_SCAN_INTERVAL_MS
                } else {
                    u.interval_ms
                }
                .max(1),
            ),
            scan_count: 0,
            consecutive_overruns: 0,
            warned_overrun: false,
        })
        .collect();
    // Same-tick execution order: task priority (lower runs first),
    // then tasks.toml declaration order.
    let mut exec_order: Vec<usize> = (0..n_units).collect();
    exec_order.sort_by_key(|&i| (units[i].priority, i));
    // Watchdog: any unit accumulating WATCHDOG_OVERRUN_THRESHOLD
    // consecutive overruns fires failsafe once and latches — the loop keeps
    // scanning so operators can see live state, but outputs stay safe until
    // the program is restarted (industrial convention).
    //
    // `watchdog_tripped` IS the latch, not just a fire-once debounce: the
    // output phase below runs only while it is clear. Setting it on trip is
    // what makes the zeros written by `enter_failsafe` stick — otherwise the
    // very next scan would push freshly computed (non-zero) values back over
    // them, and the "outputs stay safe" promise above would be a comment
    // with no code behind it.
    //
    // It is the shared flag rather than a local, so exactly one piece of
    // state answers "are outputs latched off" for both the loop and the
    // HTTP layer.
    let mut prev_paused = false;
    let mut vm_fault = false;
    // Pending fault injection (InjectScanStall). Owned by the loop:
    // written from the command drain, consumed one TICK at a time.
    let mut inject_stall_ms: u64 = 0;
    let mut inject_stall_scans: u32 = 0;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Adopt any devices the background reconnect worker delivered:
        // append to the device set and bind the mappings that were parked
        // at startup because this device wasn't there. Runs before the
        // health refresh so the adopted device flips healthy this round.
        while let Ok(dev) = reconnect_rx.try_recv() {
            let name = dev.name().to_string();
            let device_index = devices.len();
            devices.push(dev);
            let mut bound = 0usize;
            let mut parked = Vec::with_capacity(pending_mappings.len());
            for m in pending_mappings.drain(..) {
                if m.device != name {
                    parked.push(m);
                    continue;
                }
                if let Some((unit_index, direction, rm)) = resolve_mapping(
                    &m,
                    device_index,
                    &instances,
                    &var_index_by_name,
                    &debug_maps,
                ) {
                    match direction {
                        Direction::Input => unit_inputs[unit_index].push(rm),
                        Direction::Output => unit_outputs[unit_index].push(rm),
                    }
                    bound += 1;
                }
            }
            pending_mappings = parked;
            tracing::info!(
                device = %name,
                mappings = bound,
                "device joined after background reconnect; its variables are live"
            );
            // A device that joins AFTER the watchdog latched never sees
            // the failsafe pass — that ran once, over the device set as
            // it stood then — and the output phase is skipped for as
            // long as the latch holds. Without this it would sit at
            // whatever its transport defaults to, and for CANopen /
            // OPC UA the explicit failsafe VALUES (not just zeros) would
            // never be written at all.
            if watchdog_tripped.load(Ordering::Relaxed) {
                if let Some(dev) = devices.last_mut() {
                    match dev.enter_failsafe().await {
                        Ok(()) => tracing::warn!(
                            device = %name,
                            "device joined while the watchdog was latched — put \
                             straight into failsafe instead of going live"
                        ),
                        Err(e) => tracing::error!(
                            device = %name, %e,
                            "failsafe on a device adopted under a latched watchdog failed"
                        ),
                    }
                }
            }
        }

        // Mirror each live device's transport health for the monitor layer.
        // Cheap: one atomic load per device, and the lock is uncontended
        // except when an HTTP handler snapshots it. Entries for devices
        // that never connected aren't in `devices` and stay `false`.
        if let Ok(mut health) = device_health.lock() {
            for entry in health.iter_mut() {
                if let Some(d) = devices.iter().find(|d| d.name() == entry.name) {
                    entry.healthy = d.is_healthy();
                }
            }
        }

        // Drain any pending out-of-band commands (variable writes,
        // forces, unforces). Done EVERY iteration including when
        // paused — that's the whole point of pausing, you want to
        // poke values without scanning.
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                RuntimeCommand::WriteVariable {
                    name,
                    value,
                    governed,
                    ack,
                } => {
                    let result = match resolve_var(&name, &instances, &var_index_by_name) {
                        Some((u, idx)) => {
                            // Internal runtime writes (the pulse reset)
                            // bypass governance by design — see
                            // `write_variable_ungoverned`.
                            let allowed = if governed {
                                enforce_write_governance(
                                    &governance,
                                    &name,
                                    u,
                                    idx,
                                    value,
                                    &instances,
                                    &debug_maps,
                                )
                            } else {
                                Ok(value)
                            };
                            match allowed {
                                Ok(v) => match runnings[u].write_variable(VarIndex::new(idx), v) {
                                    // The value is in the VM either way. Report
                                    // — do not refuse — when its own device
                                    // cannot carry it out: scoped to THIS
                                    // variable's device, so an unrelated island
                                    // going down never costs the operator a
                                    // control it could still have used.
                                    Ok(()) => Ok(WriteOutcome {
                                        value: v,
                                        undelivered_device: unit_outputs[u]
                                            .iter()
                                            .find(|rm| rm.var_index == idx)
                                            .and_then(|rm| devices.get(rm.device_index))
                                            .filter(|dev| !dev.is_healthy())
                                            .map(|dev| dev.name().to_string()),
                                    }),
                                    Err(trap) => Err(RuntimeWriteError::Vm(format!("{trap:?}"))),
                                },
                                Err(e) => Err(e),
                            }
                        }
                        None => Err(RuntimeWriteError::UnknownVariable(name)),
                    };
                    let _ = ack.send(result);
                }
                RuntimeCommand::ForceVariable { name, value, ack } => {
                    // Reject unknown names early so the caller gets
                    // immediate feedback instead of a silent no-op.
                    let result = if resolve_var(&name, &instances, &var_index_by_name).is_some() {
                        if let Ok(mut f) = forces.lock() {
                            f.insert(name.clone(), value);
                        }
                        Ok(value)
                    } else {
                        Err(RuntimeWriteError::UnknownVariable(name))
                    };
                    let _ = ack.send(result);
                }
                RuntimeCommand::UnforceVariable { name, ack } => {
                    if let Ok(mut f) = forces.lock() {
                        f.remove(&name);
                    }
                    let _ = ack.send(Ok(()));
                }
                RuntimeCommand::InjectScanStall {
                    stall_ms,
                    scans,
                    ack,
                } => {
                    let clamped_ms = stall_ms.min(MAX_INJECT_STALL_MS);
                    let clamped_scans = scans.min(MAX_INJECT_SCANS);
                    if clamped_ms != stall_ms || clamped_scans != scans {
                        tracing::warn!(
                            requested_stall_ms = stall_ms,
                            requested_scans = scans,
                            stall_ms = clamped_ms,
                            scans = clamped_scans,
                            "scan-stall injection clamped to the safety ceiling"
                        );
                    }
                    let stall_ms = clamped_ms;
                    let scans = clamped_scans;
                    inject_stall_ms = stall_ms;
                    inject_stall_scans = scans;
                    tracing::warn!(
                        stall_ms,
                        scans,
                        "scan-stall injection armed — the next scans will \
                         deliberately overrun; expect the watchdog to trip"
                    );
                    let _ = ack.send(());
                }
            }
        }

        // Mode check. Paused → skip every unit's cycle (no IO, no run,
        // no output) — the whole plant freezes together. Step{remaining}
        // → execute this tick, decrement at the bottom; when remaining
        // hits 0 transition to Paused. Snapshots still go out while
        // paused (at the regular 10 Hz cadence) so Monitor keeps
        // showing the frozen state.
        let current_mode = mode.lock().map(|m| *m).unwrap_or(RuntimeMode::Running);
        if matches!(current_mode, RuntimeMode::Paused) {
            prev_paused = true;
            if last_snapshot.elapsed() >= SNAPSHOT_PERIOD {
                let now_us = start.elapsed().as_micros() as u64;
                let snapshot = build_snapshot(
                    &runnings,
                    &debug_maps,
                    &string_layouts,
                    &instances,
                    &shared_names,
                    &clocks,
                    now_us,
                );
                let _ = snapshot_tx.send(snapshot);
                last_snapshot = Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        }
        if prev_paused {
            // Coming out of a pause (resume or step): re-anchor every
            // unit so the frozen time isn't booked as overruns, and
            // every unit runs on this first tick — a step from pause
            // advances the entire plant by one scan.
            let now = Instant::now();
            for c in clocks.iter_mut() {
                c.next_due = now;
            }
            prev_paused = false;
        }

        // Forces are cloned once per tick (not per unit) so all units
        // see a consistent force set; resolution per name picks the
        // owning unit. Clone under the lock, write outside it.
        let force_set: Vec<(String, i32)> = match forces.lock() {
            Ok(f) => f.iter().map(|(k, &v)| (k.clone(), v)).collect(),
            Err(_) => Vec::new(),
        };

        let tick_now = Instant::now();
        let mut ran_any = false;

        // Fault injection: one stall per TICK, ahead of the unit loop.
        // Budgeting per unit-scan was wrong twice over: the watchdog
        // counts consecutive overruns PER UNIT, and a single stall makes
        // every unit due in this tick late — so an N-unit project drained
        // `scans` N x faster than it produced late ticks and the
        // documented default (THRESHOLD + 1) could never reach the
        // threshold. Sleeping here still lands inside every due unit's
        // measured window: each unit's cadence check compares `next_due`
        // against the clock read AFTER its scan body.
        if inject_stall_scans > 0 && exec_order.iter().any(|&i| clocks[i].next_due <= tick_now) {
            inject_stall_scans -= 1;
            let mut left = Duration::from_millis(inject_stall_ms);
            while !left.is_zero() {
                if stop.load(Ordering::Relaxed) {
                    // Abandon the rest of the budget: a stop request
                    // outranks a test fault.
                    inject_stall_scans = 0;
                    break;
                }
                let slice = left.min(INJECT_STALL_SLICE);
                tokio::time::sleep(slice).await;
                left -= slice;
            }
        }

        for &i in &exec_order {
            if clocks[i].next_due > tick_now {
                continue;
            }
            ran_any = true;

            // Input phase: bus → this unit's VM variables
            for rm in &unit_inputs[i] {
                let Some(dev) = devices.get_mut(rm.device_index) else {
                    continue;
                };
                match dev.read_channel(&rm.channel).await {
                    Ok(value) => {
                        let _ = if rm.is_lreal {
                            // 64-bit lane: any channel value widens to f64
                            // losslessly; the slot takes the double's bits.
                            runnings[i].write_variable_raw(
                                VarIndex::new(rm.var_index),
                                value.to_f64().to_bits(),
                            )
                        } else {
                            runnings[i].write_variable(
                                VarIndex::new(rm.var_index),
                                value.to_vm_bits(rm.is_real),
                            )
                        };
                    }
                    Err(e) => tracing::debug!(channel = %rm.channel, %e, "input read failed"),
                }
            }

            // Force phase: apply pinned variables that resolve to this
            // unit AFTER its input read (a forced value beats the bus)
            // and BEFORE its run_round (the program sees the forced
            // value).
            for (name, value) in &force_set {
                if let Some((u, idx)) = resolve_var(name, &instances, &var_index_by_name) {
                    if u == i {
                        let _ = runnings[i].write_variable(VarIndex::new(idx), *value);
                    }
                }
            }

            // Run one scan for this unit
            let now_us = start.elapsed().as_micros() as u64;
            if let Err(ctx) = runnings[i].run_round(now_us) {
                tracing::error!(instance = %instances[i], ?ctx.trap, "vm trap during run_round");
                // Record WHY before breaking — the watch write is what
                // wakes the HTTP layer's forwarder, turning a silent halt
                // into a visible fault on /status + SSE.
                fault.send_if_modified(|f| {
                    if f.is_none() {
                        *f = Some(format!("VM trap in {}: {:?}", instances[i], ctx.trap));
                        true
                    } else {
                        false
                    }
                });
                // One faulted unit stops the whole plant — consistent
                // with pause semantics ("the plant freezes together")
                // and the safest default; the wrapper's failsafe pass
                // then zeroes every output.
                vm_fault = true;
                break;
            }
            clocks[i].scan_count += 1;

            // Output phase: this unit's VM variables → bus.
            //
            // Skipped entirely once the watchdog has latched. `enter_failsafe`
            // zeroed the output mirror; the cyclic bus thread keeps flushing
            // that mirror every cycle, so leaving it alone is what holds the
            // outputs safe. Writing here would overwrite the zeros on the very
            // next scan — which is exactly the bug this guard closes. The scan
            // itself keeps running so operators still see live state.
            if !watchdog_tripped.load(Ordering::Relaxed) {
                for rm in &unit_outputs[i] {
                    let Ok(raw) = runnings[i].read_variable_raw(VarIndex::new(rm.var_index)) else {
                        continue;
                    };
                    let value = value_for_type(raw, rm.type_tag);
                    let Some(dev) = devices.get_mut(rm.device_index) else {
                        continue;
                    };
                    if let Err(e) = dev.write_channel(&rm.channel, value).await {
                        // RTSO-HOLD-0731: debug-level swallowing meant we could
                        // not know whether control_word writes were erroring
                        // invisibly during the 2026-08-24 stall. First failure
                        // per channel per process now lands in the journal.
                        let first = OUTPUT_WRITE_WARNED
                            .get_or_init(Default::default)
                            .lock()
                            .map(|mut set| set.insert(rm.channel.clone()))
                            .unwrap_or(false);
                        if first {
                            tracing::warn!(
                                channel = %rm.channel, %e,
                                "output write failed (first occurrence for this \
                                 channel; repeats at debug)"
                            );
                        } else {
                            tracing::debug!(channel = %rm.channel, %e, "output write failed");
                        }
                    }
                }
            }

            // Cadence advance + per-unit watchdog accounting. An
            // in-time scan clears the unit's overrun streak; an
            // overrun slides its anchor to `now + interval` so we
            // don't burn CPU catching up.
            let interval = clocks[i].interval;
            clocks[i].next_due += interval;
            let after = Instant::now();
            if clocks[i].next_due > after {
                clocks[i].consecutive_overruns = 0;
            } else {
                clocks[i].consecutive_overruns = clocks[i].consecutive_overruns.saturating_add(1);
                // Counted every time, unlike the warning below, which fires
                // once per unit and then goes quiet for the life of the run.
                scan_overruns.fetch_add(1, Ordering::Relaxed);
                if !clocks[i].warned_overrun {
                    let overrun = after - clocks[i].next_due;
                    tracing::warn!(
                        instance = %instances[i],
                        overrun_us = overrun.as_micros() as u64,
                        interval_us = clocks[i].interval.as_micros() as u64,
                        "scan overran its budget — sliding cadence forward and \
                         suppressing further overrun warnings (the trace will \
                         show the drift in scan_count vs wall clock)"
                    );
                    clocks[i].warned_overrun = true;
                }
                if !watchdog_tripped.load(Ordering::Relaxed)
                    && clocks[i].consecutive_overruns >= WATCHDOG_OVERRUN_THRESHOLD
                {
                    // fallthrough below trips the watchdog
                    tracing::error!(
                        instance = %instances[i],
                        consecutive = clocks[i].consecutive_overruns,
                        threshold = WATCHDOG_OVERRUN_THRESHOLD,
                        interval_us = clocks[i].interval.as_micros() as u64,
                        "watchdog tripped — engaging failsafe (outputs zeroed \
                         and latched off; restart the program to re-arm)"
                    );
                    for dev in devices.iter_mut() {
                        if let Err(e) = dev.enter_failsafe().await {
                            tracing::warn!(device = %dev.name(), %e, "watchdog failsafe call failed");
                        }
                    }
                    // Latch: from here on the output phase is skipped, so the
                    // zeros just written stay on the bus until a restart.
                    watchdog_tripped.store(true, Ordering::Relaxed);
                }
                clocks[i].next_due = after + clocks[i].interval;
            }
            // The streak the watchdog actually watches is the longest one on
            // any unit, so publish that rather than whichever unit ran last.
            consecutive_scan_overruns.store(
                clocks
                    .iter()
                    .map(|c| c.consecutive_overruns)
                    .max()
                    .unwrap_or(0),
                Ordering::Relaxed,
            );
        }
        if vm_fault {
            break;
        }

        // If we're stepping, decrement once per tick in which at least
        // one unit ran, and auto-pause at 0. Done AFTER the tick so
        // `step(1)` means "advance by exactly one scheduler tick"
        // (from pause, that's one scan of every unit).
        if ran_any {
            if let RuntimeMode::Step { remaining } = current_mode {
                if let Ok(mut m) = mode.lock() {
                    *m = if remaining <= 1 {
                        RuntimeMode::Paused
                    } else {
                        RuntimeMode::Step {
                            remaining: remaining - 1,
                        }
                    };
                }
            }
        }

        // Snapshot at ~10 Hz (also when no unit was due this tick —
        // idle wake-ups are capped at SNAPSHOT_PERIOD below).
        if last_snapshot.elapsed() >= SNAPSHOT_PERIOD {
            let now_us = start.elapsed().as_micros() as u64;
            let snapshot = build_snapshot(
                &runnings,
                &debug_maps,
                &string_layouts,
                &instances,
                &shared_names,
                &clocks,
                now_us,
            );
            let _ = snapshot_tx.send(snapshot);
            last_snapshot = Instant::now();
        }

        // Persist RETAIN values on a coarse cadence. The window of
        // potential loss on power-cut is bounded by RETAIN_FLUSH_INTERVAL.
        // Writes are atomic (tmp + rename) so a crash during flush
        // can't corrupt the file. Skipped entirely when no retain
        // vars are declared or no path was configured.
        if !retain_entries.is_empty()
            && state_path.is_some()
            && last_retain_flush.elapsed() >= RETAIN_FLUSH_INTERVAL
        {
            let now_us = start.elapsed().as_micros() as u64;
            persist_retain_values(
                state_path.as_deref().unwrap(),
                &retain_entries,
                &runnings,
                now_us,
                max_scan_count(&clocks),
            );
            last_retain_flush = Instant::now();
        }

        // Sleep until the earliest due unit. Capped at SNAPSHOT_PERIOD
        // so stop requests, commands, and paused-state snapshots stay
        // responsive even when every task interval is long.
        let earliest = clocks
            .iter()
            .map(|c| c.next_due)
            .min()
            .expect("at least one unit");
        let now = Instant::now();
        if earliest > now {
            tokio::time::sleep((earliest - now).min(SNAPSHOT_PERIOD)).await;
        }
    }

    // Final RETAIN flush on graceful exit. Captures whatever the
    // last completed scan produced — that's the right "checkpoint"
    // value to reload on next startup.
    if !retain_entries.is_empty() {
        if let Some(path) = state_path.as_deref() {
            let now_us = start.elapsed().as_micros() as u64;
            persist_retain_values(
                path,
                &retain_entries,
                &runnings,
                now_us,
                max_scan_count(&clocks),
            );
            tracing::info!(?path, "final retain flush on stop");
        }
    }

    for running in runnings {
        let _ = running.stop();
    }
}

/// The merged snapshot's scan_count: the max across units (the fastest
/// unit's count — closest analogue of the old single-unit counter).
fn max_scan_count(clocks: &[UnitClock]) -> u64 {
    clocks.iter().map(|c| c.scan_count).max().unwrap_or(0)
}

/// RTSO-HOLD-0731: channels whose output-write failure has already been
/// warned about once this process (repeats drop back to debug level).
static OUTPUT_WRITE_WARNED: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::OnceLock::new();

fn value_for_type(raw: u64, type_tag: u8) -> ChannelValue {
    match type_tag {
        iec_type_tag::BOOL => ChannelValue::Bool(raw != 0),
        iec_type_tag::USINT | iec_type_tag::UINT | iec_type_tag::BYTE | iec_type_tag::WORD => {
            ChannelValue::U16(raw as u16)
        }
        // REAL vars store IEEE-754 bits in the VM cell — decode to a true
        // float so analog outputs keep their fraction on the bus.
        iec_type_tag::REAL => ChannelValue::Real(f32::from_bits(raw as u32)),
        // LREAL: the slot is the double's full bit pattern.
        iec_type_tag::LREAL => ChannelValue::F64(f64::from_bits(raw)),
        _ => ChannelValue::I32(raw as i32),
    }
}

/// Snapshot every RETAIN variable's current VM value (across all units)
/// and atomically write the one merged state file. Keys are already
/// canonical (`instance.variable` when several units run, bare names
/// otherwise) — see the retain_entries construction. Raw slot values
/// verbatim (schema 2): lossless for every IEC type including LREAL /
/// LINT / ULINT / LWORD. Errors are logged but don't crash the scan
/// loop — losing one flush window is acceptable; halting the program
/// is not.
fn persist_retain_values(
    state_path: &std::path::Path,
    retain_entries: &[(usize, String, u16)],
    runnings: &[VmRunning],
    now_us: u64,
    scan_count: u64,
) {
    let mut vars: HashMap<String, u64> = HashMap::with_capacity(retain_entries.len());
    for (unit, key, idx) in retain_entries {
        if let Ok(raw) = runnings[*unit].read_variable_raw(VarIndex::new(*idx)) {
            vars.insert(key.clone(), raw);
        }
    }
    let state = crate::retain::build(vars, now_us, scan_count);
    if let Err(e) = crate::retain::save(state_path, &state) {
        tracing::warn!(?state_path, %e, "retain flush failed");
    }
}

/// Merge every unit's variables into one snapshot. Names declared by
/// more than one unit are disambiguated as `instance.variable`
/// (`shared_names` is precomputed at startup); unique names — and
/// everything in a single-unit run — stay bare so today's projects see
/// no UI churn.
fn build_snapshot(
    runnings: &[VmRunning],
    debug_maps: &[HashMap<u16, VarDebugInfo>],
    string_layouts: &[HashMap<u16, u32>],
    instances: &[String],
    shared_names: &std::collections::HashSet<String>,
    clocks: &[UnitClock],
    now_us: u64,
) -> VarSnapshot {
    let mut vars = Vec::new();
    for (u, running) in runnings.iter().enumerate() {
        let num_vars = running.num_variables();
        vars.reserve(num_vars as usize);
        // STRING bytes live in this unit's data region, not the var slot;
        // read from the SAME unit `u` we're iterating so multi-PROGRAM
        // instances never cross-read each other's strings.
        let data_region = running.data_region();
        // Skip slots that have no debug-map entry (unnamed VM scratch
        // storage for FB internals / non-instantiated POUs) and dedup
        // names that collide within the unit — a unit's source carries
        // every FB/FUNCTION POU, so two POUs declaring the same
        // variable name both get debug entries with that name. We keep
        // the first-seen slot; surfacing the same name twice in the
        // Monitor pane is worse than hiding the inactive duplicate
        // (which is usually idle at zero anyway).
        let mut seen = std::collections::HashSet::<String>::new();
        for i in 0..num_vars {
            let raw = match running.read_variable_raw(VarIndex::new(i)) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let Some(info) = debug_maps[u].get(&i) else {
                continue;
            };
            if !seen.insert(info.name.clone()) {
                continue;
            }
            let name = if shared_names.contains(&info.name) {
                format!("{}.{}", instances[u], info.name)
            } else {
                info.name.clone()
            };
            vars.push(VarValue {
                name,
                type_name: info.type_name.clone(),
                value: format_value(raw, info, string_layouts[u].get(&i).copied(), data_region),
                bits: raw,
            });
        }
    }
    VarSnapshot {
        timestamp_us: now_us,
        scan_count: max_scan_count(clocks),
        vars,
    }
}

/// Render a variable's slot value. STRING vars don't live in the slot —
/// their bytes are at `data_offset` in the VM's data region with layout
/// `[max_len: u16][cur_len: u16][bytes…]`; we read them out and format
/// as a single-quoted IEC literal. WSTRING isn't yet populated by
/// ironplc's codegen (literal encoding `unreachable!`s on char_width=2),
/// so we surface a placeholder rather than fake content. Everything else
/// delegates to ironplc's standard formatter.
fn format_value(
    raw: u64,
    info: &VarDebugInfo,
    string_offset: Option<u32>,
    data_region: &[u8],
) -> String {
    match info.iec_type_tag {
        iec_type_tag::STRING => {
            match string_offset.and_then(|off| read_string_value(data_region, off)) {
                Some(text) => text,
                // No layout entry, or the offset is bogus — show that we
                // know it's a string but couldn't decode it, not a lie.
                None => "'<invalid>'".into(),
            }
        }
        // ironplc's codegen doesn't yet emit WSTRING data; see
        // `compiler/codegen/src/compile.rs` `unreachable!("WSTRING literal
        // encoding is not yet supported")`. Until upstream lands it, we
        // surface a placeholder so operators know what's missing.
        iec_type_tag::WSTRING => "'<wstring>'".into(),
        _ => format_variable_value(raw, info.iec_type_tag),
    }
}

/// Reads a STRING value at `data_offset` in the VM's data region and
/// renders it as an IEC 61131-3 single-quoted literal (e.g. `'STARTUP'`).
///
/// Wire format: `[max_len: u16][cur_len: u16][cur_len bytes of UTF-8]`,
/// little-endian, as written by ironplc's codegen (see
/// `compiler/codegen/src/compile_setup.rs` `string_region_size`).
/// Non-printable bytes, `$`, and `'` are escaped per IEC string-literal
/// rules so the resulting text is a valid round-trippable literal.
///
/// Returns `None` when the layout offset is past the end of the data
/// region or the recorded `cur_len` would read off the end (e.g. a stale
/// debug section). Bridging a `None` to a placeholder is the caller's job.
fn read_string_value(data_region: &[u8], data_offset: u32) -> Option<String> {
    let off = data_offset as usize;
    if off + STRING_HEADER_BYTES > data_region.len() {
        return None;
    }
    let cur_len = u16::from_le_bytes([data_region[off + 2], data_region[off + 3]]) as usize;
    let start = off + STRING_HEADER_BYTES;
    let end = start + cur_len;
    if end > data_region.len() {
        return None;
    }
    Some(format_iec_string_literal(&data_region[start..end]))
}

/// Renders raw STRING bytes as an IEC 61131-3 single-quoted string literal.
/// Each byte is either passed through as printable ASCII, replaced with one
/// of the named `$`-escapes (`$T`, `$L`, `$P`, `$R`, `$$`, `$'`), or emitted
/// as a `$XX` two-digit hex escape. Mirrors the format ironplc uses in its
/// playground variable dump.
fn format_iec_string_literal(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 2);
    out.push('\'');
    for &b in bytes {
        match b {
            b'$' => out.push_str("$$"),
            b'\'' => out.push_str("$'"),
            0x09 => out.push_str("$T"),
            0x0A => out.push_str("$L"),
            0x0C => out.push_str("$P"),
            0x0D => out.push_str("$R"),
            0x20..=0x7E => out.push(b as char),
            _ => out.push_str(&format!("${b:02X}")),
        }
    }
    out.push('\'');
    out
}

/// Build a `var_index → data_offset` map from the container's STRING
/// layout sub-table (debug section Tag 4). Empty when no STRING vars
/// were declared.
fn build_string_layout_map(container: &Container) -> HashMap<u16, u32> {
    let mut map = HashMap::new();
    if let Some(debug) = &container.debug_section {
        for entry in &debug.string_layouts {
            map.insert(entry.var_index.raw(), entry.data_offset);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use iocore::IoError;

    /// Records the order of the safety-critical shutdown callbacks so a
    /// test can assert `enter_failsafe` runs before `shutdown` — the
    /// sequence that guarantees zeroed outputs reach the wire before a
    /// device joins its background I/O thread.
    struct MockDevice {
        name: String,
        failsafe_called: Arc<AtomicBool>,
        shutdown_called: Arc<AtomicBool>,
        failsafe_before_shutdown: Arc<AtomicBool>,
        /// Shared health flag so a test can flip transport health
        /// mid-run and watch it surface through the handle.
        healthy: Arc<AtomicBool>,
    }

    impl MockDevice {
        fn named(name: &str) -> (Self, Arc<AtomicBool>) {
            let healthy = Arc::new(AtomicBool::new(true));
            (
                MockDevice {
                    name: name.into(),
                    failsafe_called: Arc::new(AtomicBool::new(false)),
                    shutdown_called: Arc::new(AtomicBool::new(false)),
                    failsafe_before_shutdown: Arc::new(AtomicBool::new(false)),
                    healthy: healthy.clone(),
                },
                healthy,
            )
        }
    }

    #[async_trait::async_trait]
    impl IoDevice for MockDevice {
        fn name(&self) -> &str {
            &self.name
        }
        async fn read_channel(&mut self, _channel: &str) -> Result<ChannelValue, IoError> {
            Ok(ChannelValue::I32(0))
        }
        async fn write_channel(
            &mut self,
            _channel: &str,
            _value: ChannelValue,
        ) -> Result<(), IoError> {
            Ok(())
        }
        async fn enter_failsafe(&mut self) -> Result<(), IoError> {
            self.failsafe_called.store(true, Ordering::Relaxed);
            Ok(())
        }
        async fn shutdown(&mut self) -> Result<(), IoError> {
            if self.failsafe_called.load(Ordering::Relaxed) {
                self.failsafe_before_shutdown.store(true, Ordering::Relaxed);
            }
            self.shutdown_called.store(true, Ordering::Relaxed);
            Ok(())
        }
        fn is_healthy(&self) -> bool {
            self.healthy.load(Ordering::Relaxed)
        }
    }

    fn trivial_container() -> Container {
        // A PROGRAM that starts cleanly; mirrors the lib.rs test source.
        crate::compile(
            "PROGRAM main\n\
                VAR x : INT := 1; END_VAR\n\
                x := x + 1;\n\
            END_PROGRAM",
        )
        .expect("trivial program compiles")
    }

    /// One unit named `"main"` — the conventional instance name for a
    /// single-program run; tests exercising that path use this to stay
    /// representative of production call sites.
    fn single_unit(container: Container, interval_ms: u64) -> ProgramUnit {
        unit("main", container, interval_ms, 1)
    }

    fn unit(instance: &str, container: Container, interval_ms: u64, priority: i32) -> ProgramUnit {
        ProgramUnit {
            instance: instance.into(),
            task_name: "plc_task".into(),
            interval_ms,
            priority,
            container,
            retain_vars: Vec::new(),
        }
    }

    /// Wait for snapshots and return the last one received before
    /// `deadline` elapses. Panics if no snapshot arrived at all.
    async fn last_snapshot_within(
        rx: &mut broadcast::Receiver<VarSnapshot>,
        deadline: Duration,
    ) -> VarSnapshot {
        let until = Instant::now() + deadline;
        let mut last: Option<VarSnapshot> = None;
        loop {
            let now = Instant::now();
            if now >= until {
                break;
            }
            match tokio::time::timeout(until - now, rx.recv()).await {
                Ok(Ok(snap)) => last = Some(snap),
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => break,
            }
        }
        last.expect("scan loop emitted at least one snapshot")
    }

    fn var_value(snap: &VarSnapshot, name: &str) -> String {
        snap.vars
            .iter()
            .find(|v| v.name == name)
            .unwrap_or_else(|| {
                let names: Vec<&str> = snap.vars.iter().map(|v| v.name.as_str()).collect();
                panic!("variable '{name}' not in snapshot; have: {names:?}")
            })
            .value
            .clone()
    }

    #[tokio::test]
    async fn shutdown_runs_failsafe_then_device_shutdown_then_joins() {
        let failsafe_called = Arc::new(AtomicBool::new(false));
        let shutdown_called = Arc::new(AtomicBool::new(false));
        let order_ok = Arc::new(AtomicBool::new(false));
        let dev = MockDevice {
            name: "mock".into(),
            failsafe_called: failsafe_called.clone(),
            shutdown_called: shutdown_called.clone(),
            failsafe_before_shutdown: order_ok.clone(),
            healthy: Arc::new(AtomicBool::new(true)),
        };
        let devices: Vec<Box<dyn IoDevice>> = vec![Box::new(dev)];

        let handle = spawn_units_inner(
            vec![single_unit(trivial_container(), 10)],
            DeviceSource::Prebuilt(devices),
            Vec::new(),
            None,
            WriteGovernance::default(),
        );
        // Let a few scans run so we're stopping a live loop, not a cold one.
        tokio::time::sleep(Duration::from_millis(40)).await;

        // The whole drain must finish well within the supervisor budget;
        // the timeout also keeps a regression from hanging the suite.
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown joined the scan thread within 5s");

        assert!(
            failsafe_called.load(Ordering::Relaxed),
            "failsafe must run on a clean stop"
        );
        assert!(
            shutdown_called.load(Ordering::Relaxed),
            "device shutdown must run on a clean stop"
        );
        assert!(
            order_ok.load(Ordering::Relaxed),
            "failsafe must precede device shutdown so zeroed outputs flush before the join"
        );
    }

    /// A device that fails its initial connect must come back through the
    /// background reconnect worker: /discover flips to connected, health
    /// flips true, and — the part an operator actually cares about — the
    /// mapping that was parked at startup binds, so the device's register
    /// value flows into the PLC variable mid-run without a restart.
    #[tokio::test]
    async fn background_reconnect_adopts_failed_device_and_binds_mappings() {
        use project::{
            ModbusChannel, ModbusChannelKind, ModbusConfig, ModbusDataType, ModbusTcpParams,
            ModbusTransport, ModbusWordOrder,
        };

        // Reserve a port, then close the listener → initial connect refused.
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let addr = reserved.local_addr().expect("local addr");
        drop(reserved);

        let spec = DeviceSpec {
            name: "pm".into(),
            config: ProtocolConfig::Modbus(ModbusConfig {
                transport: ModbusTransport::Tcp(ModbusTcpParams {
                    host: addr.ip().to_string(),
                    port: addr.port(),
                }),
                slave_id: 1,
                poll_interval_ms: 20,
                timeout_ms: Some(200),
                reconnect_backoff_ms: None,
                channels: vec![ModbusChannel {
                    name: "hr0".into(),
                    kind: ModbusChannelKind::HoldingRegister,
                    address: 0,
                    data_type: ModbusDataType::U16,
                    word_order: ModbusWordOrder::HiLo,
                    access: Default::default(),
                }],
            }),
        };
        let mapping = Mapping {
            application: "main".into(),
            variable: "x".into(),
            direction: Direction::Input,
            device: "pm".into(),
            channel: "hr0".into(),
            unit: None,
            min: None,
            max: None,
            description: None,
        };
        // `y := x` so the input mapping's value is observable unmodified.
        let container = crate::compile(
            "PROGRAM main\n\
                VAR x : INT; y : INT; END_VAR\n\
                y := x;\n\
            END_PROGRAM",
        )
        .expect("program compiles");

        let handle = spawn_units_inner(
            vec![single_unit(container, 10)],
            DeviceSource::Specs(vec![spec]),
            vec![mapping],
            None,
            WriteGovernance::default(),
        );

        // Startup outcome: configured but down, and visibly so.
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle.device_reports().is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let reports = handle.device_reports();
        assert_eq!(reports.len(), 1);
        assert!(!reports[0].connected, "no endpoint yet — connect must fail");
        assert!(
            !handle.device_health()[0].healthy,
            "failed connect must surface as unhealthy"
        );

        // Bring a real slave up on the reserved port with hr[0] = 42.
        let slave = iomap_modbus::DemoSlave::new();
        slave.holding_registers().lock().expect("hr")[0] = 42;
        let server = tokio::spawn(iomap_modbus::run_demo_slave(addr, slave));

        // Worker backoff is 1 s, 2 s, 4 s… — well within this budget.
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if handle.device_reports()[0].connected && handle.device_health()[0].healthy {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            handle.device_reports()[0].connected,
            "background reconnect must flip the /discover report"
        );
        assert!(
            handle.device_health()[0].healthy,
            "adopted device must go healthy"
        );

        // The parked mapping must now be live: register 42 → x → y.
        let mut rx = handle.subscribe();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = false;
        while Instant::now() < deadline && !seen {
            let snap = last_snapshot_within(&mut rx, Duration::from_millis(300)).await;
            seen = var_value(&snap, "x") == "42";
        }
        assert!(
            seen,
            "register value must flow through the late-bound mapping"
        );

        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
        server.abort();
    }

    /// The handle must mirror a device's live transport health: `true`
    /// while the adapter reports healthy, `false` within a scan round of
    /// the adapter flipping (cable pulled / poll loop failing), and back —
    /// this is the surface /health and /status serve to operators, whose
    /// only other signal is "variable values stopped changing".
    #[tokio::test]
    async fn device_health_mirrors_adapter_flag() {
        let (dev, healthy) = MockDevice::named("bus0");
        let handle = spawn_units_inner(
            vec![single_unit(trivial_container(), 5)],
            DeviceSource::Prebuilt(vec![Box::new(dev)]),
            Vec::new(),
            None,
            WriteGovernance::default(),
        );

        // Wait for the connect pass to seed the health list.
        let deadline = Instant::now() + Duration::from_secs(2);
        while handle.device_health().is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let seeded = handle.device_health();
        assert_eq!(seeded.len(), 1, "one configured device");
        assert_eq!(seeded[0].name, "bus0");
        assert!(seeded[0].healthy, "healthy adapter seeds healthy");

        // Flip the adapter's flag; the scan loop refreshes each round.
        healthy.store(false, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(2);
        while handle.device_health()[0].healthy && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            !handle.device_health()[0].healthy,
            "unhealthy adapter must surface through the handle"
        );

        // And recovery propagates too.
        healthy.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !handle.device_health()[0].healthy && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(handle.device_health()[0].healthy, "recovery must surface");

        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }

    /// Overruns short of the watchdog threshold are invisible today: the log
    /// warns once per unit and then suppresses itself, and `watchdog_tripped`
    /// only covers the terminal case. A runtime quietly missing every fourth
    /// deadline looks identical on /status to one meeting all of them.
    ///
    /// These counters are what make "configured 2 ms, achieving something
    /// else" answerable without grepping a journal that has already rolled.
    #[tokio::test]
    async fn scan_overruns_are_counted_and_the_streak_clears() {
        let handle = spawn_units_inner(
            vec![single_unit(trivial_container(), 5)],
            DeviceSource::Prebuilt(vec![]),
            Vec::new(),
            None,
            WriteGovernance::default(),
        );

        // Negative control. Without it a fixture that overruns on its own
        // would make the assertions below pass for the wrong reason.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(handle.scan_overruns(), 0, "an idle 5 ms unit must keep up");
        assert_eq!(handle.consecutive_scan_overruns(), 0);

        // Stay UNDER the trip threshold: this is the middle ground that had
        // no signal at all, not the latched end state.
        handle
            .inject_scan_stall(12, WATCHDOG_OVERRUN_THRESHOLD - 2)
            .await
            .expect("inject reaches the loop");
        let deadline = Instant::now() + Duration::from_secs(2);
        while handle.scan_overruns() == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let during = handle.scan_overruns();
        assert!(
            during > 0,
            "12 ms stalls against a 5 ms interval must count"
        );
        assert!(
            !handle.watchdog_tripped(),
            "under the threshold must not trip"
        );

        // The streak clears once scans land on time again; the total does not.
        let deadline = Instant::now() + Duration::from_secs(2);
        while handle.consecutive_scan_overruns() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            handle.consecutive_scan_overruns(),
            0,
            "recovery must clear the streak — it is the distance to the trip"
        );
        assert!(
            handle.scan_overruns() >= during,
            "the total is history and must not be walked back by recovery"
        );

        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }

    /// `scan_count` is the MAX across units, so the fastest task is what
    /// makes it move — and that is the number a client needs to size a
    /// staleness window. Taking the max here (or the first unit's) would
    /// hand out a window several times too wide on a mixed-cadence project.
    #[tokio::test]
    async fn scan_period_reports_the_fastest_unit() {
        let handle = spawn_units_inner(
            vec![
                unit("slow", trivial_container(), 5_000, 1),
                unit("fast", trivial_container(), 20, 1),
            ],
            DeviceSource::Prebuilt(vec![]),
            Vec::new(),
            None,
            WriteGovernance::default(),
        );
        assert_eq!(handle.scan_period_ms(), 20);
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");

        // Normalised exactly as UnitClock is: 0 means "unspecified", which
        // the scheduler runs at the default rather than as a busy loop.
        let handle = spawn_units_inner(
            vec![unit("default", trivial_container(), 0, 1)],
            DeviceSource::Prebuilt(vec![]),
            Vec::new(),
            None,
            WriteGovernance::default(),
        );
        assert_eq!(handle.scan_period_ms(), DEFAULT_SCAN_INTERVAL_MS as u32);
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }

    /// A write must be judged against the device that carries THAT
    /// variable, not against the health of the device list as a whole.
    ///
    /// An operator's Stop is a variable write. Refusing it because some
    /// unrelated island dropped costs a control that would still have
    /// worked, and there is no coupling declaration anywhere in the schema
    /// that could honestly widen the blocked set — a device does not report
    /// what it is coupled to. So the scope is the mapping, and the answer is
    /// a report, not a refusal: the value lands in the VM and flushes if the
    /// link returns, which beats a Stop that was never commanded.
    #[tokio::test]
    async fn a_write_names_only_its_own_down_device() {
        let container = crate::compile(
            "PROGRAM main\n\
                VAR tick : INT := 1; stop_cmd : INT; internal : INT; END_VAR\n\
                tick := tick + 1;\n\
            END_PROGRAM",
        )
        .expect("program compiles");

        let (mapped, mapped_healthy) = MockDevice::named("bus_mapped");
        let (unrelated, unrelated_healthy) = MockDevice::named("bus_unrelated");
        let handle = spawn_units_inner(
            vec![single_unit(container, 5)],
            DeviceSource::Prebuilt(vec![Box::new(mapped), Box::new(unrelated)]),
            vec![project::Mapping {
                application: "main".into(),
                variable: "stop_cmd".into(),
                direction: project::Direction::Output,
                device: "bus_mapped".into(),
                channel: "coil".into(),
                unit: None,
                min: None,
                max: None,
                description: None,
            }],
            None,
            WriteGovernance::default(),
        );

        let health_of = |name: &str| {
            handle
                .device_health()
                .into_iter()
                .find(|d| d.name == name)
                .map(|d| d.healthy)
        };
        let settle = |name: &'static str, want: bool| async move {
            let deadline = Instant::now() + Duration::from_secs(2);
            while health_of(name) != Some(want) && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(health_of(name), Some(want), "{name} health must settle");
        };

        settle("bus_mapped", true).await;
        let delivered = handle.write_variable("stop_cmd", 1).await.unwrap();
        assert_eq!(
            delivered.undelivered_device, None,
            "healthy link warns nobody"
        );

        mapped_healthy.store(false, Ordering::Relaxed);
        settle("bus_mapped", false).await;
        let undelivered = handle.write_variable("stop_cmd", 1).await.unwrap();
        assert_eq!(
            undelivered.value, 1,
            "an undelivered write is still applied"
        );
        assert_eq!(
            undelivered.undelivered_device.as_deref(),
            Some("bus_mapped"),
            "the operator must not read this as a plant that obeyed"
        );

        // The point of the whole change: someone else's dead bus is not a
        // reason to take this control away.
        mapped_healthy.store(true, Ordering::Relaxed);
        settle("bus_mapped", true).await;
        unrelated_healthy.store(false, Ordering::Relaxed);
        settle("bus_unrelated", false).await;
        let unaffected = handle.write_variable("stop_cmd", 1).await.unwrap();
        assert_eq!(
            unaffected.undelivered_device, None,
            "an unrelated island going down must not cost this control"
        );

        // No Output mapping means nothing to deliver — not a silent warning.
        let internal = handle.write_variable("internal", 7).await.unwrap();
        assert_eq!(internal.undelivered_device, None);

        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }

    /// F64 device channel → LREAL var → F64 device channel, verifying
    /// the 64-bit lane end to end. The probe value carries more
    /// precision than an f32 can hold, so any accidental trip through
    /// the 32-bit path fails the exact-equality assert.
    struct LrealLoopDevice {
        name: String,
        input_value: f64,
        written: Arc<std::sync::Mutex<Option<f64>>>,
    }

    #[async_trait::async_trait]
    impl IoDevice for LrealLoopDevice {
        fn name(&self) -> &str {
            &self.name
        }
        async fn read_channel(&mut self, _channel: &str) -> Result<ChannelValue, IoError> {
            Ok(ChannelValue::F64(self.input_value))
        }
        async fn write_channel(
            &mut self,
            _channel: &str,
            value: ChannelValue,
        ) -> Result<(), IoError> {
            if let ChannelValue::F64(v) = value {
                *self.written.lock().unwrap() = Some(v);
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn lreal_mapping_round_trips_full_double_precision() {
        // Needs 64-bit mantissa: f32 would collapse the tail digits.
        let probe = 1234.5678901234567_f64;
        assert_ne!(
            probe as f32 as f64, probe,
            "probe must exceed f32 precision"
        );

        let container = crate::compile(
            "PROGRAM main\n\
                VAR lr_in : LREAL; lr_out : LREAL; END_VAR\n\
                lr_out := lr_in;\n\
            END_PROGRAM",
        )
        .expect("LREAL program compiles");

        let written = Arc::new(std::sync::Mutex::new(None));
        let dev = LrealLoopDevice {
            name: "mock".into(),
            input_value: probe,
            written: written.clone(),
        };
        let devices: Vec<Box<dyn IoDevice>> = vec![Box::new(dev)];
        let mappings = vec![
            project::Mapping {
                application: "main".into(),
                variable: "lr_in".into(),
                direction: project::Direction::Input,
                device: "mock".into(),
                channel: "ain".into(),
                unit: None,
                min: None,
                max: None,
                description: None,
            },
            project::Mapping {
                application: "main".into(),
                variable: "lr_out".into(),
                direction: project::Direction::Output,
                device: "mock".into(),
                channel: "aout".into(),
                unit: None,
                min: None,
                max: None,
                description: None,
            },
        ];

        let handle = spawn_units_inner(
            vec![single_unit(container, 10)],
            DeviceSource::Prebuilt(devices),
            mappings,
            None,
            WriteGovernance::default(),
        );
        tokio::time::sleep(Duration::from_millis(80)).await;
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown joins");

        let got = written.lock().unwrap().expect("output channel was written");
        assert_eq!(
            got, probe,
            "LREAL must cross device→VM→device without 32-bit truncation"
        );
    }

    /// Blows the scan deadline from inside the output phase, then counts
    /// every write that still arrives after the watchdog engaged failsafe.
    /// A latched watchdog must leave that count at zero — the zeros written
    /// by `enter_failsafe` have to survive until the program is restarted.
    struct StallingDevice {
        name: String,
        stall: Duration,
        failsafe_engaged: Arc<AtomicBool>,
        writes_after_failsafe: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl IoDevice for StallingDevice {
        fn name(&self) -> &str {
            &self.name
        }
        async fn read_channel(&mut self, _channel: &str) -> Result<ChannelValue, IoError> {
            Ok(ChannelValue::I32(0))
        }
        async fn write_channel(
            &mut self,
            _channel: &str,
            _value: ChannelValue,
        ) -> Result<(), IoError> {
            // Stalling here is what makes the scan overrun its interval; it
            // also puts the counter on the exact path the guard protects.
            tokio::time::sleep(self.stall).await;
            if self.failsafe_engaged.load(Ordering::Relaxed) {
                self.writes_after_failsafe.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }
        async fn enter_failsafe(&mut self) -> Result<(), IoError> {
            self.failsafe_engaged.store(true, Ordering::Relaxed);
            Ok(())
        }
        async fn shutdown(&mut self) -> Result<(), IoError> {
            Ok(())
        }
        fn is_healthy(&self) -> bool {
            true
        }
    }

    /// P1 power-on gate: once the scan watchdog trips, no non-zero output
    /// may reach the bus until an explicit restart.
    ///
    /// Before the latch existed this failed by design: `enter_failsafe`
    /// zeroed the output mirror exactly once, the trip flag only stopped
    /// failsafe from firing a *second* time, and the very next scan pushed
    /// freshly computed values straight back over the zeros. The comment
    /// promised "outputs stay safe until the program is restarted"; nothing
    /// in the executable path delivered it.
    #[tokio::test]
    async fn watchdog_trip_latches_outputs_off_until_restart() {
        let container = crate::compile(
            "PROGRAM main\n\
                VAR n : INT := 0; END_VAR\n\
                n := n + 1;\n\
            END_PROGRAM",
        )
        .expect("counter program compiles");

        let failsafe_engaged = Arc::new(AtomicBool::new(false));
        let writes_after_failsafe = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dev = StallingDevice {
            name: "mock".into(),
            // Comfortably over the 5 ms interval so every scan overruns and
            // the 5-in-a-row threshold is reached in well under the budget.
            stall: Duration::from_millis(12),
            failsafe_engaged: failsafe_engaged.clone(),
            writes_after_failsafe: writes_after_failsafe.clone(),
        };
        let devices: Vec<Box<dyn IoDevice>> = vec![Box::new(dev)];
        let mappings = vec![project::Mapping {
            application: "main".into(),
            variable: "n".into(),
            direction: project::Direction::Output,
            device: "mock".into(),
            channel: "aout".into(),
            unit: None,
            min: None,
            max: None,
            description: None,
        }];

        let handle = spawn_units_inner(
            vec![single_unit(container, 5)],
            DeviceSource::Prebuilt(devices),
            mappings,
            None,
            WriteGovernance::default(),
        );
        // ~5 stalled scans trip the watchdog; the rest of the window is the
        // part that matters — it is where a non-latching build writes again.
        tokio::time::sleep(Duration::from_millis(400)).await;
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown joins");

        assert!(
            failsafe_engaged.load(Ordering::Relaxed),
            "watchdog must trip when every scan overruns its interval"
        );
        assert_eq!(
            writes_after_failsafe.load(Ordering::Relaxed),
            0,
            "no output may be written after the watchdog latched — the zeros \
             from enter_failsafe must hold until an explicit restart"
        );
    }

    /// A latched run must be distinguishable from a healthy one. After the
    /// trip the VM keeps computing and `scan_count` keeps advancing, so
    /// variable values alone never reveal that the bus is holding zeros —
    /// the flag on the handle is the only honest signal.
    #[tokio::test]
    async fn watchdog_latch_surfaces_on_the_handle() {
        let container = crate::compile(
            "PROGRAM main\n\
                VAR n : INT := 0; END_VAR\n\
                n := n + 1;\n\
            END_PROGRAM",
        )
        .expect("counter program compiles");

        let dev = StallingDevice {
            name: "mock".into(),
            stall: Duration::from_millis(12),
            failsafe_engaged: Arc::new(AtomicBool::new(false)),
            writes_after_failsafe: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let devices: Vec<Box<dyn IoDevice>> = vec![Box::new(dev)];
        let mappings = vec![project::Mapping {
            application: "main".into(),
            variable: "n".into(),
            direction: project::Direction::Output,
            device: "mock".into(),
            channel: "aout".into(),
            unit: None,
            min: None,
            max: None,
            description: None,
        }];

        let handle = spawn_units_inner(
            vec![single_unit(container, 5)],
            DeviceSource::Prebuilt(devices),
            mappings,
            None,
            WriteGovernance::default(),
        );
        assert!(
            !handle.watchdog_tripped(),
            "a fresh run must not report a tripped watchdog"
        );

        tokio::time::sleep(Duration::from_millis(300)).await;
        let tripped = handle.watchdog_tripped();

        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown joins");

        assert!(
            tripped,
            "the latch must be visible through the handle, or a UI cannot \
             tell a latched run from a healthy one"
        );
    }

    /// The injection primitive must trip the watchdog through the real
    /// overrun path — and must be the ONLY reason it trips. The device
    /// here never stalls; the first half of the test proves the run is
    /// healthy on its own (the falsifiability guard: remove the inject
    /// call and the second half fails), the second half proves the
    /// injected stall alone reaches the latch, with outputs held off.
    #[tokio::test]
    async fn scan_stall_injection_trips_watchdog_through_the_real_path() {
        let container = crate::compile(
            "PROGRAM main\n\
                VAR n : INT := 0; END_VAR\n\
                n := n + 1;\n\
            END_PROGRAM",
        )
        .expect("counter program compiles");

        let failsafe_engaged = Arc::new(AtomicBool::new(false));
        let writes_after_failsafe = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dev = StallingDevice {
            name: "mock".into(),
            // Zero stall: the device is innocent. Only the injection may
            // blow the budget.
            stall: Duration::ZERO,
            failsafe_engaged: failsafe_engaged.clone(),
            writes_after_failsafe: writes_after_failsafe.clone(),
        };
        let devices: Vec<Box<dyn IoDevice>> = vec![Box::new(dev)];
        let mappings = vec![project::Mapping {
            application: "main".into(),
            variable: "n".into(),
            direction: project::Direction::Output,
            device: "mock".into(),
            channel: "aout".into(),
            unit: None,
            min: None,
            max: None,
            description: None,
        }];

        let handle = spawn_units_inner(
            vec![single_unit(container, 5)],
            DeviceSource::Prebuilt(devices),
            mappings,
            None,
            WriteGovernance::default(),
        );

        // Negative control: with no injection the run must stay healthy.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !handle.watchdog_tripped(),
            "a zero-stall device must not trip the watchdog by itself — \
             if this fires, the fixture is broken and the test proves nothing"
        );

        // Inject: 12 ms stalls against a 5 ms interval, six scans — one
        // more than WATCHDOG_OVERRUN_THRESHOLD.
        handle
            .inject_scan_stall(12, WATCHDOG_OVERRUN_THRESHOLD + 1)
            .await
            .expect("inject reaches the loop");
        tokio::time::sleep(Duration::from_millis(400)).await;

        let tripped = handle.watchdog_tripped();
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown joins");

        assert!(
            tripped,
            "six injected 12ms stalls against a 5ms interval must trip the \
             watchdog through the real overrun path"
        );
        assert!(
            failsafe_engaged.load(Ordering::Relaxed),
            "the trip must reach enter_failsafe"
        );
        assert_eq!(
            writes_after_failsafe.load(Ordering::Relaxed),
            0,
            "the injected trip must latch outputs off exactly like a real one"
        );
    }

    /// R1 regression: the injection budget is counted in TICKS, not
    /// unit-scans. The watchdog counts consecutive overruns PER UNIT and
    /// one stall makes every unit due in that tick late — so budgeting
    /// per unit-scan drained `scans` N x faster than it produced late
    /// ticks, and on a two-unit project the documented default
    /// (THRESHOLD + 1) never reached the threshold. The primitive
    /// silently no-opped exactly where multi-task projects live.
    ///
    /// Falsifiability: revert the hoist (decrement inside the unit loop)
    /// and this test fails while the single-unit one above still passes —
    /// which is precisely how the defect hid.
    #[tokio::test]
    async fn scan_stall_injection_trips_a_multi_unit_project_on_the_default_budget() {
        let src = "PROGRAM main\n\
                VAR n : INT := 0; END_VAR\n\
                n := n + 1;\n\
            END_PROGRAM";
        let a = crate::compile(src).expect("counter program compiles");
        let b = crate::compile(src).expect("counter program compiles");

        let handle = spawn_units_inner(
            vec![unit("a", a, 5, 1), unit("b", b, 5, 1)],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            None,
            WriteGovernance::default(),
        );

        // Negative control: two idle units must not trip on their own,
        // or the rest of the test proves nothing.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !handle.watchdog_tripped(),
            "two idle 5ms units must not trip the watchdog by themselves"
        );

        // Exactly the documented default — what the server fills in when
        // a scenario omits `scans`.
        handle
            .inject_scan_stall(12, WATCHDOG_OVERRUN_THRESHOLD + 1)
            .await
            .expect("inject reaches the loop");
        tokio::time::sleep(Duration::from_millis(600)).await;

        let tripped = handle.watchdog_tripped();
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown joins");
        assert!(
            tripped,
            "the default budget (THRESHOLD + 1) must trip the watchdog on a \
             two-unit project too — if this fails the budget is being spent \
             per unit-scan again"
        );
    }

    /// R3 regression: an injected stall must not outlive a stop request.
    /// The stall used to be one uninterruptible sleep, so `shutdown`
    /// waited out the whole thing; with the ceiling at 1 s that is a 1 s
    /// hang on every scenario teardown, and before the ceiling existed it
    /// was unbounded.
    #[tokio::test]
    async fn shutdown_is_not_blocked_by_an_injected_stall() {
        let container = crate::compile(
            "PROGRAM main\n\
                VAR n : INT := 0; END_VAR\n\
                n := n + 1;\n\
            END_PROGRAM",
        )
        .expect("counter program compiles");

        let handle = spawn_units_inner(
            vec![single_unit(container, 5)],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            None,
            WriteGovernance::default(),
        );

        // Ask for the maximum the clamp allows, many times over.
        handle
            .inject_scan_stall(MAX_INJECT_STALL_MS, 50)
            .await
            .expect("inject reaches the loop");
        tokio::time::sleep(Duration::from_millis(60)).await;

        let t0 = Instant::now();
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown joins");
        let took = t0.elapsed();
        assert!(
            took < Duration::from_millis(MAX_INJECT_STALL_MS),
            "shutdown took {took:?} — a pending stall must be abandoned on \
             stop, not slept out"
        );
    }

    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let (dev, _healthy) = MockDevice::named("mock");
        let devices: Vec<Box<dyn IoDevice>> = vec![Box::new(dev)];
        let handle = spawn_units_inner(
            vec![single_unit(trivial_container(), 10)],
            DeviceSource::Prebuilt(devices),
            Vec::new(),
            None,
            WriteGovernance::default(),
        );

        handle.shutdown().await;
        // Second call has no thread left to join; must return immediately.
        tokio::time::timeout(Duration::from_secs(1), handle.shutdown())
            .await
            .expect("second shutdown returns immediately");
    }

    /// Two units at different intervals: the 10 ms unit must rack up
    /// notably more scans than the 50 ms unit over the same window.
    /// Per-unit scan counts are observed via each program's own
    /// increment-per-scan counter variable (distinct names → bare names
    /// in the merged snapshot).
    #[tokio::test]
    async fn two_units_scan_at_their_own_intervals() {
        let fast = crate::compile(
            "PROGRAM pfast\n\
                VAR fast_count : DINT; END_VAR\n\
                fast_count := fast_count + 1;\n\
            END_PROGRAM",
        )
        .expect("fast program compiles");
        let slow = crate::compile(
            "PROGRAM pslow\n\
                VAR slow_count : DINT; END_VAR\n\
                slow_count := slow_count + 1;\n\
            END_PROGRAM",
        )
        .expect("slow program compiles");

        let handle = spawn_units_inner(
            vec![
                unit("fast_inst", fast, 10, 1),
                unit("slow_inst", slow, 50, 2),
            ],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            None,
            WriteGovernance::default(),
        );
        let mut rx = handle.subscribe();
        let snap = last_snapshot_within(&mut rx, Duration::from_millis(250)).await;
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown joins");

        let fast_scans: i64 = var_value(&snap, "fast_count")
            .parse()
            .expect("fast_count is numeric");
        let slow_scans: i64 = var_value(&snap, "slow_count")
            .parse()
            .expect("slow_count is numeric");
        assert!(slow_scans >= 1, "slow unit must run at all: {slow_scans}");
        // Nominal ratio is 5 (10 ms vs 50 ms); CI scheduling jitter
        // eats some of it, but anything ≤ 2 means per-unit cadence is
        // broken (both units sharing one clock would give ratio 1).
        assert!(
            fast_scans > 2 * slow_scans,
            "fast unit must scan >2x more often: fast={fast_scans} slow={slow_scans}"
        );
    }

    /// Constant-value input device: every `read_channel` yields the same
    /// i32 so routing tests can tell which unit's variable was fed.
    struct ConstInputDevice {
        name: String,
        value: i32,
    }

    #[async_trait::async_trait]
    impl IoDevice for ConstInputDevice {
        fn name(&self) -> &str {
            &self.name
        }
        async fn read_channel(&mut self, _channel: &str) -> Result<ChannelValue, IoError> {
            Ok(ChannelValue::I32(self.value))
        }
        async fn write_channel(
            &mut self,
            _channel: &str,
            _value: ChannelValue,
        ) -> Result<(), IoError> {
            Ok(())
        }
    }

    /// `Mapping.application` routes a device channel to ONE unit: both
    /// units declare a variable `x`, the mapping names instance "a", so
    /// only a's x sees the bus value while b's stays at its initial 0.
    /// The colliding name is read back instance-prefixed from the
    /// merged snapshot.
    #[tokio::test]
    async fn mapping_routes_to_the_named_instance_only() {
        let prog = |name: &str| {
            crate::compile(&format!(
                "PROGRAM {name}\n\
                    VAR x : INT; mirror : INT; END_VAR\n\
                    mirror := x;\n\
                END_PROGRAM"
            ))
            .expect("program compiles")
        };

        let dev = ConstInputDevice {
            name: "mock".into(),
            value: 42,
        };
        let devices: Vec<Box<dyn IoDevice>> = vec![Box::new(dev)];
        let mappings = vec![project::Mapping {
            application: "a".into(),
            variable: "x".into(),
            direction: project::Direction::Input,
            device: "mock".into(),
            channel: "ain".into(),
            unit: None,
            min: None,
            max: None,
            description: None,
        }];

        let handle = spawn_units_inner(
            vec![unit("a", prog("pa"), 10, 1), unit("b", prog("pb"), 10, 1)],
            DeviceSource::Prebuilt(devices),
            mappings,
            None,
            WriteGovernance::default(),
        );
        let mut rx = handle.subscribe();
        let snap = last_snapshot_within(&mut rx, Duration::from_millis(150)).await;
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown joins");

        assert_eq!(
            var_value(&snap, "a.x"),
            "42",
            "instance a's x must receive the device input"
        );
        assert_eq!(
            var_value(&snap, "b.x"),
            "0",
            "instance b's x must NOT receive the device input"
        );
        // The program logic of the routed unit also saw the value.
        assert_eq!(var_value(&snap, "a.mirror"), "42");
        assert_eq!(var_value(&snap, "b.mirror"), "0");
    }

    /// A STRING var initialised to a literal must surface as the quoted
    /// IEC value in the snapshot. Exercises the data-region read path
    /// (`[max_len][cur_len][bytes…]` at the layout-table offset) end to
    /// end through the live scan loop, not just the formatter — and the
    /// per-unit `string_layouts[u]` / `runnings[u].data_region()` wiring.
    #[tokio::test]
    async fn snapshot_renders_string_var_as_quoted_iec_literal() {
        let prog = crate::compile(
            "PROGRAM string_smoke\n\
                VAR operating_mode : STRING := 'STARTUP'; END_VAR\n\
            END_PROGRAM",
        )
        .expect("string program compiles");

        let handle = spawn_units_inner(
            vec![single_unit(prog, 10)],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            None,
            WriteGovernance::default(),
        );
        let mut rx = handle.subscribe();
        let snap = last_snapshot_within(&mut rx, Duration::from_millis(250)).await;
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown joins");

        assert_eq!(var_value(&snap, "operating_mode"), "'STARTUP'");
    }

    // ---- Write governance (project.toml [governance]) ----

    fn governance(mode: WriteMode, rules: Vec<project::WriteRule>) -> WriteGovernance {
        WriteGovernance {
            write_mode: mode,
            rules,
        }
    }

    /// Allowlist mode rejects a write to any variable no rule names —
    /// the write never reaches the VM, and the caller gets an explicit
    /// GovernanceDenied, not a silent skip.
    #[tokio::test]
    async fn governance_allowlist_denies_unlisted_write() {
        let handle = spawn_units_inner(
            vec![single_unit(trivial_container(), 10)],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            None,
            governance(
                WriteMode::Allowlist,
                vec![project::WriteRule {
                    variable: "somebody_else".into(),
                    min: None,
                    max: None,
                }],
            ),
        );
        let err = handle
            .write_variable("x", 5)
            .await
            .expect_err("unlisted variable must be denied");
        // The payload names the variable AND the reason, so the HTTP
        // layers' "write to '{payload}' rejected" wrap stays specific.
        assert!(
            matches!(&err, RuntimeWriteError::GovernanceDenied(n)
                if n.starts_with("x ") && n.contains("no rule allows it")),
            "unexpected error: {err:?}"
        );
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }

    /// A listed variable writes freely inside its rule's range; outside
    /// it the value is clamped to the bound and the ack echoes the
    /// clamped value (REAL travels as IEEE-754 bits in the i32 slot).
    #[tokio::test]
    async fn governance_clamps_real_write_to_rule_range() {
        let prog = crate::compile(
            "PROGRAM main\n\
                VAR sp : REAL := 0.0; END_VAR\n\
                sp := sp + 0.0;\n\
            END_PROGRAM",
        )
        .expect("real program compiles");
        let handle = spawn_units_inner(
            vec![single_unit(prog, 10)],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            None,
            governance(
                WriteMode::Allowlist,
                vec![project::WriteRule {
                    variable: "sp".into(),
                    min: Some(0.0),
                    max: Some(100.0),
                }],
            ),
        );
        let bits = |f: f32| f.to_bits() as i32;
        // Inside the range: written as asked.
        let v = handle.write_variable("sp", bits(42.5)).await.unwrap().value;
        assert_eq!(v, bits(42.5));
        // Above max: clamped to 100, and the ack says so.
        let v = handle
            .write_variable("sp", bits(150.0))
            .await
            .unwrap()
            .value;
        assert_eq!(v, bits(100.0));
        // Below min: clamped to 0.
        let v = handle.write_variable("sp", bits(-3.0)).await.unwrap().value;
        assert_eq!(v, bits(0.0));
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }

    /// Integer variables clamp through the integer lane.
    #[tokio::test]
    async fn governance_clamps_int_write() {
        let handle = spawn_units_inner(
            vec![single_unit(trivial_container(), 10)],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            None,
            governance(
                WriteMode::Allowlist,
                vec![project::WriteRule {
                    variable: "main.x".into(), // qualified rule key also matches
                    min: Some(0.0),
                    max: Some(10.0),
                }],
            ),
        );
        let v = handle.write_variable("x", 99).await.unwrap().value;
        assert_eq!(v, 10);
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }

    /// Force is the debug override: it bypasses governance entirely
    /// (ADR-0002) — the allowlist doesn't gate it (forcing an unlisted
    /// variable succeeds) and a rule's min/max don't clamp it: forcing
    /// a value OUTSIDE the rule's range pins that value unclamped,
    /// while a plain write of the same value is clamped to the bound.
    #[tokio::test]
    async fn governance_does_not_constrain_force() {
        // `y := x` never writes x, so the forced value is observable
        // in snapshots instead of being overwritten each scan.
        let prog = crate::compile(
            "PROGRAM main\n\
                VAR x : INT; y : INT; END_VAR\n\
                y := x;\n\
            END_PROGRAM",
        )
        .expect("program compiles");
        let handle = spawn_units_inner(
            vec![single_unit(prog, 10)],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            None,
            governance(
                WriteMode::Allowlist,
                vec![project::WriteRule {
                    variable: "x".into(),
                    min: Some(0.0),
                    max: Some(10.0),
                }],
            ),
        );
        // Baseline: a plain write of 77 IS clamped to the rule's max.
        assert_eq!(handle.write_variable("x", 77).await.unwrap().value, 10);
        // Unlisted variable: write denied, force allowed.
        handle
            .write_variable("y", 5)
            .await
            .expect_err("y has no rule — plain write must be denied");
        assert_eq!(handle.force_variable("y", 5).await.unwrap(), 5);
        handle.unforce_variable("y").await.unwrap();
        // Force of the same out-of-range value: unclamped…
        let v = handle.force_variable("x", 77).await.unwrap();
        assert_eq!(v, 77, "force must bypass the rule's min/max");
        // …and actually pinned at 77 in the VM, not just echoed.
        let mut rx = handle.subscribe();
        let snap = last_snapshot_within(&mut rx, Duration::from_millis(300)).await;
        let bits = snap
            .vars
            .iter()
            .find(|v| v.name == "x")
            .expect("x in snapshot")
            .bits;
        assert_eq!(
            bits as u32 as i32, 77,
            "forced value must survive scans unclamped"
        );
        handle.unforce_variable("x").await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }

    /// A degenerate rule (min > max, or a NaN bound — TOML 1.0 accepts
    /// `nan`) that slipped past load validation must DENY the write,
    /// never panic the scan thread (`f64::clamp` panics on exactly
    /// these bounds, and a scan-thread panic kills the whole run). The
    /// loop stays ALIVE: a well-ruled write on the same run still lands.
    #[tokio::test]
    async fn governance_degenerate_rule_denies_write_and_scan_loop_survives() {
        let prog = crate::compile(
            "PROGRAM main\n\
                VAR a : INT; b : INT; c : INT; END_VAR\n\
                c := c;\n\
            END_PROGRAM",
        )
        .expect("program compiles");
        let handle = spawn_units_inner(
            vec![single_unit(prog, 10)],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            None,
            governance(
                WriteMode::Allowlist,
                vec![
                    project::WriteRule {
                        variable: "a".into(),
                        min: Some(5.0), // swapped-bounds typo
                        max: Some(1.0),
                    },
                    project::WriteRule {
                        variable: "b".into(),
                        min: Some(f64::NAN),
                        max: Some(10.0),
                    },
                    project::WriteRule {
                        variable: "c".into(),
                        min: None,
                        max: None,
                    },
                ],
            ),
        );
        let err = handle
            .write_variable("a", 3)
            .await
            .expect_err("swapped bounds must deny, not clamp or panic");
        assert!(
            matches!(&err, RuntimeWriteError::GovernanceDenied(n) if n.contains("valid range")),
            "unexpected error: {err:?}"
        );
        let err = handle
            .write_variable("b", 3)
            .await
            .expect_err("NaN bound must deny");
        assert!(
            matches!(&err, RuntimeWriteError::GovernanceDenied(_)),
            "unexpected error: {err:?}"
        );
        // Not Disconnected above, and this ack proves the drain still
        // runs: the denials left the scan loop alive.
        assert_eq!(handle.write_variable("c", 9).await.unwrap().value, 9);
        assert!(handle.fault().is_none(), "no fault may be recorded");
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }

    /// Writing NaN to a min/max-ruled REAL is DENIED: NaN is
    /// unorderable, so it cannot be clamped, and passing it through
    /// would void the envelope while the log claimed "clamped".
    #[tokio::test]
    async fn governance_denies_nan_write_to_ruled_real() {
        let prog = crate::compile(
            "PROGRAM main\n\
                VAR sp : REAL := 0.0; END_VAR\n\
                sp := sp + 0.0;\n\
            END_PROGRAM",
        )
        .expect("real program compiles");
        let handle = spawn_units_inner(
            vec![single_unit(prog, 10)],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            None,
            governance(
                WriteMode::Allowlist,
                vec![project::WriteRule {
                    variable: "sp".into(),
                    min: Some(0.0),
                    max: Some(100.0),
                }],
            ),
        );
        let err = handle
            .write_variable("sp", f32::NAN.to_bits() as i32)
            .await
            .expect_err("NaN write must be denied, not 'clamped'");
        assert!(
            matches!(&err, RuntimeWriteError::GovernanceDenied(n) if n.contains("NaN")),
            "unexpected error: {err:?}"
        );
        // Envelope intact and loop alive: a finite write still works
        // and the VM value is that write, not NaN.
        let bits = |f: f32| f.to_bits() as i32;
        assert_eq!(
            handle.write_variable("sp", bits(42.5)).await.unwrap().value,
            bits(42.5)
        );
        let mut rx = handle.subscribe();
        let snap = last_snapshot_within(&mut rx, Duration::from_millis(300)).await;
        let sp = snap
            .vars
            .iter()
            .find(|v| v.name == "sp")
            .expect("sp in snapshot");
        assert_eq!(f32::from_bits(sp.bits as u32), 42.5);
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }

    /// Fractional bounds clamp in the INTEGER domain, so the applied
    /// value is always within [min, max] — `.round()` on the clamped
    /// f64 would otherwise step outside (10.6 → 11, 0.4 → 0).
    #[tokio::test]
    async fn governance_integer_clamp_never_exceeds_fractional_bounds() {
        let handle = spawn_units_inner(
            vec![single_unit(trivial_container(), 10)],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            None,
            governance(
                WriteMode::Allowlist,
                vec![project::WriteRule {
                    variable: "x".into(),
                    min: Some(0.4),
                    max: Some(10.6),
                }],
            ),
        );
        // Above: floor(10.6) = 10, never 11.
        assert_eq!(handle.write_variable("x", 99).await.unwrap().value, 10);
        // Below: ceil(0.4) = 1, never 0.
        assert_eq!(handle.write_variable("x", -5).await.unwrap().value, 1);
        // Inside: untouched.
        assert_eq!(handle.write_variable("x", 5).await.unwrap().value, 5);
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }

    /// A rule range containing no integer (min 10.2 / max 10.8 on an
    /// INT lane) cannot produce an in-range value — the write is
    /// denied, not rounded to a value outside the declared bounds. The
    /// scan loop survives the denial.
    #[tokio::test]
    async fn governance_denies_int_write_when_rule_range_has_no_integer() {
        let prog = crate::compile(
            "PROGRAM main\n\
                VAR x : INT; y : INT; END_VAR\n\
                y := y;\n\
            END_PROGRAM",
        )
        .expect("program compiles");
        let handle = spawn_units_inner(
            vec![single_unit(prog, 10)],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            None,
            governance(
                WriteMode::Allowlist,
                vec![
                    project::WriteRule {
                        variable: "x".into(),
                        min: Some(10.2),
                        max: Some(10.8),
                    },
                    project::WriteRule {
                        variable: "y".into(),
                        min: None,
                        max: None,
                    },
                ],
            ),
        );
        let err = handle
            .write_variable("x", 5)
            .await
            .expect_err("empty integer range must deny");
        assert!(
            matches!(&err, RuntimeWriteError::GovernanceDenied(n) if n.contains("integer")),
            "unexpected error: {err:?}"
        );
        // Loop alive after the denial.
        assert_eq!(handle.write_variable("y", 7).await.unwrap().value, 7);
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }

    /// f64 bounds convert to f32 INWARD: the f32 nearest to 0.1 is
    /// strictly ABOVE 0.1, so a max = 0.1 clamp target steps one ULP
    /// down (and a min = 0.1 target stays above) — the written f32
    /// never escapes the declared f64 range.
    #[tokio::test]
    async fn governance_real_clamp_stays_inside_f64_bounds() {
        let prog = crate::compile(
            "PROGRAM main\n\
                VAR hi_sp : REAL := 0.0; lo_sp : REAL := 0.0; END_VAR\n\
                hi_sp := hi_sp + 0.0;\n\
            END_PROGRAM",
        )
        .expect("real program compiles");
        let handle = spawn_units_inner(
            vec![single_unit(prog, 10)],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            None,
            governance(
                WriteMode::Allowlist,
                vec![
                    project::WriteRule {
                        variable: "hi_sp".into(),
                        min: None,
                        max: Some(0.1),
                    },
                    project::WriteRule {
                        variable: "lo_sp".into(),
                        min: Some(0.1),
                        max: None,
                    },
                ],
            ),
        );
        let bits = |f: f32| f.to_bits() as i32;
        let v = handle
            .write_variable("hi_sp", bits(0.5))
            .await
            .unwrap()
            .value;
        let f = f32::from_bits(v as u32);
        assert!(
            (f as f64) <= 0.1,
            "applied value {f} escapes the declared max 0.1"
        );
        assert!((f as f64) > 0.09, "clamp target should stay near the bound");
        let v = handle
            .write_variable("lo_sp", bits(0.0))
            .await
            .unwrap()
            .value;
        let f = f32::from_bits(v as u32);
        assert!(
            (f as f64) >= 0.1,
            "applied value {f} escapes the declared min 0.1"
        );
        assert!((f as f64) < 0.11, "clamp target should stay near the bound");
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }

    /// A finite f64 bound past f32 range (e.g. `min = 1.0e39`) admits no
    /// representable REAL — the conversion saturates to +inf on the far
    /// side, where inward stepping can't reach. The write must be denied
    /// like the integer lane's empty range, never "clamped" to literal
    /// infinity; and a NEAR-side out-of-range bound (min = -1.0e39, which
    /// every f32 satisfies) must keep behaving as no bound at all.
    #[tokio::test]
    async fn governance_denies_real_rule_beyond_f32_range() {
        let prog = crate::compile(
            "PROGRAM main\n\
                VAR far_sp : REAL := 0.0; near_sp : REAL := 0.0; END_VAR\n\
                far_sp := far_sp + 0.0;\n\
            END_PROGRAM",
        )
        .expect("real program compiles");
        let handle = spawn_units_inner(
            vec![single_unit(prog, 10)],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            None,
            governance(
                WriteMode::Allowlist,
                vec![
                    project::WriteRule {
                        variable: "far_sp".into(),
                        min: Some(1.0e39),
                        max: None,
                    },
                    project::WriteRule {
                        variable: "near_sp".into(),
                        min: Some(-1.0e39),
                        max: None,
                    },
                ],
            ),
        );
        let bits = |f: f32| f.to_bits() as i32;
        let err = handle
            .write_variable("far_sp", bits(5.0))
            .await
            .expect_err("a rule no REAL can satisfy must deny, not clamp to inf");
        assert!(
            err.to_string().contains("no representable REAL value"),
            "unexpected denial reason: {err}"
        );
        let v = handle
            .write_variable("near_sp", bits(-5.0))
            .await
            .unwrap()
            .value;
        assert_eq!(
            f32::from_bits(v as u32),
            -5.0,
            "a lower bound below -f32::MAX admits every REAL — write passes unclamped"
        );
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }

    /// LREAL rides the integer lane, like every writer that can reach
    /// governance: HTTP bodies and the northbound numeric lane send a
    /// plain number, and nothing encodes LREAL as f32 bits. Governance
    /// must clamp the value the writer actually sent — not a
    /// reinterpretation of its bits.
    #[tokio::test]
    async fn governance_clamps_lreal_write_in_the_writers_encoding() {
        let prog = crate::compile(
            "PROGRAM main\n\
                VAR sp : LREAL := 0.0; END_VAR\n\
                sp := sp + 0.0;\n\
            END_PROGRAM",
        )
        .expect("lreal program compiles");
        let handle = spawn_units_inner(
            vec![single_unit(prog, 10)],
            DeviceSource::Prebuilt(Vec::new()),
            Vec::new(),
            None,
            governance(
                WriteMode::Allowlist,
                vec![project::WriteRule {
                    variable: "sp".into(),
                    min: Some(0.0),
                    max: Some(100.0),
                }],
            ),
        );
        // In range: passes through numerically unchanged (the f32-bits
        // decode would have read 50 as 7e-44 and "clamped" it).
        assert_eq!(handle.write_variable("sp", 50).await.unwrap().value, 50);
        // Out of range: clamped in the numeric domain the writer used.
        assert_eq!(handle.write_variable("sp", 150).await.unwrap().value, 100);
        assert_eq!(handle.write_variable("sp", -5).await.unwrap().value, 0);
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }

    /// The pulse reset is the runtime's own safety action, not an
    /// external write: it bypasses governance, so a rule with min > 0
    /// still releases the momentary command to 0 instead of silently
    /// latching it at the clamp floor forever.
    #[tokio::test]
    async fn pulse_reset_bypasses_governance_and_still_resets_to_zero() {
        // `y := x` never writes x, so the reset's 0 persists.
        let prog = crate::compile(
            "PROGRAM main\n\
                VAR x : INT; y : INT; END_VAR\n\
                y := x;\n\
            END_PROGRAM",
        )
        .expect("program compiles");
        let handle = spawn_units_inner(
            vec![single_unit(prog, 10)],
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
        // The operator's write itself is governed and in range.
        let v = crate::monitor::write_with_pulse(&handle, "x", 50, Some(30))
            .await
            .expect("initial pulse write");
        assert_eq!(v.value, 50);
        // A virtual-master fixture has no Output mapping for `x`, so there is
        // nothing to deliver and nothing to warn about.
        assert_eq!(v.undelivered_device, None);
        // Give the pulse task and a few scans time to land the reset.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mut rx = handle.subscribe();
        let snap = last_snapshot_within(&mut rx, Duration::from_millis(300)).await;
        let bits = snap
            .vars
            .iter()
            .find(|v| v.name == "x")
            .expect("x in snapshot")
            .bits;
        assert_eq!(
            bits as u32 as i32, 0,
            "pulse reset must land 0 — a min = 1 rule must not latch the command"
        );
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown");
    }
}
