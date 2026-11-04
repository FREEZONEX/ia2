//! Embedded zero-config historian.
//!
//! Samples the live snapshot stream at a fixed cadence (default 1 Hz)
//! into per-variable in-memory rings (default 7200 samples ≈ 2 h), and
//! — when given a directory — appends the same samples as JSONL
//! segments so an edge box keeps history across restarts (the newest
//! segments are preloaded into the rings on start).
//!
//! Query serves min/max/last buckets so a trend over any window is one
//! HTTP GET — no client-side accumulation, no history lost on reload.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;
use ts_rs::TS;

use super::typed_value;
use crate::runtime::VarSnapshot;

/// Default sampling cadence — one sample per second per variable.
pub const DEFAULT_SAMPLE_INTERVAL_US: u64 = 1_000_000;
/// Default per-variable ring capacity (2 h at 1 Hz).
pub const DEFAULT_CAPACITY: usize = 7200;
/// Rotate the JSONL segment past this size; keep this many old ones.
const SEGMENT_MAX_BYTES: u64 = 8 * 1024 * 1024;
const SEGMENTS_KEPT: usize = 3;
/// Persisted lines that may wait for the disk. At the default 1 Hz that is
/// about eight minutes of a stalled disk before samples stop being persisted
/// (they stay in memory and keep being served).
const PERSIST_QUEUE_LINES: usize = 512;
/// How long dropping a historian waits for the writer to flush before it lets a
/// stuck disk go, so exiting the process never hangs on one.
const PERSIST_FLUSH_WAIT: Duration = Duration::from_secs(2);

/// One downsampled bucket of a variable's history.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct HistoryPoint {
    /// Bucket start, microseconds.
    pub t_us: u64,
    pub min: f64,
    pub max: f64,
    /// Last sample in the bucket — what a stepped trend line draws.
    pub v: f64,
    /// True if any sample in this bucket was stale; do not plot as live data.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub stale: Option<bool>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct HistorySeries {
    pub name: String,
    pub points: Vec<HistoryPoint>,
}

/// Full response of GET /history — series plus the window the data
/// actually covers, so clients can render "history starts here".
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct HistoryResponse {
    pub series: Vec<HistorySeries>,
    /// Oldest sample time held for any requested variable (0 = none).
    pub oldest_us: u64,
    pub sample_interval_us: u64,
}

struct Inner {
    series: HashMap<String, Vec<(u64, f64, bool)>>, // ring via rotate-trim
    last_sample_us: u64,
}

/// The segment files the writer thread appends to.
struct Persist {
    dir: PathBuf,
    file: Option<std::fs::File>,
    written: u64,
}

/// Disk persistence lives on its own thread behind a bounded queue.
///
/// The write used to happen inside `note_snapshot`, under the lock the
/// readers also take, on whichever thread fed the historian. On the edge that
/// is the one task feeding the alarm engine and the live feed: a disk that
/// stalls (a bad SD card, a full journal) held the historian's lock and froze
/// that task, and with it alarms, `scan_count` and `/events`, while queries
/// piled up behind the same lock. Now the feeder only enqueues a line: a full
/// queue (or a dead writer) drops that line from the DISK, counts it, and
/// leaves memory (and `/history`) untouched.
struct PersistQueue {
    tx: Option<SyncSender<String>>,
    thread: Option<JoinHandle<()>>,
    /// Lines that did not reach the writer: the queue was full or it is gone.
    dropped: AtomicU64,
    flush_wait: Duration,
}

impl PersistQueue {
    fn spawn(
        queue_lines: usize,
        flush_wait: Duration,
        mut sink: Box<dyn FnMut(&str) + Send>,
    ) -> Self {
        let (tx, rx) = sync_channel::<String>(queue_lines);
        let thread = std::thread::Builder::new()
            .name("history-writer".into())
            .spawn(move || {
                while let Ok(line) = rx.recv() {
                    sink(&line);
                }
            })
            .map_err(|e| tracing::warn!(%e, "history writer thread did not start — memory only"))
            .ok();
        Self {
            tx: thread.is_some().then_some(tx),
            thread,
            dropped: AtomicU64::new(0),
            flush_wait,
        }
    }

    /// Never blocks.
    fn offer(&self, line: String) {
        let Some(tx) = &self.tx else {
            self.note_dropped();
            return;
        };
        match tx.try_send(line) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => self.note_dropped(),
        }
    }

    fn note_dropped(&self) {
        let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        // 1st, 2nd, 4th, 8th ... so a stalled disk is one line, not a stream.
        if n.is_power_of_two() {
            tracing::warn!(
                dropped = n,
                "history persistence is not keeping up (disk stalled?): samples stay in \
                 memory and are served by /history, but are not being written to disk"
            );
        }
    }
}

impl Drop for PersistQueue {
    fn drop(&mut self) {
        // Closing the queue lets the writer drain what is queued and exit.
        drop(self.tx.take());
        let Some(thread) = self.thread.take() else {
            return;
        };
        let deadline = Instant::now() + self.flush_wait;
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if thread.is_finished() {
            let _ = thread.join();
        } else {
            tracing::warn!(
                "history writer did not finish flushing (disk stalled?); not waiting for it"
            );
        }
    }
}

/// Thread-safe historian handle; cheap to share behind an Arc.
///
/// The lock does not poison: a panic while it is held frees it, and the next
/// caller carries on with whatever the rings held at that moment. A ring is a
/// list of independent samples, so a sample missing from one series is the
/// worst a half-finished `note_snapshot` leaves behind; a poisoned lock would
/// instead make every later call panic, and the snapshot task that feeds this
/// is one of them.
pub struct Historian {
    inner: Mutex<Inner>,
    capacity: usize,
    sample_interval_us: u64,
    /// `None` for an in-memory historian (the IDE server's).
    persist: Option<PersistQueue>,
}

impl Historian {
    pub fn new(sample_interval_us: u64, capacity: usize, persist_dir: Option<PathBuf>) -> Self {
        let mut inner = Inner {
            series: HashMap::new(),
            last_sample_us: 0,
        };
        let mut persist = None;
        if let Some(dir) = persist_dir {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::warn!(%e, dir = %dir.display(), "history dir unavailable — memory only");
            } else {
                preload(&mut inner.series, &dir, capacity);
                let mut files = Persist {
                    dir,
                    file: None,
                    written: 0,
                };
                persist = Some(PersistQueue::spawn(
                    PERSIST_QUEUE_LINES,
                    PERSIST_FLUSH_WAIT,
                    Box::new(move |line| persist_line(&mut files, line)),
                ));
            }
        }
        Self {
            inner: Mutex::new(inner),
            capacity,
            sample_interval_us: sample_interval_us.max(100_000),
            persist,
        }
    }

    /// A historian whose persistence goes to `sink` instead of segment files,
    /// to exercise the queue (a sink that blocks stands in for a stalled disk).
    #[cfg(test)]
    fn with_sink(
        sample_interval_us: u64,
        capacity: usize,
        queue_lines: usize,
        flush_wait: Duration,
        sink: Box<dyn FnMut(&str) + Send>,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner {
                series: HashMap::new(),
                last_sample_us: 0,
            }),
            capacity,
            sample_interval_us: sample_interval_us.max(100_000),
            persist: Some(PersistQueue::spawn(queue_lines, flush_wait, sink)),
        }
    }

    /// Persisted lines that never reached the disk writer because it was not
    /// keeping up (a stalled disk): they are still in memory and served by
    /// `query`. 0 for an in-memory historian.
    pub fn persist_dropped(&self) -> u64 {
        self.persist
            .as_ref()
            .map_or(0, |p| p.dropped.load(Ordering::Relaxed))
    }

    /// Zero-config in-memory historian (IDE-side default).
    pub fn in_memory() -> Self {
        Self::new(DEFAULT_SAMPLE_INTERVAL_US, DEFAULT_CAPACITY, None)
    }

    /// Drop every ring (project closed). Persistence files are left
    /// alone — they belong to the edge's post-mortem story.
    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.series.clear();
        inner.last_sample_us = 0;
    }

    /// Feed one live snapshot; throttled internally to the sample
    /// cadence. Non-numeric variables are skipped (BOOL records 0/1).
    pub fn note_snapshot(&self, snap: &VarSnapshot) {
        let mut inner = self.inner.lock();
        // `timestamp_us` is elapsed-since-THIS-run, so it rewinds to zero every
        // time a program is started. The IDE server shares one historian across
        // runs — `clear()` only fires on project close — so without this a
        // forward-only throttle discards every sample of the new run until its
        // clock passes the old run's last one. A ten-minute run costs ten
        // minutes of the next run's history: flat trend, no error, nothing in a
        // log.
        //
        // Rings go too, not just the cursor. The axis is per-run, so samples
        // from two runs are not comparable; keeping them would leave the ring
        // non-monotonic, and `query` buckets by walking samples in order and
        // merging into the LAST point — out-of-order input there does not
        // merge, it emits points that jump backwards in time.
        if snap.timestamp_us < inner.last_sample_us {
            tracing::info!(
                previous_last_us = inner.last_sample_us,
                now_us = snap.timestamp_us,
                "snapshot clock rewound — new run; starting a fresh history window"
            );
            inner.series.clear();
            inner.last_sample_us = 0;
        }
        if snap.timestamp_us < inner.last_sample_us + self.sample_interval_us {
            return;
        }
        inner.last_sample_us = snap.timestamp_us;

        let mut line = serde_json::Map::new();
        let mut stale_vars = Vec::new();
        for var in &snap.vars {
            let tv = typed_value(&var.type_name, var.bits, &var.value);
            let v = tv
                .as_f64()
                .or_else(|| tv.as_bool().map(|b| if b { 1.0 } else { 0.0 }));
            let Some(v) = v else { continue };
            let ring = inner.series.entry(var.name.clone()).or_default();
            let stale = var.input.as_ref().is_some_and(|q| q.stale);
            ring.push((snap.timestamp_us, v, stale));
            if stale {
                stale_vars.push(var.name.as_str());
            }
            if ring.len() > self.capacity {
                let excess = ring.len() - self.capacity;
                ring.drain(..excess);
            }
            line.insert(var.name.clone(), serde_json::json!(v));
        }

        if let Some(persist) = &self.persist {
            persist.offer(
                serde_json::json!({ "t": snap.timestamp_us, "v": line, "stale": stale_vars })
                    .to_string(),
            );
        }
    }

    /// Bucketed query. `vars` empty = every recorded variable.
    pub fn query(
        &self,
        vars: &[String],
        from_us: u64,
        to_us: u64,
        step_ms: u64,
    ) -> HistoryResponse {
        let inner = self.inner.lock();
        // `step_ms` comes straight off the request. Saturating, so a step too
        // large to express in microseconds is one bucket covering everything
        // rather than an overflow (and, where it wraps to zero, a division by
        // zero in the bucketing below).
        let step_us = step_ms.max(1).saturating_mul(1000);
        let names: Vec<String> = if vars.is_empty() {
            let mut n: Vec<String> = inner.series.keys().cloned().collect();
            n.sort();
            n
        } else {
            vars.to_vec()
        };
        let mut oldest = u64::MAX;
        let series = names
            .iter()
            .map(|name| {
                let samples = inner.series.get(name).map(Vec::as_slice).unwrap_or(&[]);
                if let Some((t, _, _)) = samples.first() {
                    oldest = oldest.min(*t);
                }
                let mut points: Vec<HistoryPoint> = Vec::new();
                for (t, v, stale) in samples {
                    if *t < from_us || (to_us > 0 && *t > to_us) {
                        continue;
                    }
                    let bucket = t - (t - from_us) % step_us;
                    match points.last_mut() {
                        Some(p) if p.t_us == bucket => {
                            p.min = p.min.min(*v);
                            p.max = p.max.max(*v);
                            p.v = *v;
                            if *stale {
                                p.stale = Some(true);
                            }
                        }
                        _ => points.push(HistoryPoint {
                            t_us: bucket,
                            min: *v,
                            max: *v,
                            v: *v,
                            stale: stale.then_some(true),
                        }),
                    }
                }
                HistorySeries {
                    name: name.clone(),
                    points,
                }
            })
            .collect();
        HistoryResponse {
            series,
            oldest_us: if oldest == u64::MAX { 0 } else { oldest },
            sample_interval_us: self.sample_interval_us,
        }
    }
}

/// Append one JSONL line, rotating segments as they grow. Failures
/// degrade to memory-only with one warning — history must never take
/// the scan loop down. Runs on the writer thread only.
fn persist_line(p: &mut Persist, line: &str) {
    if p.file.is_none() || p.written > SEGMENT_MAX_BYTES {
        rotate(p);
    }
    let Some(file) = p.file.as_mut() else { return };
    match writeln!(file, "{line}") {
        Ok(()) => p.written += line.len() as u64 + 1,
        Err(e) => {
            tracing::warn!(%e, "history append failed — dropping persistence");
            p.file = None;
        }
    }
}

fn segment_paths(dir: &PathBuf) -> Vec<PathBuf> {
    let mut segs: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.extension().and_then(|s| s.to_str()) == Some("jsonl")
                        && p.file_name()
                            .and_then(|s| s.to_str())
                            .is_some_and(|n| n.starts_with("history-"))
                })
                .collect()
        })
        .unwrap_or_default();
    segs.sort();
    segs
}

fn rotate(p: &mut Persist) {
    // Name segments by wall-clock micros — sortable and unique enough.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let path = p.dir.join(format!("history-{now:020}.jsonl"));
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => {
            p.file = Some(f);
            p.written = 0;
        }
        Err(e) => {
            tracing::warn!(%e, path = %path.display(), "history segment open failed");
            p.file = None;
        }
    }
    // Prune old segments, newest kept.
    let segs = segment_paths(&p.dir);
    if segs.len() > SEGMENTS_KEPT {
        for old in &segs[..segs.len() - SEGMENTS_KEPT] {
            let _ = std::fs::remove_file(old);
        }
    }
}

/// Load the newest segments into fresh rings so an edge restart keeps
/// its recent history. Reads oldest→newest so rings end up in order.
fn preload(series: &mut HashMap<String, Vec<(u64, f64, bool)>>, dir: &PathBuf, capacity: usize) {
    for seg in segment_paths(dir) {
        let Ok(file) = std::fs::File::open(&seg) else {
            continue;
        };
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(row) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let Some(t) = row.get("t").and_then(|t| t.as_u64()) else {
                continue;
            };
            let Some(vals) = row.get("v").and_then(|v| v.as_object()) else {
                continue;
            };
            for (name, v) in vals {
                if let Some(v) = v.as_f64() {
                    let ring = series.entry(name.clone()).or_default();
                    let stale = row
                        .get("stale")
                        .and_then(|s| s.as_array())
                        .is_some_and(|s| s.iter().any(|s| s.as_str() == Some(name)));
                    ring.push((t, v, stale));
                    if ring.len() > capacity {
                        ring.remove(0);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{VarSnapshot, VarValue};

    fn snap(t_us: u64, v: f32) -> VarSnapshot {
        VarSnapshot {
            timestamp_us: t_us,
            scan_count: 0,
            device_health: None,
            vars: vec![VarValue {
                name: "flow".into(),
                type_name: "REAL".into(),
                value: format!("{v}"),
                bits: v.to_bits() as u64,
                input: None,
            }],
        }
    }

    #[test]
    fn a_restarted_program_keeps_being_recorded() {
        // `snap.timestamp_us` is elapsed-since-this-run, so it restarts at 0
        // every time a program is started. The IDE server shares ONE historian
        // across runs (clear() only fires on project close), so a throttle
        // that only ever compares forward silently drops every sample of the
        // new run until its clock passes the old run's last one — a ten-minute
        // run costs ten minutes of the next run's history, with a flat trend
        // and no error anywhere.
        let h = Historian::new(1_000_000, 100, None);
        for t in [1_000_000u64, 2_000_000, 600_000_000] {
            h.note_snapshot(&snap(t, 1.0));
        }
        assert_eq!(
            h.query(&[], 0, u64::MAX, 1000).series[0].points.len(),
            3,
            "first run recorded"
        );

        // Restart: the clock rewinds.
        h.note_snapshot(&snap(1_000_000, 2.0));
        h.note_snapshot(&snap(2_000_000, 3.0));
        let points = h.query(&[], 0, u64::MAX, 1000).series[0].points.clone();

        // A fresh window, and one that actually records — the bug was that it
        // recorded NOTHING until the new run outlived the old one.
        assert_eq!(
            points.len(),
            2,
            "the new run's samples must be kept: {points:?}"
        );
        assert_eq!(points[0].v, 2.0, "and they must be the NEW run's values");
        assert_eq!(points[1].v, 3.0);
        // Per-run axis: keeping run 1's samples alongside would leave the ring
        // non-monotonic, which `query` cannot bucket.
        assert!(
            points.windows(2).all(|w| w[0].t_us < w[1].t_us),
            "points must stay ordered in time: {points:?}"
        );
    }

    #[test]
    fn samples_throttle_and_buckets_carry_min_max() {
        let h = Historian::new(1_000_000, 100, None);
        h.note_snapshot(&snap(1_000_000, 1.0));
        h.note_snapshot(&snap(1_200_000, 99.0)); // inside throttle window — dropped
        h.note_snapshot(&snap(2_000_000, 2.0));
        h.note_snapshot(&snap(3_000_000, 8.0));
        h.note_snapshot(&snap(4_000_000, 4.0));

        // One 10s bucket: min/max/last across the KEPT samples.
        let resp = h.query(&["flow".into()], 0, 0, 10_000);
        assert_eq!(resp.series.len(), 1);
        let pts = &resp.series[0].points;
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].min, 1.0);
        assert_eq!(pts[0].max, 8.0, "throttled 99.0 must NOT appear");
        assert_eq!(pts[0].v, 4.0);
        assert_eq!(resp.oldest_us, 1_000_000);

        // 1s buckets: one point per sample.
        let resp = h.query(&["flow".into()], 0, 0, 1_000);
        assert_eq!(resp.series[0].points.len(), 4);
    }

    #[test]
    fn ring_capacity_bounds_memory() {
        let h = Historian::new(100_000, 10, None);
        for i in 0..50u64 {
            h.note_snapshot(&snap(i * 1_000_000, i as f32));
        }
        let resp = h.query(&[], 0, 0, 1_000);
        assert_eq!(resp.series[0].points.len(), 10, "ring trimmed to capacity");
        assert_eq!(resp.oldest_us, 40 * 1_000_000);
    }

    /// A `Historian` holding four 1 s samples (1 s .. 4 s) of `flow`.
    fn four_samples() -> Historian {
        let h = Historian::new(1_000_000, 100, None);
        for (t, v) in [(1, 1.0), (2, 8.0), (3, 3.0), (4, 4.0)] {
            h.note_snapshot(&snap(t * 1_000_000, v));
        }
        h
    }

    #[test]
    fn query_when_step_is_too_large_for_microseconds_then_one_bucket_not_a_panic() {
        // `step_ms` is a request parameter. Past u64::MAX / 1000 the product
        // into microseconds overflows, and 2^61 ms wraps to EXACTLY zero —
        // the next thing the bucketing does is `% step_us`.
        for step_ms in [u64::MAX / 1000 + 1, 1 << 61, u64::MAX] {
            let h = four_samples();
            let resp = h.query(&["flow".into()], 0, 0, step_ms);
            let pts = &resp.series[0].points;
            assert_eq!(pts.len(), 1, "step_ms={step_ms}: one bucket: {pts:?}");
            assert_eq!(pts[0].min, 1.0, "step_ms={step_ms}");
            assert_eq!(pts[0].max, 8.0, "step_ms={step_ms}");
            assert_eq!(
                pts[0].v, 4.0,
                "step_ms={step_ms}: last sample of the window"
            );
        }
    }

    #[test]
    fn query_when_step_is_the_largest_that_fits_then_unchanged() {
        // The boundary itself must not be caught by the guard above.
        let h = four_samples();
        let resp = h.query(&["flow".into()], 0, 0, u64::MAX / 1000);
        assert_eq!(resp.series[0].points.len(), 1);
    }

    #[test]
    fn query_when_step_is_zero_then_it_means_one_millisecond() {
        let h = four_samples();
        let resp = h.query(&["flow".into()], 0, 0, 0);
        assert_eq!(resp.series[0].points.len(), 4, "one point per 1 s sample");
    }

    #[test]
    fn query_when_window_is_inverted_or_empty_then_no_points() {
        let h = four_samples();
        // from after to: nothing can satisfy both bounds.
        let resp = h.query(&["flow".into()], 3_500_000, 1_500_000, 1_000);
        assert!(resp.series[0].points.is_empty(), "{:?}", resp.series);
        // from beyond the newest sample.
        let resp = h.query(&["flow".into()], u64::MAX, 0, 1_000);
        assert!(resp.series[0].points.is_empty());
        // The window bounds are inclusive and to_us = 0 means open-ended.
        let resp = h.query(&["flow".into()], 2_000_000, 3_000_000, 1_000);
        assert_eq!(resp.series[0].points.len(), 2);
        let resp = h.query(&["flow".into()], 2_000_000, 0, 1_000);
        assert_eq!(resp.series[0].points.len(), 3);
        // The extremes together.
        let resp = h.query(&["flow".into()], 0, u64::MAX, u64::MAX);
        assert_eq!(resp.series[0].points.len(), 1);
    }

    #[test]
    fn query_names_the_requested_series_even_when_nothing_was_recorded() {
        let h = Historian::new(1_000_000, 100, None);
        let resp = h.query(&["never_seen".into()], 0, 0, 1_000);
        assert_eq!(resp.series.len(), 1);
        assert!(resp.series[0].points.is_empty());
        assert_eq!(resp.oldest_us, 0, "0 = none");
    }

    #[test]
    fn a_panic_while_the_lock_is_held_does_not_take_later_calls_down() {
        // The historian is fed by a background task and read by request
        // handlers. With a poisoning lock one panic under it turns every
        // later call into another panic — the feeding task included, so
        // recording would stop for good. Hold the lock, panic, and check the
        // next caller still works and still sees what was recorded.
        let h = four_samples();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = h.inner.lock();
            panic!("a caller died holding the historian lock");
        }));
        assert!(caught.is_err());

        h.note_snapshot(&snap(5_000_000, 5.0));
        let pts = &h.query(&["flow".into()], 0, 0, 1_000).series[0].points;
        assert_eq!(pts.len(), 5, "history survives and keeps recording");
        h.clear();
        assert!(h.query(&[], 0, 0, 1_000).series.is_empty());
    }

    /// A disk that stalls must not stall the feeder or the readers. The write
    /// used to run inside `note_snapshot` under the lock `query` takes, so one
    /// stuck write froze the task feeding alarms and the live feed, with every
    /// history request queued behind it.
    #[test]
    fn a_stalled_disk_does_not_block_recording_or_queries() {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let delivered = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let delivered_in = delivered.clone();
        let h = std::sync::Arc::new(Historian::with_sink(
            1_000_000,
            1000,
            4,
            Duration::from_secs(5),
            Box::new(move |_line| {
                // Stands in for a write that does not return.
                let _ = release_rx.recv();
                delivered_in.fetch_add(1, Ordering::Relaxed);
            }),
        ));

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let feeder = {
            let h = h.clone();
            std::thread::spawn(move || {
                for i in 1..=50u64 {
                    h.note_snapshot(&snap(i * 1_000_000, i as f32));
                }
                let points = h.query(&["flow".into()], 0, 0, 1000).series[0].points.len();
                done_tx.send(points).unwrap();
            })
        };
        let points = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("recording and querying finish while the disk is stuck");
        feeder.join().unwrap();

        assert_eq!(points, 50, "every sample is in memory and served");
        let dropped = h.persist_dropped();
        assert!(
            dropped >= 45,
            "all but the queue's worth miss the disk: {dropped}"
        );

        // The disk comes back: what was queued is written, in order.
        for _ in 0..8 {
            let _ = release_tx.send(());
        }
        drop(h);
        let written = delivered.load(Ordering::Relaxed) as u64;
        assert_eq!(written + dropped, 50, "nothing is lost silently");
    }

    /// Dropping a historian must not hang the process on a stuck disk.
    #[test]
    fn dropping_does_not_wait_for_a_stuck_disk() {
        let (_release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let h = Historian::with_sink(
            1_000_000,
            100,
            4,
            Duration::from_millis(100),
            Box::new(move |_line| {
                let _ = release_rx.recv();
            }),
        );
        h.note_snapshot(&snap(1_000_000, 1.0));
        let started = Instant::now();
        drop(h);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "took {:?}: a stuck disk held up the drop",
            started.elapsed()
        );
    }

    #[test]
    fn persistence_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        {
            let h = Historian::new(1_000_000, 100, Some(dir.path().to_path_buf()));
            h.note_snapshot(&snap(1_000_000, 1.5));
            h.note_snapshot(&snap(2_000_000, 2.5));
        }
        // "Restart": a fresh historian preloads the segments.
        let h2 = Historian::new(1_000_000, 100, Some(dir.path().to_path_buf()));
        let resp = h2.query(&["flow".into()], 0, 0, 1_000);
        let pts = &resp.series[0].points;
        assert_eq!(pts.len(), 2, "history survived the restart");
        assert_eq!(pts[1].v, 2.5);
    }

    #[test]
    fn stale_quality_survives_bucketing_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let h = Historian::new(1_000_000, 100, Some(dir.path().to_path_buf()));
        h.note_snapshot(&snap(1_000_000, 1.5));
        let mut stale = snap(2_000_000, 1.5);
        stale.vars[0].input = Some(crate::InputQuality {
            device: "meter".into(),
            channel: "flow".into(),
            stale: true,
        });
        h.note_snapshot(&stale);
        h.note_snapshot(&snap(3_000_000, 2.5));
        let all = h.query(&[], 0, 0, 10_000);
        assert_eq!(
            all.series[0].points[0].stale,
            Some(true),
            "mixed bucket is not fresh"
        );
        drop(h);
        let h = Historian::new(1_000_000, 100, Some(dir.path().to_path_buf()));
        let points = &h.query(&[], 0, 0, 1_000).series[0].points;
        assert_eq!(points.len(), 3);
        assert_eq!(points[0].stale, None);
        assert_eq!(points[1].stale, Some(true));
        assert_eq!(points[2].stale, None, "recovery resumes fresh samples");
    }
}
