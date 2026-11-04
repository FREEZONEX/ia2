//! Deadline wait for the dedicated synchronized EtherCAT worker.

use std::{env, io, time::Instant};

const RT_PRIORITY_ENV: &str = "IA2_EC_SYNC_RT_PRIORITY";

pub(crate) fn configured_rt_priority() -> io::Result<Option<i32>> {
    match env::var(RT_PRIORITY_ENV) {
        Ok(value) => parse_rt_priority(&value).map(Some),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(io::Error::new(io::ErrorKind::InvalidInput, error)),
    }
}

fn parse_rt_priority(value: &str) -> io::Result<i32> {
    if !cfg!(target_os = "linux") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{RT_PRIORITY_ENV} requires Linux"),
        ));
    }
    value
        .parse::<i32>()
        .ok()
        .filter(|p| (1..=20).contains(p))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{RT_PRIORITY_ENV} must be 1..=20"),
            )
        })
}

#[cfg(target_os = "linux")]
fn set_round_robin(priority: i32) -> io::Result<()> {
    let params = libc::sched_param {
        sched_priority: priority,
    };
    // SAFETY: pid 0 selects only the current thread. params is initialized
    // and is read by the kernel during this call.
    if unsafe { libc::sched_setscheduler(0, libc::SCHED_RR, &params) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) struct CycleTimer {
    native: bool,
}

impl CycleTimer {
    pub(crate) fn new(synchronized: bool, rt_priority: Option<i32>) -> io::Result<Self> {
        let native = synchronized && cfg!(target_os = "linux");
        #[cfg(target_os = "linux")]
        if native {
            // SAFETY: PR_SET_TIMERSLACK takes an unsigned long in nanoseconds
            // and changes only the calling thread's timer coalescing.
            if unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, 1 as libc::c_ulong, 0, 0, 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some(priority) = rt_priority {
                set_round_robin(priority)?;
            }
        }
        #[cfg(not(target_os = "linux"))]
        debug_assert!(rt_priority.is_none());
        Ok(Self { native })
    }

    pub(crate) fn uses_native_clock(&self) -> bool {
        self.native
    }

    pub(crate) async fn wait(&mut self, deadline: Instant) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        if self.native {
            // The PDO exchange has completed before this wait. Mailbox
            // work and the pump can run when the next exchange yields.
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
                let mut timer = CycleTimer::new(synchronized, None).unwrap();
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
        let mut native = CycleTimer::new(true, None).unwrap();
        smol::block_on(native.wait(deadline)).unwrap();
        assert!(Instant::now() >= deadline);
        assert!(matches!(
            notification.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release.send(()).unwrap();
        notification.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn real_time_priority_is_explicit_bounded_and_linux_only() {
        assert_eq!(
            parse_rt_priority("10").ok(),
            cfg!(target_os = "linux").then_some(10)
        );
        for invalid in ["", "0", "-1", "21", "99", "bogus"] {
            assert!(parse_rt_priority(invalid).is_err());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn local_socket_pump_can_complete_exchange_between_native_waits() {
        use smol::io::{AsyncReadExt, AsyncWriteExt};
        use std::os::unix::net::UnixStream;

        let executor = smol::LocalExecutor::new();
        let (client, mut peer) = UnixStream::pair().unwrap();
        let mut client = smol::Async::new(client).unwrap();
        let (request, outgoing) = smol::channel::bounded::<u8>(1);
        let (incoming, response) = smol::channel::bounded::<u8>(1);
        let responder = std::thread::spawn(move || {
            use std::io::{Read, Write};
            peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            for _ in 0..16 {
                let mut byte = [0];
                peer.read_exact(&mut byte).unwrap();
                peer.write_all(&[byte[0] + 1]).unwrap();
            }
        });
        let owner = std::thread::current().id();
        let pump = executor.spawn(async move {
            while let Ok(byte) = outgoing.recv().await {
                assert_eq!(std::thread::current().id(), owner);
                client.write_all(&[byte]).await.unwrap();
                let mut reply = [0];
                client.read_exact(&mut reply).await.unwrap();
                incoming.send(reply[0]).await.unwrap();
            }
        });
        smol::block_on(executor.run(async {
            let mut timer = CycleTimer::new(true, None).unwrap();
            for sequence in 0..16 {
                timer
                    .wait(Instant::now() + Duration::from_millis(2))
                    .await
                    .unwrap();
                request.send(sequence).await.unwrap();
                let reply = smol::future::race(async { response.recv().await.ok() }, async {
                    smol::Timer::after(Duration::from_secs(1)).await;
                    None
                })
                .await;
                assert_eq!(reply, Some(sequence + 1));
            }
            drop(request);
            pump.await;
        }));
        responder.join().unwrap();
    }
}
