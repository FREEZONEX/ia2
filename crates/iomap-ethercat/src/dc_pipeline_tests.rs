//! Deterministic regression coverage for the synchronized DC handoff.
//!
//! Exercise the production gate and continuity hook in worker order: select
//! outputs, observe the returned DC sample, publish inputs, begin, commit.
//! Logical times model scheduling and exchange delays; these tests do not
//! establish physical send time or bound the real worker's response latency.
use super::{observe_dc_exchange, DcContinuity};
use crate::cycle::{Gate, Ticket};
use std::{
    future::Future,
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

type Frame = [u64; 2];
const PERIOD: Duration = Duration::from_millis(2);
const NS: Duration = Duration::from_nanos(1);

fn arm(clock: &Gate<Frame>) {
    let mut scan = Box::pin(clock.begin());
    assert!(scan
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
        .is_pending());
}

fn begin(clock: &Gate<Frame>, expected: Frame) -> Ticket {
    let mut scan = Box::pin(clock.begin());
    let Poll::Ready(Ok((ticket, image))) =
        scan.as_mut().poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("expected a fresh ticket: {:?}", clock.status());
    };
    assert_eq!(image, expected);
    ticket
}

fn assert_begin_refused(clock: &Gate<Frame>) {
    let mut scan = Box::pin(clock.begin());
    assert!(matches!(
        scan.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Err(_))
    ));
}

// Later healthy responses must neither replace the last healthy reference nor
// issue another ticket, replay a frame, or replace the first fault. Cover more
// than the trace capacity so repeated observations cannot flood the event ring.
fn assert_latched(clock: &Gate<Frame>, continuity: &mut Option<DcContinuity>, now: Instant) {
    let before = clock.status();
    assert!(before.fault.is_some());
    let previous = continuity.as_ref().unwrap();
    let (dc_ns, deadline, limit) = (previous.last_dc_ns, previous.deadline, previous.send_before);
    for n in 1..=64 {
        let next = now + PERIOD * n;
        clock.consume_before_at(next, limit, |frame| assert!(frame.is_none()));
        // Successive healthy slots where representable; a wrapped clock stays
        // invalid and must also leave the original latch untouched.
        observe_dc_exchange(
            clock,
            continuity,
            next,
            dc_ns.checked_add(2_000_000 * u64::from(n)).unwrap_or(0),
            next + PERIOD,
        );
        clock.publish_at([99, 198], next + PERIOD, next);
        assert_begin_refused(clock);
    }
    let after = clock.status();
    assert_eq!(after.fault, before.fault);
    assert_eq!(after.issued, before.issued);
    assert_eq!(after.committed, before.committed);
    assert_eq!(after.consumed, before.consumed);
    assert_eq!(after.events.len(), before.events.len());
    assert_eq!(after.events_dropped, before.events_dropped);
    let retained = continuity.as_ref().unwrap();
    assert_eq!(retained.last_dc_ns, dc_ns);
    assert_eq!(retained.deadline, deadline);
    assert_eq!(retained.send_before, limit);
}

#[test]
fn adjacent_slots_with_jitter_complete_ten_thousand_full_handoffs() {
    let clock = Gate::new(PERIOD);
    arm(&clock);
    let origin = Instant::now() + PERIOD;
    let mut continuity = Some(DcContinuity::new(
        origin,
        origin - PERIOD,
        27_000_000,
        PERIOD,
    ));
    for n in 0..=10_000 {
        let phase = [800_000, 1_271_209, 950_000, 1_100_000][n as usize % 4];
        let now = origin + Duration::from_nanos(n * 2_000_000 + phase) - Duration::from_millis(1);
        let dc_ns = (14 + n) * 2_000_000 + phase;
        let limit = continuity.as_ref().unwrap().send_before;
        clock.consume_before_at(now, limit, |frame| {
            assert_eq!(frame.copied(), n.checked_sub(1).map(|p| [p, p * 2]));
        });
        // Response/input-copy time is deliberately distinct from selection.
        let returned = now + Duration::from_micros(50);
        let next = now + Duration::from_nanos(3_000_000 - phase);
        observe_dc_exchange(&clock, &mut continuity, now, dc_ns, next);
        assert_eq!(continuity.as_ref().unwrap().last_dc_ns, dc_ns);
        if n < 10_000 {
            clock.publish_at([n, n + 1], next, returned);
            let ticket = begin(&clock, [n, n + 1]);
            clock
                .commit_at(ticket, [n, n * 2], returned + Duration::from_micros(50))
                .unwrap();
        }
        assert!(
            clock.status().fault.is_none(),
            "cycle {n}: {:?}",
            clock.status()
        );
    }
    let status = clock.status();
    assert_eq!(
        (status.issued, status.committed, status.consumed),
        (10_000, 10_000, 10_000)
    );
}

#[test]
fn phase_window_boundaries_select_or_zero_before_observing_the_response() {
    // Startup may have no ticket, whether or not the PLC has called begin.
    // In steady state a committed nonzero frame must be discarded at expiry.
    for armed in [false, true] {
        for pending in [false, true]
            .into_iter()
            .filter(|pending| armed || !*pending)
        {
            for phase in [0, 33_928, 800_000, 1_271_209, 1_999_999] {
                for offset in [-1_i64, 0, 1] {
                    let clock = Gate::new(PERIOD);
                    if armed {
                        arm(&clock);
                    }
                    let origin = Instant::now();
                    let mut continuity = Some(DcContinuity::new(
                        origin + PERIOD,
                        origin,
                        28_000_000 + phase,
                        PERIOD,
                    ));
                    let ticket = pending.then(|| {
                        clock.publish_at([1, 2], origin + PERIOD, origin);
                        let ticket = begin(&clock, [1, 2]);
                        clock
                            .commit_at(ticket, [7, 14], origin + Duration::from_micros(100))
                            .unwrap();
                        ticket
                    });
                    // Oracle uses the known slot's end, not the implementation's
                    // computed send_before, to detect loss of phase correction.
                    let selected =
                        origin + Duration::from_nanos(((4_000_000 - phase) as i64 + offset) as u64);
                    let limit = continuity.as_ref().unwrap().send_before;
                    let expired = offset >= 0;
                    clock.consume_before_at(selected, limit, |frame| {
                        assert_eq!(frame.copied(), (pending && !expired).then_some([7, 14]));
                    });
                    assert_eq!(
                        clock.is_faulted(),
                        expired,
                        "phase={phase}, offset={offset}, armed={armed}, pending={pending}"
                    );
                    let returned_dc = if expired { 32_000_000 } else { 31_999_999 };
                    observe_dc_exchange(
                        &clock,
                        &mut continuity,
                        selected,
                        returned_dc,
                        selected + PERIOD,
                    );
                    if expired {
                        let status = clock.status();
                        assert_eq!(
                            status.fault.as_deref(),
                            Some("DC slot send window expired; restart required")
                        );
                        assert_eq!(status.consumed, 0);
                        assert_eq!(
                            status
                                .events
                                .iter()
                                .filter(|e| e.kind == "dc_send_expired")
                                .count(),
                            1
                        );
                        if let Some(ticket) = ticket {
                            assert!(clock.commit_at(ticket, [8, 16], selected).is_err());
                        }
                        assert_latched(&clock, &mut continuity, selected);
                    } else {
                        assert!(!clock.is_faulted(), "{:?}", clock.status());
                        assert_eq!(clock.status().consumed, u64::from(pending));
                        clock.publish_at([3, 6], selected + PERIOD, selected);
                        assert_eq!(clock.status().issued, u64::from(pending) + u64::from(armed));
                        if armed {
                            begin(&clock, [3, 6]);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn delay_after_output_selection_latches_before_the_next_ticket() {
    let clock = Gate::new(PERIOD);
    arm(&clock);
    let origin = Instant::now();
    let mut continuity = Some(DcContinuity::new(
        origin + PERIOD,
        origin,
        29_000_000,
        PERIOD,
    ));
    clock.publish_at([1, 2], origin + PERIOD, origin);
    let ticket = begin(&clock, [1, 2]);
    clock
        .commit_at(ticket, [7, 14], origin + Duration::from_micros(100))
        .unwrap();
    let selected = origin + Duration::from_millis(3) - NS;
    clock.consume_before_at(
        selected,
        continuity.as_ref().unwrap().send_before,
        |frame| assert_eq!(frame, Some(&[7, 14])),
    );
    assert!(!clock.is_faulted());
    // The selected frame can already have reached the wire. Detection cannot
    // retract it: the response closes admission of the NEXT ticket/frame.
    let returned = selected + Duration::from_micros(35);
    observe_dc_exchange(
        &clock,
        &mut continuity,
        selected,
        32_033_928,
        selected + PERIOD,
    );
    assert!(clock.status().fault.unwrap().starts_with("skipped DC slot"));
    clock.publish_at([9, 18], selected + PERIOD, returned);
    assert!(clock.commit_at(ticket, [8, 16], returned).is_err());
    assert_eq!(clock.status().consumed, 1);
    assert_latched(&clock, &mut continuity, returned);
}

#[test]
fn invalid_dc_responses_cannot_publish_or_rearm_after_on_time_selection() {
    for (previous_dc, returned_dc, reason) in [
        (31_000_000, 0, "invalid zero DC time"),
        (31_000_000, 30_900_000, "DC time moved backwards"),
        (31_000_000, 31_000_000, "duplicate DC slot"),
        (31_000_000, 31_500_000, "duplicate DC slot"),
        (31_000_000, 34_033_928, "skipped DC slot"),
        (u64::MAX - 1000, 1000, "DC time moved backwards"),
    ] {
        let clock = Gate::new(PERIOD);
        arm(&clock);
        let origin = Instant::now();
        let mut continuity = Some(DcContinuity::new(
            origin + PERIOD,
            origin,
            previous_dc,
            PERIOD,
        ));
        clock.publish_at([1, 2], origin + PERIOD, origin);
        let ticket = begin(&clock, [1, 2]);
        clock
            .commit_at(ticket, [7, 14], origin + Duration::from_micros(100))
            .unwrap();
        let selected = origin + PERIOD;
        clock.consume_before_at(
            selected,
            continuity.as_ref().unwrap().send_before,
            |frame| assert_eq!(frame, Some(&[7, 14])),
        );
        observe_dc_exchange(
            &clock,
            &mut continuity,
            selected,
            returned_dc,
            selected + PERIOD,
        );
        let fault = clock.status().fault.unwrap();
        assert!(fault.starts_with(reason), "{fault}");
        assert!(fault.contains(&format!(
            "previous_dc_ns={previous_dc}, dc_ns={returned_dc}"
        )));
        clock.publish_at([9, 18], selected + PERIOD, selected);
        assert_latched(&clock, &mut continuity, selected);
    }
}

#[test]
fn send_expiry_preserves_the_first_fault_with_any_returned_dc_sample() {
    for returned_dc in [0, 28_900_000, 29_000_000, 31_000_000, 32_033_928] {
        let clock = Gate::new(PERIOD);
        arm(&clock);
        let origin = Instant::now();
        let mut continuity = Some(DcContinuity::new(
            origin + PERIOD,
            origin,
            29_000_000,
            PERIOD,
        ));
        clock.publish_at([1, 2], origin + PERIOD, origin);
        let ticket = begin(&clock, [1, 2]);
        clock
            .commit_at(ticket, [7, 14], origin + Duration::from_micros(100))
            .unwrap();
        let selected = origin + Duration::from_millis(3);
        clock.consume_before_at(
            selected,
            continuity.as_ref().unwrap().send_before,
            |frame| assert!(frame.is_none()),
        );
        observe_dc_exchange(
            &clock,
            &mut continuity,
            selected,
            returned_dc,
            selected + PERIOD,
        );
        assert_eq!(
            clock.status().fault.as_deref(),
            Some("DC slot send window expired; restart required")
        );
        assert_eq!(clock.status().consumed, 0);
        assert_latched(&clock, &mut continuity, selected);
    }
}

#[test]
fn host_deadline_still_applies_inside_the_phase_selection_window() {
    for late in [false, true] {
        let clock = Gate::new(PERIOD);
        arm(&clock);
        let origin = Instant::now();
        // A short next_cycle_wait can exhaust the host budget before the
        // phase-based slot limit. Both checks remain necessary.
        let deadline = origin + Duration::from_millis(1);
        let mut continuity = Some(DcContinuity::new(deadline, origin, 28_800_000, PERIOD));
        clock.publish_at([1, 2], deadline, origin);
        let ticket = begin(&clock, [1, 2]);
        clock
            .commit_at(ticket, [7, 14], origin + Duration::from_micros(100))
            .unwrap();
        let selected = deadline + PERIOD - if late { Duration::ZERO } else { NS };
        clock.consume_before_at(
            selected,
            continuity.as_ref().unwrap().send_before,
            |frame| assert_eq!(frame.copied(), (!late).then_some([7, 14])),
        );
        observe_dc_exchange(
            &clock,
            &mut continuity,
            selected,
            31_800_000,
            selected + PERIOD,
        );
        if late {
            assert_eq!(
                clock.status().fault.as_deref(),
                Some("missing output frame or missed bus cycle")
            );
            assert_latched(&clock, &mut continuity, selected);
        } else {
            assert!(!clock.is_faulted());
            clock.publish_at([9, 18], selected + PERIOD, selected);
            begin(&clock, [9, 18]);
        }
    }
}

#[test]
fn response_and_input_copy_must_finish_before_the_publish_deadline() {
    for expired in [false, true] {
        let clock = Gate::new(PERIOD);
        arm(&clock);
        let origin = Instant::now();
        let mut continuity = Some(DcContinuity::new(
            origin,
            origin - PERIOD,
            27_000_000,
            PERIOD,
        ));
        clock.consume_before_at(origin, continuity.as_ref().unwrap().send_before, |frame| {
            assert!(frame.is_none())
        });
        let next = origin + Duration::from_micros(1334);
        observe_dc_exchange(&clock, &mut continuity, origin, 29_000_000, next);
        let returned = next - if expired { Duration::ZERO } else { NS };
        clock.publish_at([1, 2], next, returned);
        if expired {
            assert_eq!(
                clock.status().fault.as_deref(),
                Some("cycle issue overlap or expired deadline")
            );
            assert_eq!(clock.status().issued, 0);
            assert_latched(&clock, &mut continuity, returned);
        } else {
            begin(&clock, [1, 2]);
            assert!(!clock.is_faulted());
        }
    }
}
