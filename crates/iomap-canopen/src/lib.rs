//! CANopen (CiA 301) IoDevice adapter — southbound link to one node on
//! a CAN bus: a servo drive, a remote I/O block, a sensor gateway.
//!
//! Transport modes per channel:
//!   - `sdo` — request/response object access, polled into the tag
//!     mirror every `poll_interval_ms` and written on demand. The
//!     configuration-rate lane.
//!   - `tpdo`/`rpdo` — process data on the CiA 301 predefined COB-IDs
//!     using the node's existing PDO mapping. TPDO frames land in the
//!     mirror as they arrive; RPDO writes update a shadow buffer and
//!     go on the wire immediately. The process-rate lane.
//!
//! One background io task owns the bus exclusively: it correlates SDO
//! request/response pairs (one in flight, CiA 301 default), folds
//! heartbeats into the health flag, and runs the SDO poll schedule.
//! `interface = "_sim"` runs the whole stack against an in-memory bus
//! with a simulated slave (see `bus::SimBus`) — same convention as
//! EtherCAT's `nic = "_sim"`.
//!
//! Health: heartbeat is the authoritative signal when the node produces
//! one (`heartbeat_timeout_ms > 0`); otherwise consecutive SDO failures
//! flip the flag, same contract as the Modbus/OPC UA adapters.
//! A heartbeat is not a channel sample: reads fail until that channel has
//! received data, and link loss invalidates the mirror until new samples
//! arrive. The scan loop retains its last value and marks the input stale.
//!
//! Failsafe: opt-in per channel, like OPC UA — only `write` channels
//! with an explicit `failsafe` value get written on shutdown/trip. The
//! adapter never sends NMT Stop at shutdown: other masters/tools may
//! share the bus, and a drive's safe state is an object write (e.g.
//! controlword shutdown), not a bus-wide state change.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use iocore::{ChannelValue, HealthTracker, HealthTransition, IoDevice, IoError};
use project::{CanopenAccess, CanopenChannel, CanopenConfig, CanopenTransport};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

pub mod bus;
pub mod frame;
#[cfg(target_os = "linux")]
mod socketcan_bus;

use bus::CanBus;
use frame::{CanFrame, SdoResponse};

/// Sentinel interface name selecting the in-memory sim bus.
pub const SIM_INTERFACE: &str = "_sim";

/// How long one SDO request may wait for its response before it counts
/// as failed. CiA 301 suggests client timeouts in the tens of ms on a
/// healthy bus; 200 ms tolerates a busy segment without wedging the
/// poll schedule.
const SDO_TIMEOUT: Duration = Duration::from_millis(200);

/// Consecutive SDO failures before unhealthy, when heartbeat monitoring
/// is disabled. Same threshold as the Modbus / OPC UA adapters.
const UNHEALTHY_AFTER_FAILURES: u32 = 3;

fn is_sim(interface: &str) -> bool {
    interface == SIM_INTERFACE || interface.is_empty()
}

pub struct CanopenDevice {
    name: String,
    channels: HashMap<String, CanopenChannel>,
    mirror: Arc<RwLock<HashMap<String, ChannelValue>>>,
    healthy: Arc<AtomicBool>,
    cmd_tx: mpsc::Sender<Cmd>,
    io_task: Option<tokio::task::JoinHandle<()>>,
}

enum Cmd {
    /// Write bytes to an object over SDO; resolves when the node acks.
    SdoWrite {
        index: u16,
        sub: u8,
        data: Vec<u8>,
        resp: oneshot::Sender<Result<(), IoError>>,
    },
    /// Update the RPDO shadow and put the frame on the wire.
    RpdoWrite {
        slot: u8,
        byte_offset: u8,
        data: Vec<u8>,
        resp: oneshot::Sender<Result<(), IoError>>,
    },
}

impl CanopenDevice {
    pub async fn connect(name: String, config: &CanopenConfig) -> Result<Self, IoError> {
        if config.node_id == 0 || config.node_id > 127 {
            return Err(IoError::Connect(format!(
                "canopen node_id {} out of range 1-127",
                config.node_id
            )));
        }
        // The channel table below is keyed by name, so a name declared twice
        // does not collide — it overwrites, and the earlier channel ceases to
        // exist while every mapping onto that name silently moves to the
        // survivor. Refuse: a config that cannot be read unambiguously is not
        // one to run a plant on. `/api/project/validate` reports the same
        // thing at author time, but the edge runtime validates nothing — this
        // is the only check that always runs.
        let dups =
            project::duplicate_channel_names(config.channels.iter().map(|c| c.name.as_str()));
        if !dups.is_empty() {
            return Err(IoError::Connect(format!(
                "device '{name}': channel name declared more than once: {}",
                dups.join(", ")
            )));
        }
        let mut channels = HashMap::new();
        for ch in &config.channels {
            if let CanopenTransport::Tpdo { slot, .. } | CanopenTransport::Rpdo { slot, .. } =
                ch.transport
            {
                if !(1..=4).contains(&slot) {
                    return Err(IoError::Connect(format!(
                        "channel '{}': PDO slot {} out of range 1-4",
                        ch.name, slot
                    )));
                }
            }
            channels.insert(ch.name.clone(), ch.clone());
        }

        let bus: Box<dyn CanBus> = if is_sim(&config.interface) {
            tracing::info!(device = %name, "canopen in simulation mode (interface=\"_sim\") — no real bus traffic");
            Box::new(bus::SimBus::connect(config))
        } else {
            #[cfg(target_os = "linux")]
            {
                tracing::info!(device = %name, interface = %config.interface, "canopen opening SocketCAN interface");
                Box::new(socketcan_bus::SocketcanBus::open(&config.interface)?)
            }
            #[cfg(not(target_os = "linux"))]
            {
                return Err(IoError::Connect(format!(
                    "canopen interface '{}': SocketCAN requires a Linux edge; \
                     use \"_sim\" on this machine",
                    config.interface
                )));
            }
        };

        let mirror = Arc::new(RwLock::new(HashMap::new()));
        let healthy = Arc::new(AtomicBool::new(true));
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let io = IoTask::new(
            name.clone(),
            config,
            bus,
            mirror.clone(),
            healthy.clone(),
            cmd_rx,
        );
        let io_task = tokio::spawn(io.run());

        tracing::info!(
            device = %name,
            node = config.node_id,
            channels = channels.len(),
            poll_ms = config.poll_interval_ms,
            "canopen adapter started"
        );

        Ok(Self {
            name,
            channels,
            mirror,
            healthy,
            cmd_tx,
            io_task: Some(io_task),
        })
    }

    fn channel(&self, name: &str) -> Result<&CanopenChannel, IoError> {
        self.channels
            .get(name)
            .ok_or_else(|| IoError::UnknownChannel(name.to_string()))
    }

    async fn write_raw(&self, ch: &CanopenChannel, value: ChannelValue) -> Result<(), IoError> {
        let (bytes, len) = frame::value_to_bytes(value, ch.data_type);
        let (tx, rx) = oneshot::channel();
        let cmd = match ch.transport {
            CanopenTransport::Sdo => Cmd::SdoWrite {
                index: ch.index,
                sub: ch.sub_index,
                data: bytes[..len].to_vec(),
                resp: tx,
            },
            CanopenTransport::Rpdo { slot, byte_offset } => Cmd::RpdoWrite {
                slot,
                byte_offset,
                data: bytes[..len].to_vec(),
                resp: tx,
            },
            CanopenTransport::Tpdo { .. } => {
                return Err(IoError::TypeMismatch {
                    channel: ch.name.clone(),
                    value,
                })
            }
        };
        self.cmd_tx
            .send(cmd)
            .await
            .map_err(|_| IoError::Transport("canopen io task gone".into()))?;
        rx.await
            .map_err(|_| IoError::Transport("canopen io task dropped the request".into()))??;
        // Reads reflect the commanded value immediately (RPDO objects
        // aren't echoed back by the node; SDO ones would only refresh
        // on the next poll).
        self.mirror
            .write()
            .expect("mirror poisoned")
            .insert(ch.name.clone(), value);
        Ok(())
    }
}

#[async_trait]
impl IoDevice for CanopenDevice {
    fn name(&self) -> &str {
        &self.name
    }

    async fn read_channel(&mut self, channel: &str) -> Result<ChannelValue, IoError> {
        self.channel(channel)?;
        self.mirror
            .read()
            .expect("mirror poisoned")
            .get(channel)
            .copied()
            .ok_or_else(|| {
                IoError::Transport(format!(
                    "canopen channel '{channel}' has no sample since connect or link loss"
                ))
            })
    }

    async fn write_channel(&mut self, channel: &str, value: ChannelValue) -> Result<(), IoError> {
        let ch = self.channel(channel)?.clone();
        if ch.access != CanopenAccess::Write {
            return Err(IoError::TypeMismatch {
                channel: channel.into(),
                value,
            });
        }
        self.write_raw(&ch, value).await
    }

    fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Only channels with an explicit `failsafe` are written — the node
    /// keeps authority over everything else (see module docs).
    async fn enter_failsafe(&mut self) -> Result<(), IoError> {
        let targets: Vec<CanopenChannel> = self
            .channels
            .values()
            .filter(|c| c.access == CanopenAccess::Write && c.failsafe.is_some())
            .cloned()
            .collect();
        let mut first_err = None;
        for ch in targets {
            let fs = ch.failsafe.expect("filtered Some");
            let value = match ch.data_type {
                project::CanopenDataType::F32 => ChannelValue::Real(fs as f32),
                _ => ChannelValue::I32(fs as i32),
            };
            if let Err(e) = self.write_raw(&ch, value).await {
                tracing::warn!(channel = %ch.name, %e, "canopen failsafe write failed");
                first_err.get_or_insert(e);
            } else {
                tracing::info!(channel = %ch.name, value = fs, "canopen failsafe applied");
            }
        }
        match first_err {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    async fn shutdown(&mut self) -> Result<(), IoError> {
        if let Some(t) = self.io_task.take() {
            t.abort();
        }
        tracing::info!(device = %self.name, "canopen adapter stopped");
        Ok(())
    }
}

// ---------------- io task ----------------

struct PendingSdo {
    index: u16,
    sub: u8,
    kind: PendingKind,
    deadline: Instant,
}

enum PendingKind {
    /// Poll read — the answer lands in the mirror under this channel.
    Read {
        channel: String,
        ty: project::CanopenDataType,
    },
    /// Commanded write — carries its wire bytes until sent, and the
    /// answer resolves this caller.
    Write {
        data: Vec<u8>,
        resp: oneshot::Sender<Result<(), IoError>>,
    },
}

struct IoTask {
    device: String,
    node: u8,
    bus: Box<dyn CanBus>,
    mirror: Arc<RwLock<HashMap<String, ChannelValue>>>,
    healthy: Arc<AtomicBool>,
    cmd_rx: mpsc::Receiver<Cmd>,

    /// SDO-transport channels on the poll schedule (read mirror).
    sdo_polled: Vec<CanopenChannel>,
    /// TPDO COB-ID → entries unpacked into the mirror on arrival.
    tpdo_map: HashMap<u16, Vec<(u8, project::CanopenDataType, String)>>,
    /// RPDO slot → shadow payload (so one channel's write doesn't zero
    /// its neighbours in the same frame).
    rpdo_shadow: HashMap<u8, ([u8; 8], usize)>,

    /// One SDO in flight (CiA 301 default server behaviour) + queue.
    in_flight: Option<PendingSdo>,
    queue: VecDeque<PendingSdo>,

    start_on_connect: bool,
    poll_interval: Duration,
    heartbeat_timeout: Option<Duration>,
    last_heartbeat: Instant,
    hb_ok: bool,
    sdo_health: HealthTracker,
}

impl IoTask {
    fn new(
        device: String,
        config: &CanopenConfig,
        bus: Box<dyn CanBus>,
        mirror: Arc<RwLock<HashMap<String, ChannelValue>>>,
        healthy: Arc<AtomicBool>,
        cmd_rx: mpsc::Receiver<Cmd>,
    ) -> Self {
        let mut sdo_polled = Vec::new();
        let mut tpdo_map: HashMap<u16, Vec<(u8, project::CanopenDataType, String)>> =
            HashMap::new();
        let mut rpdo_shadow: HashMap<u8, ([u8; 8], usize)> = HashMap::new();
        for ch in &config.channels {
            match ch.transport {
                CanopenTransport::Sdo => sdo_polled.push(ch.clone()),
                CanopenTransport::Tpdo { slot, byte_offset } => {
                    tpdo_map
                        .entry(frame::cob::tpdo(slot, config.node_id))
                        .or_default()
                        .push((byte_offset, ch.data_type, ch.name.clone()));
                }
                CanopenTransport::Rpdo { slot, byte_offset } => {
                    let entry = rpdo_shadow.entry(slot).or_insert(([0u8; 8], 0));
                    entry.1 = entry
                        .1
                        .max(byte_offset as usize + frame::type_len(ch.data_type));
                }
            }
        }
        Self {
            device,
            node: config.node_id,
            bus,
            mirror,
            healthy,
            cmd_rx,
            sdo_polled,
            tpdo_map,
            rpdo_shadow,
            in_flight: None,
            queue: VecDeque::new(),
            start_on_connect: config.start_on_connect,
            poll_interval: Duration::from_millis(config.poll_interval_ms.max(10) as u64),
            heartbeat_timeout: (config.heartbeat_timeout_ms > 0)
                .then(|| Duration::from_millis(config.heartbeat_timeout_ms as u64)),
            last_heartbeat: Instant::now(),
            hb_ok: true,
            sdo_health: HealthTracker::new(UNHEALTHY_AFTER_FAILURES),
        }
    }

    async fn run(mut self) {
        if self.start_on_connect {
            let f = frame::nmt_frame(frame::NmtCommand::Start, self.node);
            if let Err(e) = self.bus.send(f).await {
                tracing::warn!(device = %self.device, %e, "canopen NMT start failed");
            }
        }
        let mut poll = tokio::time::interval(self.poll_interval);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // House tick: SDO timeout + heartbeat watchdog, decoupled from
        // the poll rate.
        let mut house = tokio::time::interval(Duration::from_millis(50));
        house.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                res = self.bus.recv() => match res {
                    Ok(f) => self.on_frame(f).await,
                    Err(e) => {
                        tracing::error!(device = %self.device, %e, "canopen bus receive failed; io task exiting");
                        self.set_healthy(false);
                        return;
                    }
                },
                maybe = self.cmd_rx.recv() => match maybe {
                    Some(cmd) => self.on_cmd(cmd).await,
                    None => return, // device dropped
                },
                _ = poll.tick() => {
                    self.schedule_poll();
                    self.pump_queue().await;
                }
                _ = house.tick() => self.housekeeping().await,
            }
        }
    }

    fn set_healthy(&self, up: bool) {
        // Heartbeats can recover before any PDO/SDO data. Remove samples
        // from the old connection so transport recovery alone cannot make
        // those values look fresh; subsequent real samples refill the map.
        if !up {
            let mut mirror = self.mirror.write().expect("mirror poisoned");
            if self.healthy.swap(false, Ordering::Relaxed) {
                mirror.clear();
            }
        } else {
            self.healthy.store(true, Ordering::Relaxed);
        }
    }
    fn recompute_health(&self) {
        let hb = self.heartbeat_timeout.is_none() || self.hb_ok;
        self.set_healthy(hb && self.sdo_health.is_healthy());
    }

    async fn on_frame(&mut self, f: CanFrame) {
        // Heartbeat → watchdog + health.
        if f.id == frame::cob::heartbeat(self.node) {
            if frame::parse_heartbeat(&f).is_some() {
                self.last_heartbeat = Instant::now();
                if !self.hb_ok {
                    self.hb_ok = true;
                    tracing::info!(device = %self.device, "canopen heartbeat recovered");
                    self.recompute_health();
                }
            }
            return;
        }
        // TPDO → mirror.
        if let Some(entries) = self.tpdo_map.get(&f.id) {
            let mut m = self.mirror.write().expect("mirror poisoned");
            for (offset, ty, name) in entries {
                let start = *offset as usize;
                let end = start + frame::type_len(*ty);
                if end <= f.len as usize {
                    if let Some(v) = frame::bytes_to_value(&f.data[start..end], *ty) {
                        m.insert(name.clone(), v);
                    }
                }
            }
            return;
        }
        // SDO response → resolve in-flight.
        if f.id == frame::cob::sdo_response(self.node) {
            if let Some(resp) = frame::parse_sdo_response(&f) {
                self.on_sdo_response(resp).await;
            }
        }
        // EMCY and other traffic: logged at trace level only — a shared
        // bus carries plenty of frames that aren't ours.
    }

    async fn on_sdo_response(&mut self, resp: SdoResponse) {
        let Some(pending) = self.in_flight.take() else {
            return; // stray/duplicate response
        };
        // Responses carry index/sub — mismatches mean a stray answer
        // (e.g. another master's exchange); keep waiting for ours.
        let (ri, rs) = match &resp {
            SdoResponse::UploadOk { index, sub, .. }
            | SdoResponse::DownloadOk { index, sub }
            | SdoResponse::Abort { index, sub, .. }
            | SdoResponse::Segmented { index, sub } => (*index, *sub),
        };
        if (ri, rs) != (pending.index, pending.sub) {
            self.in_flight = Some(pending);
            return;
        }
        match (resp, pending.kind) {
            (SdoResponse::UploadOk { data, len, .. }, PendingKind::Read { channel, ty }) => {
                if let Some(v) = frame::bytes_to_value(&data[..len], ty) {
                    self.mirror
                        .write()
                        .expect("mirror poisoned")
                        .insert(channel, v);
                }
                self.record_sdo(true);
            }
            (SdoResponse::DownloadOk { .. }, PendingKind::Write { resp, .. }) => {
                let _ = resp.send(Ok(()));
                self.record_sdo(true);
            }
            (SdoResponse::Abort { code, index, sub }, kind) => {
                let msg = format!(
                    "SDO abort on {:#06x}:{sub}: {:#010x} ({})",
                    index,
                    code,
                    frame::abort_text(code)
                );
                match kind {
                    PendingKind::Write { resp, .. } => {
                        let _ = resp.send(Err(IoError::Transport(msg)));
                    }
                    PendingKind::Read { channel, .. } => {
                        tracing::warn!(device = %self.device, %channel, "{msg}; keeping last value");
                    }
                }
                // An abort is the node *answering* — the link works.
                self.record_sdo(true);
            }
            (SdoResponse::Segmented { index, sub }, kind) => {
                let msg = format!(
                    "object {:#06x}:{sub} needs a segmented SDO transfer (>4 bytes) — \
                     not supported; bind a ≤4-byte object instead",
                    index
                );
                match kind {
                    PendingKind::Write { resp, .. } => {
                        let _ = resp.send(Err(IoError::Transport(msg)));
                    }
                    PendingKind::Read { channel, .. } => {
                        tracing::warn!(device = %self.device, %channel, "{msg}");
                    }
                }
                self.record_sdo(true);
            }
            // Shape mismatch (upload answer to a write, etc.) — drop it.
            (_, PendingKind::Write { resp, .. }) => {
                let _ = resp.send(Err(IoError::Transport(
                    "canopen: mismatched SDO response".into(),
                )));
            }
            (_, PendingKind::Read { .. }) => {}
        }
        self.pump_queue().await;
    }

    async fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::SdoWrite {
                index,
                sub,
                data,
                resp,
            } => {
                // Writes jump the poll queue — a command should not sit
                // behind a full mirror refresh.
                self.queue.push_front(PendingSdo {
                    index,
                    sub,
                    kind: PendingKind::Write { data, resp },
                    deadline: Instant::now() + SDO_TIMEOUT,
                });
                self.pump_queue().await;
            }
            Cmd::RpdoWrite {
                slot,
                byte_offset,
                data,
                resp,
            } => {
                let (shadow, used) = self
                    .rpdo_shadow
                    .entry(slot)
                    .or_insert(([0u8; 8], (byte_offset as usize + data.len()).min(8)));
                let start = byte_offset as usize;
                let end = (start + data.len()).min(8);
                shadow[start..end].copy_from_slice(&data[..end - start]);
                let payload_len = (*used).max(end).min(8);
                let f = CanFrame::new(frame::cob::rpdo(slot, self.node), &shadow[..payload_len]);
                let _ = resp.send(self.bus.send(f).await);
            }
        }
    }

    fn schedule_poll(&mut self) {
        // Skip if the previous round hasn't drained — a slow node
        // shouldn't pile up an ever-growing queue.
        let already_queued = self
            .queue
            .iter()
            .any(|p| matches!(p.kind, PendingKind::Read { .. }));
        if already_queued {
            return;
        }
        for ch in &self.sdo_polled {
            self.queue.push_back(PendingSdo {
                index: ch.index,
                sub: ch.sub_index,
                kind: PendingKind::Read {
                    channel: ch.name.clone(),
                    ty: ch.data_type,
                },
                deadline: Instant::now() + SDO_TIMEOUT,
            });
        }
    }

    async fn housekeeping(&mut self) {
        // SDO timeout.
        if let Some(p) = &self.in_flight {
            if Instant::now() >= p.deadline {
                let p = self.in_flight.take().expect("checked Some");
                match p.kind {
                    PendingKind::Write { resp, .. } => {
                        let _ = resp.send(Err(IoError::Transport(format!(
                            "SDO write {:#06x}:{} timed out",
                            p.index, p.sub
                        ))));
                    }
                    PendingKind::Read { channel, .. } => {
                        tracing::debug!(device = %self.device, %channel, "SDO poll timed out; keeping last value");
                    }
                }
                self.record_sdo(false);
                self.pump_queue().await;
            }
        }
        // Heartbeat watchdog.
        if let Some(timeout) = self.heartbeat_timeout {
            let expired = self.last_heartbeat.elapsed() > timeout;
            if expired && self.hb_ok {
                self.hb_ok = false;
                tracing::error!(
                    device = %self.device,
                    timeout_ms = timeout.as_millis() as u64,
                    "canopen heartbeat lost; serving last-known values until it recovers"
                );
                self.recompute_health();
            }
        }
    }

    fn record_sdo(&mut self, ok: bool) {
        let transition = if ok {
            self.sdo_health.record_success()
        } else {
            self.sdo_health.record_failure()
        };
        match transition {
            HealthTransition::BecameUnhealthy => {
                tracing::error!(
                    device = %self.device,
                    consecutive_failures = self.sdo_health.consecutive_failures(),
                    "canopen SDO exchanges failing; node unresponsive"
                );
                self.recompute_health();
            }
            HealthTransition::Recovered => {
                tracing::info!(device = %self.device, "canopen SDO exchanges recovered");
                self.recompute_health();
            }
            HealthTransition::Unchanged => {}
        }
    }

    /// Fire the next queued SDO if the line is idle.
    async fn pump_queue(&mut self) {
        if self.in_flight.is_some() {
            return;
        }
        let Some(mut next) = self.queue.pop_front() else {
            return;
        };
        next.deadline = Instant::now() + SDO_TIMEOUT;
        let f = match &next.kind {
            PendingKind::Read { .. } => frame::sdo_upload_request(self.node, next.index, next.sub),
            PendingKind::Write { data, .. } => {
                frame::sdo_download_request(self.node, next.index, next.sub, data)
            }
        };
        match self.bus.send(f).await {
            Ok(()) => self.in_flight = Some(next),
            Err(e) => {
                match next.kind {
                    PendingKind::Write { resp, .. } => {
                        let _ = resp.send(Err(e));
                    }
                    PendingKind::Read { channel, .. } => {
                        tracing::warn!(device = %self.device, %channel, %e, "canopen send failed");
                    }
                }
                self.record_sdo(false);
            }
        }
    }
}

#[cfg(test)]
mod quality_tests {
    use super::*;
    use project::CanopenDataType;

    #[tokio::test]
    async fn heartbeat_recovery_needs_new_samples_for_each_channel() {
        let mut config = CanopenConfig {
            interface: "_sim".into(),
            node_id: 34,
            bitrate: None,
            poll_interval_ms: 40,
            heartbeat_timeout_ms: 400,
            start_on_connect: false,
            channels: vec![],
        };
        for (name, transport) in [
            (
                "pdo",
                CanopenTransport::Tpdo {
                    slot: 1,
                    byte_offset: 0,
                },
            ),
            ("sdo", CanopenTransport::Sdo),
        ] {
            config.channels.push(CanopenChannel {
                name: name.into(),
                index: 0x2000,
                sub_index: 0,
                data_type: CanopenDataType::U16,
                access: CanopenAccess::Read,
                transport,
                failsafe: None,
            });
        }
        let mut dev = CanopenDevice::connect("node".into(), &config)
            .await
            .unwrap();
        // Drive the I/O state machine deterministically, without timer races.
        dev.shutdown().await.unwrap();
        let (_cmd_tx, cmd_rx) = mpsc::channel(1);
        let mut io = IoTask::new(
            "node".into(),
            &config,
            Box::new(bus::SimBus::connect(&config)),
            dev.mirror.clone(),
            dev.healthy.clone(),
            cmd_rx,
        );
        dev.mirror.write().unwrap().extend([
            ("pdo".into(), ChannelValue::U16(10)),
            ("sdo".into(), ChannelValue::U16(20)),
        ]);
        assert_eq!(dev.read_channel("pdo").await.unwrap().to_i32(), 10);
        io.hb_ok = false;
        io.recompute_health();
        assert!(!dev.is_healthy());
        assert!(dev.read_channel("pdo").await.is_err());
        assert!(dev.read_channel("sdo").await.is_err());

        io.on_frame(CanFrame::new(frame::cob::heartbeat(config.node_id), &[5]))
            .await;
        assert!(dev.is_healthy());
        assert!(
            dev.read_channel("pdo").await.is_err(),
            "heartbeat is not a new PDO"
        );
        assert!(
            dev.read_channel("sdo").await.is_err(),
            "heartbeat is not a new SDO"
        );

        io.on_frame(CanFrame::new(
            frame::cob::tpdo(1, config.node_id),
            &42u16.to_le_bytes(),
        ))
        .await;
        assert_eq!(dev.read_channel("pdo").await.unwrap().to_i32(), 42);
        assert!(dev.read_channel("sdo").await.is_err());
        io.in_flight = Some(PendingSdo {
            index: 0x2000,
            sub: 0,
            kind: PendingKind::Read {
                channel: "sdo".into(),
                ty: CanopenDataType::U16,
            },
            deadline: Instant::now() + SDO_TIMEOUT,
        });
        io.on_sdo_response(SdoResponse::UploadOk {
            index: 0x2000,
            sub: 0,
            data: [77, 0, 0, 0],
            len: 2,
        })
        .await;
        assert_eq!(dev.read_channel("sdo").await.unwrap().to_i32(), 77);
    }
}
