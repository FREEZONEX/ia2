//! Process-level checks on the order of the runtime's startup: the HTTP API
//! socket is claimed before anything touches a field device.
//!
//! Why it matters: `spawn_units` starts connecting the devices and driving
//! outputs on its own thread. A bind that fails *after* that returns from
//! `main` past the safety shutdown, so the failsafe pass never runs and a
//! fieldbus thread that may be mid-bring-up is killed with the process.
//! A runtime that cannot be reached must not start driving the plant.

use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A one-task program with one Modbus TCP device. `device_port` is where the
/// test listens in place of the field device: the runtime never gets a Modbus
/// answer from it, but a TCP connection reaching it proves the runtime
/// started on that device.
fn write_project(dir: &Path, device_port: u16) {
    let write = |rel: &str, text: String| {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    write(
        "project.toml",
        "name = \"startup_order\"\nversion = \"0.1\"\n".into(),
    );
    write(
        "tasks.toml",
        "[[tasks]]\nname = \"fast\"\ninterval_ms = 20\npriority = 1\n\n\
         [[programs]]\ninstance = \"main\"\nprogram = \"main\"\ntask = \"fast\"\n"
            .into(),
    );
    write(
        "pous/main.st",
        "PROGRAM main\nVAR\n    input_value : INT;\n    ticks : DINT;\nEND_VAR\n\
         ticks := ticks + 1;\nEND_PROGRAM\n"
            .into(),
    );
    write(
        "devices/bus0.toml",
        format!(
            "name = \"bus0\"\nprotocol = \"modbus\"\nslave_id = 1\npoll_interval_ms = 20\n\
             timeout_ms = 100\n\n[transport]\nkind = \"tcp\"\nhost = \"127.0.0.1\"\n\
             port = {device_port}\n\n[[channels]]\nname = \"input\"\nkind = \"holding_register\"\n\
             address = 0\ndata_type = \"u16\"\naccess = \"read\"\n"
        ),
    );
    write(
        "iomap.toml",
        "[[mappings]]\napplication = \"main\"\nvariable = \"input_value\"\n\
         direction = \"input\"\ndevice = \"bus0\"\nchannel = \"input\"\n"
            .into(),
    );
}

fn start_runtime(project: &Path, state: &Path, bind: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_ia2-runtime"))
        .arg("--project-dir")
        .arg(project)
        .arg("--state-dir")
        .arg(state)
        .arg("--bind")
        .arg(bind)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start ia2-runtime")
}

fn wait_for_exit(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Positive control: with a free API port the same fixture DOES reach the
/// device. Without it, "no connection arrived" below could just mean the
/// fixture never connects.
#[test]
fn the_fixture_reaches_its_device_when_the_api_port_is_free() {
    let dir = tempfile::tempdir().unwrap();
    let device = TcpListener::bind("127.0.0.1:0").unwrap();
    device.set_nonblocking(true).unwrap();
    write_project(
        &dir.path().join("project"),
        device.local_addr().unwrap().port(),
    );

    let mut child = start_runtime(
        &dir.path().join("project"),
        &dir.path().join("state"),
        "127.0.0.1:0",
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut reached = false;
    while Instant::now() < deadline && !reached {
        reached = device.accept().is_ok();
        if let Some(status) = child.try_wait().unwrap() {
            panic!("the runtime exited while it should be serving: {status}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(reached, "the runtime never connected to the device fixture");
}

#[test]
fn a_runtime_that_cannot_bind_its_api_never_touches_the_field_device() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let state = dir.path().join("state");
    let device = TcpListener::bind("127.0.0.1:0").unwrap();
    device.set_nonblocking(true).unwrap();
    write_project(&project, device.local_addr().unwrap().port());

    // Someone else already holds the API port.
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let held_addr = held.local_addr().unwrap().to_string();

    let mut child = start_runtime(&project, &state, &held_addr);
    let status = wait_for_exit(&mut child, Duration::from_secs(30));
    if status.is_none() {
        let _ = child.kill();
    }
    let out = child.wait_with_output().unwrap();
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let status = status.unwrap_or_else(|| panic!("the runtime did not exit: {log}"));
    assert!(
        !status.success(),
        "a failed bind must fail the process: {log}"
    );
    assert!(
        log.contains(&format!("binding {held_addr}")),
        "the failure must say what it could not bind: {log}"
    );
    assert!(
        device.accept().is_err(),
        "the runtime reached the field device although it could not serve its API: {log}"
    );
    // Nothing the runtime does with its surroundings (here: the history
    // directory it creates under the state dir) may precede the socket claim.
    assert!(
        !state.exists(),
        "startup work ran before the API socket was claimed: {log}"
    );
}
