//! Deadline wait for the dedicated synchronized EtherCAT worker.

use std::{io, time::Instant};

pub(crate) struct CycleTimer {
    native: bool,
}

impl CycleTimer {
    pub(crate) fn new(synchronized: bool) -> io::Result<Self> {
        let native = synchronized && cfg!(target_os = "linux");
        #[cfg(target_os = "linux")]
        if native {
            // SAFETY: PR_SET_TIMERSLACK takes an unsigned long in nanoseconds
            // and changes only the calling thread's timer coalescing.
            if unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, 1 as libc::c_ulong, 0, 0, 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { native })
    }

    pub(crate) fn uses_native_clock(&self) -> bool {
        self.native
    }

    pub(crate) async fn wait(&mut self, deadline: Instant) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        if self.native {
            // This worker owns the DC schedule. Socket I/O is pumped on a
            // separate thread. Mailbox futures on this worker can run when
            // the exchange yields, with at most a cycle's extra wait here.
            if let Err(error) = sleep_until(deadline) {
                self.native = false;
                return Err(error);
            }
            return Ok(());
        }
        smol::Timer::at(deadline).await;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn sleep_until(deadline: Instant) -> io::Result<()> {
    loop {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: now points to writable timespec storage. Linux Instant
        // uses the same monotonic clock without exposing its representation.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // Sample Instant second so a preemption between samples can only
        // move the kernel deadline earlier. Recheck before returning.
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(());
        };
        let nanos = now.tv_nsec + remaining.subsec_nanos() as libc::c_long;
        let target = libc::timespec {
            tv_sec: now.tv_sec + remaining.as_secs() as libc::time_t + nanos / 1_000_000_000,
            tv_nsec: nanos % 1_000_000_000,
        };
        loop {
            // SAFETY: target is a normalized, initialized timespec, and
            // absolute waits do not use the remaining-time pointer.
            let result = unsafe {
                libc::clock_nanosleep(
                    libc::CLOCK_MONOTONIC,
                    libc::TIMER_ABSTIME,
                    &target,
                    std::ptr::null_mut(),
                )
            };
            match result {
                0 => break,
                libc::EINTR => continue,
                code => return Err(io::Error::from_raw_os_error(code)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn cycle_wait_preserves_deadlines_and_selects_native_only_for_linux_sync() {
        for synchronized in [false, true] {
            std::thread::spawn(move || {
                let mut timer = CycleTimer::new(synchronized).unwrap();
                assert_eq!(
                    timer.uses_native_clock(),
                    synchronized && cfg!(target_os = "linux")
                );
                smol::block_on(async {
                    timer.wait(Instant::now()).await.unwrap();
                    let start = Instant::now();
                    for cycle in 1..=16 {
                        let deadline = start + Duration::from_millis(2) * cycle;
                        timer.wait(deadline).await.unwrap();
                        assert!(Instant::now() >= deadline);
                    }
                });
            })
            .join()
            .unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_deadline_returns_while_shared_reactor_dispatch_is_blocked() {
        use std::{
            future::Future,
            sync::{mpsc, Arc, Mutex},
            task::{Context, Wake},
        };

        struct HoldReactor {
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl Wake for HoldReactor {
            fn wake(self: Arc<Self>) {
                self.entered.send(()).unwrap();
                // Bound the hold even when the test fails before releasing it.
                let _ = self
                    .release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2));
            }
        }
        struct Notify(mpsc::Sender<()>);
        impl Wake for Notify {
            fn wake(self: Arc<Self>) {
                let _ = self.0.send(());
            }
        }

        let (entered, observed) = mpsc::channel();
        let (release, hold) = mpsc::channel();
        let blocker = Arc::new(HoldReactor {
            entered,
            release: Mutex::new(hold),
        })
        .into();
        let mut timer = std::pin::pin!(smol::Timer::after(Duration::from_millis(40)));
        assert!(timer
            .as_mut()
            .poll(&mut Context::from_waker(&blocker))
            .is_pending());
        observed.recv_timeout(Duration::from_secs(1)).unwrap();

        let (notified, notification) = mpsc::channel();
        let waker = Arc::new(Notify(notified)).into();
        let deadline = Instant::now() + Duration::from_millis(20);
        let mut shared = std::pin::pin!(smol::Timer::at(deadline));
        assert!(shared
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        let mut native = CycleTimer::new(true).unwrap();
        smol::block_on(native.wait(deadline)).unwrap();
        assert!(Instant::now() >= deadline);
        assert!(matches!(
            notification.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release.send(()).unwrap();
        notification.recv_timeout(Duration::from_secs(1)).unwrap();
    }
}
