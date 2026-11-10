use super::test_server;
#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{self, Command};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

fn temp_logs(root: &Path, label: &str) -> std::path::PathBuf {
    let path = root.join(label);
    std::fs::create_dir(&path).unwrap();
    path
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
    eprintln!("controlled early exit");
    process::exit(17);
}

#[test]
fn reports_missing_binary_and_early_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let bind = "127.0.0.1:0".parse().unwrap();
    let missing = test_server::start(
        Command::new(tmp.path().join("missing-server")),
        bind,
        &temp_logs(tmp.path(), "missing"),
        Duration::from_secs(3),
    )
    .err()
    .unwrap();
    assert!(missing.contains("could not be executed") && missing.contains("missing-server"));
    let early = test_server::start(
        fixture("early"),
        bind,
        &temp_logs(tmp.path(), "early"),
        Duration::from_secs(3),
    )
    .err()
    .unwrap();
    assert!(early.contains("exited before readiness") && early.contains("17"));
    assert!(early.contains("controlled early exit"));
}

#[cfg(unix)]
#[test]
fn readiness_times_out_on_a_hung_health_request_and_caps_logs() {
    // Exec one process so it and both log pipes are owned by the launcher.
    // Use the current Rust fixture in "early" above and Python's socket API
    // here to keep libtest's stdout banner out of the bind protocol.
    let tmp = tempfile::tempdir().unwrap();
    let logs = temp_logs(tmp.path(), "hung");
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
    assert!(started.elapsed() < Duration::from_secs(4));
    let log = std::fs::read_to_string(logs.join("stderr.log")).unwrap();
    assert!(log.starts_with("head") && log.ends_with("tail\n"));
    assert!(log.contains("output omitted") && log.len() < 264_000);
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
