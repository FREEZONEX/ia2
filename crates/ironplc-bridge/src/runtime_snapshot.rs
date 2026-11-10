// The scan owns the VM. Send one coherent raw sample at a time to a
// separate formatter without waiting for its mailbox or sharing the VM.

use super::{DeviceHealth, InputQuality, VarSnapshot, VarValue};
use ironplc_container::{debug_format::VariableRenderer, debug_section::iec_type_tag, VarIndex};
use ironplc_vm::VmRunning;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

struct UnitPlan {
    slots: Vec<u16>,
    has_strings: bool,
}

struct UnitSample {
    slots: Vec<(u16, u64)>,
    data: Vec<u8>,
}

struct Sample {
    units: Vec<UnitSample>,
    quality: Vec<HashMap<u16, InputQuality>>,
    health: Vec<DeviceHealth>,
    timestamp_us: u64,
    scan_count: u64,
}

pub(super) struct SnapshotPublisher {
    plans: Vec<UnitPlan>,
    samples: mpsc::Sender<Sample>,
}

impl SnapshotPublisher {
    pub(super) fn new(
        io: &tokio::runtime::Handle,
        output: broadcast::Sender<VarSnapshot>,
        runnings: &[VmRunning],
        renderers: Arc<Vec<VariableRenderer>>,
        instances: Vec<String>,
        shared_names: HashSet<String>,
    ) -> Self {
        let plans = runnings
            .iter()
            .zip(renderers.iter())
            .map(|(vm, renderer)| {
                let slots: Vec<_> = (0..vm.num_variables())
                    .filter(|&index| renderer.var(index).is_some())
                    .collect();
                let has_strings = slots.iter().any(|&index| {
                    renderer.var(index).is_some_and(|info| {
                        matches!(
                            info.iec_type_tag,
                            iec_type_tag::STRING | iec_type_tag::WSTRING
                        )
                    })
                });
                UnitPlan { slots, has_strings }
            })
            .collect();
        let (samples, mut rx) = mpsc::channel(1);
        io.spawn_blocking(move || {
            while let Some(sample) = rx.blocking_recv() {
                let _ = output.send(render_sample(sample, &renderers, &instances, &shared_names));
            }
        });
        Self { plans, samples }
    }

    pub(super) fn publish(
        &self,
        runnings: &[VmRunning],
        quality: &mut [HashMap<u16, InputQuality>],
        health: &Mutex<Vec<DeviceHealth>>,
        timestamp_us: u64,
        scan_count: u64,
    ) {
        let Ok(health) = health.try_lock() else {
            return;
        };
        let health = health.clone();
        // Link loss latches stale until a fresh read. Merely recovering
        // the transport (especially when paused) cannot clear it.
        for input in quality.iter_mut().flat_map(|unit| unit.values_mut()) {
            input.stale |= !health
                .iter()
                .any(|device| device.name == input.device && device.healthy);
        }
        let Ok(permit) = self.samples.try_reserve() else {
            return;
        };
        let units = self
            .plans
            .iter()
            .zip(runnings)
            .map(|(plan, vm)| UnitSample {
                slots: plan
                    .slots
                    .iter()
                    .filter_map(|&index| {
                        vm.read_variable_raw(VarIndex::new(index))
                            .ok()
                            .map(|raw| (index, raw))
                    })
                    .collect(),
                data: if plan.has_strings {
                    vm.data_region().to_vec()
                } else {
                    Vec::new()
                },
            })
            .collect();
        permit.send(Sample {
            units,
            quality: quality.to_vec(),
            health,
            timestamp_us,
            scan_count,
        });
    }
}

fn render_sample(
    sample: Sample,
    renderers: &[VariableRenderer],
    instances: &[String],
    shared_names: &HashSet<String>,
) -> VarSnapshot {
    let mut vars = Vec::new();
    for (unit, (input_quality, samples)) in sample.quality.into_iter().zip(sample.units).enumerate()
    {
        let renderer = &renderers[unit];
        let input_by_name: HashMap<_, _> = input_quality
            .into_iter()
            .filter_map(|(index, quality)| {
                renderer
                    .var(index)
                    .map(|info| (info.name.as_str(), quality))
            })
            .collect();
        let mut seen = HashSet::new();
        for (index, bits) in samples.slots {
            let Some(info) = renderer.var(index) else {
                continue;
            };
            // Debug names can appear in multiple POU slots in one unit.
            // Keep the first readable instance, matching the monitor contract.
            if !seen.insert(&info.name) {
                continue;
            }
            let name = if shared_names.contains(&info.name) {
                format!("{}.{}", instances[unit], info.name)
            } else {
                info.name.clone()
            };
            vars.push(VarValue {
                name,
                type_name: info.type_name.clone(),
                value: renderer.render(index, bits, &samples.data).text,
                bits,
                input: input_by_name.get(info.name.as_str()).cloned(),
            });
        }
    }
    VarSnapshot {
        timestamp_us: sample.timestamp_us,
        scan_count: sample.scan_count,
        vars,
        device_health: Some(sample.health),
    }
}
