//! Explicit Linux scheduling for dedicated cyclic threads, never the process.

use std::{env, io};

/// An opt-in Linux PM QoS request. The kernel releases this request when its
/// file is closed, including worker teardown, initialization failure and exit.
/// The aggregate constraint is system-wide while this handle is alive.
#[derive(Debug)]
pub struct CpuIdleLatency {
    #[cfg(target_os = "linux")]
    _request: std::fs::File,
    effective_us: i32,
}

impl CpuIdleLatency {
    pub fn from_environment(name: &str) -> io::Result<Option<Self>> {
        match env::var(name) {
            Ok(value) => Self::request(parse_idle_latency(name, &value)?).map(Some),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(error) => Err(io::Error::new(io::ErrorKind::InvalidInput, error)),
        }
    }

    pub fn request(max_us: i32) -> io::Result<Self> {
        if !(0..=1_000_000).contains(&max_us) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CPU idle latency must be 0..=1000000 microseconds",
            ));
        }
        #[cfg(target_os = "linux")]
        {
            let mut request = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/cpu_dma_latency")?;
            let effective_us = install_idle_latency(&mut request, max_us)?;
            Ok(Self {
                _request: request,
                effective_us,
            })
        }
        #[cfg(not(target_os = "linux"))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "CPU idle latency requests require Linux",
        ))
    }

    pub fn effective_us(&self) -> i32 {
        self.effective_us
    }
}

pub fn parse_idle_latency(name: &str, value: &str) -> io::Result<i32> {
    value
        .parse::<i32>()
        .ok()
        .filter(|n| (0..=1_000_000).contains(n))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} must be 0..=1000000 microseconds"),
            )
        })
}

#[cfg(any(target_os = "linux", test))]
fn install_idle_latency(device: &mut (impl io::Read + io::Write), max_us: i32) -> io::Result<i32> {
    // This ABI takes a native-endian signed 32-bit integer. The write does
    // not advance the device offset; read returns the aggregate system limit.
    device.write_all(&max_us.to_ne_bytes())?;
    let mut bytes = [0; 4];
    device.read_exact(&mut bytes)?;
    let actual = i32::from_ne_bytes(bytes);
    if actual < 0 || actual > max_us {
        return Err(io::Error::other(format!(
            "CPU idle latency readback {actual} us exceeds requested {max_us} us"
        )));
    }
    Ok(actual)
}

pub fn configured_priority(name: &str) -> io::Result<Option<i32>> {
    match env::var(name) {
        Ok(value) => parse_priority(name, &value).map(Some),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(io::Error::new(io::ErrorKind::InvalidInput, error)),
    }
}

pub fn parse_priority(name: &str, value: &str) -> io::Result<i32> {
    if !cfg!(target_os = "linux") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{name} requires Linux"),
        ));
    }
    value
        .parse::<i32>()
        .ok()
        .filter(|p| (1..=20).contains(p))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} must be 1..=20"),
            )
        })
}

/// Apply and read back SCHED_RR on the calling thread. Call only after spawning
/// auxiliary workers: newly created native threads can inherit this policy.
/// An explicit request must fail closed if permissions or readback fail.
pub fn set_current_thread_round_robin(priority: i32) -> io::Result<()> {
    if !(1..=20).contains(&priority) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "real-time priority must be 1..=20",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        let mut params = libc::sched_param {
            sched_priority: priority,
        };
        // SAFETY: pid 0 is the calling thread, and params is initialized storage.
        if unsafe { libc::sched_setscheduler(0, libc::SCHED_RR, &params) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: getscheduler has no pointer; getparam writes initialized storage.
        let policy = unsafe { libc::sched_getscheduler(0) };
        if policy < 0 || unsafe { libc::sched_getparam(0, &mut params) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if policy != libc::SCHED_RR || params.sched_priority != priority {
            return Err(io::Error::other("round-robin scheduling readback mismatch"));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "round-robin scheduling requires Linux",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_latency_request_validates_units_and_kernel_readback() {
        struct Device {
            request: Vec<u8>,
            reply: io::Cursor<Vec<u8>>,
        }
        impl io::Write for Device {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.request.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl io::Read for Device {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.reply.read(bytes)
            }
        }
        for actual in [-1i32, 0, 20, 50, 51] {
            let mut device = Device {
                request: Vec::new(),
                reply: io::Cursor::new(actual.to_ne_bytes().to_vec()),
            };
            assert_eq!(
                install_idle_latency(&mut device, 50).is_ok(),
                (0..=50).contains(&actual)
            );
            assert_eq!(device.request, 50i32.to_ne_bytes());
        }
        let mut short = Device {
            request: Vec::new(),
            reply: io::Cursor::new(vec![0, 0]),
        };
        assert_eq!(
            install_idle_latency(&mut short, 50).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        for value in ["0", "50", "1000000"] {
            assert!(parse_idle_latency("LATENCY", value).is_ok());
        }
        for value in ["", "-1", "1000001", "50us", "1.5", "bogus"] {
            assert!(parse_idle_latency("LATENCY", value).is_err());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "explicit host qualification only: changes CPU idle QoS for this test's lifetime"]
    fn cpu_idle_latency_kernel_request_is_released_on_drop() {
        use io::Read;
        fn current() -> i32 {
            let mut file = std::fs::File::open("/dev/cpu_dma_latency").unwrap();
            let mut bytes = [0; 4];
            file.read_exact(&mut bytes).unwrap();
            i32::from_ne_bytes(bytes)
        }
        let before = current();
        let request = CpuIdleLatency::request(50).unwrap();
        assert_eq!(request.effective_us(), before.min(50));
        assert_eq!(current(), before.min(50));
        drop(request);
        assert_eq!(current(), before, "request leaked after its owner dropped");
    }

    #[test]
    fn priority_is_explicit_bounded_and_linux_only() {
        assert_eq!(
            parse_priority("TEST_RT", "9").ok(),
            cfg!(target_os = "linux").then_some(9)
        );
        for value in ["", "0", "-1", "21", "99", "9.0", "bogus"] {
            assert!(parse_priority("TEST_RT", value).is_err());
        }
        for value in [0, -1, 21, 99] {
            assert_eq!(
                set_current_thread_round_robin(value).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn policy_is_thread_local_and_permission_failure_is_explicit() {
        fn policy() -> (i32, i32) {
            let mut p = libc::sched_param { sched_priority: 0 };
            // SAFETY: current-thread query with initialized writable storage.
            let kind = unsafe { libc::sched_getscheduler(0) };
            assert_eq!(unsafe { libc::sched_getparam(0, &mut p) }, 0);
            (kind, p.sched_priority)
        }
        let parent = policy();
        std::thread::spawn(move || {
            let before = policy();
            match set_current_thread_round_robin(9) {
                Ok(()) => {
                    assert_eq!(policy(), (libc::SCHED_RR, 9));
                    eprintln!("child SCHED_RR 9 readback verified");
                }
                Err(error) => {
                    assert!(
                        matches!(error.raw_os_error(), Some(libc::EPERM | libc::EACCES)),
                        "{error}"
                    );
                    assert_eq!(policy(), before);
                    assert!(
                        env::var_os("IA2_REQUIRE_RT_TEST").is_none(),
                        "real-time permission required by test: {error}"
                    );
                    eprintln!("permission denial reported; no fallback reported as success");
                }
            }
        })
        .join()
        .unwrap();
        assert_eq!(policy(), parent, "parent thread policy changed");
    }
}
