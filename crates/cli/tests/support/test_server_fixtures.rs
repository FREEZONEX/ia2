use super::test_server;
#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

fn temp_logs(root: &Path, label: &str) -> PathBuf {
    let path = root.join(label);
    std::fs::create_dir(&path).unwrap();
    path
}

fn fixture_log_root(prefix: &str) -> (PathBuf, Option<tempfile::TempDir>) {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix);
    if let Some(root) = std::env::var_os("IA2_TEST_SERVER_LOG_DIR") {
        std::fs::create_dir_all(&root).unwrap();
        let path = builder.tempdir_in(root).unwrap().keep();
        eprintln!("startup fixture logs: {}", path.display());
        (path, None)
    } else {
        let tmp = builder.tempdir().unwrap();
        (tmp.path().to_owned(), Some(tmp))
    }
}

fn fixture(mode: &str) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "test_server_fixtures::fixture_process",
        "--nocapture",
    ])
    .env("IA2_SERVER_FIXTURE", mode);
    cmd
}

#[test]
fn fixture_process() {
    let Ok(mode) = std::env::var("IA2_SERVER_FIXTURE") else {
        return;
    };
    assert_eq!(mode, "early");
    println!("controlled stdout");
    eprintln!("controlled early exit");
    process::exit(17);
}

#[test]
fn reports_missing_binary_and_early_exit() {
    let (root, _tmp) = fixture_log_root("launch-faults-");
    let bind = "127.0.0.1:0".parse().unwrap();
    let missing = test_server::start(
        Command::new(root.join("missing-server")),
        bind,
        &temp_logs(&root, "missing"),
        Duration::from_secs(3),
    )
    .err()
    .unwrap();
    assert!(missing.contains("could not be executed") && missing.contains("missing-server"));
    let early = test_server::start(
        fixture("early"),
        bind,
        &temp_logs(&root, "early"),
        Duration::from_secs(3),
    )
    .err()
    .unwrap();
    assert!(early.contains("exited before readiness") && early.contains("17"));
    assert!(early.contains("controlled stdout"));
    assert!(early.contains("controlled early exit"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn inherited_pipe_fixture(logs: &Path, after_handshake: &str) -> Command {
    // Synchronize before the launcher exits or announces readiness. The
    // descendant inherits both log streams and holds a separate lock until
    // it dies so the test can observe cleanup without relying on PID reuse.
    const HANDSHAKE: &str = r#"import os, socket, subprocess, sys
read_fd, write_fd = os.pipe()
holder = """
import fcntl, os, signal, sys
with open(sys.argv[1], "w") as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    signal.alarm(6)
    fd = int(sys.argv[2])
    os.write(fd, b"1")
    os.close(fd)
    while True:
        signal.pause()
"""
subprocess.Popen(
    [sys.executable, "-c", holder, sys.argv[1], str(write_fd)], pass_fds=(write_fd,)
)
os.close(write_fd)
assert os.read(read_fd, 1) == b"1"
os.close(read_fd)
"#;
    let mut command = Command::new("python3");
    command
        .args(["-u", "-c", &format!("{HANDSHAKE}\n{after_handshake}")])
        .arg(logs.join("descendant.lock"));
    command
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn assert_descendant_stopped(logs: &Path) {
    use rustix::fs::{flock, FlockOperation};
    use rustix::io::Errno;

    let lock = std::fs::File::open(logs.join("descendant.lock")).unwrap();
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return,
            Err(Errno::WOULDBLOCK) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("server descendant did not release its lock: {error}"),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn early_exit_is_bounded_when_a_descendant_inherits_both_streams() {
    let (logs, _tmp) = fixture_log_root("inherited-early-");
    let command = inherited_pipe_fixture(
        &logs,
        r#"print("controlled stdout")
print("controlled early exit", file=sys.stderr)
sys.exit(17)
"#,
    );
    let timeout = Duration::from_secs(1);
    let started = Instant::now();
    let error = test_server::start(command, "127.0.0.1:0".parse().unwrap(), &logs, timeout)
        .err()
        .unwrap();
    let elapsed = started.elapsed();
    eprintln!("inherited-pipe early exit returned after {elapsed:?}");
    assert!(elapsed < timeout + test_server::CLEANUP_TIMEOUT + Duration::from_millis(500));
    assert!(
        error.contains("exited before readiness") && error.contains("17"),
        "{error}"
    );
    assert!(
        error.contains("controlled stdout") && error.contains("controlled early exit"),
        "{error}"
    );
    assert_descendant_stopped(&logs);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn dropping_a_ready_server_stops_a_descendant_that_inherits_its_streams() {
    let (logs, _tmp) = fixture_log_root("inherited-ready-");
    let command = inherited_pipe_fixture(
        &logs,
        r#"listener = socket.socket()
listener.bind(("127.0.0.1", 0))
listener.listen()
print("IA2_LISTEN_ADDR=127.0.0.1:" + str(listener.getsockname()[1]))
while True:
    conn, _ = listener.accept()
    with conn:
        conn.recv(8192)
        conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
"#,
    );
    let (_base, server) = test_server::start(
        command,
        "127.0.0.1:0".parse().unwrap(),
        &logs,
        Duration::from_secs(2),
    )
    .unwrap();
    let started = Instant::now();
    drop(server);
    let elapsed = started.elapsed();
    eprintln!("inherited-pipe cleanup returned after {elapsed:?}");
    assert!(elapsed < test_server::CLEANUP_TIMEOUT + Duration::from_millis(500));
    assert_descendant_stopped(&logs);
}

#[cfg(unix)]
#[test]
fn readiness_times_out_on_a_hung_health_request_and_caps_logs() {
    // Exec one process so it and both log pipes are owned by the launcher.
    // Use the current Rust fixture in "early" above and Python's socket API
    // here to keep libtest's stdout banner out of the bind protocol.
    let (logs, _tmp) = fixture_log_root("health-timeout-");
    let mut command = Command::new("python3");
    command.args([
        "-u",
        "-c",
        r#"import socket, sys
listener = socket.socket()
listener.bind(("127.0.0.1", 0))
listener.listen()
print("IA2_LISTEN_ADDR=127.0.0.1:" + str(listener.getsockname()[1]))
sys.stderr.write("head\n" + "x" * 524288 + "\ntail\n")
while True:
    conn, _ = listener.accept()
    with conn:
        while conn.recv(8192):
            pass
"#,
    ]);
    let started = Instant::now();
    let timeout = test_server::start(
        command,
        "127.0.0.1:0".parse().unwrap(),
        &logs,
        Duration::from_secs(2),
    )
    .err()
    .unwrap();
    assert!(timeout.contains("readiness timed out") && timeout.contains("127.0.0.1"));
    let elapsed = started.elapsed();
    eprintln!("hung health request returned after {elapsed:?}");
    assert!(
        elapsed
            < Duration::from_secs(2) + test_server::CLEANUP_TIMEOUT + Duration::from_millis(500)
    );
    let log = std::fs::read_to_string(logs.join("stderr.log")).unwrap();
    assert!(log.starts_with("head") && log.ends_with("tail\n"));
    assert!(log.contains("output omitted") && log.len() < 264_000);
}

#[cfg(unix)]
#[test]
fn concurrent_real_servers_own_distinct_ports_and_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let bind = "127.0.0.1:0".parse().unwrap();
    let first = temp_logs(tmp.path(), "first");
    let second = temp_logs(tmp.path(), "second");
    let start = |root: &Path| {
        let logs = temp_logs(root, "logs");
        let (base, child) = test_server::start(
            real_server(root, bind),
            bind,
            &logs,
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(std::fs::read_to_string(logs.join("launch.txt"))
            .unwrap()
            .contains(&format!("ready: {base}")));
        (base, child)
    };
    let ((first_base, first_child), (second_base, _second_child)) = std::thread::scope(|scope| {
        let first = scope.spawn(|| start(&first));
        let second = scope.spawn(|| start(&second));
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_ne!(first_base, second_base);
    drop(first_child);
    let status = ureq::AgentBuilder::new()
        .try_proxy_from_env(false)
        .timeout_connect(Duration::from_secs(2))
        .build()
        .get(&format!("{second_base}/health"))
        .timeout(Duration::from_secs(2))
        .call()
        .unwrap()
        .status();
    assert_eq!(status, 200);
}

#[cfg(unix)]
#[test]
fn occupied_health_endpoint_cannot_satisfy_another_childs_readiness() {
    let tmp = tempfile::tempdir().unwrap();
    let bystander = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = bystander.local_addr().unwrap();
    let answering = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = bystander.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = String::new();
            let mut reader = BufReader::new(&mut stream);
            loop {
                request.clear();
                if reader.read_line(&mut request).unwrap() == 0 || request == "\r\n" {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        }
    });
    let probe = || {
        ureq::AgentBuilder::new()
            .try_proxy_from_env(false)
            .timeout(Duration::from_secs(2))
            .build()
            .get(&format!("http://{addr}/health"))
            .call()
            .unwrap()
            .status()
    };
    assert_eq!(probe(), 200);

    let error = test_server::start(
        real_server(tmp.path(), addr),
        addr,
        &temp_logs(tmp.path(), "occupied"),
        Duration::from_secs(10),
    )
    .err()
    .unwrap();
    assert!(error.contains("exited before readiness"), "{error}");
    assert!(
        error
            .to_ascii_lowercase()
            .contains("address already in use"),
        "{error}"
    );
    assert_eq!(probe(), 200);
    answering.join().unwrap();

    // The actual runner path lets the kernel choose a free port.
    let bind = "127.0.0.1:0".parse().unwrap();
    let (base, _child) = test_server::start(
        real_server(tmp.path(), bind),
        bind,
        &temp_logs(tmp.path(), "free"),
        Duration::from_secs(10),
    )
    .unwrap();
    assert_ne!(base, "http://127.0.0.1:0");
}

#[cfg(unix)]
fn real_server(sandbox: &Path, bind: SocketAddr) -> Command {
    let mut cmd = Command::new(test_server::server_binary());
    cmd.args(["--bind", &bind.to_string(), "--demo-modbus-addr", ""])
        .env("IA2_PRINT_LISTEN_ADDR", "1")
        .env_remove("IA2_ALLOWED_ORIGINS")
        .env("HOME", sandbox)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .current_dir(sandbox);
    cmd
}
