//! Opt-in, bounded scan diagnostics. No formatting or filesystem I/O on the
//! scan thread. This is experimental instrumentation, not a realtime claim.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const CAPACITY: usize = 4096;

#[derive(Clone, Copy)]
pub(crate) struct Miss {
    pub unit: usize,
    pub scan: u64,
    pub total: u64,
    pub interval: Duration,
    // due, tick entry, input start, inputs done, logic start, logic done, finish
    pub points: [Instant; 7],
    pub sleep: Option<[Instant; 3]>, // requested, intended return, actual return
}

struct Clock {
    anchor: Instant,
    mono_ns: u64,
    uncertainty_ns: u64,
}
impl Clock {
    fn capture() -> io::Result<Self> {
        let before = Instant::now();
        // SAFETY: clock_gettime writes a valid timespec through this pointer.
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let after = Instant::now();
        let width = after.duration_since(before);
        Ok(Self {
            anchor: before + width / 2,
            mono_ns: ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64,
            uncertainty_ns: width.as_nanos() as u64,
        })
    }
    fn ns(&self, at: Instant) -> u64 {
        if at >= self.anchor {
            self.mono_ns
                .saturating_add((at - self.anchor).as_nanos() as u64)
        } else {
            self.mono_ns
                .saturating_sub((self.anchor - at).as_nanos() as u64)
        }
    }
}

#[derive(Serialize)]
struct Record {
    schema: &'static str,
    kind: &'static str,
    pid: u32,
    tid: u64,
    unit: usize,
    scan: u64,
    total: u64,
    interval_ns: u64,
    due_ns: u64,
    tick_ns: u64,
    input_start_ns: u64,
    inputs_done_ns: u64,
    logic_start_ns: u64,
    logic_done_ns: u64,
    end_ns: u64,
    deadline_ns: u64,
    late_ns: u64,
    start_delay_ns: u64,
    body_ns: u64,
    input_ns: u64,
    force_ns: u64,
    logic_ns: u64,
    output_and_accounting_ns: u64,
    sleep_ns: Option<[u64; 3]>,
    clock_uncertainty_ns: u64,
}
impl Record {
    fn new(m: Miss, clock: &Clock, tid: u64) -> Self {
        let p = m.points.map(|x| clock.ns(x));
        let interval_ns = m.interval.as_nanos() as u64;
        let deadline_ns = p[0].saturating_add(interval_ns);
        Self {
            schema: "ia2.scan.v1",
            kind: "miss",
            pid: std::process::id(),
            tid,
            unit: m.unit,
            scan: m.scan,
            total: m.total,
            interval_ns,
            due_ns: p[0],
            tick_ns: p[1],
            input_start_ns: p[2],
            inputs_done_ns: p[3],
            logic_start_ns: p[4],
            logic_done_ns: p[5],
            end_ns: p[6],
            deadline_ns,
            late_ns: p[6].saturating_sub(deadline_ns),
            start_delay_ns: p[2].saturating_sub(p[0]),
            body_ns: p[6].saturating_sub(p[2]),
            input_ns: p[3].saturating_sub(p[2]),
            force_ns: p[4].saturating_sub(p[3]),
            logic_ns: p[5].saturating_sub(p[4]),
            output_and_accounting_ns: p[6].saturating_sub(p[5]),
            sleep_ns: m.sleep.map(|s| s.map(|x| clock.ns(x))),
            clock_uncertainty_ns: clock.uncertainty_ns,
        }
    }
}

pub(crate) struct Recorder {
    sender: Option<mpsc::SyncSender<Miss>>,
    dropped: Arc<AtomicU64>,
}
impl Recorder {
    fn disabled() -> Self {
        Self {
            sender: None,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }
    pub fn from_env(instances: &[String]) -> Self {
        let Some(path) = std::env::var_os("IA2_SCAN_TRACE_JSONL") else {
            return Self::disabled();
        };
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (path, instances);
            tracing::warn!("scan diagnostics require Linux; no diagnostic capture started");
            Self::disabled()
        }
        #[cfg(target_os = "linux")]
        {
            // SAFETY: gettid takes no pointers and has no side effects.
            let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u64;
            let marker = std::env::var_os("IA2_SCAN_TRACE_MARKER");
            match Self::open(
                Path::new(&path),
                marker.as_deref().map(Path::new),
                instances,
                tid,
            ) {
                Ok(recorder) => recorder,
                Err(e) => {
                    tracing::error!(%e, "SCAN DIAGNOSTICS DISABLED; capture is not valid");
                    Self::disabled()
                }
            }
        }
    }
    pub fn point(&self) -> Option<Instant> {
        self.sender.as_ref().map(|_| Instant::now())
    }
    pub fn miss(&self, event: Miss) {
        if let Some(tx) = &self.sender {
            if tx.try_send(event).is_err() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    #[cfg(any(target_os = "linux", test))]
    fn open(
        path: &Path,
        marker: Option<&Path>,
        instances: &[String],
        tid: u64,
    ) -> io::Result<Self> {
        let log = OpenOptions::new().write(true).create_new(true).open(path)?;
        let marker = marker
            .map(|p| OpenOptions::new().write(true).open(p))
            .transpose()?;
        let clock = Clock::capture()?;
        let (tx, rx) = mpsc::sync_channel(CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let d = dropped.clone();
        let names = instances.to_vec();
        std::thread::Builder::new()
            .name("ia2-scan-trace".into())
            .spawn(move || {
                if let Err(e) = write_records(log, marker, rx, d, clock, names, tid) {
                    tracing::error!(%e, "SCAN DIAGNOSTIC WRITER FAILED; capture is incomplete");
                }
            })?;
        Ok(Self {
            sender: Some(tx),
            dropped,
        })
    }
}

#[cfg(any(target_os = "linux", test))]
fn write_records(
    mut log: File,
    mut marker: Option<File>,
    rx: mpsc::Receiver<Miss>,
    dropped: Arc<AtomicU64>,
    clock: Clock,
    names: Vec<String>,
    tid: u64,
) -> io::Result<()> {
    let header = serde_json::json!({"schema":"ia2.scan.v1", "kind":"start", "pid":std::process::id(),
        "tid":tid, "clock":"CLOCK_MONOTONIC", "clock_uncertainty_ns":clock.uncertainty_ns,
        "units":names, "queue_capacity":CAPACITY, "marker_enabled":marker.is_some(), "marker_path":std::env::var("IA2_SCAN_TRACE_MARKER").ok()});
    writeln!(log, "{header}")?;
    let (mut written, mut marker_errors) = (0u64, 0u64);
    loop {
        let end = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(m) => {
                let r = Record::new(m, &clock, tid);
                let line = serde_json::to_string(&r)?;
                writeln!(log, "{line}")?;
                if let Some(marker) = &mut marker {
                    // The marker's kernel timestamp is the writer's emission
                    // time. Attribute using payload end_ns and scan tid.
                    let payload = format!("IA2_SCAN {line}\n");
                    match marker.write(payload.as_bytes()) {
                        Ok(n) if n == payload.len() => (),
                        _ => marker_errors += 1,
                    }
                }
                written += 1;
                false
            }
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => true,
        };
        let status = serde_json::json!({"schema":"ia2.scan.v1", "kind":if end {"end"} else {"health"},
            "mono_ns":clock.ns(Instant::now()), "written":written,
            "dropped":dropped.load(Ordering::Relaxed), "marker_errors":marker_errors});
        writeln!(log, "{status}")?;
        log.flush()?;
        if end {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn miss(base: Instant) -> Miss {
        let p = [0, 2100, 2200, 2300, 2400, 2700, 3000].map(|us| base + Duration::from_micros(us));
        Miss {
            unit: 0,
            scan: 7,
            total: 2,
            interval: Duration::from_millis(2),
            points: p,
            sleep: None,
        }
    }
    #[test]
    fn decomposes_late_start_and_body_without_changing_deadline() {
        let base = Instant::now();
        let c = Clock {
            anchor: base,
            mono_ns: 1_000_000_000,
            uncertainty_ns: 20,
        };
        let r = Record::new(miss(base), &c, 42);
        assert_eq!(
            (r.late_ns, r.start_delay_ns, r.body_ns),
            (1_000_000, 2_200_000, 800_000)
        );
        assert_eq!(
            r.input_ns + r.force_ns + r.logic_ns + r.output_and_accounting_ns,
            r.body_ns
        );
        assert_eq!(r.tid, 42);
    }
    #[test]
    fn equality_is_a_zero_lateness_miss_matching_existing_counter() {
        let base = Instant::now();
        let mut m = miss(base);
        m.interval = Duration::from_millis(3);
        let c = Clock {
            anchor: base,
            mono_ns: 4_000_000,
            uncertainty_ns: 0,
        };
        assert_eq!(Record::new(m, &c, 1).late_ns, 0);
    }
    #[test]
    fn maps_times_before_anchor_and_sleep_using_same_clock() {
        let base = Instant::now();
        let mut m = miss(base);
        m.sleep = Some([
            base - Duration::from_millis(2),
            base,
            base + Duration::from_millis(1),
        ]);
        let c = Clock {
            anchor: base,
            mono_ns: 5_000_000,
            uncertainty_ns: 0,
        };
        assert_eq!(
            Record::new(m, &c, 1).sleep_ns,
            Some([3_000_000, 5_000_000, 6_000_000])
        );
    }
    #[test]
    fn full_or_disconnected_queue_never_blocks_and_counts_loss() {
        let (tx, rx) = mpsc::sync_channel(1);
        let d = Arc::new(AtomicU64::new(0));
        let r = Recorder {
            sender: Some(tx),
            dropped: d.clone(),
        };
        r.miss(miss(Instant::now()));
        r.miss(miss(Instant::now()));
        assert_eq!(d.load(Ordering::Relaxed), 1);
        drop(rx);
        r.miss(miss(Instant::now()));
        assert_eq!(d.load(Ordering::Relaxed), 2);
    }
    #[test]
    fn disabled_has_no_timestamps_or_files() {
        assert!(Recorder::disabled().point().is_none());
    }
    #[test]
    fn refuses_to_overwrite_existing_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("misses.jsonl");
        std::fs::write(&p, "original").unwrap();
        assert!(Recorder::open(&p, None, &["main".into()], 1).is_err());
        assert_eq!(std::fs::read_to_string(p).unwrap(), "original");
    }
    #[test]
    fn writes_each_miss_and_terminal_loss_summary() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("misses.jsonl");
        let r = Recorder::open(&p, None, &["main".into()], 17).unwrap();
        r.miss(miss(Instant::now()));
        r.miss(miss(Instant::now()));
        drop(r);
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            let text = std::fs::read_to_string(&p).unwrap();
            if text.lines().any(|x| x.contains("\"kind\":\"end\"")) {
                let records: Vec<serde_json::Value> = text
                    .lines()
                    .map(|l| serde_json::from_str(l).unwrap())
                    .collect();
                assert_eq!(records.iter().filter(|r| r["kind"] == "miss").count(), 2);
                assert_eq!(records.last().unwrap()["dropped"], 0);
                break;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
