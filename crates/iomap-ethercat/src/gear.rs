//! In-cycle electronic gear (B-tier motion).
//!
//! The ST-tier gear (see the eg_servo / dual_gear example projects) computes
//! the follower's target_position in the PLC scan plane (~500 Hz, async to
//! the bus), so up to a scan of phase jitter sits between "target computed"
//! and "frame sent". This module moves the per-cycle interpolation into the
//! cyclic loop itself: the slow plane only feeds parameters, and the target
//! is generated strictly cycle-aligned.
//!
//! Split:
//!   - [`GearShared`] — lock-free parameter mailbox. `write_channel` routes
//!     the configured channel names here (instead of PDI bytes); the cyclic
//!     loop reads it every tick. Engine state (engaged / trip) flows back
//!     the same way for `read_channel`.
//!   - [`GearEngine`] — pure per-cycle math, no I/O: callers hand it the
//!     follower's statusword + actual_position and (for an axis master) the
//!     master's actual_position, and get the target_position to write.
//!     Pure so the real Sync0 loop, the free-run loop, the sim ticker and
//!     unit tests all share one implementation.
//!
//! Safety model (carried over from the field-hardened ST tier, enforced in
//! the fast plane so no slow-plane mistake can bypass it):
//!   - While the follower is not in CiA402 Operation Enabled the engine
//!     shadows its actual_position — zero target jump at enable.
//!   - Engagement requires `max_travel` > 0 (locked by default) and latches
//!     the master/follower origins at the engage edge; overtravel trips to
//!     a position hold, cleared only by dropping the engage channel.
//!   - Losing Operation Enabled mid-run demands a re-arm: engage must be
//!     observed low before a new engagement, so a stale engage force can't
//!     auto-restart motion after a fault reset.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use iocore::{ChannelValue, IoError};
use project::{EthercatGear, GearMaster};
// Preserve this module's public API while sharing the contract with the linter.
pub use project::{GearParam, GearReadback};

/// Lock-free slow→fast parameter mailbox plus fast→slow feedback, one per
/// configured gear axis. f64 values are stored as `to_bits` in `AtomicU64`.
#[derive(Debug, Default)]
pub struct GearShared {
    ratio_num: AtomicU64,
    ratio_den: AtomicU64,
    ratio_step: AtomicU64,
    phase_ofs: AtomicU64,
    master_vel: AtomicU64,
    max_travel: AtomicU64,
    engage: AtomicBool,
    /// Rising edge = apply the current ratio channels to the running gear,
    /// phase-continuously. Release/Acquire-paired like `engage`: the slow
    /// plane stores ratio_num/den (Relaxed) then raises this flag, so a tick
    /// that observes it also sees those parameter writes.
    ratio_apply: AtomicBool,
    // Engine → slow plane.
    engaged_fb: AtomicBool,
    trip_fb: AtomicBool,
    /// Accepted-apply counter (handshake readback for the slow plane).
    ratio_ack: AtomicU64,
}

impl GearShared {
    fn store_f64(a: &AtomicU64, v: f64) {
        a.store(v.to_bits(), Ordering::Relaxed);
    }
    fn load_f64(a: &AtomicU64) -> f64 {
        f64::from_bits(a.load(Ordering::Relaxed))
    }

    /// Slow-plane write. Numeric `ChannelValue`s are widened to f64; the
    /// engage flag accepts Bool or any numeric (nonzero = engaged) so REAL
    /// or INT PLC vars can drive it too.
    pub fn write(&self, param: GearParam, value: &ChannelValue) {
        let as_f64 = match *value {
            ChannelValue::Bool(b) => {
                if b {
                    1.0
                } else {
                    0.0
                }
            }
            ChannelValue::U16(v) => v as f64,
            ChannelValue::I32(v) => v as f64,
            ChannelValue::Real(v) => v as f64,
            ChannelValue::F64(v) => v,
        };
        if matches!(param, GearParam::Engage | GearParam::RatioApply) {
            let flag = match param {
                GearParam::Engage => &self.engage,
                GearParam::RatioApply => &self.ratio_apply,
                _ => unreachable!(),
            };
            // Release: these flags are the mailbox's publish points. A PLC
            // scan sets ratio/phase (Relaxed stores above) and then raises
            // engage (or ratio_apply) in the same cycle; pairing this store
            // with the engine's Acquire load (see `tick`) guarantees that a
            // tick observing the flag also sees those parameter writes, so
            // the edge latch can't capture a stale (ratio, phase) against a
            // fresh request on a weakly-ordered CPU.
            flag.store(as_f64 != 0.0, Ordering::Release);
            return;
        }
        // Reject non-finite numeric params at the door: a slow-plane NaN/inf
        // (e.g. an ST `0.0/0.0` while composing a ratio) must never reach the
        // fast-plane law, where it would poison the target and silently
        // bypass the max_travel trip. Dropping the write keeps the last good
        // value in force.
        if !as_f64.is_finite() {
            return;
        }
        match param {
            GearParam::Engage | GearParam::RatioApply => unreachable!(),
            GearParam::RatioNum => Self::store_f64(&self.ratio_num, as_f64),
            GearParam::RatioDen => Self::store_f64(&self.ratio_den, as_f64),
            GearParam::RatioStep => Self::store_f64(&self.ratio_step, as_f64),
            GearParam::PhaseOfs => Self::store_f64(&self.phase_ofs, as_f64),
            GearParam::MasterVel => Self::store_f64(&self.master_vel, as_f64),
            GearParam::MaxTravel => Self::store_f64(&self.max_travel, as_f64),
        }
    }

    /// Slow-plane read: parameter echo plus live engine state.
    pub fn read(&self, rb: GearReadback) -> ChannelValue {
        match rb {
            GearReadback::Engage => ChannelValue::Bool(self.engage.load(Ordering::Relaxed)),
            GearReadback::RatioApply => {
                ChannelValue::Bool(self.ratio_apply.load(Ordering::Relaxed))
            }
            GearReadback::RatioAck => {
                ChannelValue::F64(self.ratio_ack.load(Ordering::Relaxed) as f64)
            }
            GearReadback::RatioNum => ChannelValue::F64(Self::load_f64(&self.ratio_num)),
            GearReadback::RatioDen => ChannelValue::F64(Self::load_f64(&self.ratio_den)),
            GearReadback::RatioStep => ChannelValue::F64(Self::load_f64(&self.ratio_step)),
            GearReadback::PhaseOfs => ChannelValue::F64(Self::load_f64(&self.phase_ofs)),
            GearReadback::MasterVel => ChannelValue::F64(Self::load_f64(&self.master_vel)),
            GearReadback::MaxTravel => ChannelValue::F64(Self::load_f64(&self.max_travel)),
            GearReadback::Engaged => ChannelValue::Bool(self.engaged_fb.load(Ordering::Relaxed)),
            GearReadback::Trip => ChannelValue::Bool(self.trip_fb.load(Ordering::Relaxed)),
        }
    }

    /// Failsafe hook: drop the engage request so the engine disengages on
    /// its next tick (position hold / shadow) even if the slow plane never
    /// gets another word in.
    pub fn disengage(&self) {
        // Release to stay consistent with the other engage writer, though a
        // clear only needs the value to land — the engine reads it Acquire.
        self.engage.store(false, Ordering::Release);
    }
}

/// Master position source, resolved from [`project::GearMaster`].
#[derive(Debug, Clone, Copy)]
pub enum MasterSrc {
    /// Software accumulator advanced by `master_vel` counts per cycle.
    Virtual,
    /// Another axis's actual_position, read from its input PDI each cycle.
    Axis { slave_index: u16, offset: usize },
}

/// Per-cycle gear engine for one follower axis. Owned by the cyclic worker
/// (or sim ticker); everything it needs from the slow plane arrives through
/// the shared mailbox, so `tick` is lock-free and allocation-free.
pub struct GearEngine {
    pub follower_index: u16,
    pub target_off: usize,
    pub actual_off: usize,
    pub status_off: usize,
    pub master: MasterSrc,
    shared: Arc<GearShared>,
    /// Continuous, wrap-free master position (counts). For a virtual master
    /// this is an accumulator advanced by `master_vel`; for an axis master
    /// it integrates the per-cycle *wrapping* i32 delta of the master's
    /// actual_position, so the ±2^31 encoder wrap never becomes a full-scale
    /// jump in `(master - m0)`. `None` until the first tick primes it.
    master_pos: f64,
    /// Previous raw i32 master actual, for the wrapping-delta integration.
    prev_master_raw: Option<i32>,
    /// Current output target (counts). Shadow / gear law / hold all land
    /// here; it is the single source of what goes on the wire.
    target: f64,
    was_engaged: bool,
    ratio: f64,
    /// Gear parameters latched at the engage edge (the documented contract:
    /// mid-run edits are inert until re-engage). `ratio_step` stays live —
    /// it only sets the soft-engage ramp speed.
    ratio_num_l: f64,
    ratio_den_l: f64,
    phase_l: f64,
    trip: bool,
    /// Set when Operation Enabled is lost; a fresh engagement requires the
    /// engage request to be observed low first (operator re-arm), so stale
    /// engage forces can't restart motion after a fault reset.
    need_rearm: bool,
    /// Travel-limit anchor. Latched (= s0) at the engage edge and NEVER moved
    /// by a mid-run ratio apply, so `max_travel` keeps meaning "total
    /// excursion since engage" rather than a per-segment budget that a ratio
    /// change would silently reset.
    clamp_origin: f64,
    /// Level tracker for the ratio-apply request (edge detection).
    prev_apply: bool,
    /// This tick's master advance (counts) — used by the apply rebase so the
    /// apply cycle keeps the old ratio's contribution for the in-flight
    /// master delta instead of a one-cycle zero-velocity notch.
    last_master_delta: f64,
    /// Accepted-apply counter, mirrored into the shared mailbox by emit().
    ack: u64,
}

impl GearEngine {
    pub fn new(cfg: &EthercatGear, shared: Arc<GearShared>) -> Self {
        let master = match cfg.master {
            GearMaster::Virtual => MasterSrc::Virtual,
            GearMaster::Axis {
                slave_index,
                actual_pos_offset,
            } => MasterSrc::Axis {
                slave_index,
                offset: actual_pos_offset as usize,
            },
        };
        GearEngine {
            follower_index: cfg.slave_index,
            target_off: cfg.target_pos_offset as usize,
            actual_off: cfg.actual_pos_offset as usize,
            status_off: cfg.status_word_offset as usize,
            master,
            shared,
            master_pos: 0.0,
            prev_master_raw: None,
            target: 0.0,
            was_engaged: false,
            ratio: 0.0,
            ratio_num_l: 0.0,
            ratio_den_l: 1.0,
            phase_l: 0.0,
            trip: false,
            need_rearm: false,
            clamp_origin: 0.0,
            prev_apply: false,
            last_master_delta: 0.0,
            ack: 0,
        }
    }

    /// One bus cycle. `master_actual` is `Some` for an axis master (read by
    /// the caller from that axis's input PDI) and `None` for virtual.
    /// `bus_ok` is `false` when the *previous* exchange failed, so the inputs
    /// are stale — the engine then freezes (no master advance, target held)
    /// and recovery applies no accumulated step. Returns the target_position
    /// to write into the follower's output PDI.
    pub fn tick(
        &mut self,
        follower_sw: u16,
        follower_actual: i32,
        master_actual: Option<i32>,
        bus_ok: bool,
    ) -> i32 {
        // Stale-input cycle (previous tx_rx failed): hold the target and
        // FORGET the master baseline. During an outage `sd.inputs_raw()`
        // keeps returning the pre-outage value, so re-priming to it would be
        // a no-op — and then the first fresh reading after recovery would
        // integrate the ENTIRE outage displacement in one cycle (a catch-up
        // jerk into the follower). Dropping the baseline instead makes the
        // first good cycle re-prime with a ZERO delta: the gear resyncs to
        // the master's current position and continues, accepting a one-time
        // phase slip rather than commanding the whole gap. (A virtual master
        // has no baseline to keep — it's simply frozen while `bus_ok` is
        // false.)
        if !bus_ok {
            self.prev_master_raw = None;
            return self.emit();
        }

        // Acquire pairs with the Release store in `GearShared::write`: if we
        // observe engage=true here, every parameter the slow plane wrote
        // before raising engage (ratio/phase/…) is visible, so the engage-edge
        // latch below captures a coherent snapshot even on a weakly-ordered CPU.
        let engage_in = self.shared.engage.load(Ordering::Acquire);
        // Same Release/Acquire pairing as engage: observing apply=true
        // guarantees the ratio values written before it are visible.
        let apply_in = self.shared.ratio_apply.load(Ordering::Acquire);
        // Slow-plane values are sanitized to finite at ingress (see
        // GearShared::write), so no NaN/inf can reach the law here.
        let ratio_step = GearShared::load_f64(&self.shared.ratio_step).max(0.0);
        let master_vel = GearShared::load_f64(&self.shared.master_vel);
        let max_travel = GearShared::load_f64(&self.shared.max_travel);

        // Advance the continuous, wrap-free master position, remembering this
        // tick's delta for the apply rebase below.
        self.last_master_delta = match master_actual {
            // Axis master: integrate the WRAPPING i32 delta so the encoder's
            // ±2^31 rollover contributes a small step, not a full-scale jump.
            Some(raw) => {
                let d = match self.prev_master_raw {
                    Some(prev) => raw.wrapping_sub(prev) as f64,
                    None => 0.0,
                };
                self.master_pos += d;
                self.prev_master_raw = Some(raw);
                d
            }
            // Virtual master: software accumulator, advances every tick.
            None => {
                self.master_pos += master_vel;
                master_vel
            }
        };

        // CiA402: Operation Enabled = statusword & 0x6F == 0x27.
        let enabled = (follower_sw & 0x006F) == 0x0027;
        if !enabled {
            // Shadow the feedback so the drive latches target == actual at
            // the enable instant — no jump, no following-error trip.
            self.target = follower_actual as f64;
            self.was_engaged = false;
            self.ratio = 0.0;
            self.need_rearm = true;
            self.prev_apply = apply_in; // level-track: no edge from stale-high
        } else {
            if !engage_in {
                self.need_rearm = false; // observed low while enabled: re-armed
                self.trip = false; // dropping engage is the trip reset
            }
            let engage = engage_in && !self.trip && !self.need_rearm && max_travel > 0.0;
            if engage && !self.was_engaged {
                // Engage edge: latch the travel-limit anchor and the gear
                // params (documented contract — mid-run phase edits are inert
                // until re-engage; ratio can be re-applied live, see below).
                // The follower position accumulator `self.target` is already
                // == shadowed actual here (no-jump); apply the phase offset
                // once and start the ratio ramp from 0. ratio_step stays live.
                self.clamp_origin = self.target;
                self.ratio = 0.0;
                self.ratio_num_l = GearShared::load_f64(&self.shared.ratio_num);
                self.ratio_den_l = GearShared::load_f64(&self.shared.ratio_den);
                self.phase_l = GearShared::load_f64(&self.shared.phase_ofs);
                self.target += self.phase_l;
            }
            // Mid-run ratio apply: change the gear ratio on the fly, phase-
            // continuously. Because the law integrates INCREMENTALLY
            // (`target += ratio * master_delta`, below), re-targeting the
            // ratio needs nothing but a re-latch: `self.ratio` keeps its value
            // and the existing ratio_step clamp ramps it from the OLD ratio to
            // the new one, so the follower velocity ramps monotonically with
            // no position step and no lever-arm overshoot. Gated on "already
            // engaged before this tick" (never races the engage edge), leaves
            // clamp_origin alone (max_travel still bounds TOTAL excursion since
            // engage), and is NOT a re-engage: a tripped / re-arm-pending
            // engine ignores it entirely.
            let apply_edge = apply_in && !self.prev_apply;
            self.prev_apply = apply_in;
            if apply_edge && engage && self.was_engaged {
                self.ratio_num_l = GearShared::load_f64(&self.shared.ratio_num);
                self.ratio_den_l = GearShared::load_f64(&self.shared.ratio_den);
                self.ack = self.ack.wrapping_add(1);
            }
            self.was_engaged = engage;
            if engage {
                let tgt_ratio = if self.ratio_den_l == 0.0 {
                    0.0
                } else {
                    self.ratio_num_l / self.ratio_den_l
                };
                let d = (tgt_ratio - self.ratio).clamp(-ratio_step, ratio_step);
                self.ratio += d;
                // Incremental integration: the follower advances by
                // ratio × (this tick's master delta). Identical to the
                // absolute lever form `s0 + ratio*(master-m0)` at constant
                // ratio, but during a ratio ramp the follower velocity is
                // exactly ratio×master_vel (monotonic) rather than
                // overshooting by ratiȯ × lever — which is what makes an
                // on-the-fly ratio change clean.
                let raw_target = self.target + self.ratio * self.last_master_delta;
                // Hard travel bound: clamp the commanded position to
                // ±max_travel about the engage origin, so max_travel is a
                // true limit on what reaches the wire — not just a
                // fire-after-the-fact trip. A wild ratio or any other
                // over-range demand is capped, and the trip latches to hold
                // there until engage drops.
                self.target = raw_target.clamp(
                    self.clamp_origin - max_travel,
                    self.clamp_origin + max_travel,
                );
                if (raw_target - self.clamp_origin).abs() >= max_travel {
                    self.trip = true;
                }
            }
            // Disengaged / tripped / re-arm pending: hold self.target.
        }

        self.emit()
    }

    /// Publish engine state and encode the held target for the wire.
    fn emit(&self) -> i32 {
        self.shared
            .engaged_fb
            .store(self.was_engaged, Ordering::Relaxed);
        self.shared.trip_fb.store(self.trip, Ordering::Relaxed);
        self.shared.ratio_ack.store(self.ack, Ordering::Relaxed);

        // Saturate rather than wrap at the i32 edge (±256 motor revs on a
        // 23-bit encoder). Long unidirectional runs remain a known limit.
        // `target` is always finite: params are finite by ingress guard and
        // clamped by max_travel, so round()/clamp can't see NaN here.
        self.target.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32
    }
}

/// Channel-name routing tables for one device's gear axes, consulted by
/// `write_channel` / `read_channel` before the PDI channel lookup.
#[derive(Default)]
pub struct GearRouting {
    pub writes: HashMap<String, (Arc<GearShared>, GearParam)>,
    pub reads: HashMap<String, (Arc<GearShared>, GearReadback)>,
}

impl GearRouting {
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty() && self.reads.is_empty()
    }

    /// Failsafe hook: drop every gear's engage request. The engines pick
    /// this up on their next tick and fall back to position hold / shadow.
    pub fn disengage_all(&self) {
        for (shared, _) in self.writes.values() {
            shared.disengage();
        }
    }

    pub fn write(&self, channel: &str, value: &ChannelValue) -> Option<Result<(), IoError>> {
        self.writes.get(channel).map(|(shared, param)| {
            shared.write(*param, value);
            Ok(())
        })
    }

    pub fn read(&self, channel: &str) -> Option<ChannelValue> {
        self.reads.get(channel).map(|(shared, rb)| shared.read(*rb))
    }
}

/// Connect-time check that every gear channel name is unique — across all
/// gears and within each gear — and doesn't shadow a PDO channel. Without
/// this, `build`'s HashMap inserts silently overwrite a duplicate, so a
/// second gear axis (or a fat-fingered channel rename) would misroute
/// parameters to the wrong engine with no error. Returns the first
/// collision found. Called by both the real and sim connect paths.
pub fn validate_channels(
    gears: &[EthercatGear],
    pdo_names: &std::collections::HashSet<&str>,
) -> Result<(), String> {
    project::validate_gear_channel_names(gears, pdo_names)
}

/// Build the engines + routing tables from the device config. Returns the
/// engines for the cyclic worker and the routing for the device facade.
/// Assumes [`validate_channels`] has already passed (unique names).
pub fn build(gears: &[EthercatGear]) -> (Vec<GearEngine>, GearRouting) {
    let mut engines = Vec::with_capacity(gears.len());
    let mut routing = GearRouting::default();
    for cfg in gears {
        let shared = Arc::new(GearShared::default());
        for (name, readback) in cfg.routed_channels() {
            if let Some(parameter) = readback.parameter() {
                routing
                    .writes
                    .insert(name.into(), (shared.clone(), parameter));
            }
            routing
                .reads
                .insert(name.into(), (shared.clone(), readback));
        }
        engines.push(GearEngine::new(cfg, shared));
    }
    (engines, routing)
}

/// Little-endian i32 read from an input PDI buffer; `None` if out of range.
pub fn read_i32(buf: &[u8], off: usize) -> Option<i32> {
    buf.get(off..off + 4)
        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Little-endian u16 read from an input PDI buffer; `None` if out of range.
pub fn read_u16(buf: &[u8], off: usize) -> Option<u16> {
    buf.get(off..off + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
}

/// Little-endian i32 write into an output PDI buffer; ignored if out of range
/// (misconfigured offset — the validation pass warns at connect).
pub fn write_i32(buf: &mut [u8], off: usize, v: i32) {
    if let Some(dst) = buf.get_mut(off..off + 4) {
        dst.copy_from_slice(&v.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OP_ENABLED: u16 = 0x1637; // real SV660N Operation Enabled word
    const RDY_ONLY: u16 = 0x1631; // Ready To Switch On (not enabled)

    fn virtual_config() -> EthercatGear {
        EthercatGear {
            slave_index: 0,
            target_pos_offset: 2,
            actual_pos_offset: 4,
            status_word_offset: 2,
            master: GearMaster::Virtual,
            engage_channel: "gear_engage".into(),
            ratio_num_channel: "ratio_num".into(),
            ratio_den_channel: "ratio_den".into(),
            ratio_step_channel: "ratio_step".into(),
            phase_channel: "phase_ofs".into(),
            master_vel_channel: "master_vel".into(),
            max_travel_channel: "gear_max_travel".into(),
            engaged_channel: "gear_engaged".into(),
            trip_channel: "gear_trip".into(),
            ratio_apply_channel: "gear_ratio_apply".into(),
            ratio_ack_channel: "gear_ratio_ack".into(),
        }
    }

    fn engine_virtual() -> (GearEngine, Arc<GearShared>) {
        let cfg = virtual_config();
        let shared = Arc::new(GearShared::default());
        (GearEngine::new(&cfg, shared.clone()), shared)
    }

    #[test]
    fn routing_keeps_the_existing_nine_channel_contract() {
        let (_, routing) = build(&[virtual_config()]);
        // Independent expectations pin names AND destinations. Echo-only
        // checks could miss two parameters accidentally swapped in the catalog.
        let expected = [
            ("gear_engage", GearReadback::Engage, Some(GearParam::Engage)),
            (
                "ratio_num",
                GearReadback::RatioNum,
                Some(GearParam::RatioNum),
            ),
            (
                "ratio_den",
                GearReadback::RatioDen,
                Some(GearParam::RatioDen),
            ),
            (
                "ratio_step",
                GearReadback::RatioStep,
                Some(GearParam::RatioStep),
            ),
            (
                "phase_ofs",
                GearReadback::PhaseOfs,
                Some(GearParam::PhaseOfs),
            ),
            (
                "master_vel",
                GearReadback::MasterVel,
                Some(GearParam::MasterVel),
            ),
            (
                "gear_max_travel",
                GearReadback::MaxTravel,
                Some(GearParam::MaxTravel),
            ),
            ("gear_engaged", GearReadback::Engaged, None),
            ("gear_trip", GearReadback::Trip, None),
        ];
        assert_eq!(routing.reads.len(), 9);
        assert_eq!(routing.writes.len(), 7);
        for (name, readback, parameter) in expected {
            assert_eq!(routing.reads.get(name).map(|(_, rb)| *rb), Some(readback));
            assert_eq!(routing.writes.get(name).map(|(_, p)| *p), parameter);
        }
        for name in ["gear_ratio_apply", "gear_ratio_ack", "typo"] {
            assert!(routing.read(name).is_none());
            assert!(routing.write(name, &ChannelValue::Bool(true)).is_none());
        }
    }

    #[tokio::test]
    async fn sim_facade_echoes_parameters_and_rejects_feedback_writes() {
        use iocore::IoDevice;
        let config = project::EthercatConfig {
            nic: crate::SIM_NIC.into(),
            cycle_us: 2_000,
            bringup: Default::default(),
            dc_sync: Default::default(),
            dc_static_sync_iterations: 0,
            slaves: vec![],
            channels: vec![],
            gear: vec![virtual_config()],
        };
        let mut device = crate::EthercatDevice::connect("sim_gear".into(), &config)
            .await
            .unwrap();
        for (name, value) in [
            ("gear_engage", ChannelValue::Bool(false)),
            ("ratio_num", ChannelValue::F64(2.0)),
            ("ratio_den", ChannelValue::F64(3.0)),
            ("ratio_step", ChannelValue::F64(0.25)),
            ("phase_ofs", ChannelValue::F64(-4.0)),
            ("master_vel", ChannelValue::F64(5.0)),
            ("gear_max_travel", ChannelValue::F64(0.0)),
        ] {
            device.write_channel(name, value).await.unwrap();
            assert_eq!(device.read_channel(name).await.unwrap(), value);
        }
        for name in ["gear_engaged", "gear_trip"] {
            assert_eq!(
                device.read_channel(name).await.unwrap(),
                ChannelValue::Bool(false)
            );
            assert!(device
                .write_channel(name, ChannelValue::Bool(true))
                .await
                .is_err());
        }
        for name in ["gear_ratio_apply", "gear_ratio_ack", "typo"] {
            assert!(device.read_channel(name).await.is_err());
            assert!(device
                .write_channel(name, ChannelValue::Bool(true))
                .await
                .is_err());
        }
        device.shutdown().await.unwrap();
    }

    #[test]
    fn renamed_routes_use_the_configured_name() {
        let mut cfg = virtual_config();
        cfg.ratio_num_channel = "custom_ratio".into();
        let (_, routing) = build(&[cfg]);
        routing
            .write("custom_ratio", &ChannelValue::F64(3.0))
            .unwrap()
            .unwrap();
        assert_eq!(routing.read("custom_ratio"), Some(ChannelValue::F64(3.0)));
        assert!(routing.read("ratio_num").is_none());
        assert!(routing
            .write("ratio_num", &ChannelValue::F64(1.0))
            .is_none());
    }

    #[test]
    fn every_route_takes_its_name_from_the_config() {
        let defaults = virtual_config();
        let mut cfg = virtual_config();
        for name in [
            &mut cfg.engage_channel,
            &mut cfg.ratio_num_channel,
            &mut cfg.ratio_den_channel,
            &mut cfg.ratio_step_channel,
            &mut cfg.phase_channel,
            &mut cfg.master_vel_channel,
            &mut cfg.max_travel_channel,
            &mut cfg.engaged_channel,
            &mut cfg.trip_channel,
        ] {
            *name = format!("axis2_{name}");
        }
        let (_, routing) = build(std::slice::from_ref(&cfg));
        assert_eq!(routing.reads.len(), 9);
        assert_eq!(routing.writes.len(), 7);
        assert!(routing.reads.keys().all(|k| k.starts_with("axis2_")));
        assert!(routing.writes.keys().all(|k| k.starts_with("axis2_")));
        // A catalog entry that hard-coded its default instead of reading the
        // config would leave the old name routed on a fully renamed axis.
        for (name, _) in defaults.routed_channels() {
            assert!(routing.read(name).is_none(), "{name}");
            assert!(
                routing.write(name, &ChannelValue::F64(1.0)).is_none(),
                "{name}"
            );
        }
    }

    #[test]
    fn is_bool_matches_the_runtime_value_lane() {
        let shared = GearShared::default();
        // Written out independently of `is_bool`, and exhaustively: a new
        // readback variant breaks compilation here instead of silently
        // skewing the I/O linter's boolean range warning.
        let expects_bool = |rb: GearReadback| match rb {
            GearReadback::Engage
            | GearReadback::RatioApply
            | GearReadback::Engaged
            | GearReadback::Trip => true,
            GearReadback::RatioAck
            | GearReadback::RatioNum
            | GearReadback::RatioDen
            | GearReadback::RatioStep
            | GearReadback::PhaseOfs
            | GearReadback::MasterVel
            | GearReadback::MaxTravel => false,
        };
        for rb in [
            GearReadback::Engage,
            GearReadback::RatioApply,
            GearReadback::RatioAck,
            GearReadback::RatioNum,
            GearReadback::RatioDen,
            GearReadback::RatioStep,
            GearReadback::PhaseOfs,
            GearReadback::MasterVel,
            GearReadback::MaxTravel,
            GearReadback::Engaged,
            GearReadback::Trip,
        ] {
            assert_eq!(rb.is_bool(), expects_bool(rb), "{rb:?}");
            assert_eq!(
                matches!(shared.read(rb), ChannelValue::Bool(_)),
                expects_bool(rb),
                "{rb:?}"
            );
        }
    }

    fn set(shared: &GearShared, p: GearParam, v: f64) {
        shared.write(p, &ChannelValue::F64(v));
    }

    #[test]
    fn shadows_actual_until_enabled_then_holds() {
        let (mut e, _s) = engine_virtual();
        // Not enabled: target shadows whatever the feedback says.
        assert_eq!(e.tick(RDY_ONLY, 12345, None, true), 12345);
        assert_eq!(e.tick(RDY_ONLY, -777, None, true), -777);
        // Enable: target latched (holds), no jump.
        assert_eq!(e.tick(OP_ENABLED, -777, None, true), -777);
        assert_eq!(
            e.tick(OP_ENABLED, -700, None, true),
            -777,
            "hold, not follow"
        );
    }

    #[test]
    fn engage_refused_without_max_travel() {
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 1.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 1.0);
        set(&s, GearParam::MasterVel, 100.0);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        for _ in 0..10 {
            assert_eq!(
                e.tick(OP_ENABLED, 0, None, true),
                0,
                "locked while max_travel=0"
            );
        }
        assert_eq!(s.read(GearReadback::Engaged), ChannelValue::Bool(false));
    }

    #[test]
    fn steady_state_ratio_exact() {
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 2.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 1.0); // hard engage: full ratio in 1 tick
        set(&s, GearParam::MasterVel, 100.0);
        set(&s, GearParam::MaxTravel, 1e9);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        let t1 = e.tick(OP_ENABLED, 0, None, true);
        let mut prev = t1;
        for _ in 0..50 {
            let t = e.tick(OP_ENABLED, 0, None, true);
            assert_eq!(t - prev, 200, "2:1 of 100 cnt/cycle master");
            prev = t;
        }
        assert_eq!(s.read(GearReadback::Engaged), ChannelValue::Bool(true));
    }

    #[test]
    fn soft_engage_ramps_and_disengage_holds() {
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 1.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 0.1);
        set(&s, GearParam::MasterVel, 100.0);
        set(&s, GearParam::MaxTravel, 1e9);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        let mut last = 0;
        for _ in 0..20 {
            last = e.tick(OP_ENABLED, 0, None, true);
        }
        assert!(last > 0);
        s.write(GearParam::Engage, &ChannelValue::Bool(false));
        let hold = e.tick(OP_ENABLED, 0, None, true);
        assert_eq!(hold, last, "disengage holds position");
        assert_eq!(e.tick(OP_ENABLED, 0, None, true), hold, "keeps holding");
    }

    #[test]
    fn negative_ratio_step_is_inert_not_runaway() {
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 2.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, -0.5);
        set(&s, GearParam::MasterVel, 100.0);
        set(&s, GearParam::MaxTravel, 1e9);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        for _ in 0..20 {
            assert_eq!(e.tick(OP_ENABLED, 0, None, true), 0, "ratio never ramps");
        }
    }

    #[test]
    fn overtravel_trips_holds_and_needs_engage_drop() {
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 1.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 1.0);
        set(&s, GearParam::MasterVel, 100.0);
        set(&s, GearParam::MaxTravel, 450.0);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        let mut targets = Vec::new();
        for _ in 0..10 {
            targets.push(e.tick(OP_ENABLED, 0, None, true));
        }
        // Trips once |target| >= 450 (one cycle of overshoot allowed).
        let peak = *targets.iter().max().unwrap();
        assert!((450..=550).contains(&peak), "peak {peak}");
        assert_eq!(*targets.last().unwrap(), peak, "held after trip");
        assert_eq!(s.read(GearReadback::Trip), ChannelValue::Bool(true));
        // Still engaged-requested: stays held (trip persists).
        assert_eq!(e.tick(OP_ENABLED, 0, None, true), peak);
        // Drop engage → trip clears; re-engage runs again from new origin.
        s.write(GearParam::Engage, &ChannelValue::Bool(false));
        e.tick(OP_ENABLED, 0, None, true);
        assert_eq!(s.read(GearReadback::Trip), ChannelValue::Bool(false));
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        e.tick(OP_ENABLED, 0, None, true); // engage tick: at new origin (= held peak)
        let t = e.tick(OP_ENABLED, 0, None, true);
        assert!(t > peak, "re-engaged and following again");
    }

    #[test]
    fn enable_loss_requires_rearm_before_new_engagement() {
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 1.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 1.0);
        set(&s, GearParam::MasterVel, 100.0);
        set(&s, GearParam::MaxTravel, 1e9);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        e.tick(OP_ENABLED, 0, None, true); // engage tick: latches origin
        assert!(e.tick(OP_ENABLED, 0, None, true) > 0, "engaged and moving");
        // Drive drops out (fault / disable); engage force left high.
        e.tick(RDY_ONLY, 500, None, true);
        // Back to enabled with the stale engage still high: must NOT move.
        let t0 = e.tick(OP_ENABLED, 500, None, true);
        for _ in 0..10 {
            assert_eq!(e.tick(OP_ENABLED, 500, None, true), t0, "no auto-restart");
        }
        // Operator re-arms: engage low, then high again → fresh engagement.
        s.write(GearParam::Engage, &ChannelValue::Bool(false));
        e.tick(OP_ENABLED, 500, None, true);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        e.tick(OP_ENABLED, 500, None, true);
        assert!(
            e.tick(OP_ENABLED, 500, None, true) > t0,
            "moves after explicit re-arm"
        );
    }

    #[test]
    fn axis_master_follows_actual() {
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 1.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 1.0);
        set(&s, GearParam::MaxTravel, 1e9);
        // Real bring-up order: shadow while not enabled (s0 arms at the
        // follower's actual), enable with engage low (re-arm), then engage.
        e.tick(RDY_ONLY, 1000, Some(5000), true); // shadow: target=1000
        e.tick(OP_ENABLED, 1000, Some(5000), true); // enabled, engage low
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        e.tick(OP_ENABLED, 1000, Some(5000), true); // engage: m0=5000, s0=1000
        let t = e.tick(OP_ENABLED, 1000, Some(5300), true);
        assert_eq!(t, 1300, "s0 + 1.0*(master-m0)");
        let t = e.tick(OP_ENABLED, 1000, Some(4000), true);
        assert_eq!(t, 0, "follows master backwards too");
    }

    #[test]
    fn axis_master_i32_wrap_is_a_small_step_not_full_scale() {
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 1.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 1.0);
        set(&s, GearParam::MaxTravel, 1e9);
        // Engage with the master near the i32 max.
        let near_max = i32::MAX - 100;
        e.tick(RDY_ONLY, 0, Some(near_max), true);
        e.tick(OP_ENABLED, 0, Some(near_max), true);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        let base = e.tick(OP_ENABLED, 0, Some(near_max), true); // engage edge
                                                                // Advance 200 counts, which wraps i32 (MAX-100 + 200 -> MIN+99).
        let wrapped = near_max.wrapping_add(200);
        assert!(wrapped < 0, "precondition: the master actually wrapped");
        let t = e.tick(OP_ENABLED, 0, Some(wrapped), true);
        assert_eq!(t - base, 200, "wrap contributes a +200 step, not ~-4.3e9");
    }

    #[test]
    fn nan_param_is_rejected_at_ingress_and_target_stays_finite() {
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 2.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 1.0);
        set(&s, GearParam::MasterVel, 100.0);
        set(&s, GearParam::MaxTravel, 1e9);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        e.tick(OP_ENABLED, 0, None, true);
        let good = e.tick(OP_ENABLED, 0, None, true);
        // Slow plane writes NaN (e.g. ST 0.0/0.0) to ratio_den; must be dropped.
        s.write(GearParam::RatioDen, &ChannelValue::F64(f64::NAN));
        s.write(GearParam::RatioNum, &ChannelValue::F64(f64::INFINITY));
        assert_eq!(
            s.read(GearReadback::RatioDen),
            ChannelValue::F64(1.0),
            "NaN write dropped, last good value kept"
        );
        // Engine keeps producing finite, monotonic targets (2:1 unchanged).
        let t1 = e.tick(OP_ENABLED, 0, None, true);
        let t2 = e.tick(OP_ENABLED, 0, None, true);
        assert_eq!(t1 - good, 200);
        assert_eq!(t2 - t1, 200, "still 2:1, not poisoned to 0");
    }

    #[test]
    fn mid_run_ratio_and_phase_edits_are_inert_until_reengage() {
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 1.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 10.0); // hard engage: full ratio in 1 tick
        set(&s, GearParam::MasterVel, 100.0);
        set(&s, GearParam::MaxTravel, 1e9);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        e.tick(OP_ENABLED, 0, None, true); // engage: latch 1:1, phase 0
        let a = e.tick(OP_ENABLED, 0, None, true);
        let b = e.tick(OP_ENABLED, 0, None, true);
        assert_eq!(b - a, 100, "1:1");
        // Mid-run edits: must NOT take effect while engaged.
        set(&s, GearParam::RatioNum, 5.0);
        set(&s, GearParam::PhaseOfs, 1_000_000.0);
        let c = e.tick(OP_ENABLED, 0, None, true);
        let d = e.tick(OP_ENABLED, 0, None, true);
        assert_eq!(d - c, 100, "still 1:1, no phase step — edits inert");
        // Re-engage picks up the new params.
        s.write(GearParam::Engage, &ChannelValue::Bool(false));
        e.tick(OP_ENABLED, 0, None, true);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        e.tick(OP_ENABLED, 0, None, true); // engage: latch 5:1
        let p = e.tick(OP_ENABLED, 0, None, true);
        let q = e.tick(OP_ENABLED, 0, None, true);
        assert_eq!(q - p, 500, "now 5:1 after re-engage");
    }

    #[test]
    fn max_travel_hard_clamps_the_commanded_excursion() {
        let (mut e, s) = engine_virtual();
        // Absurd ratio typo: 1000:1. Without the hard clamp this steps ~1e5
        // counts in the first engaged cycle.
        set(&s, GearParam::RatioNum, 1000.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 1000.0);
        set(&s, GearParam::MasterVel, 100.0);
        set(&s, GearParam::MaxTravel, 450.0);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        let base = e.tick(OP_ENABLED, 0, None, true); // engage edge, s0 here
        let mut peak_excursion = 0;
        for _ in 0..8 {
            let t = e.tick(OP_ENABLED, 0, None, true);
            peak_excursion = peak_excursion.max((t - base).abs());
        }
        assert!(
            peak_excursion <= 450,
            "excursion {peak_excursion} must never exceed max_travel, even for a wild ratio"
        );
        assert_eq!(s.read(GearReadback::Trip), ChannelValue::Bool(true));
    }

    #[test]
    fn stale_bus_cycle_freezes_master_and_holds_target() {
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 1.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 1.0);
        set(&s, GearParam::MasterVel, 100.0);
        set(&s, GearParam::MaxTravel, 1e9);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        e.tick(OP_ENABLED, 0, None, true);
        let before = e.tick(OP_ENABLED, 0, None, true);
        // 5 stale cycles (bus down): master must NOT integrate, target holds.
        for _ in 0..5 {
            assert_eq!(
                e.tick(OP_ENABLED, 0, None, false),
                before,
                "held during outage"
            );
        }
        // Recovery: exactly one cycle of advance, no 5-cycle catch-up.
        let after = e.tick(OP_ENABLED, 0, None, true);
        assert_eq!(
            after - before,
            100,
            "single-cycle step on recovery, not 600"
        );
    }

    #[test]
    fn stale_bus_axis_master_resyncs_without_catchup_step() {
        // The dangerous case: an AXIS master keeps physically turning during
        // a bus outage while its PDI image is frozen at the pre-outage value.
        // On recovery the engine must resync to the master's new position
        // with ZERO commanded step, not integrate the whole outage gap.
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 1.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 1.0);
        set(&s, GearParam::MaxTravel, 1e9);
        e.tick(RDY_ONLY, 0, Some(1000), true);
        e.tick(OP_ENABLED, 0, Some(1000), true);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        let base = e.tick(OP_ENABLED, 0, Some(1000), true); // engage edge
                                                            // Outage: 5 stale cycles, PDI frozen at the last-good raw (1000).
        for _ in 0..5 {
            assert_eq!(e.tick(OP_ENABLED, 0, Some(1000), false), base, "held");
        }
        // Recovery: fresh raw is 1600 (master moved 600 during the outage).
        let after = e.tick(OP_ENABLED, 0, Some(1600), true);
        assert_eq!(
            after, base,
            "resync with zero step, NOT a +600 catch-up jerk"
        );
        // And it follows normally from the new baseline afterwards.
        let next = e.tick(OP_ENABLED, 0, Some(1650), true);
        assert_eq!(next - after, 50, "resumes 1:1 from the resynced position");
    }

    // ---- dynamic ratio apply (phase-continuous on-the-fly ratio change) ----

    /// Engage at `ratio` (hard, single-tick), run to steady state, return the
    /// engine + shared so a test can then pulse ratio_apply with a new ratio.
    fn engaged_at(ratio: f64) -> (GearEngine, Arc<GearShared>) {
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, ratio);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 100.0); // hard: reach ratio in one tick
        set(&s, GearParam::MasterVel, 100.0);
        set(&s, GearParam::MaxTravel, 1e9);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        for _ in 0..3 {
            e.tick(OP_ENABLED, 0, None, true);
        }
        (e, s)
    }

    fn pulse_apply(s: &GearShared, num: f64, den: f64, step: f64) {
        set(s, GearParam::RatioNum, num);
        set(s, GearParam::RatioDen, den);
        set(s, GearParam::RatioStep, step);
        s.write(GearParam::RatioApply, &ChannelValue::Bool(true));
    }

    #[test]
    fn ratio_apply_is_phase_continuous_no_step() {
        let (mut e, s) = engaged_at(1.0); // 1:1 -> 100 cnt/cycle
        let before = e.tick(OP_ENABLED, 0, None, true);
        let a = e.tick(OP_ENABLED, 0, None, true);
        assert_eq!(a - before, 100, "1:1 steady");
        // Apply 2:1 with a SMALL ramp: the apply cycle must NOT jump — its
        // single-cycle delta stays ~ old ratio * master_vel (100), not
        // (2-1)*(master_pos-m0) which would be a large step.
        pulse_apply(&s, 2.0, 1.0, 0.01);
        let at_apply = e.tick(OP_ENABLED, 0, None, true);
        let step = at_apply - a;
        assert!(
            (95..=115).contains(&step),
            "apply cycle delta {step} must stay near the old-ratio velocity (no jump)"
        );
    }

    #[test]
    fn ratio_apply_ramps_from_old_to_new_and_converges() {
        let (mut e, s) = engaged_at(1.0);
        pulse_apply(&s, 2.0, 1.0, 0.05); // ramp 1->2 at 0.05/cycle
        let mut prev = e.tick(OP_ENABLED, 0, None, true);
        let mut deltas = Vec::new();
        for _ in 0..60 {
            let t = e.tick(OP_ENABLED, 0, None, true);
            deltas.push(t - prev);
            prev = t;
        }
        // Monotonic non-decreasing per-cycle delta from ~100 toward ~200.
        for w in deltas.windows(2) {
            assert!(w[1] >= w[0] - 1, "ratio ramps monotonically up");
        }
        assert_eq!(*deltas.last().unwrap(), 200, "converges exactly to 2:1");
    }

    #[test]
    fn ratio_apply_ramps_down_and_into_negative() {
        let (mut e, s) = engaged_at(2.0); // 200 cnt/cycle
        pulse_apply(&s, -1.0, 1.0, 0.05); // 2 -> -1
        let mut prev = e.tick(OP_ENABLED, 0, None, true);
        let mut deltas = Vec::new();
        for _ in 0..90 {
            let t = e.tick(OP_ENABLED, 0, None, true);
            deltas.push(t - prev);
            prev = t;
        }
        for w in deltas.windows(2) {
            assert!(w[1] <= w[0] + 1, "ratio ramps monotonically down");
        }
        assert_eq!(
            *deltas.last().unwrap(),
            -100,
            "converges to -1:1 (reversed)"
        );
    }

    #[test]
    fn ratio_apply_uncompensated_step_is_never_emitted() {
        // Pin the compensation: run master far from the engage origin so the
        // UNcompensated path (re-latch ratio without rebasing m0/s0) would
        // emit a huge step. Assert the engine does not.
        let (mut e, s) = engaged_at(1.0);
        for _ in 0..100 {
            e.tick(OP_ENABLED, 0, None, true); // master_pos now ~ +10000
        }
        let a = e.tick(OP_ENABLED, 0, None, true);
        pulse_apply(&s, 3.0, 1.0, 0.02);
        let at_apply = e.tick(OP_ENABLED, 0, None, true);
        // Uncompensated would be s0 + 3*(master_pos - m0) ≈ +2*10000 step.
        assert!(
            (at_apply - a).abs() < 300,
            "compensated: no lever-arm step ({}) at apply",
            at_apply - a
        );
    }

    #[test]
    fn ratio_apply_keeps_total_travel_clamp_origin() {
        // clamp_origin stays at the engage origin: a ratio apply must not
        // reset the max_travel budget. Engage, travel near the limit, apply,
        // and assert the trip still fires on TOTAL excursion from engage.
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 1.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 100.0);
        set(&s, GearParam::MasterVel, 100.0);
        set(&s, GearParam::MaxTravel, 450.0);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        e.tick(OP_ENABLED, 0, None, true); // engage, clamp_origin = 0
                                           // Travel 3 cycles (~300), then apply a new ratio.
        for _ in 0..3 {
            e.tick(OP_ENABLED, 0, None, true);
        }
        pulse_apply(&s, 1.0, 1.0, 100.0); // same ratio, just an apply
        let mut tripped = false;
        for _ in 0..5 {
            e.tick(OP_ENABLED, 0, None, true);
            if s.read(GearReadback::Trip) == ChannelValue::Bool(true) {
                tripped = true;
            }
        }
        assert!(
            tripped,
            "max_travel still bounds TOTAL excursion after apply"
        );
    }

    #[test]
    fn ratio_apply_is_not_a_reengage() {
        // Apply must not clear a trip nor reset the soft-engage ramp: a
        // tripped engine ignores apply and stays held.
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 1.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 100.0);
        set(&s, GearParam::MasterVel, 100.0);
        set(&s, GearParam::MaxTravel, 250.0);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        for _ in 0..6 {
            e.tick(OP_ENABLED, 0, None, true); // trips past 250
        }
        assert_eq!(s.read(GearReadback::Trip), ChannelValue::Bool(true));
        let held = e.tick(OP_ENABLED, 0, None, true);
        pulse_apply(&s, 2.0, 1.0, 100.0);
        let after = e.tick(OP_ENABLED, 0, None, true);
        assert_eq!(after, held, "apply on a tripped engine is a no-op (held)");
        assert_eq!(s.read(GearReadback::Trip), ChannelValue::Bool(true));
    }

    #[test]
    fn ratio_apply_is_edge_not_level_and_acks() {
        let (mut e, s) = engaged_at(1.0);
        assert_eq!(s.read(GearReadback::RatioAck), ChannelValue::F64(0.0));
        // Hold apply HIGH across many cycles: exactly ONE rebase / ONE ack.
        pulse_apply(&s, 2.0, 1.0, 100.0);
        for _ in 0..10 {
            e.tick(OP_ENABLED, 0, None, true);
        }
        assert_eq!(
            s.read(GearReadback::RatioAck),
            ChannelValue::F64(1.0),
            "level-high applies exactly once"
        );
        // Drop and re-pulse -> second ack.
        s.write(GearParam::RatioApply, &ChannelValue::Bool(false));
        e.tick(OP_ENABLED, 0, None, true);
        pulse_apply(&s, 3.0, 1.0, 100.0);
        e.tick(OP_ENABLED, 0, None, true);
        assert_eq!(s.read(GearReadback::RatioAck), ChannelValue::F64(2.0));
    }

    #[test]
    fn ratio_apply_ignored_while_disengaged_or_disabled() {
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 1.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 100.0);
        set(&s, GearParam::MaxTravel, 1e9);
        // Not engaged: apply pulse is a no-op, no ack.
        pulse_apply(&s, 2.0, 1.0, 100.0);
        for _ in 0..5 {
            e.tick(OP_ENABLED, 0, None, true);
        }
        assert_eq!(s.read(GearReadback::RatioAck), ChannelValue::F64(0.0));
        // Not enabled: apply pulse is a no-op even if engage is set.
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        for _ in 0..3 {
            e.tick(RDY_ONLY, 0, None, true);
        }
        assert_eq!(s.read(GearReadback::RatioAck), ChannelValue::F64(0.0));
    }

    #[test]
    fn ratio_apply_stale_high_across_enable_does_not_edge() {
        // apply held HIGH while disabled must not fire an edge when the drive
        // comes enabled+engaged (level-tracked in the not-enabled branch).
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 1.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 100.0);
        set(&s, GearParam::MasterVel, 100.0);
        set(&s, GearParam::MaxTravel, 1e9);
        s.write(GearParam::RatioApply, &ChannelValue::Bool(true)); // stale high
        for _ in 0..3 {
            e.tick(RDY_ONLY, 0, None, true);
        }
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        for _ in 0..5 {
            e.tick(OP_ENABLED, 0, None, true);
        }
        assert_eq!(
            s.read(GearReadback::RatioAck),
            ChannelValue::F64(0.0),
            "stale-high apply must not edge on enable"
        );
    }

    #[test]
    fn ratio_apply_rejects_non_finite_new_ratio() {
        let (mut e, s) = engaged_at(2.0);
        let a = e.tick(OP_ENABLED, 0, None, true);
        let b = e.tick(OP_ENABLED, 0, None, true);
        assert_eq!(b - a, 200);
        // NaN into ratio_num is dropped at ingress; apply keeps last-good 2:1.
        s.write(GearParam::RatioNum, &ChannelValue::F64(f64::NAN));
        s.write(GearParam::RatioApply, &ChannelValue::Bool(true));
        let c = e.tick(OP_ENABLED, 0, None, true);
        let d = e.tick(OP_ENABLED, 0, None, true);
        assert_eq!(d - c, 200, "still 2:1 — NaN ratio never reached the law");
    }

    #[test]
    fn ratio_apply_mid_soft_engage_ramp() {
        // Apply while the initial soft-engage ramp hasn't reached the target:
        // rebase keeps the partial ratio and re-targets, still continuous.
        let (mut e, s) = engine_virtual();
        set(&s, GearParam::RatioNum, 4.0);
        set(&s, GearParam::RatioDen, 1.0);
        set(&s, GearParam::RatioStep, 0.05); // slow ramp toward 4:1
        set(&s, GearParam::MasterVel, 100.0);
        set(&s, GearParam::MaxTravel, 1e9);
        s.write(GearParam::Engage, &ChannelValue::Bool(true));
        for _ in 0..10 {
            e.tick(OP_ENABLED, 0, None, true); // ratio ~0.5, still ramping
        }
        let a = e.tick(OP_ENABLED, 0, None, true);
        let pre = e.tick(OP_ENABLED, 0, None, true);
        let vel_pre = pre - a;
        pulse_apply(&s, 1.0, 1.0, 0.05); // retarget to 1:1 from partial
        let at_apply = e.tick(OP_ENABLED, 0, None, true);
        assert!(
            (at_apply - pre - vel_pre).abs() <= 6,
            "continuous through an apply during the soft-engage ramp"
        );
    }

    #[test]
    fn validate_channels_catches_gear_collisions() {
        use std::collections::HashSet;
        let mut a = EthercatGear {
            slave_index: 0,
            target_pos_offset: 2,
            actual_pos_offset: 4,
            status_word_offset: 2,
            master: GearMaster::Virtual,
            engage_channel: "gear_engage".into(),
            ratio_num_channel: "ratio_num".into(),
            ratio_den_channel: "ratio_den".into(),
            ratio_step_channel: "ratio_step".into(),
            phase_channel: "phase_ofs".into(),
            master_vel_channel: "master_vel".into(),
            max_travel_channel: "gear_max_travel".into(),
            engaged_channel: "gear_engaged".into(),
            trip_channel: "gear_trip".into(),
            ratio_apply_channel: "gear_ratio_apply".into(),
            ratio_ack_channel: "gear_ratio_ack".into(),
        };
        let empty: HashSet<&str> = HashSet::new();
        assert!(validate_channels(std::slice::from_ref(&a), &empty).is_ok());
        // PDO shadow.
        let pdo: HashSet<&str> = ["ratio_num"].into_iter().collect();
        assert!(validate_channels(std::slice::from_ref(&a), &pdo).is_err());
        // Two default-named gears collide.
        let b = a.clone();
        assert!(validate_channels(&[a.clone(), b], &empty).is_err());
        // Intra-gear collision.
        a.phase_channel = "ratio_num".into();
        assert!(validate_channels(std::slice::from_ref(&a), &empty).is_err());
    }
}
