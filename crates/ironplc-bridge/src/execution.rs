//! Bounded execution of IA2's single-instance VMs through IronPLC's public
//! instruction hook. Scheduling stays in runtime.rs. A hook pause is terminal:
//! the caller discards the interrupted VM and never publishes its outputs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ironplc_container::FunctionId;
use ironplc_vm::{DebugHook, FaultContext, HookAction, PauseReason, RoundOutcome, VmRunning};

/// Hard per-scan ceilings, independent of the five-overrun cadence watchdog.
/// A finite instruction budget also terminates a loop on hosts whose clock
/// resolution is coarse. Clock/Stop checks are amortized over 256 opcodes.
pub(crate) const MAX_SCAN_INSTRUCTIONS: u64 = 10_000_000;
pub(crate) const MAX_SCAN_TIME: Duration = Duration::from_secs(1);
/// Stop is a request for the next scan boundary: a healthy program finishes
/// the scan in flight, so its outputs and final RETAIN checkpoint stay
/// consistent. Only a scan still running this long after Stop was first
/// sampled is treated as hung and discarded.
pub(crate) const STOP_GRACE: Duration = Duration::from_millis(250);
const CHECK_INTERVAL: u64 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanOutcome {
    /// The scan ran to its end; Stop, if pending, is handled at the boundary.
    Completed,
    /// Stop outlasted `STOP_GRACE` inside a scan: the VM state is partial.
    Stopped,
    BudgetExceeded(&'static str),
}

struct ScanGuard<'a> {
    stop: &'a AtomicBool,
    deadline: Instant,
    stop_seen: Option<Instant>,
    stop_grace: Duration,
    instructions: u64,
    limit: u64,
    outcome: ScanOutcome,
}

impl ScanGuard<'_> {
    fn check_clock_and_stop(&mut self) {
        if self.stop.load(Ordering::Relaxed) && self.stop_grace_expired() {
            self.outcome = ScanOutcome::Stopped;
        } else {
            self.check_deadline();
        }
    }

    fn stop_grace_expired(&mut self) -> bool {
        let seen = *self.stop_seen.get_or_insert_with(Instant::now);
        seen.elapsed() >= self.stop_grace
    }

    fn check_deadline(&mut self) {
        if self.outcome == ScanOutcome::Completed && Instant::now() >= self.deadline {
            self.outcome = ScanOutcome::BudgetExceeded("time limit");
        }
    }
}

impl DebugHook for ScanGuard<'_> {
    #[inline]
    fn before_instruction(&mut self, _function: FunctionId, _pc: usize, _op: u8) -> HookAction {
        if self.instructions.is_multiple_of(CHECK_INTERVAL) {
            self.check_clock_and_stop();
        }
        if self.outcome == ScanOutcome::Completed && self.instructions >= self.limit {
            self.outcome = ScanOutcome::BudgetExceeded("instruction limit");
        }
        if self.outcome != ScanOutcome::Completed {
            // IronPLC exposes a pause, not an abort, on this hook. The bridge
            // treats it as terminal; it is never a user-visible debug step.
            return HookAction::Pause(PauseReason::Step);
        }
        self.instructions += 1;
        HookAction::Continue
    }
}

pub(crate) fn run_scan(
    vm: &mut VmRunning<'_>,
    uptime_us: u64,
    stop: &AtomicBool,
    watchdog_us: u64,
) -> Result<ScanOutcome, FaultContext> {
    let time_limit = if watchdog_us == 0 {
        MAX_SCAN_TIME
    } else {
        Duration::from_micros(watchdog_us).min(MAX_SCAN_TIME)
    };
    let mut guard = ScanGuard {
        stop,
        deadline: Instant::now() + time_limit,
        stop_seen: None,
        stop_grace: STOP_GRACE,
        instructions: 0,
        limit: MAX_SCAN_INSTRUCTIONS,
        outcome: ScanOutcome::Completed,
    };
    let result = vm.run_round_debug(uptime_us, &mut guard)?;
    if matches!(result, RoundOutcome::Completed) {
        // Also cover a slow final opcode / a scan shorter than the sampling
        // interval. Only the clock: a completed scan is consistent, so a
        // pending Stop is honoured by the caller at this boundary instead of
        // discarding the scan (and with it the final RETAIN checkpoint).
        guard.check_deadline();
    }
    Ok(guard.outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironplc_vm::{Vm, VmBuffers};

    #[test]
    fn nested_infinite_loop_hits_the_shared_instruction_budget() {
        let c = crate::compile(
            "FUNCTION spin : DINT VAR x : DINT; END_VAR
             WHILE TRUE DO x := x + 1; END_WHILE; spin := x; END_FUNCTION
             PROGRAM main VAR y : DINT; END_VAR y := spin(); END_PROGRAM",
        )
        .unwrap();
        let mut bufs = VmBuffers::from_container(&c);
        let mut vm = Vm::new().load(&c, &mut bufs).unwrap().start().unwrap();
        let stop = AtomicBool::new(false);
        let mut guard = ScanGuard {
            stop: &stop,
            deadline: Instant::now() + Duration::from_secs(5),
            stop_seen: None,
            stop_grace: Duration::ZERO,
            instructions: 0,
            limit: 1000,
            outcome: ScanOutcome::Completed,
        };
        assert!(matches!(
            vm.run_round_debug(0, &mut guard).unwrap(),
            RoundOutcome::Paused(_)
        ));
        assert_eq!(
            guard.outcome,
            ScanOutcome::BudgetExceeded("instruction limit")
        );
        assert_eq!(guard.instructions, 1000);
        assert!(vm.debug_frames().len() > 1, "interrupted inside the callee");
    }

    #[test]
    fn stop_is_observed_inside_an_in_flight_scan() {
        let stop = AtomicBool::new(false);
        let mut guard = ScanGuard {
            stop: &stop,
            deadline: Instant::now() + Duration::from_secs(5),
            stop_seen: None,
            stop_grace: Duration::ZERO,
            instructions: 0,
            limit: MAX_SCAN_INSTRUCTIONS,
            outcome: ScanOutcome::Completed,
        };
        assert_eq!(
            guard.before_instruction(FunctionId::SCAN, 0, 0),
            HookAction::Continue
        );
        stop.store(true, Ordering::Relaxed);
        for _ in 0..CHECK_INTERVAL {
            if matches!(
                guard.before_instruction(FunctionId::SCAN, 0, 0),
                HookAction::Pause(_)
            ) {
                assert_eq!(guard.outcome, ScanOutcome::Stopped);
                return;
            }
        }
        panic!("Stop must interrupt within 256 instructions");
    }

    #[test]
    fn expired_time_budget_stops_before_executing_an_instruction() {
        let stop = AtomicBool::new(false);
        let mut guard = ScanGuard {
            stop: &stop,
            deadline: Instant::now(),
            stop_seen: None,
            stop_grace: Duration::ZERO,
            instructions: 0,
            limit: MAX_SCAN_INSTRUCTIONS,
            outcome: ScanOutcome::Completed,
        };
        assert!(matches!(
            guard.before_instruction(FunctionId::SCAN, 0, 0),
            HookAction::Pause(_)
        ));
        assert_eq!(guard.outcome, ScanOutcome::BudgetExceeded("time limit"));
        assert_eq!(guard.instructions, 0);
    }

    #[test]
    fn stop_is_deferred_until_its_grace_has_elapsed() {
        let stop = AtomicBool::new(true);
        let grace = Duration::from_millis(40);
        let mut guard = ScanGuard {
            stop: &stop,
            deadline: Instant::now() + Duration::from_secs(30),
            stop_seen: None,
            stop_grace: grace,
            instructions: 0,
            limit: MAX_SCAN_INSTRUCTIONS,
            outcome: ScanOutcome::Completed,
        };
        // The first sample sees Stop but a healthy scan still gets its grace.
        assert_eq!(
            guard.before_instruction(FunctionId::SCAN, 0, 0),
            HookAction::Continue
        );
        let seen = guard.stop_seen.expect("first sighting is recorded");
        let give_up = Instant::now() + Duration::from_secs(10);
        loop {
            if matches!(
                guard.before_instruction(FunctionId::SCAN, 0, 0),
                HookAction::Pause(_)
            ) {
                break;
            }
            assert!(Instant::now() < give_up, "Stop must end a hung scan");
        }
        assert_eq!(guard.outcome, ScanOutcome::Stopped);
        assert!(seen.elapsed() >= grace, "interrupted before the grace");
    }

    #[test]
    fn a_pending_stop_does_not_discard_a_scan_that_completes() {
        let c =
            crate::compile("PROGRAM main VAR x : DINT; END_VAR x := x + 1; END_PROGRAM").unwrap();
        let mut bufs = VmBuffers::from_container(&c);
        let mut vm = Vm::new().load(&c, &mut bufs).unwrap().start().unwrap();
        let stop = AtomicBool::new(true);
        assert_eq!(
            run_scan(&mut vm, 0, &stop, 0).unwrap(),
            ScanOutcome::Completed,
            "the caller honours Stop at the boundary; the finished scan is consistent"
        );
    }
}
