//! The runtime owns the last recorder reference after draining the bridge.
//! Exercise the complete process shutdown on the existing `_sim` fixture.

use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CAPTURE: &str = "/axis-capture/bus";
const ARCHIVE: &str = "axis-capture/last-shutdown.json";

struct Edge {
    child: Child,
    port: u16,
}

impl Drop for Edge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn eventually<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < until, "timed out waiting for edge fixture");
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl Edge {
    fn start(state: &Path, log: &Path) -> Self {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/axis_capture_sim");
        let edge = Self {
            port,
            child: Command::new(env!("CARGO_BIN_EXE_ia2-runtime"))
                .args(["--bind", &format!("127.0.0.1:{port}"), "--project-dir"])
                .arg(fixture)
                .arg("--state-dir")
                .arg(state)
                .env("RUST_LOG", "info")
                .env_remove("IA2_ALLOWED_ORIGINS")
                .stdout(fs::File::create(log).unwrap())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        };
        edge.bus();
        edge
    }

    fn request(&self, method: &str, path: &str, body: &str) -> std::io::Result<(u16, String)> {
        let mut sock = TcpStream::connect(("127.0.0.1", self.port))?;
        sock.set_read_timeout(Some(Duration::from_secs(3)))?;
        write!(
            sock,
            "{method} {path} HTTP/1.1\r\nhost: 127.0.0.1:{}\r\n\
             content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            self.port,
            body.len(),
        )?;
        let mut response = String::new();
        sock.read_to_string(&mut response)?;
        let (head, payload) = response.split_once("\r\n\r\n").expect("HTTP headers");
        Ok((
            head.split_whitespace().nth(1).unwrap().parse().unwrap(),
            payload.into(),
        ))
    }

    fn bus(&self) -> Value {
        eventually(|| {
            let (status, text) = self.request("GET", CAPTURE, "").ok()?;
            if status != 200 {
                return None;
            }
            let value: Value = serde_json::from_str(&text).unwrap();
            value["bus"].is_object().then_some(value)
        })
    }

    fn arm(&self, limit: usize) -> Value {
        let bus = self.bus();
        let body = json!({
            "runtime_id": bus["runtime_id"],
            "bus_generation": bus["bus"]["generation"],
            "slave": 0,
            "counts_per_revolution": 8388608,
            "max_samples": limit,
            "duration_ms": 30000
        });
        let (status, text) = self.request("POST", CAPTURE, &body.to_string()).unwrap();
        assert_eq!(status, 201, "{text}");
        serde_json::from_str(&text).unwrap()
    }

    fn shutdown(mut self) {
        assert_eq!(self.request("POST", "/stop", "").unwrap().0, 200);
        let status = eventually(|| self.child.try_wait().unwrap());
        assert!(status.success(), "runtime shutdown returned {status}");
    }
}

#[test]
fn shutdown_preserves_the_active_capture_after_the_bridge_drains() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let log = temp.path().join("log");
    let edge = Edge::start(&state, &log);
    let start = edge.arm(10_000);
    eventually(|| {
        let progress = edge.bus()["capture"].clone();
        assert!(progress["end_reason"].is_null());
        (progress["samples"].as_u64().unwrap() >= 4).then_some(())
    });
    edge.shutdown();

    let path = state.join(ARCHIVE);
    let archive = fs::read(&path).unwrap();
    let raw: Value = serde_json::from_slice(&archive).unwrap();
    assert_eq!(raw["scope"], start["scope"]);
    assert_eq!(raw["capture_id"], start["capture_id"]);
    assert_eq!(raw["end_reason"], "worker_stopped");
    assert_eq!(raw["simulated"], true);
    assert!(raw["samples"].as_array().unwrap().len() >= 4);
    let log_text = fs::read_to_string(&log).unwrap();
    assert!(
        log_text.find("bridge drained;").unwrap()
            < log_text
                .find("axis capture saved after bridge drain")
                .unwrap()
    );

    Edge::start(&state, &log).shutdown();
    assert_eq!(archive, fs::read(path).unwrap());
}

#[test]
fn shutdown_keeps_a_frozen_archive_byte_for_byte() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let edge = Edge::start(&state, &temp.path().join("log"));
    let start = edge.arm(8);
    eventually(|| {
        let progress = edge.bus()["capture"].clone();
        (progress["end_reason"] == "sample_limit").then_some(())
    });
    let path = format!("{CAPTURE}/{}", start["capture_id"].as_str().unwrap());
    let (status, download) = edge.request("GET", &path, "").unwrap();
    assert_eq!(status, 200);
    edge.shutdown();
    assert_eq!(fs::read(state.join(ARCHIVE)).unwrap(), download.as_bytes());
}
