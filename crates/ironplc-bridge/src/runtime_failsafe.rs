// Each in-flight safe-value attempt owns its adapter. The scan keeps the
// slot and last cycle diagnostics, but cannot read or write that adapter
// until its owner returns it. Teardown joins the owners before reuse.

use super::{
    attempt_device_failsafe, begin_failsafe_attempts, isolate_device_call, record_failsafe_result,
    ChannelValue, DeviceFailsafeError, IoDevice, SharedFailsafeStatus,
};
use iocore::{cycle::CycleStatus, IoError};
use std::time::Duration;
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
            let job = spawn_blocking_on_io(&self.io, move || {
                runtime
                    .block_on(attempt_device_failsafe(device.as_mut(), &status))
                    .ok();
                device
            });
            self.jobs.push((index, job));
        }
    }

    pub(super) async fn collect_ready(
        &mut self,
        devices: &mut [Box<dyn IoDevice>],
        status: &SharedFailsafeStatus,
    ) {
        let mut i = 0;
        while i < self.jobs.len() {
            if self.jobs[i].1.is_finished() {
                let (index, job) = self.jobs.swap_remove(i);
                if let Some(device) = join_owner(job, devices[index].name(), status).await {
                    devices[index] = device;
                }
            } else {
                i += 1;
            }
        }
    }

    /// Each device reaches its final safe attempt and shutdown as soon as
    /// its own owner is available. A queued reconnect gets fenced and driven
    /// safe even when an unrelated adapter's previous attempt is still stuck.
    pub(super) async fn shutdown(
        &mut self,
        devices: &mut [Box<dyn IoDevice>],
        status: &SharedFailsafeStatus,
    ) -> usize {
        let previous_jobs = self.jobs.len();
        self.start(devices, status, |_, _| true);
        let jobs = std::mem::take(&mut self.jobs);
        let io = &self.io;
        let closes = jobs
            .into_iter()
            .enumerate()
            .map(|(position, (index, job))| {
                let name = devices[index].name().to_owned();
                async move {
                    let Some(mut device) = join_owner(job, &name, status).await else {
                        return true;
                    };
                    let runtime = io.clone();
                    let status = status.clone();
                    let result = spawn_blocking_on_io(io, move || {
                        runtime.block_on(async move {
                            // start appended a final pass for every ready slot.
                            // A previous owner still owes that pass after returning.
                            if position < previous_jobs {
                                begin_failsafe_attempts(&status, [device.name()]);
                                let _ = attempt_device_failsafe(device.as_mut(), &status).await;
                            }
                            // Give the adapter worker a flush opportunity. This
                            // delay does not establish transmission or safety.
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            isolate_device_call(device.shutdown()).await
                        })
                    })
                    .await;
                    match result {
                        Ok(Ok(Ok(()))) => false,
                        Ok(Ok(Err(error))) => {
                            tracing::warn!(device = %name, %error, "device shutdown failed");
                            true
                        }
                        Ok(Err(panic)) => {
                            tracing::error!(device = %name, %panic, "device shutdown panicked");
                            true
                        }
                        Err(error) => {
                            tracing::error!(device = %name, %error, "device shutdown owner failed");
                            true
                        }
                    }
                }
            });
        futures_util::future::join_all(closes)
            .await
            .into_iter()
            .filter(|failed| *failed)
            .count()
    }
}

fn spawn_blocking_on_io<T: Send + 'static>(
    io: &Handle,
    operation: impl FnOnce() -> T + Send + 'static,
) -> JoinHandle<T> {
    // Tokio may create a native thread at spawn_blocking's call site. Dispatch
    // from the ordinary I/O worker so it cannot inherit the scan's SCHED_RR.
    io.spawn(async move {
        tokio::task::spawn_blocking(operation)
            .await
            .expect("I/O blocking worker failed")
    })
}

async fn join_owner(
    job: JoinHandle<Box<dyn IoDevice>>,
    name: &str,
    status: &SharedFailsafeStatus,
) -> Option<Box<dyn IoDevice>> {
    match job.await {
        Ok(device) => Some(device),
        Err(error) => {
            record_failsafe_result(
                status,
                name,
                Some(DeviceFailsafeError {
                    device: name.into(),
                    error: format!("failsafe owner failed: {error}"),
                }),
            );
            tracing::error!(device = %name, %error, "failsafe owner failed");
            None
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
