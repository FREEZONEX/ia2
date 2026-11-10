//! Independent real-server fixture. Use the same target and profile to
//! build `server` before testing. Only loopback and synthetic projects.

#![cfg(unix)]

use std::fs::{self, File};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const LIBRARY: &str = "controls";
const PROJECT: &str = "library_fixture";
mod fixture_sources {
    include!("../../project/tests/fixtures/library_sources.rs");
}
use fixture_sources::{FIRST_NEW, FIRST_OLD, MAIN, SECOND_NEW, SECOND_OLD};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    _server: ChildGuard,
    scratch: tempfile::TempDir,
    store: project::ProjectStore,
    url: String,
    cs: PathBuf,
}

impl Fixture {
    fn new(baseline: Option<PathBuf>) -> Self {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().join("project");
        let store = project::ProjectStore::create(root, PROJECT).unwrap();
        store.write_pou_source("main", MAIN).unwrap();
        let blocks = store.root().join("pous/lib/controls");
        fs::create_dir_all(&blocks).unwrap();
        fs::write(blocks.join("first.st"), FIRST_OLD).unwrap();
        fs::write(blocks.join("second.st"), SECOND_OLD).unwrap();
        fs::write(
            blocks.join("spare.st"),
            "FUNCTION_BLOCK FB_Spare\nEND_FUNCTION_BLOCK\n",
        )
        .unwrap();
        fs::write(
            store.root().join("project.toml"),
            format!("name = '{PROJECT}'\nextra = 'keep'\n[libraries]\ncontrols = '1.0'\nother = '9.0'\n"),
        )
        .unwrap();
        Self::serve(scratch, store, baseline)
    }

    fn serve(
        scratch: tempfile::TempDir,
        store: project::ProjectStore,
        baseline: Option<PathBuf>,
    ) -> Self {
        let registry = scratch.path().join("registry");
        let blocks = registry.join("controls/pous");
        fs::create_dir_all(&blocks).unwrap();
        fs::write(
            registry.join("controls/library.toml"),
            "name = 'controls'\nversion = '2.0'\n",
        )
        .unwrap();
        fs::write(blocks.join("first.st"), FIRST_NEW).unwrap();
        fs::write(blocks.join("second.st"), SECOND_NEW).unwrap();

        let cs = PathBuf::from(env!("CARGO_BIN_EXE_cs"));
        let (server, cs) = match baseline {
            Some(server) => {
                let cs = server.with_file_name("cs");
                (server, cs)
            }
            None => (cs.with_file_name("server"), cs),
        };
        assert!(
            server.is_file(),
            "build the matching server binary first: {server:?}"
        );
        let home = scratch.path().join("home");
        fs::create_dir(&home).unwrap();
        let log = scratch.path().join("server.log");
        let mut child = ChildGuard(
            Command::new(&server)
                .args([
                    "--bind",
                    "127.0.0.1:0",
                    "--demo-modbus-addr",
                    "",
                    "--library-dir",
                ])
                .arg(&registry)
                .current_dir(scratch.path())
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", home.join("config"))
                .env("RUST_LOG", "server=info")
                .env("NO_COLOR", "1")
                .env_remove("IA2_ALLOWED_ORIGINS")
                .stdout(Stdio::null())
                .stderr(File::create(&log).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let agent = ureq::AgentBuilder::new()
            .try_proxy_from_env(false)
            .timeout(Duration::from_secs(2))
            .build();
        let url = loop {
            let logs = fs::read_to_string(&log).unwrap();
            if let Some(port) = logs.lines().find_map(|line| {
                if !line.contains("server listening") {
                    return None;
                }
                let (_, address) = line.split_once("addr=127.0.0.1:")?;
                address.split_whitespace().next()?.parse::<u16>().ok()
            }) {
                let url = format!("http://127.0.0.1:{port}");
                if agent.get(&format!("{url}/health")).call().is_ok() {
                    break url;
                }
            }
            assert!(
                Instant::now() < deadline && child.0.try_wait().unwrap().is_none(),
                "{server:?} failed readiness: {logs}",
            );
            std::thread::sleep(Duration::from_millis(15));
        };
        agent
            .post(&format!("{url}/api/projects/open"))
            .send_json(serde_json::json!({"path": store.root()}))
            .unwrap();
        eprintln!(
            "library fixture server {server:?}, CLI {cs:?}, project {:?}",
            store.root()
        );
        Self {
            _server: child,
            scratch,
            store,
            url,
            cs,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        let result = Command::new(&self.cs)
            .args(["--server", &self.url, "--project", self.store.name()])
            .args(args)
            .current_dir(self.scratch.path())
            .output()
            .unwrap();
        eprintln!(
            "{args:?}: {}\n{}{}",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        result
    }

    fn ok(&self, args: &[&str]) -> Output {
        let result = self.run(args);
        assert!(result.status.success(), "{args:?} failed: {result:?}");
        result
    }

    fn prove(&self, value: i32) {
        self.ok(&["project", "check", self.store.root().to_str().unwrap()]);
        let scenario = self.scratch.path().join("sum.toml");
        fs::write(
            &scenario,
            format!("description = 'imported block sum'\n[[steps]]\nexpect = {{ var = 'total', op = 'eq', value = {value}.0, within_ms = 3000 }}\n"),
        )
        .unwrap();
        self.ok(&["sim", "run", scenario.to_str().unwrap()]);
    }
}

#[test]
fn library_cli_import_updates_behavior_and_preserves_unselected_files() {
    let fixture = Fixture::new(None);
    fixture.prove(11);
    let root = fixture.store.root();
    let spare = root.join("pous/lib/controls/spare.st");
    let spare_bytes = fs::read(&spare).unwrap();
    let chosen = fixture.ok(&[
        "library",
        "import",
        LIBRARY,
        "--blocks",
        "first.st,first.st",
    ]);
    let value: serde_json::Value = serde_json::from_slice(&chosen.stdout).unwrap();
    assert_eq!(
        value["imported"],
        serde_json::json!(["first.st", "first.st"])
    );
    assert_eq!(
        fs::read_to_string(root.join("pous/lib/controls/second.st")).unwrap(),
        SECOND_OLD
    );
    let updated = fixture.ok(&["library", "import", LIBRARY]);
    let value: serde_json::Value = serde_json::from_slice(&updated.stdout).unwrap();
    assert_eq!(value["version"], "2.0");
    assert_eq!(fixture.store.read_libraries().unwrap()["other"], "9.0");
    assert_eq!(fs::read(spare).unwrap(), spare_bytes);
    fixture.prove(22);

    if let Some(directory) = std::env::var_os("IA2_LIBRARY_CRASH_CASES") {
        for (name, value) in [
            ("prepared", 11),
            ("first", 11),
            ("manifest", 11),
            ("committed", 22),
        ] {
            let root = PathBuf::from(&directory).join(name);
            let recovered = project::ProjectStore::open(root).unwrap();
            Fixture::serve(tempfile::tempdir().unwrap(), recovered, None).prove(value);
        }
    }
}

#[test]
fn library_blocked_destination_keeps_candidate_files_and_reproduces_baseline_failure() {
    let baseline = std::env::var_os("IA2_LIBRARY_BASELINE_SERVER").map(PathBuf::from);
    let fixture = Fixture::new(baseline.clone());
    let root = fixture.store.root();
    let first = root.join("pous/lib/controls/first.st");
    let second = root.join("pous/lib/controls/second.st");
    let manifest = root.join("project.toml");
    let before = fs::read(&manifest).unwrap();
    assert_eq!(fs::read_to_string(&first).unwrap(), FIRST_OLD);
    assert_eq!(fs::read_to_string(&second).unwrap(), SECOND_OLD);
    let saved = fixture.scratch.path().join("second-v1.st");
    fs::rename(&second, &saved).unwrap();
    fs::create_dir(&second).unwrap();
    fs::write(second.join("blocker"), b"occupied").unwrap();
    let failure = fixture.run(&[
        "library",
        "import",
        LIBRARY,
        "--blocks",
        "first.st,second.st",
    ]);
    assert_eq!(failure.status.code(), Some(3), "{failure:?}");
    assert_eq!(fs::read(&manifest).unwrap(), before);
    assert_eq!(
        fs::read_to_string(&first).unwrap(),
        if baseline.is_some() {
            FIRST_NEW
        } else {
            FIRST_OLD
        },
    );
    assert_eq!(fs::read(second.join("blocker")).unwrap(), b"occupied");
    eprintln!(
        "first block before:\n{FIRST_OLD}after:\n{}",
        fs::read_to_string(first).unwrap()
    );
    if baseline.is_none() {
        fs::remove_dir_all(&second).unwrap();
        fs::rename(&saved, &second).unwrap();
        fixture.prove(11);
    }
}

#[test]
fn library_compile_rejects_a_project_while_writer_owns_its_lock() {
    let fixture = Fixture::new(None);
    let root = fixture.store.root();
    let lock = File::open(root).unwrap();
    lock.lock().unwrap();
    let first = root.join("pous/lib/controls/first.st");
    fs::write(&first, FIRST_NEW).unwrap();
    let check = fixture.run(&["project", "check", root.to_str().unwrap()]);
    assert_ne!(check.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&check.stderr).contains("library transaction is busy"));
    let run = fixture.run(&["run"]);
    assert_ne!(run.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&run.stderr).contains("library transaction is busy"));
    fs::write(first, FIRST_OLD).unwrap();
    drop(lock);
    fixture.prove(11);
}

#[test]
fn library_cli_does_not_replay_when_committed_response_is_lost() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    let fixture = Fixture::new(None);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    let upstream = fixture.url.clone();
    let (done, finished) = mpsc::channel::<()>();
    let proxy = std::thread::spawn(move || {
        let mut requests = Vec::new();
        let agent = ureq::AgentBuilder::new()
            .try_proxy_from_env(false)
            .timeout(Duration::from_secs(5))
            .build();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut reader = BufReader::new(stream);
                    let mut request = String::new();
                    reader.read_line(&mut request).unwrap();
                    let mut request = request.split_whitespace();
                    assert_eq!(request.next(), Some("POST"));
                    let path = request.next().unwrap();
                    assert!(matches!(
                        path,
                        "/api/agent/heartbeat" | "/api/library/import"
                    ));
                    let mut line = String::new();
                    let mut length = None;
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(value) =
                            line.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            length = Some(value.trim().parse::<usize>().unwrap());
                        }
                    }
                    let mut body = vec![0; length.expect("Content-Length")];
                    reader.read_exact(&mut body).unwrap();
                    let body = serde_json::from_slice::<serde_json::Value>(&body).unwrap();
                    let response = agent
                        .post(&format!("{upstream}{path}"))
                        .set("X-IA2-Project", PROJECT)
                        .send_json(body)
                        .unwrap();
                    let status = response.status();
                    let body = response.into_string().unwrap();
                    requests.push((path.to_owned(), status));
                    eprintln!("proxy POST {path}: upstream {status} {body}");
                    if path == "/api/agent/heartbeat" {
                        write!(
                            reader.get_mut(),
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                            body.len(),
                        )
                        .unwrap();
                    }
                    // Only the import response is lost, after the real
                    // server has committed it and returned HTTP 200.
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if !matches!(
                        finished.recv_timeout(Duration::from_millis(10)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        return requests;
                    }
                }
                Err(error) => panic!("fixture proxy: {error}"),
            }
        }
    });
    let output = Command::new(&fixture.cs)
        .args([
            "--server",
            &proxy_url,
            "--project",
            PROJECT,
            "library",
            "import",
            LIBRARY,
        ])
        .output()
        .unwrap();
    drop(done);
    assert_eq!(
        proxy.join().unwrap(),
        [
            ("/api/agent/heartbeat".into(), 200),
            ("/api/library/import".into(), 200),
        ],
        "only one import request reached the server",
    );
    let error = String::from_utf8_lossy(&output.stderr);
    eprintln!("client {}: {error}", output.status);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(error.contains("/api/library/import"), "{error}");
    let readback = fixture.ok(&["--json", "ls", "library"]);
    let libraries: serde_json::Value = serde_json::from_slice(&readback.stdout).unwrap();
    assert_eq!(libraries[0]["name"], LIBRARY);
    assert_eq!(libraries[0]["imported_version"], "2.0");
    assert_eq!(fixture.store.read_libraries().unwrap()[LIBRARY], "2.0");
    fixture.prove(22);
}
