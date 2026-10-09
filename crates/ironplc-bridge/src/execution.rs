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

/// Which per-scan ceiling ended a scan, with its value, so the fault can say
/// what was exceeded instead of leaving the reader to look the number up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Budget {
    Instructions(u64),
    Time(Duration),
}

impl Budget {
    pub(crate) fn describe(self) -> String {
        match self {
            Budget::Instructions(n) => format!("instruction limit ({n} opcodes per scan)"),
            Budget::Time(d) if d >= Duration::from_millis(1) => {
                format!("time limit ({} ms per scan)", d.as_millis())
            }
            Budget::Time(d) => format!("time limit ({} us per scan)", d.as_micros()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanOutcome {
    /// The scan ran to its end; Stop, if pending, is handled at the boundary.
    Completed,
    /// Stop outlasted `STOP_GRACE` inside a scan: the VM state is partial.
    Stopped,
    BudgetExceeded(Budget),
}

struct ScanGuard<'a> {
    stop: &'a AtomicBool,
    deadline: Instant,
    time_limit: Duration,
    stop_seen: Option<Instant>,
    stop_grace: Duration,
    /// Opcodes executed before the current window began.
    instructions: u64,
    /// Opcodes the current window admits, and how many of them are left. The
    /// per-opcode work is one decrement; the clock, Stop and the instruction
    /// ceiling are looked at when a window runs out.
    window_len: u64,
    window_left: u64,
    limit: u64,
    outcome: ScanOutcome,
}

impl<'a> ScanGuard<'a> {
    fn new(stop: &'a AtomicBool, time_limit: Duration, stop_grace: Duration, limit: u64) -> Self {
        Self {
            stop,
            deadline: Instant::now() + time_limit,
            time_limit,
            stop_seen: None,
            stop_grace,
            instructions: 0,
            // An empty first window: the first opcode is a checkpoint, so
            // Stop and the clock are looked at before anything executes.
            window_len: 0,
            window_left: 0,
            limit,
            outcome: ScanOutcome::Completed,
        }
    }

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
            self.outcome = ScanOutcome::BudgetExceeded(Budget::Time(self.time_limit));
        }
    }

    /// The window just ran out, i.e. every opcode it admitted has executed.
    /// Account for them, look at Stop, the clock and the instruction ceiling,
    /// and open the next window, or end the scan.
    #[inline(never)]
    fn checkpoint(&mut self) -> HookAction {
        self.instructions += self.window_len;
        self.check_clock_and_stop();
        if self.outcome == ScanOutcome::Completed && self.instructions >= self.limit {
            self.outcome = ScanOutcome::BudgetExceeded(Budget::Instructions(self.limit));
        }
        if self.outcome != ScanOutcome::Completed {
            // IronPLC exposes a pause, not an abort, on this hook. The bridge
            // treats it as terminal; it is never a user-visible debug step.
            return HookAction::Pause(PauseReason::Step);
        }
        // This call's opcode opens the next window: CHECK_INTERVAL opcodes,
        // fewer when the ceiling falls inside it, so the ceiling is exact.
        self.window_len = (self.limit - self.instructions).min(CHECK_INTERVAL);
        self.window_left = self.window_len - 1;
        HookAction::Continue
    }
}

impl DebugHook for ScanGuard<'_> {
    #[inline]
    fn before_instruction(&mut self, _function: FunctionId, _pc: usize, _op: u8) -> HookAction {
        if self.window_left > 0 {
            self.window_left -= 1;
            return HookAction::Continue;
        }
        self.checkpoint()
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
    let mut guard = ScanGuard::new(stop, time_limit, STOP_GRACE, MAX_SCAN_INSTRUCTIONS);
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
        let mut guard = ScanGuard::new(&stop, Duration::from_secs(5), Duration::ZERO, 1000);
        assert!(matches!(
            vm.run_round_debug(0, &mut guard).unwrap(),
            RoundOutcome::Paused(_)
        ));
        assert_eq!(
            guard.outcome,
            ScanOutcome::BudgetExceeded(Budget::Instructions(1000))
        );
        assert_eq!(guard.instructions, 1000);
        assert!(vm.debug_frames().len() > 1, "interrupted inside the callee");
    }

    #[test]
    fn stop_is_observed_inside_an_in_flight_scan() {
        let stop = AtomicBool::new(false);
        let mut guard = ScanGuard::new(
            &stop,
            Duration::from_secs(5),
            Duration::ZERO,
            MAX_SCAN_INSTRUCTIONS,
        );
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
        let mut guard =
            ScanGuard::new(&stop, Duration::ZERO, Duration::ZERO, MAX_SCAN_INSTRUCTIONS);
        assert!(matches!(
            guard.before_instruction(FunctionId::SCAN, 0, 0),
            HookAction::Pause(_)
        ));
        assert_eq!(
            guard.outcome,
            ScanOutcome::BudgetExceeded(Budget::Time(Duration::ZERO))
        );
        assert_eq!(guard.instructions, 0);
    }

    #[test]
    fn stop_is_deferred_until_its_grace_has_elapsed() {
        let stop = AtomicBool::new(true);
        let grace = Duration::from_millis(40);
        let mut guard =
            ScanGuard::new(&stop, Duration::from_secs(30), grace, MAX_SCAN_INSTRUCTIONS);
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

    /// Counts opcodes and never pauses.
    struct CountOpcodes(u64);

    impl DebugHook for CountOpcodes {
        fn before_instruction(&mut self, _: FunctionId, _: usize, _: u8) -> HookAction {
            self.0 += 1;
            HookAction::Continue
        }
    }

    /// One scan of `source` under a guard with the given opcode ceiling and
    /// no time or Stop pressure: how the round ended, the guard's verdict,
    /// and how many opcodes it had counted.
    fn guarded_scan(source: &str, limit: u64) -> (RoundOutcome, ScanOutcome, u64) {
        let c = crate::compile(source).unwrap();
        let mut bufs = VmBuffers::from_container(&c);
        let mut vm = Vm::new().load(&c, &mut bufs).unwrap().start().unwrap();
        let stop = AtomicBool::new(false);
        let mut guard = ScanGuard::new(&stop, Duration::from_secs(30), Duration::ZERO, limit);
        let round = vm.run_round_debug(0, &mut guard).unwrap();
        (round, guard.outcome, guard.instructions)
    }

    const SPIN: &str =
        "PROGRAM main VAR x : DINT; END_VAR WHILE TRUE DO x := x + 1; END_WHILE; END_PROGRAM";

    #[test]
    fn the_instruction_ceiling_admits_exactly_that_many_opcodes() {
        let source = "PROGRAM main VAR i : DINT; a : DINT; END_VAR
                      FOR i := 1 TO 300 DO a := a + i; END_FOR; END_PROGRAM";
        let c = crate::compile(source).unwrap();
        let mut bufs = VmBuffers::from_container(&c);
        let mut vm = Vm::new().load(&c, &mut bufs).unwrap().start().unwrap();
        let mut count = CountOpcodes(0);
        vm.run_round_debug(0, &mut count).unwrap();
        let total = count.0;
        assert!(
            total > 3 * CHECK_INTERVAL && !total.is_multiple_of(CHECK_INTERVAL),
            "the scan ({total} opcodes) must end strictly inside a window"
        );

        for limit in [total, total + 1, total + CHECK_INTERVAL] {
            let (round, outcome, _) = guarded_scan(source, limit);
            assert_eq!(round, RoundOutcome::Completed, "limit {limit}");
            assert_eq!(outcome, ScanOutcome::Completed, "limit {limit}");
        }
        let (round, outcome, executed) = guarded_scan(source, total - 1);
        assert!(matches!(round, RoundOutcome::Paused(_)));
        assert_eq!(
            outcome,
            ScanOutcome::BudgetExceeded(Budget::Instructions(total - 1))
        );
        assert_eq!(
            executed,
            total - 1,
            "stopped before the opcode over the line"
        );
    }

    #[test]
    fn the_instruction_ceiling_is_exact_at_window_edges() {
        for limit in [0, 1, 2, 255, 256, 257, 511, 512, 513, 1000] {
            let (round, outcome, executed) = guarded_scan(SPIN, limit);
            assert!(matches!(round, RoundOutcome::Paused(_)), "limit {limit}");
            assert_eq!(
                outcome,
                ScanOutcome::BudgetExceeded(Budget::Instructions(limit)),
                "limit {limit}"
            );
            assert_eq!(executed, limit, "limit {limit}");
        }
    }

    #[test]
    fn a_budget_fault_names_the_ceiling_and_its_value() {
        assert_eq!(
            Budget::Instructions(MAX_SCAN_INSTRUCTIONS).describe(),
            "instruction limit (10000000 opcodes per scan)"
        );
        assert_eq!(
            Budget::Time(MAX_SCAN_TIME).describe(),
            "time limit (1000 ms per scan)"
        );
        // A short explicit container watchdog is what the reader must see.
        assert_eq!(
            Budget::Time(Duration::from_millis(20)).describe(),
            "time limit (20 ms per scan)"
        );
        assert_eq!(
            Budget::Time(Duration::from_micros(500)).describe(),
            "time limit (500 us per scan)"
        );
    }
}
