//! Process ownership, readiness and diagnostics for the CLI integration tests.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const LISTEN_PREFIX: &str = "IA2_LISTEN_ADDR=";
const LOG_HALF: usize = 128 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(15);
pub(super) const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

pub fn server_binary() -> PathBuf {
    // Cargo resolves the cs path in this build's target triple and profile.
    let cs = assert_cmd::cargo::cargo_bin("cs");
    cs.parent()
        .expect("cs binary directory")
        .join(format!("server{}", std::env::consts::EXE_SUFFIX))
}

pub struct ServerProcess {
    child: Child,
    drains: Vec<JoinHandle<io::Result<()>>>,
    cancel_drains: Arc<AtomicBool>,
    record: PathBuf,
    stopped: bool,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if !self.stopped {
            let outcome = self.stop();
            if let Ok(mut record) = OpenOptions::new().append(true).open(&self.record) {
                let _ = writeln!(record, "{outcome}");
            }
        }
    }
}

impl ServerProcess {
    fn stop(&mut self) -> String {
        self.stopped = true;
        let deadline = Instant::now() + CLEANUP_TIMEOUT;
        let mut errors = Vec::new();
        let stop = self.kill_owned_processes();
        let mut exit = None;
        loop {
            if exit.is_none() {
                exit = self.child.try_wait().transpose();
            }
            if exit.is_some() && self.drains.iter().all(JoinHandle::is_finished) {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(remaining.min(POLL_INTERVAL));
        }
        if let Err(error) = stop {
            // Darwin returns EPERM for a group containing only zombies. Recheck
            // after reaping the leader without sending another signal.
            if !self.group_has_exited() {
                errors.push(format!("sending stop signal: {error}"));
            }
        }
        self.cancel_drains.store(true, Ordering::Relaxed);
        for drain in self.drains.drain(..) {
            if drain.is_finished() {
                match drain.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => errors.push(format!("collecting server logs: {error}")),
                    Err(_) => errors.push("server log thread panicked".into()),
                }
            } else {
                // A writer outside the group or an I/O stall can withhold EOF.
                // Never wait past the cleanup deadline to join a reader.
                errors.push("server log stream did not close before the cleanup deadline".into());
            }
        }
        let status = match exit {
            Some(Ok(status)) => format!("exit: {status}"),
            Some(Err(error)) => format!("exit status unavailable: {error}"),
            None => "exit status unavailable before the cleanup deadline".into(),
        };
        if errors.is_empty() {
            status
        } else {
            format!("{status}\ncleanup: {}", errors.join("\n"))
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn kill_owned_processes(&mut self) -> io::Result<()> {
        use rustix::io::Errno;
        use rustix::process::{kill_process_group, Pid, Signal};

        // The group leader has not been reaped. Its PID still reserves this
        // group even if it exited before its descendants closed their pipes.
        match kill_process_group(Pid::from_child(&self.child), Signal::KILL) {
            Ok(()) | Err(Errno::SRCH) => Ok(()),
            Err(error) => {
                let _ = self.child.kill();
                Err(error.into())
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn group_has_exited(&self) -> bool {
        use rustix::io::Errno;
        use rustix::process::{test_kill_process_group, Pid};

        test_kill_process_group(Pid::from_child(&self.child)) == Err(Errno::SRCH)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn group_has_exited(&self) -> bool {
        false
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn kill_owned_processes(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn check_exit(&mut self) -> Result<(), String> {
        use rustix::process::{waitid, Pid, WaitId, WaitIdOptions};

        let status = waitid(
            WaitId::Pid(Pid::from_child(&self.child)),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .map_err(|error| format!("checking server exit status: {error}"))?;
        if status.is_some() {
            return Err("server exited before readiness".into());
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn check_exit(&mut self) -> Result<(), String> {
        match self.child.try_wait() {
            Ok(None) => Ok(()),
            Ok(Some(status)) => Err(format!("server exited before readiness: {status}")),
            Err(e) => Err(format!("checking server exit status: {e}")),
        }
    }
}

fn log_pipe() -> io::Result<(io::PipeReader, io::PipeWriter)> {
    let (reader, writer) = io::pipe()?;
    #[cfg(unix)]
    {
        use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
        fcntl_setfl(&reader, fcntl_getfl(&reader)? | OFlags::NONBLOCK)?;
    }
    Ok((reader, writer))
}

/// Allow `timeout` for readiness and at most `CLEANUP_TIMEOUT` more for
/// reaping and draining on failure. Dropping a ready server uses the same
/// cleanup budget.
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

    let (out_pipe, out_writer) = log_pipe().map_err(|e| format!("creating stdout pipe: {e}"))?;
    let (err_pipe, err_writer) = log_pipe().map_err(|e| format!("creating stderr pipe: {e}"))?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let deadline = Instant::now() + timeout;
    let child = match command
        .stdin(Stdio::null())
        .stdout(out_writer)
        .stderr(err_writer)
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
    // Drop the Command's copies of the writers so the readers can see EOF.
    drop(command);
    let (bound_tx, bound_rx) = mpsc::channel();
    let cancel_drains = Arc::new(AtomicBool::new(false));
    let mut process = ServerProcess {
        child,
        record: record.clone(),
        drains: Vec::new(),
        cancel_drains,
        stopped: false,
    };
    let readiness = (|| {
        for (name, pipe, log, bound) in [
            ("stdout", out_pipe, stdout, Some(bound_tx)),
            ("stderr", err_pipe, stderr, None),
        ] {
            let drain = start_drain(pipe, log, bound, &process.cancel_drains)
                .map_err(|error| format!("starting {name} log reader: {error}"))?;
            process.drains.push(drain);
        }
        wait_ready(&mut process, requested, &bound_rx, deadline)
    })();
    match readiness {
        Ok(base) => {
            let _ = fs::write(
                record,
                format!("{context}\npid: {}\nready: {base}\n", process.child.id()),
            );
            Ok((base, process))
        }
        Err(cause) => {
            let outcome = process.stop();
            let message = format!(
                "{cause}\n{context}\npid: {}\n{outcome}\nstdout tail:\n{}\nstderr tail:\n{}",
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
                .min(POLL_INTERVAL),
        );
    }
}

fn start_drain(
    pipe: io::PipeReader,
    log: File,
    bound: Option<Sender<SocketAddr>>,
    cancelled: &Arc<AtomicBool>,
) -> io::Result<JoinHandle<io::Result<()>>> {
    let cancelled = Arc::clone(cancelled);
    thread::Builder::new().spawn(move || drain(pipe, log, bound, &cancelled))
}

fn drain(
    mut pipe: impl Read,
    mut log: File,
    mut bound: Option<Sender<SocketAddr>>,
    cancelled: &AtomicBool,
) -> io::Result<()> {
    let mut line = Vec::new();
    let mut chunk = [0; 8192];
    let mut head_left = LOG_HALF;
    let mut tail = std::collections::VecDeque::with_capacity(LOG_HALF);
    let mut omitted = false;
    let read_result = loop {
        if cancelled.load(Ordering::Relaxed) {
            break Ok(());
        }
        let size = match pipe.read(&mut chunk) {
            Ok(0) => break Ok(()),
            Ok(size) => size,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(POLL_INTERVAL);
                continue;
            }
            Err(error) => break Err(error),
        };
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
        log.write_all(&bytes[..head])?;
        head_left -= head;
        let rest = &bytes[head..];
        let overflow = (tail.len() + rest.len()).saturating_sub(LOG_HALF);
        omitted |= overflow != 0;
        tail.drain(..overflow);
        tail.extend(rest);
    };
    if omitted {
        log.write_all(b"\n[intervening output omitted]\n")?;
    }
    let (first, second) = tail.as_slices();
    log.write_all(first)?;
    log.write_all(second)?;
    read_result
}

fn tail(path: &Path) -> String {
    let contents = (|| {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(4096)))?;
        let mut bytes = Vec::new();
        file.take(4096).read_to_end(&mut bytes)?;
        Ok::<_, std::io::Error>(bytes)
    })();
    match contents {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => format!("could not read {}: {error}", path.display()),
    }
}
