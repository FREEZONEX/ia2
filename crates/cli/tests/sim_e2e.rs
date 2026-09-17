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
    let server_bin = server_binary();

    // Free port: bind-then-drop; the server grabs it a moment later.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let base = format!("http://127.0.0.1:{port}");

    let home = sandbox.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let child = StdCommand::new(&server_bin)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        // No demo slave — keeps the test from fighting over port 5502
        // with a dev server on the same machine.
        .arg("--demo-modbus-addr")
        .arg("")
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
