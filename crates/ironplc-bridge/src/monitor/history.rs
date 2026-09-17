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
use std::sync::Mutex;

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
    series: HashMap<String, Vec<(u64, f64)>>, // ring via rotate-trim
    last_sample_us: u64,
    persist: Option<Persist>,
}

struct Persist {
    dir: PathBuf,
    file: Option<std::fs::File>,
    written: u64,
}

/// Thread-safe historian handle; cheap to share behind an Arc.
pub struct Historian {
    inner: Mutex<Inner>,
    capacity: usize,
    sample_interval_us: u64,
}

impl Historian {
    pub fn new(sample_interval_us: u64, capacity: usize, persist_dir: Option<PathBuf>) -> Self {
        let mut inner = Inner {
            series: HashMap::new(),
            last_sample_us: 0,
            persist: None,
        };
        if let Some(dir) = persist_dir {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::warn!(%e, dir = %dir.display(), "history dir unavailable — memory only");
            } else {
                preload(&mut inner.series, &dir, capacity);
                inner.persist = Some(Persist {
                    dir,
                    file: None,
                    written: 0,
                });
            }
        }
        Self {
            inner: Mutex::new(inner),
            capacity,
            sample_interval_us: sample_interval_us.max(100_000),
        }
    }

    /// Zero-config in-memory historian (IDE-side default).
    pub fn in_memory() -> Self {
        Self::new(DEFAULT_SAMPLE_INTERVAL_US, DEFAULT_CAPACITY, None)
    }

    /// Drop every ring (project closed). Persistence files are left
    /// alone — they belong to the edge's post-mortem story.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().expect("historian lock");
        inner.series.clear();
        inner.last_sample_us = 0;
    }

    /// Feed one live snapshot; throttled internally to the sample
    /// cadence. Non-numeric variables are skipped (BOOL records 0/1).
    pub fn note_snapshot(&self, snap: &VarSnapshot) {
        let mut inner = self.inner.lock().expect("historian lock");
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
        for var in &snap.vars {
            let tv = typed_value(&var.type_name, var.bits, &var.value);
            let v = tv
                .as_f64()
                .or_else(|| tv.as_bool().map(|b| if b { 1.0 } else { 0.0 }));
            let Some(v) = v else { continue };
            let ring = inner.series.entry(var.name.clone()).or_default();
            ring.push((snap.timestamp_us, v));
            if ring.len() > self.capacity {
                let excess = ring.len() - self.capacity;
                ring.drain(..excess);
            }
            line.insert(var.name.clone(), serde_json::json!(v));
        }

        if let Some(persist) = inner.persist.as_mut() {
            persist_line(persist, snap.timestamp_us, &line);
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
        let inner = self.inner.lock().expect("historian lock");
        let step_us = (step_ms.max(1)) * 1000;
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
                if let Some((t, _)) = samples.first() {
                    oldest = oldest.min(*t);
                }
                let mut points: Vec<HistoryPoint> = Vec::new();
                for (t, v) in samples {
                    if *t < from_us || (to_us > 0 && *t > to_us) {
                        continue;
                    }
                    let bucket = t - (t - from_us) % step_us;
                    match points.last_mut() {
                        Some(p) if p.t_us == bucket => {
                            p.min = p.min.min(*v);
                            p.max = p.max.max(*v);
                            p.v = *v;
                        }
                        _ => points.push(HistoryPoint {
                            t_us: bucket,
                            min: *v,
                            max: *v,
                            v: *v,
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
/// the scan loop down.
fn persist_line(p: &mut Persist, t_us: u64, values: &serde_json::Map<String, serde_json::Value>) {
    if p.file.is_none() || p.written > SEGMENT_MAX_BYTES {
        rotate(p);
    }
    let Some(file) = p.file.as_mut() else { return };
    let line = serde_json::json!({ "t": t_us, "v": values }).to_string();
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
fn preload(series: &mut HashMap<String, Vec<(u64, f64)>>, dir: &PathBuf, capacity: usize) {
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
                    ring.push((t, v));
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
            vars: vec![VarValue {
                name: "flow".into(),
                type_name: "REAL".into(),
                value: format!("{v}"),
                bits: v.to_bits() as u64,
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
}
