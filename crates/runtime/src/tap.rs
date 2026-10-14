//! The snapshot tap: the one task that takes every snapshot off the scan loop
//! and hands it to the historian, the alarm engine, the latest-snapshot slot
//! behind `/health` and `/status`, and the `/events` stream.
//!
//! The scan loop is its own thread, so nothing here can stop the program. What
//! can stop is everything that is derived from it: history, process alarms,
//! `scan_count`, the live feed. That makes this task's health worth reporting
//! (`SnapshotTapHealth`, on `/health` and `/status`) and worth protecting:
//!
//! - each stage runs under its own `catch_unwind`, so one stage failing does
//!   not stop the others or end the task;
//! - falling behind the scan loop (`Lagged`) skips the snapshots it missed and
//!   carries on, where a bare `while let Ok(..) = rx.recv()` would have ended
//!   the task for good the first time a stage stalled for longer than the
//!   channel holds;
//! - `running` is cleared by a drop guard, so it is `false` however the task
//!   ends, a panic or an abort included.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ironplc_bridge::monitor::{self, AlarmEngine, Historian, SnapshotTapHealth};
use ironplc_bridge::VarSnapshot;
use parking_lot::Mutex;
use tokio::sync::broadcast::{self, error::RecvError};

/// How long the tap may sit inside one snapshot before it is called stalled.
/// The scan loop emits a snapshot every 100 ms and a healthy stage takes
/// microseconds to a few milliseconds; two seconds is twenty periods of slack
/// for a slow disk and still short enough to matter to an operator.
const STALL_AFTER: Duration = Duration::from_secs(2);

/// One consumer of the snapshot stream, named for the fault report.
pub type Stage = (&'static str, Box<dyn Fn(&VarSnapshot) + Send>);

/// The tap's own state, shared with the HTTP handlers.
pub struct TapStatus {
    running: AtomicBool,
    faults: AtomicU64,
    last_fault: Mutex<Option<String>>,
    /// When `last_fault` happened.
    last_fault_at: Mutex<Option<Instant>>,
    /// When the snapshot now being processed began; `None` between snapshots.
    busy_since: Mutex<Option<Instant>>,
    stall_after: Duration,
}

impl TapStatus {
    /// A tap that is about to be spawned: `running` until [`run`] ends.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            running: AtomicBool::new(true),
            faults: AtomicU64::new(0),
            last_fault: Mutex::new(None),
            last_fault_at: Mutex::new(None),
            busy_since: Mutex::new(None),
            stall_after: STALL_AFTER,
        })
    }

    pub fn health(&self) -> SnapshotTapHealth {
        self.health_at(Instant::now())
    }

    /// [`Self::health`] as of `now`, so the stall threshold is testable.
    fn health_at(&self, now: Instant) -> SnapshotTapHealth {
        let stalled_for = self
            .busy_since
            .lock()
            .map(|since| now.saturating_duration_since(since))
            .filter(|busy| *busy >= self.stall_after);
        SnapshotTapHealth {
            running: self.running.load(Ordering::Relaxed),
            faults: self.faults.load(Ordering::Relaxed),
            last_fault: self.last_fault.lock().clone(),
            stalled_for_ms: stalled_for.map(|d| d.as_millis() as u64),
            last_fault_age_ms: self
                .last_fault_at
                .lock()
                .map(|at| now.saturating_duration_since(at).as_millis() as u64),
        }
    }

    fn begin_snapshot(&self, now: Instant) {
        *self.busy_since.lock() = Some(now);
    }

    fn end_snapshot(&self) {
        *self.busy_since.lock() = None;
    }

    fn record_fault(&self, what: String) {
        *self.last_fault.lock() = Some(what.clone());
        *self.last_fault_at.lock() = Some(Instant::now());
        let n = self.faults.fetch_add(1, Ordering::Relaxed) + 1;
        // A stage that fails on every snapshot would write ten log lines a
        // second for as long as the runtime lives; the count and the latest
        // text stay available on /health and /status instead.
        if n.is_power_of_two() {
            tracing::error!(
                faults = n,
                %what,
                "snapshot tap fault (logged at the 1st, 2nd, 4th, 8th, ... occurrence)"
            );
        }
    }
}

/// Clears `running` when the task's future is dropped, however it ended.
struct EndGuard(Arc<TapStatus>);

impl Drop for EndGuard {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::Relaxed);
        tracing::warn!(
            "snapshot tap ended: history, process alarms and /status are no longer being fed"
        );
    }
}

/// The consumers `main` wires up, in the order they have always run.
pub fn standard_stages(
    historian: Arc<Historian>,
    alarms: Arc<Mutex<AlarmEngine>>,
    latest: Arc<Mutex<Option<VarSnapshot>>>,
    events: broadcast::Sender<VarSnapshot>,
) -> Vec<Stage> {
    vec![
        (
            "historian",
            Box::new(move |snap| historian.note_snapshot(snap)),
        ),
        (
            "alarms",
            Box::new(move |snap| {
                alarms.lock().note_snapshot(snap, monitor::wall_now_us());
            }),
        ),
        (
            "latest",
            Box::new(move |snap| *latest.lock() = Some(snap.clone())),
        ),
        (
            "events",
            Box::new(move |snap| {
                // No subscribers is fine.
                let _ = events.send(snap.clone());
            }),
        ),
    ]
}

fn feed(stages: &[Stage], snap: &VarSnapshot, status: &TapStatus) {
    for (name, stage) in stages {
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| stage(snap))) {
            status.record_fault(format!(
                "the {name} stage panicked on scan {}: {}",
                snap.scan_count,
                monitor::panic_message(payload.as_ref())
            ));
        }
    }
}

/// Take snapshots off `rx` until the sender goes away.
pub async fn run(
    mut rx: broadcast::Receiver<VarSnapshot>,
    stages: Vec<Stage>,
    status: Arc<TapStatus>,
) {
    let _ended = EndGuard(status.clone());
    loop {
        match rx.recv().await {
            Ok(snap) => {
                status.begin_snapshot(Instant::now());
                feed(&stages, &snap, &status);
                status.end_snapshot();
            }
            Err(RecvError::Lagged(skipped)) => status.record_fault(format!(
                "fell behind the scan loop: {skipped} snapshots were skipped"
            )),
            Err(RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    fn snap(scan: u64) -> VarSnapshot {
        VarSnapshot {
            timestamp_us: scan * 1_000_000,
            scan_count: scan,
            device_health: None,
            vars: vec![ironplc_bridge::VarValue {
                name: "level".into(),
                type_name: "REAL".into(),
                value: "1".into(),
                bits: 1.0f32.to_bits() as u64,
                input: None,
            }],
        }
    }

    fn counter_stage(name: &'static str, n: &Arc<AtomicUsize>) -> Stage {
        let n = n.clone();
        (
            name,
            Box::new(move |_| {
                n.fetch_add(1, Ordering::SeqCst);
            }),
        )
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !done() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    #[test]
    fn health_serializes_with_the_keys_the_readers_use() {
        let h = TapStatus::new().health();
        assert_eq!(
            serde_json::to_value(&h).unwrap(),
            serde_json::json!(
                {"running": true, "faults": 0, "last_fault": null, "stalled_for_ms": null,
                 "last_fault_age_ms": null}
            ),
            "present and null, so clients can rely on the keys"
        );
    }

    /// `faults` only grows, so it cannot say whether the task is failing now.
    /// The age of the latest incident can, to a reader that polls once.
    #[test]
    fn last_fault_age_counts_from_the_latest_incident() {
        let status = TapStatus::new();
        assert_eq!(status.health().last_fault_age_ms, None, "none yet");

        status.record_fault("the alarms stage panicked on scan 3: boom".into());
        let soon = status.health().last_fault_age_ms.expect("set by the fault");
        assert!(soon < 1000, "just happened: {soon} ms");

        let later = status.health_at(Instant::now() + Duration::from_secs(60));
        let age = later.last_fault_age_ms.expect("still reported a minute on");
        assert!((60_000..61_000).contains(&age), "{age} ms");
        assert_eq!(
            later.faults, 1,
            "the count is unchanged by the passing of time"
        );
    }

    /// The threshold itself, with the clock handed in: a snapshot that has
    /// been in progress for less than the limit is a busy tap, not a stalled
    /// one; at the limit it is stalled, and says for how long; between
    /// snapshots it is neither.
    #[test]
    fn stalled_for_ms_appears_at_the_threshold_and_clears_between_snapshots() {
        let status = TapStatus::new();
        let t0 = Instant::now();
        assert_eq!(
            status.health_at(t0).stalled_for_ms,
            None,
            "nothing in progress"
        );

        status.begin_snapshot(t0);
        let just_under = t0 + STALL_AFTER - Duration::from_millis(1);
        assert_eq!(status.health_at(just_under).stalled_for_ms, None);
        assert_eq!(
            status.health_at(t0 + STALL_AFTER).stalled_for_ms,
            Some(STALL_AFTER.as_millis() as u64)
        );
        assert_eq!(
            status
                .health_at(t0 + Duration::from_secs(30))
                .stalled_for_ms,
            Some(30_000),
            "it keeps counting while it stays stuck"
        );

        status.end_snapshot();
        assert_eq!(
            status
                .health_at(t0 + Duration::from_secs(30))
                .stalled_for_ms,
            None,
            "finishing the snapshot clears it"
        );
    }

    /// The case the fault count cannot see: a stage that blocks. It neither
    /// panics nor lags, so `running` stays true and `faults` stays 0 — but the
    /// state is readable from another thread while the stage is stuck, and
    /// clears once it returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_blocked_stage_is_reported_stalled_from_another_thread() {
        let (tx, rx) = broadcast::channel(8);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        let stages: Vec<Stage> = vec![(
            "disk",
            Box::new(move |_| {
                // Stands in for a write that does not return.
                let _ = release_rx.lock().unwrap().recv();
            }),
        )];
        let mut status = TapStatus::new();
        Arc::get_mut(&mut status).unwrap().stall_after = Duration::from_millis(50);
        let task = tokio::spawn(run(rx, stages, status.clone()));

        tx.send(snap(1)).unwrap();
        until("the tap to be reported stalled", || {
            status.health().stalled_for_ms.is_some()
        })
        .await;
        let h = status.health();
        assert!(h.running, "stuck is not ended");
        assert_eq!(
            h.faults, 0,
            "stuck is not a fault: nothing panicked or lagged"
        );

        release_tx.send(()).unwrap();
        until("the stall to clear", || {
            status.health().stalled_for_ms.is_none()
        })
        .await;
        drop(tx);
        drop(release_tx);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn a_stage_that_panics_is_reported_and_the_others_keep_running() {
        let (tx, rx) = broadcast::channel(16);
        let before = Arc::new(AtomicUsize::new(0));
        let after = Arc::new(AtomicUsize::new(0));
        let stages: Vec<Stage> = vec![
            counter_stage("first", &before),
            (
                "broken",
                Box::new(|_| panic!("the alarm table has no such row")),
            ),
            counter_stage("last", &after),
        ];
        let status = TapStatus::new();
        let task = tokio::spawn(run(rx, stages, status.clone()));

        for scan in 1..=3 {
            tx.send(snap(scan)).unwrap();
        }
        until("three snapshots through every stage", || {
            after.load(Ordering::SeqCst) == 3
        })
        .await;

        assert_eq!(before.load(Ordering::SeqCst), 3);
        let h = status.health();
        assert!(h.running, "one failing stage does not end the tap");
        assert_eq!(h.faults, 3);
        let last = h.last_fault.expect("the latest incident is described");
        assert!(
            last.contains("the broken stage panicked on scan 3"),
            "{last}"
        );
        assert!(last.contains("no such row"), "{last}");

        drop(tx);
        task.await.unwrap();
        assert!(!status.health().running);
    }

    #[tokio::test]
    async fn falling_behind_skips_snapshots_and_the_tap_keeps_running() {
        // Capacity 2, ten sent before the task reads anything: its first
        // receive is `Lagged(8)`. A bare `while let Ok(..)` ended the task
        // there, for good, with nothing recording that it had.
        let (tx, rx) = broadcast::channel(2);
        for scan in 1..=10 {
            tx.send(snap(scan)).unwrap();
        }
        let seen = Arc::new(AtomicUsize::new(0));
        let newest = Arc::new(AtomicU64::new(0));
        let newest_in = newest.clone();
        let stages: Vec<Stage> = vec![
            counter_stage("count", &seen),
            (
                "newest",
                Box::new(move |s| newest_in.store(s.scan_count, Ordering::SeqCst)),
            ),
        ];
        let status = TapStatus::new();
        let task = tokio::spawn(run(rx, stages, status.clone()));

        until("the two snapshots the channel still held", || {
            seen.load(Ordering::SeqCst) == 2
        })
        .await;
        assert_eq!(newest.load(Ordering::SeqCst), 10);
        let h = status.health();
        assert!(h.running, "lagging is a fault, not the end of the tap");
        assert_eq!(h.faults, 1);
        let last = h.last_fault.expect("described");
        assert!(last.contains("8 snapshots were skipped"), "{last}");

        // And it is still taking new ones.
        tx.send(snap(11)).unwrap();
        until("the snapshot after the lag", || {
            newest.load(Ordering::SeqCst) == 11
        })
        .await;

        drop(tx);
        task.await.unwrap();
        assert!(!status.health().running, "the sender going away ends it");
    }

    #[tokio::test]
    async fn running_is_false_however_the_task_ends() {
        let (tx, rx) = broadcast::channel::<VarSnapshot>(2);
        let status = TapStatus::new();
        let task = tokio::spawn(run(rx, vec![], status.clone()));
        tokio::task::yield_now().await;
        assert!(status.health().running);

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            !status.health().running,
            "an aborted tap must not read as still feeding history and alarms"
        );
        drop(tx);
    }

    #[tokio::test]
    async fn the_standard_stages_feed_history_alarms_latest_and_events() {
        let historian = Arc::new(Historian::in_memory());
        let alarms = Arc::new(Mutex::new(AlarmEngine::default()));
        let latest = Arc::new(Mutex::new(None));
        let (events_tx, mut events_rx) = broadcast::channel(4);
        let stages = standard_stages(historian.clone(), alarms.clone(), latest.clone(), events_tx);
        let names: Vec<&str> = stages.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["historian", "alarms", "latest", "events"]);

        let status = TapStatus::new();
        feed(&stages, &snap(5), &status);

        assert_eq!(status.health().faults, 0);
        assert_eq!(latest.lock().as_ref().map(|s| s.scan_count), Some(5));
        assert_eq!(events_rx.try_recv().unwrap().scan_count, 5);
        let history = historian.query(&[], 0, 0, 1_000);
        assert_eq!(history.series.len(), 1, "the historian recorded `level`");
    }
}
