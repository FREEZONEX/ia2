//! Passive observation of an existing synchronized exchange.

use std::collections::HashMap;
use std::num::{NonZeroU16, NonZeroU32};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iocore::axis_capture::{
    AxisCapture, Bus, Capture, CaptureError, Command, EndReason, Exchange, Feedback, InputQuality,
    PdoField, Progress, Scope, SlaveIdentity, Start, Status, MAX_DURATION_MS,
};
use iocore::axis_replay::{Sampling, MAX_SAMPLES};
use iocore::ChannelValue;
use parking_lot::Mutex;
use project::{EthercatConfig, EthercatDataType, EthercatPdoDirection};

#[cfg(test)]
mod tests;

use crate::cycle::MAX_SYNC_PDI_BYTES;

const CONTROL: usize = 0;
const TARGET: usize = 1;
const STATUS: usize = 2;
const POSITION: usize = 3;
const FOLLOWING: usize = 4;
const TORQUE: usize = 5;
const ERROR: usize = 6;

const FIELDS: [(&str, u16, EthercatPdoDirection, EthercatDataType, u8); 7] = {
    use EthercatDataType::{I16, I32, U16};
    use EthercatPdoDirection::{RxPdo, TxPdo};
    [
        ("controlword", 0x6040, RxPdo, U16, 16),
        ("target_position", 0x607a, RxPdo, I32, 32),
        ("statusword", 0x6041, TxPdo, U16, 16),
        ("actual_position", 0x6064, TxPdo, I32, 32),
        ("following_error", 0x60f4, TxPdo, I32, 32),
        ("actual_torque", 0x6077, TxPdo, I16, 16),
        ("error_code", 0x603f, TxPdo, U16, 16),
    ]
};

struct Mapping {
    slave: u16,
    fields: [PdoField; FIELDS.len()],
    names: [String; FIELDS.len()],
}

impl Mapping {
    fn from_config(config: &EthercatConfig, slave: u16) -> Option<Self> {
        let mut fields = Vec::with_capacity(FIELDS.len());
        let mut names = Vec::with_capacity(FIELDS.len());
        for (role, object, direction, data_type, width_bits) in FIELDS {
            let mut matches = config.channels.iter().filter(|ch| {
                ch.slave_index == slave
                    && ch.direction == direction
                    && ch.pdo_index == object
                    && ch.sub_index == 0
            });
            let ch = matches.next()?;
            if matches.next().is_some()
                || ch.data_type != data_type
                || ch.bit_length != width_bits
                || ch.pdi_bit_offset != 0
            {
                return None;
            }
            let field = PdoField {
                role,
                direction: if direction == EthercatPdoDirection::RxPdo {
                    "rx_pdo"
                } else {
                    "tx_pdo"
                },
                object,
                sub_index: 0,
                offset_bytes: ch.pdi_byte_offset,
                width_bits,
                signed: data_type != EthercatDataType::U16,
            };
            if fields.iter().any(|other: &PdoField| {
                other.direction == field.direction
                    && byte_range(other).start < byte_range(&field).end
                    && byte_range(&field).start < byte_range(other).end
            }) {
                return None;
            }
            fields.push(field);
            names.push(ch.name.clone());
        }
        Some(Self {
            slave,
            fields: fields.try_into().ok()?,
            names: names.try_into().ok()?,
        })
    }

    fn fits(&self, slave: &SlaveIdentity) -> bool {
        self.fields.iter().all(|field| {
            let extent = if field.direction == "rx_pdo" {
                slave.output_bytes
            } else {
                slave.input_bytes
            };
            byte_range(field).end <= usize::from(extent)
        })
    }
}

fn byte_range(field: &PdoField) -> std::ops::Range<usize> {
    let start = usize::from(field.offset_bytes);
    start..start + usize::from(field.width_bits / 8)
}

struct Ready {
    metadata: Bus,
    slaves: Vec<SlaveIdentity>,
}

struct Recording {
    tag: u32,
    mapping: usize,
    deadline: Instant,
    used: usize,
    data: Arc<Capture>,
}

#[derive(Default)]
struct State {
    bus: Option<Ready>,
    recording: Option<Recording>,
    last_tag: u32,
}

pub(crate) struct Recorder {
    name: String,
    simulated: bool,
    cycle_ns: NonZeroU32,
    mappings: Vec<Mapping>,
    state: Mutex<State>,
    // High bits hold the active recording's tag. Low bits count attempts
    // missed due to contention. Tagging prevents a late drop counting
    // against a new recording after stop/start.
    active: AtomicU64,
}

pub(crate) struct Session<'a> {
    recorder: &'a Recorder,
    generation: u64,
    origin: Instant,
    expected_wkc: u16,
}

#[derive(Clone, Copy)]
pub(crate) struct Attempt<'a> {
    tag: u32,
    sequence: u64,
    mapping: &'a Mapping,
}

impl Attempt<'_> {
    pub(crate) fn slave(self) -> usize {
        usize::from(self.mapping.slave)
    }

    fn raw<const N: usize>(self, bytes: &[u8], field: usize) -> Option<[u8; N]> {
        bytes
            .get(byte_range(&self.mapping.fields[field]))?
            .try_into()
            .ok()
    }

    pub(crate) fn command(self, bytes: &[u8]) -> Option<Command> {
        Some(Command {
            controlword: u16::from_le_bytes(self.raw(bytes, CONTROL)?),
            target_raw: i32::from_le_bytes(self.raw(bytes, TARGET)?),
        })
    }

    pub(crate) fn feedback(self, bytes: &[u8]) -> Option<Feedback> {
        Some(Feedback {
            statusword: u16::from_le_bytes(self.raw(bytes, STATUS)?),
            position_raw: i32::from_le_bytes(self.raw(bytes, POSITION)?),
            following_error_counts: i32::from_le_bytes(self.raw(bytes, FOLLOWING)?),
            torque_permille: i16::from_le_bytes(self.raw(bytes, TORQUE)?),
            error_code: u16::from_le_bytes(self.raw(bytes, ERROR)?),
        })
    }

    pub(crate) fn sim_pdo(
        self,
        values: &HashMap<String, ChannelValue>,
    ) -> Option<(Command, Feedback)> {
        let value = |field| values.get(&self.mapping.names[field]).map(|v| v.to_i32());
        Some((
            Command {
                controlword: value(CONTROL)? as u16,
                target_raw: value(TARGET)?,
            },
            Feedback {
                statusword: value(STATUS)? as u16,
                position_raw: value(POSITION)?,
                following_error_counts: value(FOLLOWING)?,
                torque_permille: value(TORQUE)? as i16,
                error_code: value(ERROR)? as u16,
            },
        ))
    }
}

pub(crate) enum Reply {
    Received {
        dc_ns: u64,
        wkc: u16,
        all_op: bool,
        feedback: Option<Feedback>,
    },
    Timeout,
    Failed,
}

impl Recorder {
    pub(crate) fn new(name: &str, config: &EthercatConfig, simulated: bool) -> Option<Arc<Self>> {
        let cycle_ns = NonZeroU32::new(config.cycle_us.checked_mul(1_000)?)?;
        if !config.scan_sync || !Sampling::supports_cycle(cycle_ns.get()) {
            return None;
        }
        let mut slaves: Vec<_> = config.channels.iter().map(|ch| ch.slave_index).collect();
        slaves.sort_unstable();
        slaves.dedup();
        let mappings: Vec<_> = slaves
            .into_iter()
            .filter_map(|slave| Mapping::from_config(config, slave))
            .collect();
        (!mappings.is_empty()).then(|| {
            Arc::new(Self {
                name: name.into(),
                simulated,
                cycle_ns,
                mappings,
                state: Mutex::new(State::default()),
                active: AtomicU64::new(0),
            })
        })
    }

    pub(crate) fn open_session(
        &self,
        expected_wkc: u16,
        slaves: Vec<SlaveIdentity>,
    ) -> Option<Session<'_>> {
        static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
        let wkc = NonZeroU16::new(expected_wkc)?;
        let pdi_bytes: usize = slaves
            .iter()
            .map(|slave| usize::from(slave.input_bytes) + usize::from(slave.output_bytes))
            .sum();
        if pdi_bytes > MAX_SYNC_PDI_BYTES {
            return None;
        }
        let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        let capturable_slaves = self
            .mappings
            .iter()
            .filter_map(|mapping| {
                slaves
                    .iter()
                    .any(|slave| slave.index == mapping.slave && mapping.fits(slave))
                    .then_some(mapping.slave)
            })
            .collect();
        let mut state = self.state.lock();
        self.freeze(&mut state, EndReason::BusSessionEnded);
        state.bus = Some(Ready {
            metadata: Bus {
                generation,
                simulated: self.simulated,
                cycle_ns: self.cycle_ns,
                expected_wkc: wkc,
                capturable_slaves,
            },
            slaves,
        });
        Some(Session {
            recorder: self,
            generation,
            origin: Instant::now(),
            expected_wkc,
        })
    }

    fn note_drop(&self, tag: u32) {
        let active = self.active.load(Ordering::Acquire);
        if active >> 32 == u64::from(tag) {
            let dropped = (active as u32).saturating_add(1);
            let next = (u64::from(tag) << 32) | u64::from(dropped);
            // The single exchange worker counts drops. A competing
            // update can only retire or replace this tag.
            let _ = self
                .active
                .compare_exchange(active, next, Ordering::AcqRel, Ordering::Relaxed);
        }
    }

    fn freeze(&self, state: &mut State, reason: EndReason) {
        let active = self.active.swap(0, Ordering::AcqRel);
        if active != 0 {
            let rec = state.recording.as_mut().unwrap();
            let capture = Arc::get_mut(&mut rec.data).expect("active buffer is private");
            capture.samples.truncate(rec.used);
            capture.end_reason = Some(reason);
            capture.dropped_samples = active as u32;
        }
    }

    fn expire(&self, state: &mut State, now: Instant) {
        if state.recording.as_ref().is_some_and(|r| now >= r.deadline)
            && self.active.load(Ordering::Relaxed) != 0
        {
            self.freeze(state, EndReason::DurationLimit);
        }
    }

    fn progress(&self, rec: &Recording) -> Progress {
        let capture = &rec.data;
        Progress {
            capture_id: capture.capture_id.clone(),
            scope: capture.scope.clone(),
            samples: rec.used,
            dropped_samples: if capture.end_reason.is_some() {
                capture.dropped_samples
            } else {
                self.active.load(Ordering::Acquire) as u32
            },
            end_reason: capture.end_reason,
        }
    }

    fn start_capture(&self, request: Start) -> Result<Progress, CaptureError> {
        if !(1..=MAX_SAMPLES).contains(&request.max_samples)
            || !(1..=MAX_DURATION_MS).contains(&request.duration_ms)
        {
            return Err(CaptureError::Invalid(format!(
                "max_samples must be 1..={MAX_SAMPLES}, duration_ms 1..={MAX_DURATION_MS}"
            )));
        }
        let mut state = self.state.lock();
        self.expire(&mut state, Instant::now());
        if self.active.load(Ordering::Relaxed) != 0 {
            return Err(CaptureError::Conflict("a capture is already recording"));
        }
        let bus = state
            .bus
            .as_ref()
            .ok_or(CaptureError::Conflict("bus session unavailable"))?;
        if request.bus_generation != bus.metadata.generation {
            return Err(CaptureError::Conflict("bus generation changed"));
        }
        if !bus.metadata.capturable_slaves.contains(&request.slave) {
            return Err(CaptureError::Invalid(format!(
                "slave {} has no complete supported axis PDO map",
                request.slave
            )));
        }
        let slave = bus
            .slaves
            .iter()
            .find(|slave| slave.index == request.slave)
            .unwrap()
            .clone();
        let mapping = self
            .mappings
            .iter()
            .position(|m| m.slave == request.slave)
            .unwrap();
        let tag = state
            .last_tag
            .checked_add(1)
            .ok_or(CaptureError::Capacity)?;
        let scope = Scope {
            runtime_id: request.runtime_id,
            bus_generation: request.bus_generation,
            device: self.name.clone(),
            slave: request.slave,
        };
        let mut samples = Vec::new();
        samples
            .try_reserve_exact(request.max_samples)
            .map_err(|_| CaptureError::Capacity)?;
        // Initialize the full allocation before activating capture.
        // The exchange loop never grows the buffer or initializes new pages.
        samples.resize(request.max_samples, Exchange::default());
        let data = Capture {
            capture_schema_version: 1,
            capture_id: format!("{}-{}-{tag}", scope.runtime_id, scope.bus_generation),
            scope,
            slave,
            mapping: self.mappings[mapping].fields,
            sampling: Sampling {
                cycle_ns: self.cycle_ns,
                expected_wkc: bus.metadata.expected_wkc,
                counts_per_revolution: request.counts_per_revolution,
            },
            simulated: self.simulated,
            max_samples: request.max_samples,
            duration_ms: request.duration_ms,
            dropped_samples: 0,
            end_reason: None,
            samples,
        };
        state.last_tag = tag;
        state.recording = Some(Recording {
            tag,
            mapping,
            deadline: Instant::now() + Duration::from_millis(u64::from(request.duration_ms)),
            used: 0,
            data: Arc::new(data),
        });
        self.active.store(u64::from(tag) << 32, Ordering::Release);
        Ok(self.progress(state.recording.as_ref().unwrap()))
    }
}

impl AxisCapture for Recorder {
    fn status(&self) -> Status {
        let mut state = self.state.lock();
        self.expire(&mut state, Instant::now());
        Status {
            bus: state.bus.as_ref().map(|bus| bus.metadata.clone()),
            capture: state.recording.as_ref().map(|rec| self.progress(rec)),
        }
    }

    fn start(&self, request: Start) -> Result<Progress, CaptureError> {
        self.start_capture(request)
    }

    fn stop(&self, id: &str) -> Result<Progress, CaptureError> {
        let mut state = self.state.lock();
        let exists = state
            .recording
            .as_ref()
            .is_some_and(|rec| rec.data.capture_id == id);
        if !exists {
            return Err(CaptureError::NotFound);
        }
        self.expire(&mut state, Instant::now());
        self.freeze(&mut state, EndReason::OperatorStop);
        Ok(self.progress(state.recording.as_ref().unwrap()))
    }

    fn frozen(&self, id: &str) -> Result<Arc<Capture>, CaptureError> {
        let mut state = self.state.lock();
        self.expire(&mut state, Instant::now());
        let rec = state
            .recording
            .as_ref()
            .filter(|rec| rec.data.capture_id == id)
            .ok_or(CaptureError::NotFound)?;
        if rec.data.end_reason.is_none() {
            return Err(CaptureError::Conflict("capture is still recording"));
        }
        Ok(rec.data.clone())
    }

    fn freeze_for_shutdown(&self) -> Option<Arc<Capture>> {
        let mut state = self.state.lock();
        self.expire(&mut state, Instant::now());
        // The sim worker can still be winding down. Retiring the tag also
        // prevents an exchange in flight from appending to the frozen archive.
        self.freeze(&mut state, EndReason::WorkerStopped);
        state.bus = None;
        state.recording.as_ref().map(|rec| rec.data.clone())
    }
}

impl Session<'_> {
    pub(crate) fn begin(&self, sequence: u64, now: Instant) -> Option<Attempt<'_>> {
        let tag = (self.recorder.active.load(Ordering::Acquire) >> 32) as u32;
        if tag == 0 {
            return None;
        }
        let Some(mut state) = self.recorder.state.try_lock() else {
            self.recorder.note_drop(tag);
            return None;
        };
        self.recorder.expire(&mut state, now);
        if state.bus.as_ref()?.metadata.generation != self.generation {
            return None;
        }
        let rec = state.recording.as_ref()?;
        (rec.tag == tag && rec.data.end_reason.is_none()).then(|| Attempt {
            tag,
            sequence,
            mapping: &self.recorder.mappings[rec.mapping],
        })
    }

    pub(crate) fn expected_wkc(&self) -> u16 {
        self.expected_wkc
    }

    pub(crate) fn finish(
        &self,
        attempt: Attempt<'_>,
        tx: Instant,
        rx: Instant,
        output_ticket: Option<u64>,
        command: Option<Command>,
        reply: Reply,
    ) {
        let Some(mut state) = self.recorder.state.try_lock() else {
            self.recorder.note_drop(attempt.tag);
            return;
        };
        self.recorder.expire(&mut state, rx);
        let Some(rec) = state
            .recording
            .as_mut()
            .filter(|rec| rec.tag == attempt.tag && rec.data.end_reason.is_none())
        else {
            return;
        };
        let (dc_ns, wkc, all_op, input_quality, feedback) = match reply {
            Reply::Received {
                dc_ns,
                wkc,
                all_op,
                feedback,
            } => {
                let (quality, fresh) = if !all_op {
                    (InputQuality::NotOperational, None)
                } else if wkc != self.expected_wkc {
                    (InputQuality::UnexpectedWkc, None)
                } else if feedback.is_none() {
                    (InputQuality::MissingPdi, None)
                } else {
                    (InputQuality::Fresh, feedback)
                };
                (
                    (dc_ns != 0).then_some(dc_ns),
                    Some(wkc),
                    Some(all_op),
                    quality,
                    fresh,
                )
            }
            Reply::Timeout => (None, None, None, InputQuality::Timeout, None),
            Reply::Failed => (None, None, None, InputQuality::ExchangeFailed, None),
        };
        let capture = Arc::get_mut(&mut rec.data).expect("active buffer is private");
        let dc_gap = capture.samples[..rec.used]
            .last()
            .filter(|last| attempt.sequence.checked_sub(last.attempt) == Some(1))
            .and_then(|last| last.dc_ns.zip(dc_ns))
            .and_then(|(previous, current)| capture.sampling.dc_gap(previous, current));
        let elapsed = |now: Instant| now.saturating_duration_since(self.origin).as_nanos() as u64;
        capture.samples[rec.used] = Exchange {
            attempt: attempt.sequence,
            tx_monotonic_ns: elapsed(tx),
            rx_monotonic_ns: elapsed(rx),
            output_ticket,
            dc_ns,
            wkc,
            all_op,
            input_quality,
            dc_gap,
            command,
            feedback,
        };
        rec.used += 1;
        if rec.used == capture.max_samples {
            self.recorder.freeze(&mut state, EndReason::SampleLimit);
        }
    }

    pub(crate) fn close(&self, reason: EndReason) {
        let mut state = self.recorder.state.lock();
        if state
            .bus
            .as_ref()
            .is_some_and(|bus| bus.metadata.generation == self.generation)
        {
            self.recorder.freeze(&mut state, reason);
            state.bus = None;
        }
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        self.close(EndReason::WorkerStopped);
    }
}
