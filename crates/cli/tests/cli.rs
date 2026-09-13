//! End-to-end tests for the `cs` CLI.
//!
//! We use `assert_cmd` to invoke the compiled binary as a real
//! subprocess, then check stdout / stderr / exit code. This is the
//! same view an agent gets — if these tests pass, the agent-facing
//! contract is intact.
//!
//! The online surface (quartet, api, error contract) is tested against
//! a minimal in-process HTTP mock (`MockServer`), so these tests need
//! no running ia2-server.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;

use assert_cmd::Command;
use predicates::str::contains;

fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    p
}

fn good_ld() -> PathBuf {
    fixture("good.ld.json")
}
fn bad_ld() -> PathBuf {
    fixture("bad.ld.json")
}
fn good_fbd() -> PathBuf {
    fixture("good.fbd.json")
}
fn bad_fbd() -> PathBuf {
    fixture("bad.fbd.json")
}
fn good_sfc() -> PathBuf {
    fixture("good.sfc.json")
}
fn bad_sfc() -> PathBuf {
    fixture("bad.sfc.json")
}

fn cs() -> Command {
    Command::cargo_bin("cs").expect("compiled cs binary should exist")
}

// ================================================================
//  Offline analysis: check / transpile / symbols (unchanged surface)
// ================================================================

#[test]
fn check_clean_file_exits_zero() {
    cs().arg("check")
        .arg(good_ld())
        .assert()
        .success()
        .stderr(contains("clean"));
}

#[test]
fn check_dirty_file_exits_one_and_reports_diagnostic() {
    let out = cs().arg("check").arg(bad_ld()).assert().code(1);
    let assert = out.get_output();
    let stderr = String::from_utf8_lossy(&assert.stderr);
    assert!(
        stderr.contains("Variable not defined"),
        "expected the undefined-var diagnostic in stderr; got:\n{stderr}",
    );
    assert!(
        stderr.contains("rung loose · coil 0"),
        "ld_location should be printed in human mode; got:\n{stderr}",
    );
}

#[test]
fn check_json_mode_emits_structured_payload() {
    let out = cs()
        .arg("check")
        .arg(bad_ld())
        .arg("--json")
        .assert()
        .code(1);
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("--json output must be valid JSON");
    assert_eq!(v["ok"], false);
    let diag = &v["files"][0]["diagnostics"][0];
    assert_eq!(diag["ld_location"]["kind"], "coil");
    assert_eq!(diag["ld_location"]["rung_id"], "loose");
    assert_eq!(diag["ld_location"]["coil_index"], 0);
}

#[test]
fn check_multiple_files_aggregates_results() {
    cs().arg("check")
        .arg(good_ld())
        .arg(bad_ld())
        .arg("--json")
        .assert()
        .code(1) // any-error policy
        .stdout(contains("\"ok\": false"))
        .stdout(contains("\"diagnostics\": []"))
        .stdout(contains("nope"));
}

#[test]
fn check_problem_code_prints_doc_exits_zero() {
    // `cs check P4007` — a problem code is a valid check target; the
    // full RST explanation prints on stdout.
    let out = cs().arg("check").arg("P4007").assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    assert!(stdout.starts_with("P4007"), "got:\n{stdout}");
    assert!(
        stdout.to_lowercase().contains("variable"),
        "expected RST body to mention 'variable'; got:\n{stdout}"
    );
}

#[test]
fn check_unknown_problem_code_exits_one() {
    cs().arg("check")
        .arg("P99999")
        .assert()
        .code(1)
        .stderr(contains("no documentation"));
}

#[test]
fn check_explain_appends_problem_doc_to_human_output() {
    let out = cs()
        .arg("check")
        .arg(bad_ld())
        .arg("--explain")
        .assert()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr);
    assert!(stderr.contains("P4007"), "got:\n{stderr}");
    assert!(
        stderr.contains("variable=nope") || stderr.contains("nope"),
        "expected context line mentioning the offending var; got:\n{stderr}"
    );
    assert!(
        stderr.to_lowercase().contains("example"),
        "explanation should include the RST 'Example' section; got:\n{stderr}"
    );
}

#[test]
fn check_json_includes_context_related_explanation() {
    let out = cs()
        .arg("check")
        .arg(bad_ld())
        .arg("--json")
        .assert()
        .code(1);
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let diag = &v["files"][0]["diagnostics"][0];
    if !diag["context"].is_null() {
        assert!(diag["context"].is_array());
    }
    if !diag["related"].is_null() {
        assert!(diag["related"].is_array());
    }
    let expl = diag["explanation"]
        .as_str()
        .expect("ironplc P-codes must carry an embedded explanation");
    assert!(
        expl.to_lowercase().contains("variable"),
        "explanation should describe the error: {expl}"
    );
    let ctx = diag["context"]
        .as_array()
        .expect("P4007 always carries a `variable=…` context line");
    assert!(
        ctx.iter()
            .any(|c| c.as_str().unwrap_or("").contains("nope")),
        "expected `variable=nope` in context, got: {ctx:?}"
    );
}

#[test]
fn check_unknown_extension_is_usage_error() {
    let tmp = tempfile::NamedTempFile::with_suffix(".plc").unwrap();
    cs().arg("check")
        .arg(tmp.path())
        .assert()
        // exit 2: fix your invocation, not your source.
        .code(2)
        .stderr(contains("can't infer language"));
}

#[test]
fn symbols_lists_variables_and_fb_instances() {
    let out = cs()
        .arg("symbols")
        .arg(good_fbd())
        .arg("--json")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let arr = v.as_array().unwrap();
    let names: Vec<&str> = arr.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"tick"));
    assert!(names.contains(&"rst"));
    assert!(names.contains(&"done"));
    assert!(names.contains(&"rt"));
    assert!(names.contains(&"cu"));
    let directions: Vec<&str> = arr
        .iter()
        .map(|s| s["direction"].as_str().unwrap())
        .collect();
    assert!(directions.contains(&"fb_instance"));
}

#[test]
fn symbols_name_filter_narrows_results() {
    let out = cs()
        .arg("symbols")
        .arg(good_fbd())
        .arg("--name")
        .arg("tick")
        .arg("--json")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1, "expected one match for 'tick': {arr:?}");
    assert_eq!(arr[0]["name"], "tick");
}

#[test]
fn symbols_filter_with_no_match_exits_one() {
    cs().arg("symbols")
        .arg(good_fbd())
        .arg("--name")
        .arg("XX-NO-SUCH-SYMBOL")
        .assert()
        .code(1);
}

#[test]
fn transpile_ld_emits_st_on_stdout() {
    let out = cs().arg("transpile").arg(good_ld()).assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    assert!(stdout.contains("PROGRAM good"), "got:\n{stdout}");
    assert!(stdout.contains("VAR_INPUT"));
    assert!(
        stdout.contains("armTimer(IN := start_btn"),
        "FB call should be present; got:\n{stdout}"
    );
}

#[test]
fn transpile_with_map_includes_source_map() {
    let out = cs()
        .arg("transpile")
        .arg(good_ld())
        .arg("--with-map")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(v["st"].is_string(), "st field must be a string");
    assert!(v["source_map"].is_array(), "source_map must be an array");
    let map = v["source_map"].as_array().unwrap();
    let has_variable = map.iter().any(|e| !e.is_null() && e["kind"] == "variable");
    let has_rung_or_coil = map
        .iter()
        .any(|e| !e.is_null() && (e["kind"] == "rung" || e["kind"] == "coil"));
    assert!(has_variable && has_rung_or_coil, "got:\n{stdout}");
}

#[test]
fn transpile_st_file_echoes_source() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let st = tmp_dir.path().join("foo.st");
    fs::write(&st, "PROGRAM foo\n  VAR x : BOOL; END_VAR\nEND_PROGRAM\n").unwrap();
    let out = cs().arg("transpile").arg(&st).assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    assert!(stdout.contains("PROGRAM foo"));
}

#[test]
fn fbd_check_clean_file_exits_zero() {
    cs().arg("check")
        .arg(good_fbd())
        .assert()
        .success()
        .stderr(contains("clean"));
}

#[test]
fn fbd_check_dirty_file_reports_fbd_location() {
    let out = cs()
        .arg("check")
        .arg(bad_fbd())
        .arg("--json")
        .assert()
        .code(1);
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let diag = &v["files"][0]["diagnostics"][0];
    assert!(diag["ld_location"].is_null());
    assert_eq!(diag["fbd_location"]["kind"], "block");
    assert_eq!(diag["fbd_location"]["block_id"], "b0");
}

#[test]
fn fbd_transpile_emits_topo_sorted_calls() {
    let out = cs().arg("transpile").arg(good_fbd()).assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    assert!(stdout.contains("rt : R_TRIG;"), "got:\n{stdout}");
    assert!(stdout.contains("cu : CTU;"), "got:\n{stdout}");
    assert!(stdout.contains("cu(CU := rt.Q"), "got:\n{stdout}");
    let edge = stdout.find("rt(CLK").unwrap();
    let counter = stdout.find("cu(CU").unwrap();
    assert!(edge < counter, "edge block must execute before counter");
    assert!(stdout.contains("done := cu.Q;"), "got:\n{stdout}");
}

#[test]
fn sfc_check_clean_file_exits_zero() {
    cs().arg("check")
        .arg(good_sfc())
        .assert()
        .success()
        .stderr(contains("clean"));
}

#[test]
fn sfc_check_dirty_file_reports_sfc_location() {
    let out = cs()
        .arg("check")
        .arg(bad_sfc())
        .arg("--json")
        .assert()
        .code(1);
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let diag = &v["files"][0]["diagnostics"][0];
    assert!(diag["ld_location"].is_null());
    assert!(diag["fbd_location"].is_null());
    assert_eq!(diag["sfc_location"]["kind"], "action");
    assert_eq!(diag["sfc_location"]["step"], "running");
    assert_eq!(diag["sfc_location"]["action_index"], 0);
}

#[test]
fn sfc_transpile_emits_step_dispatch_and_transition_cascade() {
    let out = cs().arg("transpile").arg(good_sfc()).assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    assert!(
        stdout.contains("__sfc_step : STRING[31] := 'idle';"),
        "got:\n{stdout}"
    );
    assert!(
        stdout.contains("IF __sfc_step = 'filling' THEN"),
        "got:\n{stdout}"
    );
    assert!(stdout.contains("inlet := TRUE;"), "got:\n{stdout}");
    assert!(
        stdout.contains("IF __sfc_step = 'idle' AND (start_btn) THEN"),
        "got:\n{stdout}"
    );
    assert!(
        stdout.contains("ELSIF __sfc_step = 'filling' AND (tank_full) THEN"),
        "got:\n{stdout}"
    );
    let actions_pos = stdout.find("(* === SFC actions === *)").unwrap();
    let snap_pos = stdout.find("__sfc_prev := __sfc_step;").unwrap();
    let trans_pos = stdout.find("(* === SFC transitions === *)").unwrap();
    assert!(actions_pos < snap_pos && snap_pos < trans_pos);
}

// ================================================================
//  Offline project tools
// ================================================================

#[test]
fn project_info_lists_pous_and_devices() {
    let proj = setup_demo_project();
    let out = cs()
        .arg("project")
        .arg("info")
        .arg(proj.path())
        .arg("--json")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["name"], "smoke");
    let pous = v["pous"].as_array().unwrap();
    assert_eq!(pous.len(), 1);
    assert_eq!(pous[0], "main");
}

#[test]
fn project_check_clean_exits_zero() {
    let proj = setup_demo_project();
    cs().arg("project")
        .arg("check")
        .arg(proj.path())
        .assert()
        .success()
        .stderr(contains("compiles cleanly"));
}

// ================================================================
//  The quartet + api + error contract, against a mock server
// ================================================================

/// Minimal canned-response HTTP server: each entry is
/// (method, path-prefix, status, body). Handles up to `max` requests
/// then stops. Records every request line + headers for assertions.
struct MockServer {
    addr: String,
    handle: Option<std::thread::JoinHandle<Vec<String>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl MockServer {
    fn start(routes: Vec<(&'static str, &'static str, u16, String)>, max: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_server = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..max {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                if stop_server.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                // Read until end of headers.
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(_) => break,
                    }
                }
                let head_end = buf
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|p| p + 4)
                    .unwrap_or(buf.len());
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                // Drain the body if Content-Length says there is one.
                let content_len = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                let mut body = buf[head_end..].to_vec();
                while body.len() < content_len {
                    match stream.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => body.extend_from_slice(&tmp[..n]),
                        Err(_) => break,
                    }
                }
                seen.push(head.clone());

                let request_line = head.lines().next().unwrap_or_default().to_string();
                let mut parts = request_line.split(' ');
                let method = parts.next().unwrap_or_default();
                let path = parts.next().unwrap_or_default();
                let (status, resp_body) = routes
                    .iter()
                    .find(|(m, p, _, _)| *m == method && path.starts_with(p))
                    .map(|(_, _, s, b)| (*s, b.clone()))
                    .unwrap_or((404, "{\"error\":\"mock: no route\"}".to_string()));
                let reason = match status {
                    200 => "OK",
                    404 => "Not Found",
                    422 => "Unprocessable Entity",
                    500 => "Internal Server Error",
                    _ => "X",
                };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                    resp_body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
            seen
        });
        MockServer {
            addr,
            handle: Some(handle),
            stop,
        }
    }

    /// Stop accepting and return every request head seen so far.
    fn finish(mut self) -> Vec<String> {
        // Poke the listener so the accept loop can exit if it's still
        // waiting on an accept that will never come.
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = std::net::TcpStream::connect(self.addr.trim_start_matches("http://"));
        self.handle.take().unwrap().join().unwrap()
    }
}

#[test]
fn agent_run_closes_session_when_command_cannot_start() {
    let mock = MockServer::start(
        vec![
            ("POST", "/api/agent/session/start", 200, "{}".into()),
            ("POST", "/api/agent/session/end", 200, "{}".into()),
            ("POST", "/api/agent/heartbeat", 200, "{}".into()),
        ],
        8, // The heartbeat keeper may run before or after the failed spawn.
    );
    let missing = tempfile::tempdir().unwrap().path().join("missing-command");
    cs().arg("--server")
        .arg(&mock.addr)
        .args(["agent", "run", "--label", "spawn failure", "--"])
        .arg(missing)
        .assert()
        .code(3)
        .stderr(contains("spawning"));
    let seen = mock.finish();
    assert!(seen
        .iter()
        .any(|r| r.starts_with("POST /api/agent/session/start ")));
    assert!(seen
        .iter()
        .any(|r| r.starts_with("POST /api/agent/session/end ")));
}

#[test]
fn server_4xx_body_reaches_stderr_and_exits_two() {
    let mock = MockServer::start(
        vec![(
            "PUT",
            "/api/iomap",
            422,
            "missing field `application` — every mapping names its PROGRAM".to_string(),
        )],
        2, // announce heartbeat + PUT
    );
    cs().arg("--server")
        .arg(&mock.addr)
        .arg("set")
        .arg("iomap")
        .arg("--from")
        .arg("-")
        .write_stdin("{\"mappings\":[]}")
        .assert()
        .code(2)
        .stderr(contains("missing field `application`"));
    mock.finish();
}

#[test]
fn server_5xx_exits_three() {
    let mock = MockServer::start(vec![("GET", "/api/iomap", 500, "boom".to_string())], 1);
    cs().arg("--server")
        .arg(&mock.addr)
        .arg("get")
        .arg("iomap")
        .assert()
        .code(3)
        .stderr(contains("boom"));
    mock.finish();
}

#[test]
fn connection_refused_exits_three() {
    // Nothing listens on this port (bind then drop to reserve-and-free).
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    cs().arg("--server")
        .arg(&addr)
        .arg("get")
        .arg("iomap")
        .assert()
        .code(3)
        .stderr(contains("is the server running?"));
}

#[test]
fn get_pou_prints_raw_source() {
    let src = "PROGRAM main\n  VAR x : BOOL; END_VAR\nEND_PROGRAM\n";
    let body = serde_json::json!({ "path": "main", "source": src, "declarations": [] });
    let mock = MockServer::start(vec![("GET", "/api/pous/main", 200, body.to_string())], 1);
    let out = cs()
        .arg("--server")
        .arg(&mock.addr)
        .arg("get")
        .arg("pous/main.st")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    assert_eq!(stdout, src, "raw source must round-trip byte-for-byte");
    mock.finish();
}

#[test]
fn project_flag_adds_header_on_every_request() {
    let mock = MockServer::start(
        vec![("PUT", "/api/tasks", 200, "{\"ok\":true}".to_string())],
        2, // announce heartbeat + PUT
    );
    cs().arg("--server")
        .arg(&mock.addr)
        .arg("--project")
        .arg("lineB")
        .arg("set")
        .arg("tasks")
        .arg("--from")
        .arg("-")
        .write_stdin("{\"tasks\":[]}")
        .assert()
        .success();
    let seen = mock.finish();
    assert!(
        seen.iter()
            .any(|h| h.to_ascii_lowercase().contains("x-ia2-project: lineb")),
        "X-IA2-Project header missing; requests seen:\n{seen:?}"
    );
}

#[test]
fn set_pou_creates_then_puts_when_missing() {
    // GET → 404 (doesn't exist), POST create → 200, PUT source → 200.
    let mock = MockServer::start(
        vec![
            ("GET", "/api/pous/newpou", 404, "no such pou".to_string()),
            (
                "POST",
                "/api/pous",
                200,
                "{\"path\":\"newpou\"}".to_string(),
            ),
            ("PUT", "/api/pous/newpou", 200, "{\"ok\":true}".to_string()),
        ],
        4, // announce heartbeat + GET + POST + PUT
    );
    cs().arg("--server")
        .arg(&mock.addr)
        .arg("set")
        .arg("pous/newpou.st")
        .arg("--from")
        .arg("-")
        .write_stdin("PROGRAM newpou\nEND_PROGRAM\n")
        .assert()
        .success()
        .stderr(contains("✓ set pous/newpou.st"));
    let seen: Vec<String> = mock
        .finish()
        .into_iter()
        .filter(|h| !h.contains("/api/agent/heartbeat"))
        .collect();
    assert_eq!(seen.len(), 3, "expected GET, POST, PUT; saw:\n{seen:?}");
    assert!(seen[0].starts_with("GET /api/pous/newpou"));
    assert!(seen[1].starts_with("POST /api/pous"));
    assert!(seen[2].starts_with("PUT /api/pous/newpou"));
}

#[test]
fn api_escape_hatch_posts_anywhere() {
    let mock = MockServer::start(
        vec![(
            "POST",
            "/api/edges/pi/attach",
            200,
            "{\"attached\":true}".to_string(),
        )],
        2, // announce heartbeat + POST
    );
    cs().arg("--server")
        .arg(&mock.addr)
        .arg("api")
        .arg("POST")
        .arg("/api/edges/pi/attach")
        .assert()
        .success()
        .stdout(contains("\"attached\": true"));
    mock.finish();
}

#[test]
fn ls_with_unknown_kind_is_usage_error() {
    cs().arg("ls")
        .arg("gadgets")
        .assert()
        .code(2)
        .stderr(contains("known resource kinds"));
}

#[test]
fn ls_root_prints_kind_overview_without_server() {
    // The overview itself is static; the open-projects suffix is
    // best-effort and silently skipped when no server answers.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    cs().arg("--server")
        .arg(&addr)
        .arg("ls")
        .assert()
        .success()
        .stdout(contains("pous"))
        .stdout(contains("devices"))
        .stdout(contains("iomap"));
}

/// Build a minimum viable project on a tempdir — one POU, no devices,
/// a single task binding `main` so `compile_project` succeeds.
fn setup_demo_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("project.toml"),
        "name = \"smoke\"\nversion = \"0.1\"\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("tasks.toml"),
        r#"
[[tasks]]
name = "t1"
interval_ms = 100
priority = 1

[[programs]]
instance = "main"
program = "main"
task = "t1"
"#,
    )
    .unwrap();
    fs::create_dir(dir.path().join("pous")).unwrap();
    fs::write(
        dir.path().join("pous/main.st"),
        "PROGRAM main\n  VAR x : BOOL; END_VAR\n  x := TRUE;\nEND_PROGRAM\n",
    )
    .unwrap();
    fs::create_dir(dir.path().join("devices")).unwrap();
    fs::create_dir(dir.path().join("edges")).unwrap();
    fs::write(dir.path().join("iomap.toml"), "[[mappings]]\n").unwrap();
    dir
}

#[test]
fn project_flag_encodes_unicode_and_literal_percent_for_http_headers() {
    let mock = MockServer::start(vec![("GET", "/api/project", 200, "{}".to_owned())], 1);
    cs().arg("--server")
        .arg(&mock.addr)
        .arg("--project")
        .arg("控制 %20")
        .args(["get", "project"])
        .assert()
        .success();
    let seen = mock.finish();
    assert!(
        seen.iter()
            .any(|h| h.contains("%E6%8E%A7%E5%88%B6%20%2520")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|h| h
            .to_ascii_lowercase()
            .contains("x-ia2-project-encoding: percent")),
        "{seen:?}"
    );
}
