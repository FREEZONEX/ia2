// Each in-flight safe-value attempt owns its adapter. The scan keeps the
// slot and last cycle diagnostics, but cannot read or write that adapter
// until its owner returns it. Teardown joins the owners before reuse.

use super::{
    attempt_device_failsafe, begin_failsafe_attempts, ChannelValue, IoDevice, SharedFailsafeStatus,
};
use iocore::{cycle::CycleStatus, IoError};
use tokio::{runtime::Handle, task::JoinHandle};

pub(super) struct FailsafeJobs {
    io: Handle,
    jobs: Vec<(usize, JoinHandle<Box<dyn IoDevice>>)>,
}

impl FailsafeJobs {
    pub(super) fn new(io: Handle) -> Self {
        Self {
            io,
            jobs: Vec::new(),
        }
    }

    pub(super) fn start(
        &mut self,
        devices: &mut [Box<dyn IoDevice>],
        status: &SharedFailsafeStatus,
        selected: impl Fn(usize, &str) -> bool,
    ) {
        let targets: Vec<_> = devices
            .iter()
            .enumerate()
            .filter(|(index, dev)| {
                selected(*index, dev.name()) && !self.jobs.iter().any(|(active, _)| active == index)
            })
            .map(|(index, _)| index)
            .collect();
        begin_failsafe_attempts(status, targets.iter().map(|&index| devices[index].name()));
        // Fence every selected transport before dispatching waits.
        for &index in &targets {
            let dev = &mut devices[index];
            if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                dev.inhibit_outputs();
            })) {
                let error = super::panic_message(panic.as_ref());
                tracing::error!(device = %dev.name(), %error, "adapter output inhibit panicked");
            }
        }
        for index in targets {
            let dev = &mut devices[index];
            let placeholder = Box::new(UnavailableDevice {
                name: dev.name().into(),
                cycle: std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    dev.cycle_status()
                }))
                .ok()
                .flatten(),
            });
            let mut device = std::mem::replace(dev, placeholder);
            let status = status.clone();
            let runtime = self.io.clone();
            // Poll independent adapters on separate blocking workers.
            // Even a synchronous driver section cannot park another
            // adapter's attempt or the scan's observation path.
            let job = self.io.spawn_blocking(move || {
                runtime
                    .block_on(attempt_device_failsafe(device.as_mut(), &status))
                    .ok();
                device
            });
            self.jobs.push((index, job));
        }
    }

    pub(super) async fn collect(
        &mut self,
        devices: &mut [Box<dyn IoDevice>],
        status: &SharedFailsafeStatus,
        wait: bool,
    ) {
        let mut i = 0;
        while i < self.jobs.len() {
            if wait || self.jobs[i].1.is_finished() {
                let (index, job) = self.jobs.swap_remove(i);
                match job.await {
                    Ok(device) => devices[index] = device,
                    Err(error) => {
                        let name = devices[index].name();
                        super::record_failsafe_result(
                            status,
                            name,
                            Some(super::DeviceFailsafeError {
                                device: name.into(),
                                error: format!("failsafe owner failed: {error}"),
                            }),
                        );
                        tracing::error!(device = %name, %error, "failsafe owner failed");
                    }
                }
            } else {
                i += 1;
            }
        }
    }
}

struct UnavailableDevice {
    name: String,
    cycle: Option<CycleStatus>,
}

#[async_trait::async_trait]
impl IoDevice for UnavailableDevice {
    fn name(&self) -> &str {
        &self.name
    }

    fn cycle_status(&self) -> Option<CycleStatus> {
        self.cycle.clone()
    }

    fn is_healthy(&self) -> bool {
        false
    }

    async fn read_channel(&mut self, _: &str) -> Result<ChannelValue, IoError> {
        Err(IoError::Transport(
            "adapter unavailable during failsafe".into(),
        ))
    }

    async fn write_channel(&mut self, _: &str, _: ChannelValue) -> Result<(), IoError> {
        Err(IoError::Transport(
            "adapter unavailable during failsafe".into(),
        ))
    }

    async fn enter_failsafe(&mut self) -> Result<(), IoError> {
        Err(IoError::Transport(
            "failsafe adapter owner unavailable".into(),
        ))
    }

    async fn shutdown(&mut self) -> Result<(), IoError> {
        Err(IoError::Transport(
            "failsafe adapter owner unavailable".into(),
        ))
    }
}
