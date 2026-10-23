//! End-to-end tests against a REAL `server` binary, the bundled
//! `examples/sim_smoke` project, and the cs binary as a subprocess.
//!
//! `cs sim run` is the headline case — the exact loop an agent runs to prove
//! generated logic — and anything else that only breaks when the CLI's own
//! request meets the real router belongs here too.
//!
//! Requires the server binary beside `cs`: run `cargo build -p server`
//! first, using the same target/profile as this test. A missing server
//! fails the test instead of silently leaving the scenario unverified.
//!
//! Start servers only through `spawn_server` and open projects only through
//! `TestServer::open_project`: the server persists per-user state, and those
//! two keep it inside the test's tempdir.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Stdio};

use assert_cmd::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn server_binary() -> PathBuf {
    // assert_cmd resolves custom CARGO_TARGET_DIR, target triples, and
    // release profiles. The server is a sibling, including `.exe` on Windows.
    let cs = assert_cmd::cargo::cargo_bin("cs");
    let p = cs
        .parent()
        .expect("cs binary directory")
        .join(format!("server{}", std::env::consts::EXE_SUFFIX));
    assert!(
        p.is_file(),
        "sim_e2e requires {}: run cargo build -p server with the same target/profile first",
        p.display()
    );
    p
}

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn cs(server: &str) -> Command {
    let mut c = Command::cargo_bin("cs").expect("cs binary");
    c.arg("--server").arg(server);
    c
}

/// A running server whose per-user state lives under `sandbox`.
struct TestServer {
    base: String,
    sandbox: PathBuf,
    home: PathBuf,
    _child: ServerGuard,
}

/// Start a real server on a free loopback port and wait for `/health`.
///
/// Opening a project writes `last_opened` to `dirs::config_dir()/IA2/state.toml`
/// and the open-projects list to `default_projects_dir()`, and startup reads
/// both back. With the developer's real home, every `cargo test` pointed their
/// state file at a tempdir that no longer exists, and the test server started
/// with their real projects open. `dirs` derives those paths from `HOME`
/// (plus `XDG_CONFIG_HOME` on Linux, where the projects dir without a
/// `user-dirs.dirs` falls back to `./projects` under the working directory),
/// so all three point into `sandbox`. On Windows `dirs` uses known folders,
/// which these variables do not move.
fn spawn_server(sandbox: &Path) -> TestServer {
    spawn_server_with(sandbox, &[], &[])
}

/// [`spawn_server`] with extra command-line arguments and environment variables.
fn spawn_server_with(sandbox: &Path, args: &[&str], envs: &[(&str, &str)]) -> TestServer {
    spawn_server_on(sandbox, "127.0.0.1", args, envs)
}

fn spawn_server_on(
    sandbox: &Path,
    bind_ip: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> TestServer {
    let server_bin = server_binary();

    // Free port: bind-then-drop; the server grabs it a moment later.
    let l = TcpListener::bind((bind_ip, 0)).unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let base = format!("http://127.0.0.1:{port}");

    let home = sandbox.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let child = StdCommand::new(&server_bin)
        .arg("--bind")
        .arg(std::net::SocketAddr::new(bind_ip.parse().unwrap(), port).to_string())
        // No demo slave — keeps the test from fighting over port 5502
        // with a dev server on the same machine.
        .arg("--demo-modbus-addr")
        .arg("")
        .args(args)
        .env_remove("IA2_ALLOWED_ORIGINS")
        .envs(envs.iter().copied())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .current_dir(sandbox)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    let child = ServerGuard(child);

    // Wait for /health (up to ~5 s).
    let mut up = false;
    for _ in 0..50 {
        let ok = cs(&base)
            .arg("api")
            .arg("GET")
            .arg("/health")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            up = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(up, "server did not come up on {base}");

    TestServer {
        base,
        sandbox: sandbox.to_path_buf(),
        home,
        _child: child,
    }
}

impl TestServer {
    /// `POST /api/projects/open`, then prove the state it persisted stayed
    /// in the sandbox.
    fn open_project(&self, proj: &Path) {
        cs(&self.base)
            .arg("api")
            .arg("POST")
            .arg("/api/projects/open")
            .arg("--from")
            .arg("-")
            .write_stdin(serde_json::json!({ "path": proj }).to_string())
            .assert()
            .success();
        self.assert_state_sandboxed(proj);
    }

    /// Checked rather than assumed: a server that ignored `HOME` would still
    /// pass every other assertion here while rewriting the real files.
    fn assert_state_sandboxed(&self, proj: &Path) {
        if cfg!(windows) {
            // Known folders ignore `HOME` (see `spawn_server`), so there is
            // no sandbox to check.
            return;
        }

        // `dirs::config_dir()` for the HOME the server was given.
        let config_dir = if cfg!(target_os = "macos") {
            self.home.join("Library/Application Support")
        } else {
            self.home.join(".config")
        };
        let state_file = config_dir.join("IA2/state.toml");
        let text = std::fs::read_to_string(&state_file).unwrap_or_else(|e| {
            panic!("opening a project must write {}: {e}", state_file.display())
        });
        let state: toml::Table = toml::from_str(&text).unwrap();
        let last_opened = state
            .get("last_opened")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("no last_opened in {}:\n{text}", state_file.display()));
        assert_eq!(
            Path::new(last_opened).canonicalize().unwrap(),
            proj.canonicalize().unwrap(),
            "{} names the project just opened",
            state_file.display()
        );

        // The server's own projects dir, as `GET /api/fs/browse` lists it
        // when no path is given. A relative answer (Linux fallback) is
        // relative to the server's working directory, the sandbox; `join`
        // keeps an absolute one as is.
        let out = cs(&self.base)
            .arg("api")
            .arg("GET")
            .arg("/api/fs/browse")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "GET /api/fs/browse failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let listing: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let projects_dir = self
            .sandbox
            .join(listing["path"].as_str().expect("fs/browse path"));
        assert!(
            projects_dir.starts_with(&self.sandbox),
            "server projects dir {} is outside the sandbox {}",
            projects_dir.display(),
            self.sandbox.display()
        );
        let open_list = projects_dir.join(".ia2-open-projects.json");
        assert!(
            open_list.is_file(),
            "opening a project must write {}",
            open_list.display()
        );
    }
}

#[test]
fn sim_run_proves_and_refutes_against_a_real_server() {
    // Copy the example project to a tempdir so runs never dirty the repo.
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("sim_smoke");
    copy_dir(&repo_root().join("examples/sim_smoke"), &proj);

    // Declared after `tmp`, so the server is stopped before its sandbox
    // is deleted.
    let server = spawn_server(tmp.path());
    server.open_project(&proj);

    // The bundled scenario must pass end to end (fills tank, alarm
    // raises, no overflow) — this is the agent's self-verification loop.
    cs(&server.base)
        .arg("sim")
        .arg("run")
        .arg(proj.join("scenarios/fill.toml"))
        .assert()
        .success()
        .stderr(predicates::str::contains("scenario passed"));

    // And a wrong expectation must FAIL with exit 1 and name the step.
    let bad = tmp.path().join("bad.toml");
    std::fs::write(
        &bad,
        "[[steps]]\nexpect = { var = \"level\", op = \"lt\", value = -1.0, within_ms = 600 }\n",
    )
    .unwrap();
    cs(&server.base)
        .arg("sim")
        .arg("run")
        .arg(&bad)
        .assert()
        .code(1)
        .stderr(predicates::str::contains("scenario FAILED"));
}

#[test]
fn valve_reset_cannot_clear_active_dual_limits() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("valve_reset");
    std::fs::create_dir_all(proj.join("pous")).unwrap();
    std::fs::write(
        proj.join("project.toml"),
        "name = \"valve_reset\"\nversion = \"0.1\"\n",
    )
    .unwrap();
    std::fs::write(proj.join("tasks.toml"), "[[tasks]]\nname = \"fast\"\ninterval_ms = 10\npriority = 1\n[[programs]]\ninstance = \"main\"\nprogram = \"main\"\ntask = \"fast\"\n").unwrap();
    // Compile the actual library block, never a copy of its fault logic.
    std::fs::copy(
        repo_root().join("library/process-control/pous/fb_valve.st"),
        proj.join("pous/fb_valve.st"),
    )
    .unwrap();
    std::fs::write(
        proj.join("pous/main.st"),
        r#"
PROGRAM main
VAR
    valve : FB_VALVE;
    open_request : BOOL;
    close_request : BOOL;
    open_limit : BOOL;
    close_limit : BOOL;
    external_fault : BOOL;
    reset_request : BOOL;
    valve_open : BOOL;
    valve_fault : BOOL;
    valve_status : INT;
END_VAR
valve(cmd_open := open_request, cmd_close := close_request, remote := TRUE,
      zso := open_limit, zsc := close_limit, fault_in := external_fault, reset := reset_request,
      travel_timeout_s := 0.0, dt_s := 0.01);
valve_open := valve.out_open;
valve_fault := valve.fault;
valve_status := valve.status;
END_PROGRAM
"#,
    )
    .unwrap();
    let server = spawn_server(tmp.path());
    server.open_project(&proj);
    cs(&server.base)
        .args(["project", "check"])
        .arg(&proj)
        .assert()
        .success();
    let trace = tmp.path().join("valve-reset.jsonl");
    let result = cs(&server.base)
        .args(["--project", "valve_reset", "sim", "run"])
        .arg(repo_root().join("library/process-control/scenarios/valve-reset.toml"))
        .arg("--trace")
        .arg(&trace)
        .timeout(std::time::Duration::from_secs(20))
        .assert();
    if !result.get_output().status.success() {
        eprintln!(
            "valve reset trace: {}",
            std::fs::read_to_string(trace).unwrap_or_default()
        );
    }
    result
        .success()
        .stderr(predicates::str::contains("scenario passed"));
}

#[test]
fn infinite_loop_scenario_reports_a_fault_and_rejects_a_healthy_run() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("execution_budget");
    copy_dir(&repo_root().join("examples/execution_budget"), &proj);
    let server = spawn_server(tmp.path());
    server.open_project(&proj);
    cs(&server.base)
        .args(["project", "check"])
        .arg(&proj)
        .assert()
        .success();
    let trace = tmp.path().join("fault.jsonl");
    cs(&server.base)
        .args(["--project", "execution_budget", "sim", "run"])
        .arg(proj.join("scenarios/bounded.toml"))
        .arg("--trace")
        .arg(&trace)
        .timeout(std::time::Duration::from_secs(15))
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "runtime stopped with fault: VM execution budget exceeded",
        ));
    assert!(std::fs::read_to_string(trace)
        .unwrap()
        .contains("VM execution budget exceeded"));

    // A fresh run clears the previous fault. A stale snapshot / a healthy
    // running program must not satisfy an expected terminal fault.
    let bad = tmp.path().join("no-fault.toml");
    std::fs::write(&bad, "[[steps]]\nexpect_fault = { contains = \"VM execution budget exceeded\", within_ms = 400 }\n").unwrap();
    cs(&server.base)
        .args(["--project", "execution_budget", "sim", "run"])
        .arg(&bad)
        .timeout(std::time::Duration::from_secs(5))
        .assert()
        .code(1)
        .stderr(predicates::str::contains("last_error=None"));
}

#[test]
fn stale_agent_cannot_remove_another_writers_alarm_or_program_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("sim_smoke");
    copy_dir(&repo_root().join("examples/sim_smoke"), &proj);
    let server = spawn_server(tmp.path());
    server.open_project(&proj);

    cs(&server.base)
        .args(["set", "pous/fresh.st", "--from", "-"])
        .write_stdin("PROGRAM fresh\nEND_PROGRAM\n")
        .assert()
        .success();
    cs(&server.base)
        .args(["get", "pous/fresh.st"])
        .assert()
        .success()
        .stdout("PROGRAM fresh\nEND_PROGRAM\n");

    for resource in ["alarms", "pous/main.st"] {
        let version = tmp
            .path()
            .join(format!("{}.etag", resource.replace('/', "-")));
        let old = cs(&server.base)
            .args(["get", resource, "--etag-file"])
            .arg(&version)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let other = if resource == "alarms" {
            let mut doc: serde_json::Value = serde_json::from_slice(&old).unwrap();
            doc["alarms"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "id":"overflow_trip", "variable":"overflow", "condition":"is_true",
                    "severity":"critical", "message":"Overflow trip"
                }));
            doc.to_string()
        } else {
            format!(
                "{}\n(* another writer's interlock edit *)\n",
                String::from_utf8(old.clone()).unwrap()
            )
        };
        cs(&server.base)
            .args(["set", resource, "--from", "-", "--if-match"])
            .arg(format!("@{}", version.display()))
            .write_stdin(other.clone())
            .assert()
            .success();
        // A second agent/IDE read must NOT replace the first writer's version.
        cs(&server.base).args(["get", resource]).assert().success();
        cs(&server.base)
            .args(["set", resource, "--from", "-", "--if-match"])
            .arg(format!("@{}", version.display()))
            .write_stdin(old)
            .assert()
            .code(2)
            .stderr(predicates::str::contains("HTTP 412"))
            .stderr(predicates::str::contains("re-read"));
        let saved = cs(&server.base)
            .args(["get", resource])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        if resource == "alarms" {
            let saved: serde_json::Value = serde_json::from_slice(&saved).unwrap();
            assert!(saved["alarms"]
                .as_array()
                .unwrap()
                .iter()
                .any(|alarm| alarm["id"] == "overflow_trip"));
        } else {
            assert!(String::from_utf8(saved)
                .unwrap()
                .contains("another writer's interlock edit"));
        }
    }
}

/// A resource name with non-ASCII characters must survive the trip through
/// the CLI's URL encoder and the server's router.
///
/// `url_encode` used to encode CHARS rather than bytes, so `泵` (U+6CF5)
/// became `%6CF5` — the server decodes `%6C` as `l` and leaves `F5`
/// literal, and the lookup misses. Nothing in the project store restricts
/// names to ASCII, and this codebase's operators do not write in ASCII.
#[test]
fn a_non_ascii_resource_name_round_trips_through_the_real_router() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("sim_smoke");
    copy_dir(&repo_root().join("examples/sim_smoke"), &proj);

    let server = spawn_server(tmp.path());
    server.open_project(&proj);

    // Created with a name the CLI never has to encode (JSON body), then
    // read back through the resource path, which it does.
    cs(&server.base)
        .arg("api")
        .arg("POST")
        .arg("/api/devices")
        .arg("--from")
        .arg("-")
        .write_stdin("{\"name\":\"泵1\",\"protocol\":\"modbus\"}")
        .assert()
        .success();

    cs(&server.base)
        .arg("get")
        .arg("devices/泵1")
        .assert()
        .success()
        .stdout(predicates::str::contains("泵1"));

    // And the file really is the one that was asked for.
    assert!(
        proj.join("devices/泵1.toml").is_file(),
        "the device was created under its own name"
    );
}

fn copy_dir(src: &PathBuf, dst: &PathBuf) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

#[test]
fn unavailable_device_has_stale_inputs_and_an_acknowledgeable_default_alarm() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("device_health");
    copy_dir(&repo_root().join("examples/device_health"), &proj);
    // Own the port for the whole test but never answer a Modbus request.
    // This is deterministic communication loss on loopback, not a bench probe.
    let unavailable = TcpListener::bind("127.0.0.1:0").unwrap();
    let device = proj.join("devices/bus0.toml");
    let config = std::fs::read_to_string(&device).unwrap().replace(
        "port = 65534",
        &format!("port = {}", unavailable.local_addr().unwrap().port()),
    );
    std::fs::write(&device, config).unwrap();
    assert!(!proj.join("alarms.toml").exists(), "zero-config regression");
    let server = spawn_server(tmp.path());
    server.open_project(&proj);
    cs(&server.base)
        .args(["api", "POST", "/api/run"])
        .assert()
        .success();
    cs(&server.base)
        .args(["sim", "run", "--no-run"])
        .arg(proj.join("scenarios/unavailable.toml"))
        .assert()
        .success()
        .stderr(predicates::str::contains("scenario passed"));
    let output = cs(&server.base)
        .args(["api", "GET", "/api/runtime/snapshot"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let snapshot: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let vars = snapshot["vars"].as_array().unwrap();
    let input = vars.iter().find(|v| v["name"] == "input_value").unwrap();
    assert_eq!(input["input"]["stale"], true);
    assert_eq!(input["input"]["device"], "bus0");
    assert_eq!(input["input"]["channel"], "input");
    // The IDE receives SSE, not the one-shot route: prove the wire event
    // preserves quality as well, including snapshots forwarded by the server.
    use std::io::BufRead;
    let stream = ureq::get(&format!("{}/api/events", server.base))
        .timeout(std::time::Duration::from_secs(3))
        .call()
        .unwrap();
    let event = std::io::BufReader::new(stream.into_reader())
        .lines()
        .take(30)
        .find_map(|line| {
            let line = line.unwrap();
            let data = line.strip_prefix("data:")?;
            let event: serde_json::Value = serde_json::from_str(data.trim()).unwrap();
            (event["type"] == "snapshot").then_some(event)
        })
        .expect("SSE must emit a snapshot");
    let streamed = event["data"]["vars"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "input_value")
        .unwrap();
    assert_eq!(streamed["input"], input["input"]);
    assert_eq!(event["data"]["device_health"][0]["healthy"], false);
    assert!(vars
        .iter()
        .find(|v| v["name"] == "ticks")
        .unwrap()
        .get("input")
        .is_none());
    let output = cs(&server.base)
        .args(["api", "POST", "/api/runtime/alarms/__device%2Fbus0/ack"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let alarm: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(alarm["active"], true);
    assert_eq!(alarm["acked"], true);
    cs(&server.base)
        .args(["api", "POST", "/api/stop"])
        .assert()
        .success();
}

/// One HTTP/1.1 request with exactly the headers given, over a raw socket, so
/// the `Host` and `Origin` are what a browser (or an attacker's DNS name)
/// would send. Returns the status code and the response head.
fn raw_request(
    base: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String) {
    use std::io::{Read, Write};
    let addr = base.trim_start_matches("http://");
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut request = format!("{method} {path} HTTP/1.1\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    ));
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                // A WebSocket upgrade keeps the socket open: the head is the answer.
                if raw.windows(4).any(|w| w == b"\r\n\r\n") && raw.starts_with(b"HTTP/1.1 101") {
                    break;
                }
            }
        }
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let head = text.split("\r\n\r\n").next().unwrap_or("").to_string();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status line");
    (status, head)
}

/// The IDE server has no authentication, so a web page in the user's browser
/// must not be able to use it: not by cross-origin `fetch`, not by a
/// cross-site WebSocket handshake, not through a rebound DNS name. And the
/// callers that must keep working — `cs` and curl (no `Origin`), the UI the
/// server hosts itself (same origin) — still do.
#[test]
fn a_web_page_cannot_drive_the_real_server_but_cs_and_the_hosted_ui_can() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("sim_smoke");
    copy_dir(&repo_root().join("examples/sim_smoke"), &proj);
    let server = spawn_server(tmp.path());
    let addr = server.base.trim_start_matches("http://").to_string();
    let json = ("content-type", "application/json");
    let open_body = format!("{{\"path\":\"{}\"}}", proj.display());

    // A foreign page: every kind of request is refused, and the refusal carries no CORS headers.
    let evil = ("origin", "https://evil.example");
    let host = ("host", addr.as_str());
    for (method, path, body) in [
        ("GET", "/api/fs/browse?path=/", String::new()),
        ("POST", "/api/projects/open", open_body.clone()),
        ("POST", "/api/stop", String::new()),
    ] {
        let (status, head) = raw_request(&server.base, method, path, &[host, evil, json], &body);
        assert_eq!(status, 403, "{method} {path}: {head}");
        assert!(
            !head
                .to_ascii_lowercase()
                .contains("access-control-allow-origin"),
            "{head}"
        );
    }
    let preflight = [
        host,
        evil,
        ("access-control-request-method", "POST"),
        (
            "access-control-request-headers",
            "content-type,x-ia2-project",
        ),
    ];
    let (status, head) = raw_request(
        &server.base,
        "OPTIONS",
        "/api/projects/open",
        &preflight,
        "",
    );
    assert_eq!(status, 403, "{head}");
    let upgrade = [
        host,
        evil,
        ("connection", "Upgrade"),
        ("upgrade", "websocket"),
        ("sec-websocket-version", "13"),
        ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
    ];
    let (status, head) = raw_request(&server.base, "GET", "/api/lsp", &upgrade, "");
    assert_eq!(
        status, 403,
        "a cross-site WebSocket handshake must not be upgraded: {head}"
    );

    // DNS rebinding: Host and Origin agree, but the name is not a loopback one.
    let port = addr.rsplit(':').next().unwrap();
    let name = format!("rebind.evil.example:{port}");
    let origin = format!("http://{name}");
    let rebound = [("host", name.as_str()), ("origin", origin.as_str()), json];
    let (status, head) = raw_request(
        &server.base,
        "POST",
        "/api/projects/open",
        &rebound,
        &open_body,
    );
    assert_eq!(status, 403, "{head}");
    let (status, _) = raw_request(
        &server.base,
        "GET",
        "/health",
        &[("host", name.as_str())],
        "",
    );
    assert_eq!(status, 403, "even a plain read under a rebound name");

    // Nothing above reached a handler: no project was opened.
    let (status, head) = raw_request(&server.base, "GET", "/api/project", &[host], "");
    assert_eq!(
        status, 409,
        "no project must be open yet (409 = no project): {head}"
    );

    // The UI served by this server: same origin, with an Origin header, works - including the WebSocket upgrade.
    let same = format!("http://{addr}");
    let ui = [host, ("origin", same.as_str()), json];
    let (status, head) = raw_request(&server.base, "POST", "/api/projects/open", &ui, &open_body);
    assert_eq!(status, 200, "{head}");
    let ui_upgrade = [
        host,
        ("origin", same.as_str()),
        ("connection", "Upgrade"),
        ("upgrade", "websocket"),
        ("sec-websocket-version", "13"),
        ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
    ];
    let (status, head) = raw_request(&server.base, "GET", "/api/lsp", &ui_upgrade, "");
    assert_eq!(status, 101, "{head}");

    // `cs` (no Origin) keeps working through the real router.
    cs(&server.base)
        .args(["api", "GET", "/api/project"])
        .assert()
        .success();
}

/// A shell that loads the UI from some other origin is named on the command
/// line or in the environment; only that origin gets in, and it is answered
/// with the headers a page needs to read the reply.
#[test]
fn a_named_origin_may_call_the_real_server_and_no_other_page_may() {
    let tmp = tempfile::tempdir().unwrap();
    let by_flag = spawn_server_with(
        tmp.path(),
        &["--allow-origin", "http://localhost:1420"],
        &[],
    );
    let by_env_dir = tempfile::tempdir().unwrap();
    let by_env = spawn_server_with(
        by_env_dir.path(),
        &[],
        &[(
            "IA2_ALLOWED_ORIGINS",
            "http://localhost:1420, https://shell.example",
        )],
    );
    for (server, named) in [
        (&by_flag, "http://localhost:1420"),
        (&by_env, "http://localhost:1420"),
        (&by_env, "https://shell.example"),
    ] {
        let addr = server.base.trim_start_matches("http://");
        let pairs = [("host", addr), ("origin", named)];
        let (status, head) = raw_request(&server.base, "GET", "/health", &pairs, "");
        assert_eq!(status, 200, "{named}: {head}");
        assert!(
            head.to_ascii_lowercase()
                .contains(&format!("access-control-allow-origin: {named}")),
            "{head}"
        );
        let foreign = [("host", addr), ("origin", "https://evil.example")];
        let (status, _) = raw_request(&server.base, "GET", "/health", &foreign, "");
        assert_eq!(status, 403, "a page that was not named stays out");
    }
}

#[test]
fn a_mapped_ipv6_listener_does_not_disable_the_real_servers_host_check() {
    let tmp = tempfile::tempdir().unwrap();
    let server = spawn_server_on(tmp.path(), "::ffff:127.0.0.1", &[], &[]);
    let port = server.base.rsplit(':').next().unwrap();
    let rebound = format!("rebind.example:{port}");
    let origin = format!("http://{rebound}");
    for method in ["GET", "POST"] {
        let (status, head) = raw_request(
            &server.base,
            method,
            "/api/projects/open",
            &[
                ("host", &rebound),
                ("origin", &origin),
                ("content-type", "application/json"),
            ],
            "{}",
        );
        assert_eq!(status, 403, "{method}: {head}");
    }
}

#[cfg(unix)]
#[test]
fn a_cross_site_originless_get_never_launches_ssh_but_cli_and_ui_reads_still_work() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("sim_smoke");
    copy_dir(&repo_root().join("examples/sim_smoke"), &proj);
    std::fs::create_dir_all(proj.join("edges")).unwrap();
    std::fs::write(
        proj.join("edges/dummy.toml"),
        "name = \"dummy\"\nhost = \"must-not-resolve.invalid\"\n",
    )
    .unwrap();
    let mock_bin = tmp.path().join("mock-bin");
    std::fs::create_dir(&mock_bin).unwrap();
    let calls = tmp.path().join("ssh-calls");
    let ssh = mock_bin.join("ssh");
    std::fs::write(&ssh,
        "#!/bin/sh\nprintf 'called\\n' >> \"$IA2_TEST_SSH_CALLS\"\nprintf '{\"status\":\"ok\",\"scan_count\":1}\\n'\n",
    ).unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    // PATH contains only the stub: even a broken fixture cannot reach real SSH.
    let server = spawn_server_with(
        tmp.path(),
        &[],
        &[
            ("PATH", mock_bin.to_str().unwrap()),
            ("IA2_TEST_SSH_CALLS", calls.to_str().unwrap()),
        ],
    );
    server.open_project(&proj);
    let host = ("host", server.base.trim_start_matches("http://"));
    for site in ["cross-site", "same-site"] {
        for (mode, dest) in [
            ("no-cors", "image"),
            ("no-cors", "script"),
            ("navigate", "document"),
            ("navigate", "iframe"),
        ] {
            let (status, head) = raw_request(
                &server.base,
                "GET",
                "/api/edges/dummy/probe",
                &[
                    host,
                    ("sec-fetch-site", site),
                    ("sec-fetch-mode", mode),
                    ("sec-fetch-dest", dest),
                ],
                "",
            );
            assert_eq!(status, 403, "{site} {mode} {dest}: {head}");
            assert!(!calls.exists(), "a denied request reached SSH");
        }
    }
    for pairs in [vec![host], vec![host, ("sec-fetch-site", "same-origin")]] {
        let (status, head) = raw_request(&server.base, "GET", "/api/edges/dummy/probe", &pairs, "");
        assert_eq!(status, 200, "{head}");
    }
    assert_eq!(std::fs::read_to_string(calls).unwrap().lines().count(), 2);
}

#[test]
fn a_top_level_link_can_open_the_ide_but_cannot_navigate_to_its_api() {
    let tmp = tempfile::tempdir().unwrap();
    let web = tmp.path().join("web");
    std::fs::create_dir(&web).unwrap();
    std::fs::write(
        web.join("index.html"),
        "<!doctype html><title>fixture IDE</title>",
    )
    .unwrap();
    let server = spawn_server_with(tmp.path(), &["--static-dir", web.to_str().unwrap()], &[]);
    let host = ("host", server.base.trim_start_matches("http://"));
    let navigation = [
        host,
        ("sec-fetch-site", "cross-site"),
        ("sec-fetch-mode", "navigate"),
        ("sec-fetch-dest", "document"),
    ];
    for path in ["/", "/index.html"] {
        assert_eq!(
            raw_request(&server.base, "GET", path, &navigation, "").0,
            200,
            "{path}"
        );
    }
    for path in ["/api/fs/browse", "/api/edges/dummy/probe", "/health"] {
        assert_eq!(
            raw_request(&server.base, "GET", path, &navigation, "").0,
            403,
            "{path}"
        );
    }
    assert_eq!(
        raw_request(
            &server.base,
            "GET",
            "/",
            &[
                host,
                ("sec-fetch-site", "cross-site"),
                ("sec-fetch-mode", "navigate"),
                ("sec-fetch-dest", "iframe"),
            ],
            ""
        )
        .0,
        403
    );
}

#[test]
fn a_named_ipv6_origin_works_with_browser_canonical_headers_and_preflights() {
    let tmp = tempfile::tempdir().unwrap();
    let server = spawn_server_with(
        tmp.path(),
        &["--allow-origin", "http://[0:0:0:0:0:0:0:1]:1420"],
        &[],
    );
    let host = ("host", server.base.trim_start_matches("http://"));
    let headers = [
        host,
        ("origin", "http://[::1]:1420"),
        ("sec-fetch-site", "cross-site"),
    ];
    let (status, head) = raw_request(&server.base, "GET", "/health", &headers, "");
    assert_eq!(status, 200, "{head}");
    assert!(head
        .to_ascii_lowercase()
        .contains("access-control-allow-origin: http://[::1]:1420"));
    let mut preflight = headers.to_vec();
    preflight.push(("access-control-request-method", "POST"));
    assert_eq!(
        raw_request(&server.base, "OPTIONS", "/api/check", &preflight, "").0,
        204
    );
}
