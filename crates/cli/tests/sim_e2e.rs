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

use std::net::TcpListener;
use std::path::PathBuf;
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

#[test]
fn sim_run_proves_and_refutes_against_a_real_server() {
    let server_bin = server_binary();

    // Free port: bind-then-drop; the server grabs it a moment later.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let base = format!("http://127.0.0.1:{port}");

    // Copy the example project to a tempdir so runs never dirty the repo.
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("sim_smoke");
    copy_dir(&repo_root().join("examples/sim_smoke"), &proj);

    let child = StdCommand::new(&server_bin)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        // No demo slave — keeps the test from fighting over port 5502
        // with a dev server on the same machine.
        .arg("--demo-modbus-addr")
        .arg("")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    let _guard = ServerGuard(child);

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

    cs(&base)
        .arg("api")
        .arg("POST")
        .arg("/api/projects/open")
        .arg("--from")
        .arg("-")
        .write_stdin(format!("{{\"path\":\"{}\"}}", proj.display()))
        .assert()
        .success();

    // The bundled scenario must pass end to end (fills tank, alarm
    // raises, no overflow) — this is the agent's self-verification loop.
    cs(&base)
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
    cs(&base)
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
    let server_bin = server_binary();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let base = format!("http://127.0.0.1:{port}");

    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("sim_smoke");
    copy_dir(&repo_root().join("examples/sim_smoke"), &proj);

    let child = StdCommand::new(&server_bin)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--demo-modbus-addr")
        .arg("")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    let _guard = ServerGuard(child);

    let mut up = false;
    for _ in 0..50 {
        if cs(&base)
            .arg("api")
            .arg("GET")
            .arg("/health")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            up = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(up, "server did not come up on {base}");

    cs(&base)
        .arg("api")
        .arg("POST")
        .arg("/api/projects/open")
        .arg("--from")
        .arg("-")
        .write_stdin(format!("{{\"path\":\"{}\"}}", proj.display()))
        .assert()
        .success();

    // Created with a name the CLI never has to encode (JSON body), then
    // read back through the resource path, which it does.
    cs(&base)
        .arg("api")
        .arg("POST")
        .arg("/api/devices")
        .arg("--from")
        .arg("-")
        .write_stdin("{\"name\":\"泵1\",\"protocol\":\"modbus\"}")
        .assert()
        .success();

    cs(&base)
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
