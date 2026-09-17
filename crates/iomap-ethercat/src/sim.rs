//! In-memory PDO buffer ("sim mode"). Selected when `EthercatConfig.nic`
//! equals `"_sim"`. Used for:
//!  - macOS development (raw L2 sockets need root + BPF; not portable)
//!  - Demo / IDE round-trip verification without hardware
//!  - CI tests of the iomap layer
//!
//! Output channels echo what the program writes; input channels start
//! at zero. Channel references (unknown slave index, duplicate name) and
//! PDI ranges are validated exactly as in real mode, so those iomap
//! mistakes surface early.
//!
//! Channel *shapes* are not: sim keeps values per name and never touches the
//! bit packers, so entries real mode refuses (a lane narrower than the
//! entry, a non-byte-aligned multi-bit entry, a zero bit_length) still
//! connect here — deliberately, so legacy sim-only configs keep working.
//! They are logged as a WARN naming what the real bus will do, because a
//! sim run is meant to be evidence about the bench.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use iocore::{ChannelValue, IoDevice, IoError};
use project::{EthercatChannel, EthercatConfig, EthercatDataType, EthercatPdoDirection};

use crate::validate;

pub struct SimEthercat {
    name: String,
    values: Arc<Mutex<HashMap<String, ChannelValue>>>,
    channels: HashMap<String, EthercatChannel>,
    /// Slow-plane routing for in-cycle gear parameter channels (same
    /// surface as the real device).
    gear_routing: crate::gear::GearRouting,
    /// Stops the sim gear ticker thread on drop/shutdown.
    gear_stop: Arc<AtomicBool>,
    discovered: Vec<crate::SlaveDiscovery>,
}

impl SimEthercat {
    pub async fn connect(name: String, config: &EthercatConfig) -> Result<Self, IoError> {
        // Each channel must point at a known slave index, and no two
        // channels may share a name — same references check as the real path.
        validate::validate_channel_refs(&config.channels, &config.slaves)
            .map_err(IoError::Connect)?;

        // Sim stores values per name and never touches the bit packers, so a
        // channel shape the PDI accessors cannot serve connects here and is
        // refused on the real bus. That difference is the point of this
        // warning: proving a project in sim is supposed to be evidence about
        // the bench, and silently accepting what the bench will reject makes
        // it evidence about nothing. WARN rather than refuse — legacy
        // sim-only configs with odd shapes keep working, they just say so.
        if let Err(why) = validate::validate_channel_shapes(&config.channels) {
            tracing::warn!(
                device = %name,
                %why,
                "ethercat sim: channel shapes a real bus would REFUSE at connect — \
                 sim serves them from its per-name value map, real mode cannot"
            );
        }

        let channels: HashMap<String, EthercatChannel> = config
            .channels
            .iter()
            .map(|c| (c.name.clone(), c.clone()))
            .collect();
        let values: HashMap<String, ChannelValue> = config
            .channels
            .iter()
            .map(|c| (c.name.clone(), zero_for(c.data_type)))
            .collect();

        // Sim "discovery": report the configured slaves with PDI extents
        // derived from their channels, so /discover works without hardware
        // (dev / CI / authoring against a sim bus).
        let discovered: Vec<crate::SlaveDiscovery> = config
            .slaves
            .iter()
            .map(|s| {
                let extent = |dir: EthercatPdoDirection| -> u16 {
                    config
                        .channels
                        .iter()
                        .filter(|c| c.slave_index == s.index && c.direction == dir)
                        .map(|c| c.pdi_byte_offset + byte_size(c.bit_length))
                        .max()
                        .unwrap_or(0)
                };
                crate::SlaveDiscovery {
                    index: s.index,
                    name: s.name.clone(),
                    input_bytes: extent(EthercatPdoDirection::TxPdo),
                    output_bytes: extent(EthercatPdoDirection::RxPdo),
                    vendor_id: s.vendor_id,
                    product_id: s.product_id,
                }
            })
            .collect();

        // Same connect-time PDI range validation as the real path, run
        // against the derived extents. Byte-aligned channels fit their
        // own extent by construction; what this actually catches is a
        // bit-packed entry spilling past the byte its extent accounts
        // for (e.g. bit_offset 7 with bit_length 2) — a layout the real
        // PDI accessors could never serve, surfaced here at connect so
        // sim and real reject the same configs.
        if !config.slaves.is_empty() {
            validate::validate_pdi_ranges(&config.channels, &discovered)
                .map_err(IoError::Connect)?;
        }

        // In-cycle gear axes: same engine + routing as the real device. A
        // background ticker stands in for the cyclic loop, modelling an
        // IDEAL follower drive: always Operation Enabled, actual == last
        // target. Virtual-master gearing is therefore fully exercisable in
        // sim (the engine math itself is unit-tested); an axis master has
        // no live feedback here (sim inputs are all zero), so that path
        // holds position in sim and is validated on the real bus.
        let values = Arc::new(Mutex::new(values));
        // Same channel-uniqueness validation as the real path so sim rejects
        // the same misconfigurations (collisions / PDO shadowing).
        let pdo_names: std::collections::HashSet<&str> =
            config.channels.iter().map(|c| c.name.as_str()).collect();
        crate::gear::validate_channels(&config.gear, &pdo_names).map_err(IoError::Connect)?;
        let (engines, gear_routing) = crate::gear::build(&config.gear);
        let gear_stop = Arc::new(AtomicBool::new(false));
        if !engines.is_empty() {
            // Surface each engine's live target through its follower's
            // target_position RxPDO channel (matched by slave + offset) so
            // /status and iomap inputs can observe it, like the real PDI echo.
            let target_names: Vec<Option<String>> = engines
                .iter()
                .map(|e| {
                    config
                        .channels
                        .iter()
                        .find(|c| {
                            c.slave_index == e.follower_index
                                && c.direction == EthercatPdoDirection::RxPdo
                                && c.pdi_byte_offset as usize == e.target_off
                        })
                        .map(|c| c.name.clone())
                })
                .collect();
            let values_t = values.clone();
            let stop_t = gear_stop.clone();
            let cycle = std::time::Duration::from_micros(config.cycle_us.max(100) as u64);
            let mut engines = engines;
            std::thread::Builder::new()
                .name("ec-sim-gear".into())
                .spawn(move || {
                    // Ideal drive: feed the engine its own last target as
                    // the follower actual; statusword = Operation Enabled.
                    let mut last: Vec<i32> = vec![0; engines.len()];
                    while !stop_t.load(Ordering::Relaxed) {
                        for (i, eng) in engines.iter_mut().enumerate() {
                            let master = match eng.master {
                                crate::gear::MasterSrc::Virtual => None,
                                crate::gear::MasterSrc::Axis { .. } => Some(0),
                            };
                            let t = eng.tick(0x0027, last[i], master, true);
                            last[i] = t;
                            if let Some(Some(name)) = target_names.get(i) {
                                if let Ok(mut v) = values_t.lock() {
                                    v.insert(name.clone(), ChannelValue::I32(t));
                                }
                            }
                        }
                        std::thread::sleep(cycle);
                    }
                })
                .ok();
        }

        tracing::info!(
            name = %name,
            slaves = config.slaves.len(),
            channels = config.channels.len(),
            gear_axes = config.gear.len(),
            "ethercat device ready (sim mode)"
        );
        Ok(Self {
            name,
            values,
            channels,
            gear_routing,
            gear_stop,
            discovered,
        })
    }

    fn channel(&self, name: &str) -> Result<&EthercatChannel, IoError> {
        self.channels
            .get(name)
            .ok_or_else(|| IoError::UnknownChannel(name.into()))
    }

    /// Configured slaves as a discovery report (sim has no real bus).
    pub fn discovered(&self) -> Vec<crate::SlaveDiscovery> {
        self.discovered.clone()
    }
}

#[async_trait]
impl IoDevice for SimEthercat {
    fn name(&self) -> &str {
        &self.name
    }

    async fn read_channel(&mut self, channel: &str) -> Result<ChannelValue, IoError> {
        if let Some(v) = self.gear_routing.read(channel) {
            return Ok(v);
        }
        let _meta = self.channel(channel)?;
        Ok(self
            .values
            .lock()
            .expect("sim values poisoned")
            .get(channel)
            .copied()
            .unwrap_or(ChannelValue::I32(0)))
    }

    async fn write_channel(&mut self, channel: &str, value: ChannelValue) -> Result<(), IoError> {
        if let Some(res) = self.gear_routing.write(channel, &value) {
            return res;
        }
        let meta = self.channel(channel)?.clone();
        if meta.direction == EthercatPdoDirection::TxPdo {
            return Err(IoError::TypeMismatch {
                channel: channel.into(),
                value,
            });
        }
        let coerced = coerce_to_type(value, meta.data_type);
        self.values
            .lock()
            .expect("sim values poisoned")
            .insert(channel.into(), coerced);
        Ok(())
    }

    /// Zero every RxPDO (output) channel. TxPDO entries are inputs from
    /// the bus and stay untouched. Matches `RealEthercat::enter_failsafe`
    /// semantics so sim and real behave identically from the scan
    /// loop's perspective.
    async fn enter_failsafe(&mut self) -> Result<(), IoError> {
        let to_zero: Vec<(String, EthercatDataType)> = self
            .channels
            .values()
            .filter(|c| c.direction == EthercatPdoDirection::RxPdo)
            .map(|c| (c.name.clone(), c.data_type))
            .collect();
        self.gear_routing.disengage_all();
        let mut values = self.values.lock().expect("sim values poisoned");
        for (name, ty) in to_zero {
            values.insert(name, zero_for(ty));
        }
        tracing::info!(device = %self.name, "ethercat (sim) failsafe applied");
        Ok(())
    }
}

impl Drop for SimEthercat {
    fn drop(&mut self) {
        self.gear_stop.store(true, Ordering::Relaxed);
    }
}

/// Bytes occupied by a PDO entry of `bit_length` bits (rounded up; a
/// sub-byte entry still occupies one byte).
fn byte_size(bit_length: u8) -> u16 {
    (bit_length as u16).div_ceil(8)
}

fn zero_for(ty: EthercatDataType) -> ChannelValue {
    match ty {
        EthercatDataType::Bool => ChannelValue::Bool(false),
        EthercatDataType::U8
        | EthercatDataType::I8
        | EthercatDataType::U16
        | EthercatDataType::I16 => ChannelValue::U16(0),
        EthercatDataType::U32 | EthercatDataType::I32 => ChannelValue::I32(0),
        EthercatDataType::Real => ChannelValue::Real(0.0),
    }
}

fn coerce_to_type(value: ChannelValue, ty: EthercatDataType) -> ChannelValue {
    match ty {
        EthercatDataType::Bool => ChannelValue::Bool(value.to_i32() != 0),
        EthercatDataType::U8
        | EthercatDataType::I8
        | EthercatDataType::U16
        | EthercatDataType::I16 => ChannelValue::U16(value.to_i32() as u16),
        EthercatDataType::U32 | EthercatDataType::I32 => ChannelValue::I32(value.to_i32()),
        // Keep the fraction — sim mirrors what a real REAL PDO carries.
        EthercatDataType::Real => ChannelValue::Real(value.to_f32()),
    }
}
