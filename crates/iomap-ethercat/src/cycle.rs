//! One outstanding ticket, one complete frame, one consumer. No queue catch-up.
//! Both real and simulated bus workers use this exact state machine.
use iocore::{
    cycle::{CycleEvent, CycleStatus},
    IoError,
};
use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};
use tokio::sync::watch;

const TRACE_CAPACITY: usize = 32;
/// First DC output-selection expiry, sampled after acquiring the gate lock.
/// Returned to the worker so diagnostics can separate wakeup from selection
/// delay without logging or doing file I/O while holding the handoff lock.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DcSelectionMiss {
    pub sequence: u64,
    pub checked_at: Instant,
    pub send_before: Instant,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ticket {
    pub sequence: u64,
    deadline: Instant,
}
struct State<T> {
    armed: bool,
    ticket: Option<(Ticket, T)>,
    pending: Option<T>,
    begun: u64,
    status: CycleStatus,
    events: VecDeque<CycleEvent>,
}
pub(crate) struct Gate<T> {
    pub period: Duration,
    origin: Instant,
    state: Mutex<State<T>>,
    wake: watch::Sender<u64>,
}
impl<T: Clone> Gate<T> {
    pub fn new(period: Duration) -> Self {
        Self {
            period,
            origin: Instant::now(),
            state: Mutex::new(State {
                armed: false,
                ticket: None,
                pending: None,
                begun: 0,
                status: CycleStatus {
                    period_us: period.as_micros() as u64,
                    ..Default::default()
                },
                events: VecDeque::with_capacity(TRACE_CAPACITY),
            }),
            wake: watch::channel(0).0,
        }
    }
    fn event(
        &self,
        s: &mut State<T>,
        kind: &str,
        sequence: u64,
        now: Instant,
        deadline: Option<Instant>,
    ) {
        if s.events.len() == TRACE_CAPACITY {
            s.events.pop_front();
            s.status.events_dropped += 1;
        }
        s.events.push_back(CycleEvent {
            sequence,
            kind: kind.into(),
            host_us: now.saturating_duration_since(self.origin).as_micros() as u64,
            deadline_us: deadline
                .map(|d| d.saturating_duration_since(self.origin).as_micros() as u64),
        });
    }
    fn fail(&self, s: &mut State<T>, reason: &str, now: Instant) {
        if s.status.fault.is_none() {
            s.status.fault = Some(reason.into());
            self.event(s, "fault", s.status.issued, now, None);
        }
        s.pending = None;
        self.wake.send_modify(|v| *v = v.wrapping_add(1));
    }
    pub fn trip(&self, reason: &str) {
        self.fail(
            &mut self.state.lock().expect("cycle gate poisoned"),
            reason,
            Instant::now(),
        );
    }
    pub fn is_faulted(&self) -> bool {
        self.state
            .lock()
            .expect("cycle gate poisoned")
            .status
            .fault
            .is_some()
    }
    pub fn status(&self) -> CycleStatus {
        let s = self.state.lock().expect("cycle gate poisoned");
        let mut status = s.status.clone();
        status.events = s.events.iter().cloned().collect();
        status
    }
    pub async fn begin(&self) -> Result<(Ticket, T), IoError> {
        let mut wake = self.wake.subscribe();
        loop {
            {
                let mut s = self.state.lock().expect("cycle gate poisoned");
                if let Some(why) = &s.status.fault {
                    return Err(IoError::Transport(why.clone()));
                }
                s.armed = true;
                if let Some((ticket, image)) = s.ticket.clone() {
                    if ticket.sequence > s.begun {
                        s.begun = ticket.sequence;
                        self.event(
                            &mut s,
                            "begin",
                            ticket.sequence,
                            Instant::now(),
                            Some(ticket.deadline),
                        );
                        return Ok((ticket, image));
                    }
                }
            }
            wake.changed()
                .await
                .map_err(|_| IoError::Transport("cycle clock closed".into()))?;
        }
    }
    pub fn commit(&self, ticket: Ticket, image: T) -> Result<(), IoError> {
        self.commit_with_clock(ticket, image, Instant::now)
    }
    fn commit_with_clock(
        &self,
        ticket: Ticket,
        image: T,
        now: impl FnOnce() -> Instant,
    ) -> Result<(), IoError> {
        let mut s = self.state.lock().expect("cycle gate poisoned");
        // Lock contention consumes the same deadline budget as scan work.
        let now = now();
        if let Some(why) = &s.status.fault {
            return Err(IoError::Transport(why.clone()));
        }
        let valid = s
            .ticket
            .as_ref()
            .is_some_and(|(t, _)| t.sequence == ticket.sequence)
            && s.begun == ticket.sequence
            && s.pending.is_none()
            && now < ticket.deadline;
        if !valid {
            self.fail(&mut s, "late, duplicate or stale cycle commit", now);
            return Err(IoError::Transport(
                "cycle commit refused; restart required".into(),
            ));
        }
        s.pending = Some(image);
        s.status.committed += 1;
        self.event(
            &mut s,
            "commit",
            ticket.sequence,
            now,
            Some(ticket.deadline),
        );
        Ok(())
    }
    /// Callback runs under the gate lock. Failsafe cannot race an accepted
    /// frame back onto the mirror. None means ZERO outputs, never hold-last.
    pub fn consume(&self, apply: impl FnOnce(Option<&T>)) {
        let _ = self.consume_before(None, apply);
    }
    /// The real DC worker additionally bounds selection by the predicted
    /// end of the next reference-clock slot. Check AFTER taking the lock.
    pub fn consume_before(
        &self,
        send_before: Option<Instant>,
        apply: impl FnOnce(Option<&T>),
    ) -> Option<DcSelectionMiss> {
        self.consume_with_clock(Instant::now, send_before, apply)
    }
    fn consume_with_clock(
        &self,
        now: impl FnOnce() -> Instant,
        send_before: Option<Instant>,
        apply: impl FnOnce(Option<&T>),
    ) -> Option<DcSelectionMiss> {
        let mut s = self.state.lock().expect("cycle gate poisoned");
        let now = now();
        let mut miss = None;
        if s.status.fault.is_none() {
            if let Some(limit) = send_before.filter(|limit| now >= *limit) {
                let sequence = s.status.issued;
                miss = Some(DcSelectionMiss {
                    sequence,
                    checked_at: now,
                    send_before: limit,
                });
                self.event(&mut s, "dc_send_expired", sequence, now, Some(limit));
                self.fail(&mut s, "DC slot send window expired; restart required", now);
            }
        }
        if s.status.fault.is_none() {
            if let Some((ticket, _)) = s.ticket.take() {
                if now >= ticket.deadline + self.period || s.pending.is_none() {
                    self.fail(&mut s, "missing output frame or missed bus cycle", now);
                } else {
                    s.status.consumed += 1;
                    self.event(
                        &mut s,
                        "consume",
                        ticket.sequence,
                        now,
                        Some(ticket.deadline),
                    );
                }
            }
        }
        apply(s.pending.as_ref());
        s.pending = None;
        miss
    }
    /// Called only after a successful OP exchange and a complete input copy.
    pub fn publish(&self, image: T, deadline: Instant) {
        self.publish_with_clock(image, deadline, Instant::now);
    }
    fn publish_with_clock(&self, image: T, deadline: Instant, now: impl FnOnce() -> Instant) {
        let mut s = self.state.lock().expect("cycle gate poisoned");
        let now = now();
        if !s.armed || s.status.fault.is_some() {
            return;
        }
        if s.ticket.is_some() || deadline <= now {
            self.fail(&mut s, "cycle issue overlap or expired deadline", now);
            return;
        }
        s.status.issued += 1;
        let ticket = Ticket {
            sequence: s.status.issued,
            deadline,
        };
        s.ticket = Some((ticket, image));
        self.event(&mut s, "issue", ticket.sequence, now, Some(deadline));
        self.wake.send_modify(|v| *v = v.wrapping_add(1));
    }
}

// Explicit logical time is available only to deterministic state-machine tests.
// Production callers always sample the monotonic clock after acquiring the lock.
#[cfg(test)]
impl<T: Clone> Gate<T> {
    pub(crate) fn commit_at(&self, ticket: Ticket, image: T, now: Instant) -> Result<(), IoError> {
        self.commit_with_clock(ticket, image, || now)
    }
    pub(crate) fn consume_at(&self, now: Instant, apply: impl FnOnce(Option<&T>)) {
        let _ = self.consume_with_clock(|| now, None, apply);
    }
    pub(crate) fn publish_at(&self, image: T, deadline: Instant, now: Instant) {
        self.publish_with_clock(image, deadline, || now);
    }
}

pub(crate) fn validate(config: &project::EthercatConfig) -> Result<(), IoError> {
    config
        .scan_sync_error()
        .map_or(Ok(()), |why| Err(IoError::Connect(why.into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn armed() -> Gate<[u64; 2]> {
        let g = Gate::new(Duration::from_millis(2));
        g.state.lock().unwrap().armed = true;
        g
    }
    fn begin_at(g: &Gate<[u64; 2]>) -> Ticket {
        let mut s = g.state.lock().unwrap();
        let t = s.ticket.as_ref().unwrap().0;
        s.begun = t.sequence;
        t
    }
    #[test]
    fn ten_thousand_two_ms_frames_are_complete_and_exactly_once_under_work_jitter() {
        let g = armed();
        let mut now = g.origin;
        let mut wire = Vec::new();
        for n in 1..=10_000 {
            g.publish_at([n - 1, (n - 1) * 2], now + g.period, now);
            let t = begin_at(&g);
            // Deterministic producer work varies, always within the bus deadline.
            g.commit_at(
                t,
                [n, 2 * n],
                now + Duration::from_micros(100 + n * 37 % 1700),
            )
            .unwrap();
            now += g.period;
            g.consume_at(now, |frame| wire.push(*frame.unwrap()));
        }
        assert_eq!(wire, (1..=10_000).map(|n| [n, n * 2]).collect::<Vec<_>>());
        let status = g.status();
        assert_eq!(
            (status.issued, status.committed, status.consumed),
            (10_000, 10_000, 10_000)
        );
        assert!(status.fault.is_none());
        assert_eq!(status.events.len(), TRACE_CAPACITY);
        assert_eq!(status.events_dropped, 30_000 - TRACE_CAPACITY as u64);
    }
    #[test]
    fn legacy_latest_value_mailbox_repeats_and_skips_with_independent_phase() {
        // Same nominal 2 ms clocks; bounded alternating scheduling jitter.
        let production: Vec<_> = (1..100)
            .map(|n| (n * 2000 + if n % 2 == 0 { 1200 } else { 100 }, n))
            .collect();
        let sampled: Vec<_> = (1..100)
            .map(|cycle| {
                production
                    .iter()
                    .rfind(|(at, _)| *at <= cycle * 2000 + 600)
                    .map_or(0, |(_, n)| *n)
            })
            .collect();
        assert!(sampled.windows(2).any(|p| p[1] == p[0]));
        assert!(sampled.windows(2).any(|p| p[1] == p[0] + 2));
    }
    #[test]
    fn late_missing_duplicate_stale_and_missed_bus_cycle_latch_zero() {
        for fault in 0..5 {
            let g = armed();
            let now = g.origin;
            g.publish_at([0, 0], now + g.period, now);
            let t = begin_at(&g);
            match fault {
                0 => {
                    assert!(g.commit_at(t, [1, 2], now + g.period).is_err());
                }
                1 => {} // no output commit
                2 => {
                    g.commit_at(t, [1, 2], now).unwrap();
                    assert!(g.commit_at(t, [2, 4], now).is_err());
                }
                3 => {
                    assert!(g
                        .commit_at(Ticket { sequence: 0, ..t }, [1, 2], now)
                        .is_err());
                }
                _ => {
                    g.commit_at(t, [1, 2], now).unwrap();
                }
            }
            let consume_at = if fault == 4 {
                now + g.period * 2
            } else {
                now + g.period
            };
            g.consume_at(consume_at, |frame| assert!(frame.is_none()));
            assert!(g.status().fault.is_some());
            // A healthy transport and fresh data NEVER re-arm a failed gate.
            g.publish_at([8, 16], now + g.period * 4, now + g.period * 3);
            assert!(g.commit_at(t, [1, 2], now).is_err());
            g.consume_at(now + g.period * 4, |frame| assert!(frame.is_none()));
            assert_eq!(g.status().consumed, 0);
        }
    }
    #[tokio::test]
    async fn begin_wakes_from_bus_issue_and_input_snapshot_is_fixed() {
        let g = std::sync::Arc::new(Gate::new(Duration::from_millis(20)));
        let reader = g.clone();
        let task = tokio::spawn(async move { reader.begin().await.unwrap() });
        tokio::task::yield_now().await;
        let now = Instant::now();
        g.publish_at([3, 6], now + g.period, now);
        let (ticket, image) = task.await.unwrap();
        assert_eq!(image, [3, 6]);
        g.commit_at(ticket, [4, 8], Instant::now()).unwrap();
        g.consume_at(now + g.period, |v| assert_eq!(v, Some(&[4, 8])));
        assert_eq!(image, [3, 6]);
    }
    #[test]
    fn failsafe_cannot_be_overwritten_by_a_previously_committed_frame() {
        let g = armed();
        let now = g.origin;
        g.publish_at([0, 0], now + g.period, now);
        let t = begin_at(&g);
        g.commit_at(t, [1, 2], now).unwrap();
        g.trip("stop");
        g.consume_at(now + g.period, |v| assert!(v.is_none()));
        assert_eq!(g.status().fault.as_deref(), Some("stop"));
    }
    #[test]
    fn overlapping_or_expired_issue_is_rejected() {
        for expired in [false, true] {
            let g = armed();
            let now = g.origin;
            g.publish_at([0, 0], if expired { now } else { now + g.period }, now);
            if !expired {
                g.publish_at([1, 2], now + g.period, now);
            }
            assert!(g.status().fault.is_some());
        }
    }

    #[test]
    fn dc_selection_deadline_rejects_subperiod_lateness_and_latches_without_a_ticket() {
        for armed in [false, true] {
            for late in [false, true] {
                let g = Gate::new(Duration::from_millis(2));
                g.state.lock().unwrap().armed = armed;
                let now = g.origin;
                let deadline = now + g.period;
                let limit = deadline + Duration::from_millis(1);
                let ticket = if armed {
                    g.publish_at([0, 0], deadline, now);
                    let ticket = begin_at(&g);
                    g.commit_at(ticket, [1, 2], now).unwrap();
                    Some(ticket)
                } else {
                    None
                };
                let selected_at = limit
                    - if late {
                        Duration::ZERO
                    } else {
                        Duration::from_nanos(1)
                    };
                let miss = g.consume_with_clock(
                    || selected_at,
                    Some(limit),
                    |frame| {
                        assert_eq!(frame, (armed && !late).then_some(&[1, 2]));
                    },
                );
                assert_eq!(miss.is_some(), late);
                if let Some(miss) = miss {
                    assert_eq!(miss.sequence, u64::from(armed));
                    assert_eq!(miss.checked_at, selected_at);
                    assert_eq!(miss.send_before, limit);
                    assert!(
                        g.consume_with_clock(
                            || limit + g.period,
                            Some(limit),
                            |frame| {
                                assert!(frame.is_none());
                            }
                        )
                        .is_none(),
                        "only the first expiry is reported"
                    );
                }
                assert_eq!(g.is_faulted(), late);
                if late {
                    let status = g.status();
                    assert_eq!(status.consumed, 0);
                    let event = status
                        .events
                        .iter()
                        .find(|e| e.kind == "dc_send_expired")
                        .unwrap();
                    assert_eq!(event.deadline_us, Some(3000));
                    if let Some(ticket) = ticket {
                        assert!(g.commit_at(ticket, [8, 9], now).is_err());
                    }
                    g.publish_at([8, 9], limit + g.period, limit);
                    g.consume_at(limit + g.period, |image| assert!(image.is_none()));
                }
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum ContendedOperation {
        Commit,
        Consume,
        DcConsume,
        Publish,
    }

    fn expired_while_waiting_for_lock(operation: ContendedOperation) {
        use std::sync::{mpsc, Arc};

        let g = Arc::new(Gate::new(Duration::from_millis(200)));
        g.state.lock().unwrap().armed = true;
        let issued_at = Instant::now();
        let deadline = issued_at + g.period;
        let ticket = if matches!(operation, ContendedOperation::Publish) {
            None
        } else {
            g.publish_at([0, 0], deadline, issued_at);
            let ticket = begin_at(&g);
            if matches!(
                operation,
                ContendedOperation::Consume | ContendedOperation::DcConsume
            ) {
                g.commit_at(ticket, [1, 2], issued_at).unwrap();
            }
            Some(ticket)
        };
        // Model a status reader preempted while holding the gate mutex.
        // These calls exercise the production clock, not logical test time.
        let holder = g.state.lock().unwrap();
        let worker_gate = g.clone();
        let (entered, observed) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            entered.send(Instant::now()).unwrap();
            match operation {
                ContendedOperation::Commit => {
                    assert!(worker_gate.commit(ticket.unwrap(), [1, 2]).is_err());
                }
                ContendedOperation::Consume => {
                    worker_gate.consume(|frame| assert!(frame.is_none()));
                }
                ContendedOperation::DcConsume => {
                    let _ = worker_gate
                        .consume_before(Some(deadline), |frame| assert!(frame.is_none()));
                }
                ContendedOperation::Publish => worker_gate.publish([0, 0], deadline),
            }
        });
        let entered_at = observed.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            entered_at < deadline,
            "worker must enter before its deadline"
        );
        let expires = deadline
            + if matches!(operation, ContendedOperation::Consume) {
                g.period
            } else {
                Duration::ZERO
            };
        std::thread::sleep(
            (expires + Duration::from_millis(10)).saturating_duration_since(Instant::now()),
        );
        drop(holder);
        worker.join().unwrap();
        let status = g.status();
        assert!(status.fault.is_some(), "{operation:?}: {status:?}");
        assert_eq!(status.consumed, 0);
        g.consume(|frame| assert!(frame.is_none()));
        assert!(smol::block_on(g.begin()).is_err());
        // Later traffic cannot re-arm the failed run.
        g.publish([8, 16], Instant::now() + g.period);
        assert!(g.is_faulted());
    }

    #[test]
    fn mutex_wait_cannot_accept_a_late_commit() {
        expired_while_waiting_for_lock(ContendedOperation::Commit);
    }

    #[test]
    fn mutex_wait_cannot_consume_a_frame_after_a_missed_cycle() {
        expired_while_waiting_for_lock(ContendedOperation::Consume);
    }

    #[test]
    fn mutex_wait_cannot_select_an_output_past_the_dc_slot_limit() {
        expired_while_waiting_for_lock(ContendedOperation::DcConsume);
    }

    #[test]
    fn mutex_wait_cannot_publish_an_expired_ticket() {
        expired_while_waiting_for_lock(ContendedOperation::Publish);
    }
}
