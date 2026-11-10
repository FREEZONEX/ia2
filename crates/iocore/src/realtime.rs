//! Explicit Linux scheduling for dedicated cyclic threads, never the process.

use std::{env, io};

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
