//! Process ownership, readiness and diagnostics for the CLI integration tests.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const LISTEN_PREFIX: &str = "IA2_LISTEN_ADDR=";
const LOG_HALF: usize = 128 * 1024;

pub fn server_binary() -> PathBuf {
    // Cargo resolves the cs path in this build's target triple and profile.
    let cs = assert_cmd::cargo::cargo_bin("cs");
    cs.parent()
        .expect("cs binary directory")
        .join(format!("server{}", std::env::consts::EXE_SUFFIX))
}

pub struct ServerProcess {
    child: Child,
    drains: Vec<JoinHandle<()>>,
    record: PathBuf,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if !self.drains.is_empty() {
            let status = self.stop();
            if let Ok(mut record) = OpenOptions::new().append(true).open(&self.record) {
                let _ = writeln!(record, "exit: {status:?}");
            }
        }
    }
}

impl ServerProcess {
    fn stop(&mut self) -> std::io::Result<ExitStatus> {
        let _ = self.child.kill();
        let status = self.child.wait();
        for drain in self.drains.drain(..) {
            let _ = drain.join();
        }
        status
    }

    fn check_exit(&mut self) -> Result<(), String> {
        match self.child.try_wait() {
            Ok(None) => Ok(()),
            Ok(Some(status)) => Err(format!("server exited before readiness: {status}")),
            Err(e) => Err(format!("checking server exit status: {e}")),
        }
    }
}

pub fn start(
    mut command: Command,
    requested: SocketAddr,
    logs: &Path,
    timeout: Duration,
) -> Result<(String, ServerProcess), String> {
    let program = PathBuf::from(command.get_program());
    let stdout = File::create(logs.join("stdout.log")).map_err(|e| e.to_string())?;
    let stderr = File::create(logs.join("stderr.log")).map_err(|e| e.to_string())?;
    // Keep arguments and environment separate. Sandbox and caller overrides
    // may contain credentials, so never dump the environment.
    let context = format!(
        "binary: {}\nargs: {:?}\nrequested bind: {requested}\nlogs: {}",
        program.display(),
        command.get_args().collect::<Vec<_>>(),
        logs.display(),
    );
    let record = logs.join("launch.txt");
    fs::write(&record, format!("{context}\n")).map_err(|e| e.to_string())?;

    let deadline = Instant::now() + timeout;
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            let cause = format!(
                "server binary could not be executed: {e}; build -p server in the same target/profile"
            );
            let message = format!("{cause}\n{context}");
            let _ = fs::write(record, &message);
            return Err(message);
        }
    };
    let out_pipe = child.stdout.take().expect("piped stdout");
    let err_pipe = child.stderr.take().expect("piped stderr");
    let (bound_tx, bound_rx) = mpsc::channel();
    let mut process = ServerProcess {
        child,
        record: record.clone(),
        drains: vec![
            thread::spawn(move || drain(out_pipe, stdout, Some(bound_tx))),
            thread::spawn(move || drain(err_pipe, stderr, None)),
        ],
    };
    match wait_ready(&mut process, requested, &bound_rx, deadline) {
        Ok(base) => {
            let _ = fs::write(
                record,
                format!("{context}\npid: {}\nready: {base}\n", process.child.id()),
            );
            Ok((base, process))
        }
        Err(cause) => {
            let status = process.stop();
            let message = format!(
                "{cause}\n{context}\npid: {}\nexit: {status:?}\nstdout tail:\n{}\nstderr tail:\n{}",
                process.child.id(),
                tail(&logs.join("stdout.log")),
                tail(&logs.join("stderr.log"))
            );
            let _ = fs::write(record, &message);
            Err(message)
        }
    }
}

fn wait_ready(
    process: &mut ServerProcess,
    requested: SocketAddr,
    bound_rx: &Receiver<SocketAddr>,
    deadline: Instant,
) -> Result<String, String> {
    let mut base = None;
    loop {
        process.check_exit()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "server readiness timed out (bound address: {base:?})"
            ));
        }
        if base.is_none() {
            if let Ok(bound) = bound_rx.try_recv() {
                if bound.ip() != requested.ip()
                    || bound.port() == 0
                    || (requested.port() != 0 && bound.port() != requested.port())
                {
                    return Err(format!("server reported unexpected listen address {bound}"));
                }
                // Existing mapped-IPv6 host guard tests reach the listener
                // over its IPv4 loopback address.
                base = Some(format!("http://127.0.0.1:{}", bound.port()));
            }
        }
        if let Some(base) = &base {
            let attempt = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(250));
            // ureq's connect timeout takes precedence over the request timeout.
            if !attempt.is_zero()
                && ureq::AgentBuilder::new()
                    .redirects(0)
                    .try_proxy_from_env(false)
                    .timeout_connect(attempt)
                    .build()
                    .get(&format!("{base}/health"))
                    .timeout(attempt)
                    .call()
                    .is_ok_and(|response| response.status() == 200)
                && Instant::now() <= deadline
            {
                process.check_exit()?;
                return Ok(base.clone());
            }
        }
        thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(15)),
        );
    }
}

fn drain(mut pipe: impl Read, mut log: File, mut bound: Option<Sender<SocketAddr>>) {
    let mut line = Vec::new();
    let mut chunk = [0; 8192];
    let mut head_left = LOG_HALF;
    let mut tail = std::collections::VecDeque::with_capacity(LOG_HALF);
    let mut omitted = false;
    while let Ok(size) = pipe.read(&mut chunk) {
        if size == 0 {
            break;
        }
        let bytes = &chunk[..size];
        if let Some(sender) = &bound {
            // This opt-in announcement is the first stdout line. Limit
            // buffering even if a broken binary writes a huge line.
            let end = bytes.iter().position(|&byte| byte == b'\n');
            let first = &bytes[..end.unwrap_or(size)];
            if line.len() + first.len() > 128 {
                bound = None;
            } else {
                line.extend_from_slice(first);
                if end.is_some() {
                    if let Some(address) = std::str::from_utf8(&line)
                        .ok()
                        .and_then(|line| line.strip_prefix(LISTEN_PREFIX))
                        .and_then(|value| value.trim().parse().ok())
                    {
                        let _ = sender.send(address);
                    }
                    bound = None;
                }
            }
        }
        let head = size.min(head_left);
        let _ = log.write_all(&bytes[..head]);
        head_left -= head;
        let rest = &bytes[head..];
        let overflow = (tail.len() + rest.len()).saturating_sub(LOG_HALF);
        omitted |= overflow != 0;
        tail.drain(..overflow);
        tail.extend(rest);
    }
    if omitted {
        let _ = log.write_all(b"\n[intervening output omitted]\n");
    }
    let (first, second) = tail.as_slices();
    let _ = log.write_all(first);
    let _ = log.write_all(second);
}

fn tail(path: &Path) -> String {
    let contents = (|| {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(4096)))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok::<_, std::io::Error>(bytes)
    })();
    match contents {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => format!("could not read {}: {error}", path.display()),
    }
}
