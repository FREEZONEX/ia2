//! Edge orchestration: SSH-based probe, deploy, and attach.
//!
//! Trust model:
//!  - The dev machine has `ssh` available and resolves `host` via
//!    `~/.ssh/config` (keys, agent, jump hosts).
//!  - The edge box has `ia2-runtime` either pre-installed at
//!    `<install_dir>/current/runtime` (after `infra/install.sh`) or it's
//!    pushed by deploy.
//!  - Remote network access to the runtime's monitor server is **only**
//!    via the SSH port-forward set up by `attach`. The runtime always
//!    binds `127.0.0.1` on the edge.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use ironplc_bridge::monitor::{SnapshotTapHealth, SupervisionInForce};
use ironplc_bridge::DeviceHealth;
use project::Edge;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use ts_rs::TS;

// ============================================================
//  Active attachments — port-forward + chosen local port
// ============================================================

/// One live `ssh -N -L` child plus the local port it's listening on.
struct ActiveAttachment {
    local_port: u16,
    /// Keeping the `Child` alive is the whole point — `kill_on_drop(true)`
    /// is what tears down the tunnel when the entry is removed/replaced.
    #[allow(dead_code)]
    child: Child,
}

#[derive(Default)]
pub struct AttachmentRegistry {
    /// Keyed by `(project_name, edge_name)`. Two projects with an
    /// identically-named edge keep separate tunnels; closing one
    /// doesn't touch the other. Replacing an entry drops the previous
    /// Child, which (because the runtime spawned it with
    /// `kill_on_drop(true)`) terminates the ssh tunnel cleanly.
    by_key: Mutex<HashMap<(String, String), ActiveAttachment>>,
}

impl AttachmentRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn current_port(&self, project_name: &str, edge_name: &str) -> Option<u16> {
        self.by_key
            .lock()
            .get(&(project_name.to_string(), edge_name.to_string()))
            .map(|a| a.local_port)
    }

    pub fn insert(&self, project_name: String, edge_name: String, local_port: u16, child: Child) {
        self.by_key.lock().insert(
            (project_name, edge_name),
            ActiveAttachment { local_port, child },
        );
    }

    /// Stop the port-forward for one edge (if any). Returns whether
    /// something was actually running.
    pub fn detach(&self, project_name: &str, edge_name: &str) -> bool {
        let mut guard = self.by_key.lock();
        guard
            .remove(&(project_name.to_string(), edge_name.to_string()))
            .is_some()
    }

    /// Drop every tunnel attached to a given project. Called from
    /// `/api/projects/{name}/close` so closing a project tears down
    /// its tunnels without affecting other projects' tunnels.
    pub fn detach_all_for_project(&self, project_name: &str) {
        let mut guard = self.by_key.lock();
        guard.retain(|(p, _), _| p != project_name);
    }
}

// ============================================================
//  Probe — quick reachability + version snapshot
// ============================================================

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct EdgeProbe {
    /// `true` when the ssh + curl chain reached the runtime's `/health`.
    pub reachable: bool,
    /// Latest scan count from `/health`, if reachable.
    pub scan_count: Option<u64>,
    /// Uptime reported by the runtime's `/health`, if reachable.
    pub uptime_secs: Option<u64>,
    /// Runtime version (from `/status`), if reachable. Empty if probe
    /// stopped at the cheaper `/health` step.
    pub runtime_version: Option<String>,
    /// Whether every configured fieldbus device on the edge is connected.
    /// `Some(false)` means the runtime is up and scanning but at least one
    /// bus is down — inputs frozen, outputs dropped — which is emphatically
    /// NOT the same as "reachable". `None` when unreachable, or when the
    /// edge runs a build predating per-device health in `/health`.
    pub fieldbus_healthy: Option<bool>,
    /// Names of the devices behind a `fieldbus_healthy: Some(false)`, so a
    /// caller can say *which* bus died without a second round trip.
    pub unhealthy_devices: Vec<String>,
    /// `Some(true)` when the edge's scan watchdog has latched its outputs
    /// off. Such a runtime is reachable AND `fieldbus_healthy` — it answers
    /// HTTP and keeps scanning — while driving nothing, so a script that
    /// gates only on those two would call a dead plant healthy. `None` when
    /// unreachable, or when the edge runs a build predating the flag.
    pub watchdog_tripped: Option<bool>,
    /// Why the outputs are latched off, when `watchdog_tripped` is true: the
    /// scan watchdog, an execution-budget trap, or the project's
    /// alarm-supervision policy. A reader must not call every latch a watchdog
    /// trip. `None` when not latched, when unreachable, or when the edge runs a
    /// build that predates the field (which could only have latched by the
    /// scan watchdog).
    pub latch_reason: Option<String>,
    /// Why the edge's program stopped (a VM trap, say), when it did. Such a
    /// runtime is reachable — it answers HTTP — while running nothing, so
    /// like `watchdog_tripped` this must be read before calling the edge
    /// healthy. `None` when unreachable, while the program runs, or when
    /// the edge runs a build whose `/health` predates the field (its
    /// `/status` still carries `fault`).
    pub fault: Option<String>,
    /// Whether the edge's history, process alarms and `scan_count` are still
    /// being fed from its scan loop. A runtime whose feeding task has ended
    /// is reachable, fieldbus-healthy and running its program, while it
    /// records no history and evaluates no alarms, so a gate that reads only
    /// the fields above calls it healthy. `running: false` is the persistent
    /// state; a `faults` count that climbs between two probes means it is
    /// failing now. `None` when unreachable, or when the edge runs a build
    /// whose `/health` predates the field.
    pub snapshot_tap: Option<SnapshotTapHealth>,
    /// The alarm-supervision policy the edge's runtime is actually enforcing
    /// (what it read at startup). `None` when unreachable, or when the runtime
    /// predates the field: for a project that declares `on_loss = "safe_state"`
    /// that means the declared protection is NOT in force, because such a
    /// runtime ignores the table. The post-restart check of a deploy compares
    /// this with the project's declaration.
    pub supervision: Option<SupervisionInForce>,
    /// First line of stderr / error message when unreachable. Gives the
    /// user enough hint to fix `~/.ssh/config` or `install_dir`.
    pub error: Option<String>,
}

/// systemd unit name for the edge runtime. The unit is the single source
/// of truth on the box for *where* the runtime listens (its `--bind` port)
/// and *which* project dir it runs — so when the configured port doesn't
/// answer we ask systemd rather than failing blind on one fixed port.
const EDGE_UNIT: &str = "ia2";

/// What the box's service manager reports about the runtime. Authoritative:
/// the edge config's `runtime_port` is only a fast-path hint, so a port/path
/// drift (or a stopped service) yields a real answer instead of a bare
/// connection failure.
#[derive(Debug, Default)]
struct ServiceState {
    /// `ActiveState` verbatim (`active`|`inactive`|`failed`|…). Empty when
    /// systemd or the unit isn't present (non-systemd edge / not installed).
    active_state: String,
    /// Port parsed from the unit's ExecStart `--bind 127.0.0.1:PORT`.
    bind_port: Option<u16>,
    /// `--project-dir` from ExecStart — lets deploy detect path drift.
    project_dir: Option<String>,
}

impl ServiceState {
    fn is_active(&self) -> bool {
        self.active_state == "active"
    }
}

/// Ask systemd on the edge about the runtime unit (one ssh round-trip).
/// `systemctl show` prints empty values for an unknown unit and still exits
/// 0, so we parse defensively and never error on "no such unit".
async fn query_service(edge: &Edge) -> ServiceState {
    let cmd =
        format!("systemctl show {EDGE_UNIT} -p ActiveState -p ExecStart --no-pager 2>/dev/null");
    let Ok(out) = ssh_cmd(edge).arg(cmd).output().await else {
        return ServiceState::default();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut st = ServiceState::default();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("ActiveState=") {
            st.active_state = v.trim().to_string();
        } else if line.starts_with("ExecStart=") {
            // The value embeds `argv[]=<bin> --project-dir <dir> --bind host:port ; …`.
            // Scan adjacent tokens for the two flags we care about.
            let toks: Vec<&str> = line.split_whitespace().collect();
            for w in toks.windows(2) {
                match w[0] {
                    "--bind" => st.bind_port = w[1].rsplit(':').next().and_then(|p| p.parse().ok()),
                    "--project-dir" => st.project_dir = Some(w[1].to_string()),
                    _ => {}
                }
            }
        }
    }
    st
}

/// Result of one `ssh host curl …` attempt, split so callers can tell
/// "the box is unreachable" (ssh) apart from "nothing is listening on that
/// port" (curl) — the two need very different remedies.
enum CurlOutcome {
    Body(String),
    /// curl ran but couldn't connect (e.g. exit 7) — runtime not on that port.
    NotListening,
    SshFailed(String),
}

async fn run_ssh_curl(edge: &Edge, remote_cmd: &str) -> CurlOutcome {
    let out = match ssh_cmd(edge).arg(remote_cmd).output().await {
        Ok(o) => o,
        Err(e) => return CurlOutcome::SshFailed(format!("spawn ssh: {e}")),
    };
    if out.status.success() {
        return CurlOutcome::Body(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    // ssh uses exit 255 for its own connect/auth failures; any other code is
    // the remote curl's (e.g. 7 = connection refused → nothing on that port).
    if out.status.code() == Some(255) {
        CurlOutcome::SshFailed(first_line(&String::from_utf8_lossy(&out.stderr)).to_string())
    } else {
        CurlOutcome::NotListening
    }
}

/// Reach the edge runtime over ssh+curl without hanging on one fixed port.
/// Strategy: try the configured port first (the fast common case); if
/// nothing answers there, consult systemd — is the service even running,
/// and what port did it actually bind? Returns the body or a layered,
/// actionable error. `make_cmd(port)` builds the remote curl command.
async fn edge_runtime_curl(
    edge: &Edge,
    make_cmd: impl Fn(u16) -> String,
) -> Result<String, String> {
    match run_ssh_curl(edge, &make_cmd(edge.runtime_port)).await {
        CurlOutcome::Body(b) => return Ok(b),
        CurlOutcome::SshFailed(e) => return Err(format!("ssh to {} failed: {e}", edge.host)),
        CurlOutcome::NotListening => {} // fall through to the source of truth
    }

    let svc = query_service(edge).await;
    if !svc.is_active() {
        let state = if svc.active_state.is_empty() {
            "not installed".to_string()
        } else {
            svc.active_state.clone()
        };
        return Err(format!(
            "runtime not reachable on {host}: systemd unit '{EDGE_UNIT}' is {state} \
             — start it with `sudo systemctl start {EDGE_UNIT}`",
            host = edge.host,
        ));
    }
    match svc.bind_port {
        // Active, but on a different port than configured — recover via the real one.
        Some(p) if p != edge.runtime_port => match run_ssh_curl(edge, &make_cmd(p)).await {
            CurlOutcome::Body(b) => Ok(b),
            _ => Err(format!(
                "'{EDGE_UNIT}' is active on {host} bound to :{p}, but the edge config has \
                 runtime_port={cfg} and neither answers — reconcile runtime_port with the unit",
                host = edge.host,
                cfg = edge.runtime_port,
            )),
        },
        _ => Err(format!(
            "'{EDGE_UNIT}' is active on {host} but not answering on :{} — health endpoint may be down",
            edge.runtime_port,
            host = edge.host,
        )),
    }
}

/// Probe the edge runtime's `/health` (port-resilient — see `edge_runtime_curl`).
pub async fn probe_edge(edge: &Edge) -> EdgeProbe {
    let body = match edge_runtime_curl(edge, |port| {
        format!("curl --silent --max-time 3 http://127.0.0.1:{port}/health")
    })
    .await
    {
        Ok(b) => b,
        Err(e) => return unreachable_probe(e),
    };
    probe_from_health_body(&body)
}

fn unreachable_probe(error: String) -> EdgeProbe {
    EdgeProbe {
        reachable: false,
        scan_count: None,
        uptime_secs: None,
        runtime_version: None,
        fieldbus_healthy: None,
        unhealthy_devices: vec![],
        watchdog_tripped: None,
        latch_reason: None,
        fault: None,
        snapshot_tap: None,
        supervision: None,
        error: Some(error),
    }
}

/// Map an edge runtime's `/health` body onto an `EdgeProbe`. Split out of
/// `probe_edge` so the wire contract is unit-testable without an ssh hop —
/// the whole class of bug this guards is "a field the edge sent got dropped
/// on the floor here", which no integration test would have caught either.
fn probe_from_health_body(body: &str) -> EdgeProbe {
    #[derive(Deserialize)]
    struct Health {
        status: String,
        uptime_secs: u64,
        scan_count: u64,
        /// Defaulted so probing an edge whose runtime predates per-device
        /// health degrades to "unknown" instead of failing the whole probe.
        #[serde(default)]
        fieldbus_healthy: Option<bool>,
        #[serde(default)]
        devices: Vec<DeviceHealth>,
        /// Defaulted for the same reason as `fieldbus_healthy`: an older
        /// edge build simply omits it, which must read as "unknown".
        #[serde(default)]
        watchdog_tripped: Option<bool>,
        /// Absent on an older edge build, which reads as "none reported".
        #[serde(default)]
        latch_reason: Option<String>,
        /// Absent on an older edge build, which reads as "none reported".
        #[serde(default)]
        fault: Option<String>,
        /// Absent on an older edge build, which reads as "unknown".
        #[serde(default)]
        snapshot_tap: Option<SnapshotTapHealth>,
        /// Absent on a runtime that predates it, which reads as "unknown".
        #[serde(default)]
        supervision: Option<SupervisionInForce>,
    }
    let Ok(parsed) = serde_json::from_str::<Health>(body) else {
        return unreachable_probe(format!("unexpected body: {}", first_line(body)));
    };
    if parsed.status != "ok" {
        return unreachable_probe(format!("runtime not ok: {}", parsed.status));
    }
    EdgeProbe {
        reachable: true,
        scan_count: Some(parsed.scan_count),
        uptime_secs: Some(parsed.uptime_secs),
        runtime_version: None,
        fieldbus_healthy: parsed.fieldbus_healthy,
        unhealthy_devices: parsed
            .devices
            .iter()
            .filter(|d| !d.healthy)
            .map(|d| d.name.clone())
            .collect(),
        watchdog_tripped: parsed.watchdog_tripped,
        latch_reason: parsed.latch_reason,
        fault: parsed.fault,
        snapshot_tap: parsed.snapshot_tap,
        supervision: parsed.supervision,
        error: None,
    }
}

// ============================================================
//  Logs — pull recent runtime log lines over ssh+curl
// ============================================================

/// GET a JSON endpoint on the edge runtime (over ssh, same trust model
/// as `probe`) and return the body verbatim. One helper behind all the
/// read-side edge proxies:
///   `/logs?tail=N` — recent captured log lines (EtherCAT discovery,
///   bus health, connect errors that otherwise live only in journald);
///   `/discover` — per-device connect status + EtherCAT topology, so
///   the IDE can author PDO maps against the real bus;
///   `/system` — NICs / serial ports / arch, so device configs are
///   authored against real edge facts rather than guesses;
///   `/status` — project, scan count, debug mode/forces, and the last
///   VarSnapshot (with per-variable types, which `cs runtime --edge`
///   uses to pack force/write values).
pub async fn fetch_edge_json(
    edge: &Edge,
    path_and_query: &str,
) -> Result<serde_json::Value, String> {
    let body = edge_runtime_curl(edge, |port| {
        format!("curl --silent --max-time 4 'http://127.0.0.1:{port}{path_and_query}'")
    })
    .await?;
    serde_json::from_str::<serde_json::Value>(&body).map_err(|e| {
        format!(
            "unexpected {path_and_query} body: {} ({e})",
            first_line(&body)
        )
    })
}

// ============================================================
//  Online debug control — proxy pause/step/write/force to the edge
// ============================================================

/// Error from proxying an online-debug op to the edge runtime.
/// `Status` carries the edge's own non-2xx answer so the route can
/// replay status + body VERBATIM (ADR-0002 truthfulness: a governance
/// 403 on the edge must still be a 403 after the ssh hop — collapsing
/// it into a 500 would misreport a policy denial as an infrastructure
/// failure). `Transport` is everything that prevented an HTTP
/// conversation with the edge (ssh, curl, an unparseable response).
#[derive(Debug, PartialEq)]
pub enum EdgeRuntimeError {
    Status { code: u16, body: String },
    Transport(String),
}

/// POST a JSON body to an edge runtime control endpoint over ssh+curl
/// (`pause` / `resume` / `step` / `write` / `force` / `unforce`). The
/// caller must whitelist `path` — it's interpolated into the remote
/// command. Single quotes in the body are escaped for the shell. The
/// remote curl appends the HTTP status on a trailing line (`-w`) so
/// the edge's own 4xx/5xx answers come back typed instead of being
/// collapsed into a transport error.
pub async fn proxy_edge_runtime_op(
    edge: &Edge,
    path: &str,
    body: &serde_json::Value,
    origin: Option<&str>,
) -> Result<serde_json::Value, EdgeRuntimeError> {
    let body_str = body.to_string().replace('\'', r"'\''");
    let origin_header = origin_header_arg(origin);
    let resp = edge_runtime_curl(edge, |port| {
        format!(
            "curl --silent --max-time 4 -w '\\n%{{http_code}}' -X POST \
             -H 'Content-Type: application/json' \
             {origin_header}-d '{body_str}' http://127.0.0.1:{port}/{path}"
        )
    })
    .await
    .map_err(EdgeRuntimeError::Transport)?;
    parse_edge_runtime_response(&resp)
}

/// Build the `-H 'x-ia2-origin: …'` fragment for the remote curl. The
/// header value is interpolated into a remote shell command, so it
/// must be shell-safe — but a non-empty label is SANITIZED (via the
/// shared `state::sanitize_origin`: `[A-Za-z0-9._-]`, 64-char cap),
/// never silently dropped. Dropping it would make the server-side
/// overlay (which saw the declared origin) and the edge's audit ring
/// (which would record "anonymous") disagree about the same write.
fn origin_header_arg(origin: Option<&str>) -> String {
    origin
        .and_then(crate::state::sanitize_origin)
        .map(|o| format!("-H 'x-ia2-origin: {o}' "))
        .unwrap_or_default()
}

/// Interpret the `<body>\n<status>` shape produced by the remote
/// curl's `-w '\n%{http_code}'`. Split out of `proxy_edge_runtime_op`
/// so the status passthrough is unit-testable without an ssh hop.
fn parse_edge_runtime_response(resp: &str) -> Result<serde_json::Value, EdgeRuntimeError> {
    let Some((body, code)) = resp
        .trim_end()
        .rsplit_once('\n')
        .and_then(|(body, code)| Some((body, code.trim().parse::<u16>().ok()?)))
    else {
        return Err(EdgeRuntimeError::Transport(format!(
            "edge runtime answered without a status marker: {}",
            first_line(resp)
        )));
    };
    if (200..300).contains(&code) {
        // The edge returns JSON on every 2xx.
        serde_json::from_str::<serde_json::Value>(body).map_err(|e| {
            EdgeRuntimeError::Transport(format!(
                "unexpected edge runtime body: {} ({e})",
                first_line(body)
            ))
        })
    } else {
        Err(EdgeRuntimeError::Status {
            code,
            body: body.trim().to_string(),
        })
    }
}

// ============================================================
//  Deploy — atomic versioned dir + symlink swap + systemctl restart
// ============================================================

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct DeployReport {
    pub ok: bool,
    /// Timestamped version directory created on the edge (e.g.
    /// `2026-05-12T08-30-00`).
    pub version: String,
    /// Tail of stdout/stderr from the remote script — useful for
    /// surfacing the "what just happened" to the user.
    pub log: String,
    /// Structured caveat on an otherwise-successful deploy: the
    /// install_dir/systemd drift warning, or a fieldbus device still down
    /// when the health check passed. `None` = nothing to flag.
    /// Machine-readable so clients don't have to grep the log.
    pub warning: Option<String>,
    /// What the restarted runtime reported: whether the deployed program
    /// is actually running. `ok` is `false` when continuous operation is
    /// not confirmed after a restart, although the
    /// new version is installed and current. `None` when the deploy failed
    /// before anything was restarted.
    pub health: Option<DeployHealth>,
}

/// The deployed program's state after the restart, read from the edge
/// runtime's `/status`.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[ts(export)]
pub struct DeployHealth {
    pub state: DeployHealthState,
    /// What the check saw: the scan count, the fault, what never happened,
    /// or why nothing was checked.
    pub detail: String,
    /// Devices down at the last read. The program runs, but inputs from
    /// these are frozen; right after a restart a device may still be
    /// connecting.
    pub unhealthy_devices: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum DeployHealthState {
    /// Consecutive reads show continuous mode and advancing scans without a fault.
    Running,
    /// The runtime answers, but its program stopped (the fault is in
    /// `detail`) or its scan watchdog latched the outputs off.
    Faulted,
    /// The runtime explicitly reports paused or single-step mode.
    NotRunning,
    /// The program may well be running, but the alarm-supervision policy the
    /// project declares (`[supervision] on_loss = "safe_state"`) is NOT the one
    /// the edge's runtime is enforcing: most often a runtime built before the
    /// setting, which ignores it without a word (a deploy that ships no binary
    /// keeps the edge's own). The declared protection does not exist. Fails
    /// the deploy.
    Unprotected,
    /// The runtime answers, but the task that feeds its history, its process
    /// alarms and the scan counter this check reads has stopped, or is
    /// failing right now. Nothing is recorded and no alarm is evaluated, and
    /// because `scan_count` freezes with it, the program's progress cannot be
    /// confirmed either. Fails the deploy: a runtime that cannot raise an
    /// alarm is not one to leave running unattended.
    Unmonitored,
    /// The check could not confirm progress within its deadline. This is
    /// not evidence that the program or the physical equipment stopped.
    Unknown,
    /// Nothing to check: no restart happened, or the service runs from
    /// another install_dir (see `warning`).
    NotChecked,
}

impl DeployHealthState {
    /// Whether the deploy achieved what it is for: the new program running.
    fn deploy_ok(self) -> bool {
        matches!(self, Self::Running | Self::NotChecked)
    }
}

/// How long deploy waits after a restart for the new program's first scan.
/// Devices connect before the first scan (an EtherCAT walk, a slow Modbus
/// connect), so this is generous; a healthy program answers in seconds.
const HEALTH_WINDOW: Duration = Duration::from_secs(30);
/// Between two reads of the edge's `/status`.
const HEALTH_POLL: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum DeployError {
    #[error("packaging project: {0}")]
    Pack(String),
    #[error("ssh: {0}")]
    Ssh(#[from] io::Error),
    #[error("remote script failed (exit {0}):\n{1}")]
    Remote(i32, String),
}

/// Deploy a project directory + optional runtime binary to one edge.
///
/// `snapshot`        private project copy that passed the runtime preflight.
/// `runtime_binary`  path to a built `ia2-runtime` binary
///                   for the edge's architecture. Optional — when None,
///                   the deploy reuses whatever binary is already under
///                   `<install_dir>/current/runtime`.
/// `web_dist`        built web assets (the IDE server's own
///                   `--static-dir`). Optional — when present they land
///                   at `<install_dir>/current/web` so the edge runtime
///                   can serve the standalone HMI panel; when None the
///                   remote script carries the previous version's `web/`
///                   forward (same rule as the binary).
pub async fn deploy_to_edge(
    edge: &Edge,
    snapshot: &crate::deploy_snapshot::DeploySnapshot,
    runtime_binary: Option<&std::path::Path>,
    web_dist: Option<&std::path::Path>,
) -> Result<DeployReport, DeployError> {
    let project_dir = snapshot.project_dir();
    // ---- Pack the checked project (+ optional binary) into a tar stream ----
    // Stream the checked copy with `tar -cf -` into SSH; no additional
    // archive file is needed. The script on the edge extracts to a
    // timestamped dir and atomically flips the symlink.
    let mut tar = Command::new("tar");
    tar.arg("-cf").arg("-");
    // Keep host-only metadata out of the archive — a macOS bsdtar
    // otherwise embeds pax records the edge's GNU tar warns about once
    // per entry, drowning the deploy log.
    tar.args(host_tar_metadata_flags());
    tar.arg("-C")
        .arg(project_dir.parent().unwrap_or(project_dir))
        .arg(
            project_dir
                .file_name()
                .map(|n| n.to_owned())
                .ok_or_else(|| DeployError::Pack("project dir has no name".into()))?,
        );
    if let Some(bin) = runtime_binary {
        let bin = bin
            .canonicalize()
            .map_err(|e| DeployError::Pack(e.to_string()))?;
        tar.arg("-C")
            .arg(bin.parent().unwrap())
            .arg(bin.file_name().unwrap());
    }
    let web_basename = match web_dist {
        Some(dir) => {
            let dir = dir
                .canonicalize()
                .map_err(|e| DeployError::Pack(e.to_string()))?;
            let name = dir
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
                .ok_or_else(|| DeployError::Pack("web dist dir has no utf-8 name".into()))?;
            tar.arg("-C").arg(dir.parent().unwrap()).arg(&name);
            Some(name)
        }
        None => None,
    };
    tar.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut tar_child = tar.spawn().map_err(|e| DeployError::Pack(e.to_string()))?;
    let mut tar_stdout = tar_child
        .stdout
        .take()
        .ok_or_else(|| DeployError::Pack("tar stdout missing".into()))?;

    // ---- ssh remote script ----
    let project_basename = project_dir
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| DeployError::Pack("project dir name not utf-8".into()))?;
    let binary_basename = runtime_binary
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .map(str::to_string);
    let script = remote_deploy_script(
        &edge.install_dir,
        project_basename,
        binary_basename.as_deref(),
        web_basename.as_deref(),
    );

    let mut ssh = ssh_cmd(edge)
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut ssh_stdin = ssh.stdin.take().expect("ssh stdin");

    // Stream tar → ssh stdin while we wait. The pump's result is
    // awaited below — a broken stream must fail the deploy, not be
    // discovered later as a half-written version on the edge.
    let pump = tokio::spawn(async move {
        let res = tokio::io::copy(&mut tar_stdout, &mut ssh_stdin).await;
        let _ = ssh_stdin.shutdown().await;
        res
    });

    let out = ssh.wait_with_output().await?;
    let pump_result = pump.await;
    let tar_status = tar_child
        .wait()
        .await
        .map_err(|e| DeployError::Pack(format!("waiting for tar: {e}")))?;

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = strip_pax_keyword_warnings(&String::from_utf8_lossy(&out.stderr));
    let combined = format!(
        "{stdout}{}{stderr}",
        if !stderr.is_empty() {
            "\n--stderr--\n"
        } else {
            ""
        }
    );

    // Remote verdict first — when the script failed, ITS error is the
    // story (a dying remote also breaks the local tar/pump with EPIPE,
    // which would otherwise mask the cause).
    if !out.status.success() {
        return Err(DeployError::Remote(
            out.status.code().unwrap_or(-1),
            combined,
        ));
    }

    // Truthfulness gate: the remote said OK, but the LOCAL half must
    // also have completed — tar exited cleanly and every byte reached
    // ssh. `set -euo pipefail` on the remote catches most truncations,
    // but not a tar that dies exactly on an entry boundary.
    if !tar_status.success() {
        return Err(DeployError::Pack(format!(
            "local tar exited with {tar_status} — the uploaded archive was incomplete"
        )));
    }
    match pump_result {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            return Err(DeployError::Pack(format!(
                "tar stream to the edge aborted mid-transfer: {e}"
            )));
        }
        Err(e) => {
            return Err(DeployError::Pack(format!("tar stream task panicked: {e}")));
        }
    }

    // The script prints `VERSION=<ts>` as its last informational line.
    // Its absence from a "successful" run means the script we shipped
    // and the script that ran have drifted — refuse to guess.
    let Some(version) = combined
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("VERSION="))
        .map(str::to_string)
    else {
        return Err(DeployError::Remote(
            0,
            format!("remote script succeeded but printed no VERSION= line — deploy state unknown\n{combined}"),
        ));
    };

    // Guard the classic drift: deploying to an `install_dir` the running
    // service doesn't actually read. systemd is the source of truth — if its
    // ExecStart runs from a different tree, this deploy is invisible to it.
    // Surfaced BOTH as a structured `warning` field and prepended to the
    // log, so agents don't have to grep prose.
    let mut combined = combined;
    let mut warning = None;
    let restarted = combined.lines().any(|l| l.starts_with("RESTARTED="));
    let svc = query_service(edge).await;
    if let Some(svc_root) = svc
        .project_dir
        .as_deref()
        .map(|pd| pd.strip_suffix("/current/project").unwrap_or(pd))
    {
        if svc_root != edge.install_dir {
            let msg = format!(
                "deployed to install_dir={} but systemd '{EDGE_UNIT}' runs from {} — \
                 the service will NOT see this deploy. Reconcile the edge's install_dir with the \
                 unit's INSTALL_DIR.",
                edge.install_dir, svc_root,
            );
            combined = format!("WARNING: {msg}\n{combined}");
            warning = Some(msg);
        }
    }

    // A restart that succeeded says nothing about the program: it may
    // fault on its first scan, or its VM may never start, while systemd
    // reports the unit active. Ask the runtime itself.
    let health = if warning.is_some() {
        DeployHealth {
            state: DeployHealthState::NotChecked,
            detail: "the service runs from another install_dir, so its state says nothing \
                     about this deploy"
                .into(),
            unhealthy_devices: Vec::new(),
        }
    } else if !restarted {
        DeployHealth {
            state: DeployHealthState::NotChecked,
            detail: format!(
                "nothing was restarted ({EDGE_UNIT}.service is not enabled or systemd is absent)"
            ),
            unhealthy_devices: Vec::new(),
        }
    } else {
        watch_restarted_program(
            || fetch_edge_json(edge, "/status"),
            HEALTH_WINDOW,
            HEALTH_POLL,
            snapshot.supervision(),
        )
        .await
    };
    let ok = health.state.deploy_ok();
    combined = format!("{combined}\nHEALTH={:?}: {}\n", health.state, health.detail);
    if !ok {
        let prev = combined
            .lines()
            .find_map(|l| l.strip_prefix("PREV="))
            .filter(|p| !p.is_empty());
        combined.push_str(&format!(
            "Version {version} is installed and current, but continuous operation was not confirmed. \
             Inspect runtime and plant state before redeploying or rolling back{} \
             (docs/edge-deploy.md, Rollback).\n",
            prev.map(|p| format!(" to {p}")).unwrap_or_default()
        ));
    } else if health.state == DeployHealthState::Running && !health.unhealthy_devices.is_empty() {
        warning = Some(format!(
            "the program runs, but {} {} down at the check (it may still be connecting): {}",
            health.unhealthy_devices.len(),
            if health.unhealthy_devices.len() == 1 {
                "device was"
            } else {
                "devices were"
            },
            health.unhealthy_devices.join(", ")
        ));
    }

    Ok(DeployReport {
        ok,
        version,
        log: combined,
        warning,
        health: Some(health),
    })
}

/// One read of `/status`. Missing fields remain unknown, never healthy defaults.
#[derive(Debug, Default)]
struct StatusReading {
    scan_count: Option<u64>,
    uptime_secs: Option<u64>,
    runtime_id: Option<String>,
    mode: Option<String>,
    fault: Option<String>,
    watchdog_tripped: bool,
    unhealthy_devices: Vec<String>,
    /// `snapshot_tap` from the runtime's `/status`; `None` for a build that
    /// predates it (unknown, never a failure).
    tap_running: Option<bool>,
    tap_faults: Option<u64>,
    tap_last_fault: Option<String>,
    /// `snapshot_tap.stalled_for_ms`: the tap is stuck inside one snapshot.
    tap_stalled_ms: Option<u64>,
    /// Why the outputs are latched off (`latch_reason`), when they are.
    latch_reason: Option<String>,
    /// The alarm-supervision policy the runtime says it is enforcing:
    /// (`on_loss`, `grace_secs`). `None` for a runtime that predates the field.
    supervision: Option<(String, u64)>,
}

fn status_reading(v: &serde_json::Value) -> StatusReading {
    StatusReading {
        scan_count: v.get("scan_count").and_then(|n| n.as_u64()),
        uptime_secs: v.get("uptime_secs").and_then(|n| n.as_u64()),
        runtime_id: v
            .get("runtime_id")
            .and_then(|n| n.as_str())
            .map(String::from),
        mode: v
            .pointer("/mode/kind")
            .and_then(|n| n.as_str())
            .map(String::from),
        fault: v.get("fault").and_then(|f| f.as_str()).map(String::from),
        watchdog_tripped: v
            .get("watchdog_tripped")
            .and_then(|w| w.as_bool())
            .unwrap_or(false),
        unhealthy_devices: v
            .get("device_health")
            .and_then(|d| d.as_array())
            .into_iter()
            .flatten()
            .filter(|d| d.get("healthy").and_then(|h| h.as_bool()) == Some(false))
            .filter_map(|d| d.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect(),
        tap_running: v.pointer("/snapshot_tap/running").and_then(|r| r.as_bool()),
        tap_faults: v.pointer("/snapshot_tap/faults").and_then(|n| n.as_u64()),
        tap_stalled_ms: v
            .pointer("/snapshot_tap/stalled_for_ms")
            .and_then(|n| n.as_u64()),
        latch_reason: v
            .get("latch_reason")
            .and_then(|r| r.as_str())
            .map(String::from),
        supervision: v.get("supervision").and_then(|s| {
            Some((
                s.get("on_loss")?.as_str()?.to_string(),
                s.get("grace_secs")?.as_u64()?,
            ))
        }),
        tap_last_fault: v
            .pointer("/snapshot_tap/last_fault")
            .and_then(|t| t.as_str())
            .map(String::from),
    }
}

/// Whether the policy the project declares is the one the runtime reports it is
/// enforcing; `Some(why not)` when it is not.
///
/// A runtime that reports nothing is an older build that ignores `[supervision]`:
/// that is only a problem for a project that declares something other than the
/// default (`report` asks for nothing to be enforced). For `report` the grace is
/// irrelevant and not compared.
fn supervision_not_in_force(
    declared: &project::SupervisionConfig,
    reported: Option<&(String, u64)>,
) -> Option<String> {
    use project::SupervisionAction::{Report, SafeState};
    let want = match declared.on_loss {
        Report => "report",
        SafeState => "safe_state",
    };
    match reported {
        None => (declared.on_loss == SafeState).then(|| {
            format!(
                "the project declares `[supervision] on_loss = \"safe_state\"` (grace {} s), but \
                 the runtime on this edge does not report an alarm-supervision policy: it \
                 predates the setting and ignores it, so the declared protection is NOT in \
                 force. Deploy a runtime built from this version (a deploy that ships no \
                 binary keeps the edge's own)",
                declared.grace_secs
            )
        }),
        Some((mode, grace)) => {
            let same = mode == want
                && (declared.on_loss != SafeState || *grace == u64::from(declared.grace_secs));
            (!same).then(|| {
                format!(
                    "the project declares on_loss = {want}{}, but the runtime on this edge \
                     reports on_loss = {mode}, grace {grace} s: the declared policy is NOT the \
                     one in force",
                    if declared.on_loss == SafeState {
                        format!(", grace_secs = {}", declared.grace_secs)
                    } else {
                        String::new()
                    }
                )
            })
        }
    }
}

impl StatusReading {
    fn advances_from(&self, previous: &Self) -> bool {
        let counters_advance = matches!((previous.scan_count, self.scan_count),
            (Some(before), Some(after)) if before > 0 && after > before);
        let uptime_continues = matches!((previous.uptime_secs, self.uptime_secs),
            (Some(before), Some(after)) if after >= before);
        // New runtimes supply a process identity. Older runtimes can only
        // offer the weaker evidence of a counter and uptime that did not reset.
        let same_process = match (&previous.runtime_id, &self.runtime_id) {
            (Some(before), Some(after)) => before == after,
            (None, None) => uptime_continues,
            _ => false,
        };
        counters_advance && uptime_continues && same_process
    }
}

/// Observe advancing scans in continuous mode. Equal counters are inconclusive:
/// a valid long-period task may not advance every poll. The entire check,
/// including SSH reads and sleeps, shares one hard deadline.
async fn watch_restarted_program<F, Fut>(
    mut read: F,
    window: Duration,
    poll: Duration,
    declared: &project::SupervisionConfig,
) -> DeployHealth
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value, String>>,
{
    let verdict = |state, detail: String, unhealthy_devices| DeployHealth {
        state,
        detail,
        unhealthy_devices,
    };
    let deadline = tokio::time::Instant::now() + window;
    let mut previous: Option<StatusReading> = None;
    // The tap's fault count at the previous read, kept across every mode: a
    // count that rises between two reads means it is failing now.
    let mut tap_faults_before: Option<u64> = None;
    let mut detail = "no status observation".to_string();
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        match tokio::time::timeout_at(deadline, read()).await {
            Ok(Ok(v)) => {
                let r = status_reading(&v);
                if let Some(fault) = r.fault {
                    return verdict(
                        DeployHealthState::Faulted,
                        format!("the program stopped: {fault}"),
                        r.unhealthy_devices,
                    );
                }
                if r.watchdog_tripped {
                    // The flag is one latch, whoever set it; the reason says who.
                    let detail = match r.latch_reason.as_deref() {
                        Some(why) => format!("the runtime latched its output phase off: {why}"),
                        None => "the scan watchdog latched the runtime's output phase off".into(),
                    };
                    return verdict(DeployHealthState::Faulted, detail, r.unhealthy_devices);
                }
                // The declared policy must be the one in force, whatever else is
                // true: a project that asked for `safe_state` and runs under a
                // runtime that ignores it is unprotected, and nothing else here
                // would say so.
                if let Some(detail) = supervision_not_in_force(declared, r.supervision.as_ref()) {
                    return verdict(DeployHealthState::Unprotected, detail, r.unhealthy_devices);
                }
                // Before the mode and progress checks: a stopped tap freezes
                // the very `scan_count` they read, and the specific cause
                // beats a generic "progress not confirmed".
                let latest = r
                    .tap_last_fault
                    .as_deref()
                    .map(|t| format!(" (latest: {t})"))
                    .unwrap_or_default();
                if r.tap_running == Some(false) {
                    return verdict(
                        DeployHealthState::Unmonitored,
                        format!(
                            "the runtime's snapshot task has stopped: no history is recorded \
                             and no alarm is evaluated, and the scan count this check reads is \
                             frozen with it, so the program's progress cannot be confirmed{latest}"
                        ),
                        r.unhealthy_devices,
                    );
                }
                if let Some(ms) = r.tap_stalled_ms {
                    return verdict(
                        DeployHealthState::Unmonitored,
                        format!(
                            "the runtime's snapshot task is stuck on one snapshot ({ms} ms): \
                             history, process alarms, the scan count and the live feed are not \
                             updating, so the program's progress cannot be confirmed{latest}"
                        ),
                        r.unhealthy_devices,
                    );
                }
                if let (Some(before), Some(now)) = (tap_faults_before, r.tap_faults) {
                    if now > before {
                        return verdict(
                            DeployHealthState::Unmonitored,
                            format!(
                                "the runtime's snapshot task is failing ({before} -> {now} faults \
                                 between two reads): history and alarm evaluation are missing \
                                 snapshots{latest}"
                            ),
                            r.unhealthy_devices,
                        );
                    }
                }
                if r.tap_faults.is_some() {
                    tap_faults_before = r.tap_faults;
                }
                // A fault that has not recurred does not fail the deploy, but
                // the verdict must not read as if there had been none.
                let tap_note = match r.tap_faults {
                    Some(n) if n > 0 => format!(
                        "; the snapshot task recorded {n} fault(s) since the runtime started{latest}"
                    ),
                    _ => String::new(),
                };
                match r.mode.as_deref() {
                    Some("paused" | "step") => {
                        return verdict(
                            DeployHealthState::NotRunning,
                            "the program is paused or single-stepping, not running continuously"
                                .into(),
                            r.unhealthy_devices,
                        )
                    }
                    Some("running") => {
                        if previous.as_ref().is_some_and(|p| r.advances_from(p)) {
                            return verdict(
                                DeployHealthState::Running,
                                format!(
                                    "the program is running ({} scans){tap_note}",
                                    r.scan_count.unwrap()
                                ),
                                r.unhealthy_devices,
                            );
                        }
                        detail = format!(
                            "runtime answered, but continuous scan progress was not confirmed \
                            (scan_count={:?}, uptime_secs={:?})",
                            r.scan_count, r.uptime_secs
                        );
                        previous = Some(r);
                    }
                    _ => {
                        detail = "runtime did not report a recognized execution mode".into();
                        previous = None;
                    }
                }
            }
            Ok(Err(e)) => {
                detail = e;
                previous = None;
            }
            Err(_) => {
                detail = "status read did not complete before the deadline".into();
                break;
            }
        }
        tokio::time::sleep_until((tokio::time::Instant::now() + poll).min(deadline)).await;
    }
    verdict(
        DeployHealthState::Unknown,
        format!(
            "program state unknown after {} s: {detail}; stopped state is not confirmed",
            window.as_secs()
        ),
        Vec::new(),
    )
}

/// Metadata-suppression flags for the host `tar`, probed once per
/// process. bsdtar (the macOS default) records xattrs as
/// `LIBARCHIVE.xattr.*` pax headers, BSD file flags as `SCHILY.fflags`,
/// and Finder blobs as AppleDouble `._*` copies; the edge's GNU tar
/// prints "Ignoring unknown extended header keyword" for each such
/// record. `--no-xattrs` drops the xattr headers, `--no-fflags` the
/// fflags, `--no-mac-metadata` the AppleDouble copies. The
/// COPYFILE_DISABLE=1 env only gates that last, AppleDouble path —
/// measured on bsdtar 3.5.3 the pax headers still get written with it
/// set, so the env var alone doesn't cut it. GNU tar stores none of
/// this unless asked but rejects the two bsdtar-only flags as unknown
/// options, hence the version sniff rather than passing them blindly.
fn host_tar_metadata_flags() -> &'static [&'static str] {
    static FLAGS: OnceLock<&'static [&'static str]> = OnceLock::new();
    FLAGS.get_or_init(|| {
        let version = std::process::Command::new("tar")
            .arg("--version")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        tar_metadata_flags(&version)
    })
}

fn tar_metadata_flags(version: &str) -> &'static [&'static str] {
    if version.contains("bsdtar") {
        &["--no-xattrs", "--no-fflags", "--no-mac-metadata"]
    } else {
        &[]
    }
}

/// Drop the per-record "Ignoring unknown extended header keyword" lines
/// GNU tar prints when pax metadata slips through anyway (a host tar the
/// probe didn't recognise, or an archive packed by an older server).
/// Everything else in stderr is kept verbatim.
fn strip_pax_keyword_warnings(stderr: &str) -> String {
    stderr
        .lines()
        .filter(|l| !l.contains("Ignoring unknown extended header keyword"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Quote `s` as a single shell word using single quotes, which disable
/// all expansion. Unlike Rust's `{:?}` Debug formatting — whose
/// double-quote escaping still lets `$(…)` / backticks run — this is safe
/// for interpolating arbitrary values into a remote shell script. An
/// embedded single quote is closed, escaped, and reopened (`'\''`), the
/// same trick `proxy_edge_runtime_op` uses for the curl body.
fn sh_squote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Build the shell snippet that runs on the edge. Reads the project
/// tarball from stdin, extracts into a timestamped version dir, swaps
/// the `current` symlink atomically (rename(2) of a temp symlink), and
/// restarts the systemd unit. Old versions are kept; rollback is just a
/// symlink swap.
///
/// A failed restart rolls `current` back to where it pointed before. The
/// swap happens before the restart, so without that the box is left armed:
/// the running process is still the old one — which is what the error says —
/// but `current` names a version that has never started here, and the next
/// reboot or watchdog adopts it silently. A failed restart does not tell us
/// whether the old process survived; rollback restores files, not runtime state.
fn remote_deploy_script(
    install_dir: &str,
    project_basename: &str,
    binary_basename: Option<&str>,
    web_basename: Option<&str>,
) -> String {
    let bin_swap = match binary_basename {
        Some(name) => {
            let bin = sh_squote(name);
            format!(
                "if [ -f \"$DEST/\"{bin} ]; then\n  chmod +x \"$DEST/\"{bin}\n  mv \"$DEST/\"{bin} \"$DEST/runtime\"\nfi\n",
            )
        }
        None => String::new(),
    };
    // Normalise the bundled web assets (whatever their local dir was
    // called, usually `dist`) to `$DEST/web` — the fixed path the systemd
    // unit's `--static-dir` points at.
    let web_swap = match web_basename {
        Some(name) if name != "web" => {
            let web = sh_squote(name);
            format!("if [ -d \"$DEST/\"{web} ]; then\n  mv \"$DEST/\"{web} \"$DEST/web\"\nfi\n",)
        }
        _ => String::new(),
    };
    // Single-quote every value that lands in the remote script. `{:?}`
    // (Debug) is NOT shell quoting: it escapes `"`/`\` but leaves `$(…)`
    // and backticks live inside the resulting double-quoted assignment.
    let install_dir = sh_squote(install_dir);
    let project_basename = sh_squote(project_basename);
    format!(
        r#"set -euo pipefail
INSTALL_DIR={install_dir}
PROJECT={project_basename}
# Where `current` points before this deploy touches it — the rollback
# target if the restart fails. Empty on a first install.
PREV=""
if [ -L "$INSTALL_DIR/current" ]; then
  PREV=$(readlink "$INSTALL_DIR/current")
fi
echo "PREV=$PREV"
TS=$(date -u +%Y-%m-%dT%H-%M-%SZ)
mkdir -p "$INSTALL_DIR/versions"
DEST=$(mktemp -d "$INSTALL_DIR/versions/$TS.XXXXXX")
# Match the old version-directory traversal permissions; the service may
# run as a different account from the SSH deployer.
chmod 755 "$DEST"
TS=$(basename "$DEST")
# Extract everything the dev machine streamed in.
tar -xf - -C "$DEST"
# If a project subdir was bundled, lift its contents up so the layout is
# always $DEST/project + $DEST/runtime (whether the binary was sent or not).
if [ -d "$DEST/$PROJECT" ] && [ "$PROJECT" != "project" ]; then
  mv "$DEST/$PROJECT" "$DEST/project"
fi
{bin_swap}{web_swap}# Carry forward the runtime binary from `current` if this deploy didn't ship one.
if [ ! -f "$DEST/runtime" ] && [ -f "$INSTALL_DIR/current/runtime" ]; then
  cp "$INSTALL_DIR/current/runtime" "$DEST/runtime"
fi
# Same for the HMI panel assets — a dev-server deploy (no dist) keeps
# whatever panel the edge already had.
if [ ! -d "$DEST/web" ] && [ -d "$INSTALL_DIR/current/web" ]; then
  cp -R "$INSTALL_DIR/current/web" "$DEST/web"
fi
if [ ! -x "$DEST/runtime" ]; then
  echo "no runtime binary in $DEST and no prior current to copy from" >&2
  exit 2
fi
# Atomic symlink swap: rename(2) over an existing symlink is atomic on
# every Linux filesystem worth using.
TMPLINK="$INSTALL_DIR/.current.new"
ln -sfn "$DEST" "$TMPLINK"
mv -Tf "$TMPLINK" "$INSTALL_DIR/current"
echo "VERSION=$TS"
# Reload the unit if systemd is available; tolerate environments without
# systemd (handy in containers / tests). A FAILED restart fails the
# deploy — reporting success while the old code keeps running would be
# a lie the operator discovers at the worst possible time.
if command -v systemctl >/dev/null 2>&1; then
  if systemctl is-enabled --quiet {EDGE_UNIT} 2>/dev/null; then
    RESTART_ERR=""
    USER_RESTART_ERR=""
    if RESTART_ERR=$(sudo -n systemctl restart {EDGE_UNIT} 2>&1); then
      echo "RESTARTED={EDGE_UNIT}"
    elif USER_RESTART_ERR=$(systemctl --user restart {EDGE_UNIT} 2>&1); then
      echo "RESTARTED={EDGE_UNIT}"
    else
      # Restore the prior link (or its absence). Restart may have stopped the
      # old process before failing; do not claim any process is still running.
      # A `current` left pointing at a version that failed to start means
      # the next reboot, watchdog or `systemctl start` silently adopts it.
      # The files stay under versions/ for a human to inspect and re-swap.
      if [ -n "$PREV" ]; then
        ln -sfn "$PREV" "$TMPLINK"
        mv -Tf "$TMPLINK" "$INSTALL_DIR/current"
        echo "ROLLED_BACK=$PREV" >&2
      else
        rm -- "$INSTALL_DIR/current"
        echo "ROLLED_BACK=none (no previous current link)" >&2
      fi
      echo "ERROR: staged $DEST but FAILED to restart the {EDGE_UNIT} unit; restored previous current link state; runtime state is unconfirmed" >&2
      printf 'system restart: %s\nuser restart: %s\n' "$RESTART_ERR" "$USER_RESTART_ERR" >&2
      exit 3
    fi
  else
    echo "({EDGE_UNIT}.service not enabled — install it once via infra/install.sh; the new version is staged but nothing restarted)" >&2
  fi
fi
"#,
    )
}

// ============================================================
//  Attach — SSH port-forward + ephemeral local port
// ============================================================

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct AttachInfo {
    /// Local TCP port the IDE backend should proxy to.
    pub local_port: u16,
}

/// Start an `ssh -N -L 127.0.0.1:<local_port>:127.0.0.1:<edge.runtime_port> <host>`
/// and stash the child in `registry` keyed by `(project_name, edge.name)`.
/// Re-attaching the same `(project, edge)` pair while one is live
/// replaces the previous tunnel; an identically-named edge in a
/// different project is independent.
pub async fn attach_edge(
    project_name: &str,
    edge: &Edge,
    registry: &AttachmentRegistry,
) -> io::Result<AttachInfo> {
    // Pick an ephemeral local port by binding briefly then releasing it
    // back to the OS — ssh will grab it a moment later. The race window
    // stays (nothing reserves the port in between), but `forward_options`
    // makes losing it an error instead of a misroute.
    let probe = TcpListener::bind("127.0.0.1:0").await?;
    let local_port = probe.local_addr()?.port();
    drop(probe);

    // Kill any previous tunnel for this (project, edge) pair first.
    registry.detach(project_name, &edge.name);

    let mut child = ssh_cmd(edge)
        .arg("-N")
        .args(forward_options(local_port, edge.runtime_port))
        // Drop privileges: no PTY, no stdin/stdout (we're not running a
        // command), kill on drop so the tunnel goes away if the server
        // exits unexpectedly.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    // Briefly wait until the local port is actually accepting connections
    // — otherwise the UI's first /events probe races the tunnel and 502s.
    let mut ready = false;
    for _ in 0..30 {
        if tokio::net::TcpStream::connect(format!("127.0.0.1:{local_port}"))
            .await
            .is_ok()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // Bail early if ssh already died (bad host, auth failure, etc.).
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
    }
    if !ready {
        let mut err_buf = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut err_buf).await;
        }
        let _ = child.kill().await;
        return Err(io::Error::other(format!(
            "ssh port-forward to {host} never came up: {err}",
            host = edge.host,
            err = first_line(&err_buf)
        )));
    }

    registry.insert(
        project_name.to_string(),
        edge.name.clone(),
        local_port,
        child,
    );
    Ok(AttachInfo { local_port })
}

// ============================================================
//  Helpers
// ============================================================

/// Build the base ssh command with our usual options: explicit port,
/// optional user, connect timeout, BatchMode (so we never hang on a
/// password prompt — keys / agent only).
///
/// The destination is an argv entry, not a shell word, so metacharacters in
/// it are inert — but `ssh` parses any argument starting with `-` as an
/// OPTION, and `-oProxyCommand=…` would run a command on this machine. That
/// is why `ProjectStore` refuses such a value on read, write and create
/// (`validate_edge_target`); this function relies on an `Edge` having come
/// through that door.
pub fn ssh_cmd(edge: &Edge) -> Command {
    let mut c = Command::new("ssh");
    // A cancelled health check must not leave an SSH child waiting forever.
    c.kill_on_drop(true);
    c.arg("-p")
        .arg(edge.ssh_port.to_string())
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new");
    let target = if edge.ssh_user.is_empty() {
        edge.host.clone()
    } else {
        format!("{}@{}", edge.ssh_user, edge.host)
    };
    c.arg(target);
    c
}

/// The forwarding half of the attach command line.
///
/// `ExitOnForwardFailure=yes` is load-bearing, not hygiene. Without it a
/// failed LOCAL bind is non-fatal to ssh: it logs "bind: Address already in
/// use", stays connected and forwards nothing. The readiness probe in
/// `attach_edge` connects to the port to decide the tunnel is up — and on a
/// lost port race that connect SUCCEEDS, because the process that won the
/// port is listening there. The IDE would then proxy the whole runtime
/// session to an unrelated local listener and call it attached. With the
/// option, ssh exits, `try_wait` sees it, and the caller gets ssh's own
/// error.
fn forward_options(local_port: u16, runtime_port: u16) -> [String; 4] {
    [
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-L".into(),
        // Remote side is the edge's loopback: the runtime never binds a
        // routable address, so the tunnel is the only way in.
        format!("{local_port}:127.0.0.1:{runtime_port}"),
    ]
}

fn first_line(s: &str) -> &str {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim_end()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[path = "r11_fixture_export.rs"]
    mod r11_fixture_export;

    /// `ExitOnForwardFailure=yes` must stay on the attach command line: it
    /// is what turns a lost local-port race into an ssh exit instead of a
    /// live tunnel that forwards nothing while the readiness probe talks to
    /// whoever won the port.
    #[test]
    fn the_attach_forward_exits_on_bind_failure_and_targets_edge_loopback() {
        let opts = forward_options(45_000, 13_001);
        assert_eq!(opts[0], "-o");
        assert_eq!(opts[1], "ExitOnForwardFailure=yes");
        assert_eq!(opts[2], "-L");
        assert_eq!(
            opts[3], "45000:127.0.0.1:13001",
            "the remote end is the edge's loopback — the runtime binds nothing routable"
        );
    }

    /// Hermetic end-to-end run of `remote_deploy_script` — the exact
    /// bytes we ssh to edges — under a local bash with a tar stream on
    /// stdin, against a tmpdir INSTALL_DIR. macOS `mv` lacks GNU's
    /// `-Tf`, so the test PATH carries a tiny shim that emulates the
    /// one invocation shape the script uses (replace a symlink).
    fn run_deploy_script(
        install_dir: &std::path::Path,
        script: &str,
        tar_dir: &std::path::Path,
        tar_args: &[&str],
    ) -> std::process::Output {
        use std::io::Write as _;
        use std::process::{Command as StdCommand, Stdio as StdStdio};

        // PATH shim dir with a GNU-flavoured `mv`.
        let shim = install_dir.join(".shim");
        std::fs::create_dir_all(&shim).unwrap();
        let mv = shim.join("mv");
        std::fs::write(
            &mv,
            "#!/bin/bash\nif [ \"$1\" = \"-Tf\" ]; then rm -rf \"$3\"; exec /bin/mv \"$2\" \"$3\"; fi\nexec /bin/mv \"$@\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&mv, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let tar_bytes = StdCommand::new("tar")
            .arg("-cf")
            .arg("-")
            .args(host_tar_metadata_flags())
            .arg("-C")
            .arg(tar_dir)
            .args(tar_args)
            .output()
            .unwrap();
        assert!(tar_bytes.status.success(), "local tar failed");

        let path = format!(
            "{}:{}",
            shim.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut child = StdCommand::new("bash")
            .arg("-c")
            .arg(script)
            .env("PATH", path)
            .stdin(StdStdio::piped())
            .stdout(StdStdio::piped())
            .stderr(StdStdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&tar_bytes.stdout)
            .unwrap();
        drop(child.stdin.take());
        child.wait_with_output().unwrap()
    }

    #[test]
    fn deploy_script_extracts_swaps_symlink_and_prints_version() {
        let tmp = tempfile::tempdir().unwrap();
        let install = tmp.path().join("ia2");
        std::fs::create_dir_all(&install).unwrap();

        // A minimal "project" dir + a fake runtime binary to stream in.
        let stage = tmp.path().join("stage");
        std::fs::create_dir_all(stage.join("myproj/pous")).unwrap();
        std::fs::write(stage.join("myproj/project.toml"), "name = \"myproj\"\n").unwrap();
        std::fs::write(stage.join("ia2-runtime"), "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                stage.join("ia2-runtime"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }

        let script = remote_deploy_script(
            install.to_str().unwrap(),
            "myproj",
            Some("ia2-runtime"),
            None,
        );
        let out = run_deploy_script(&install, &script, &stage, &["myproj", "ia2-runtime"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "script failed: status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            out.status
        );

        // VERSION= printed, version dir laid out, symlink swapped.
        let version = stdout
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix("VERSION="))
            .expect("script must print VERSION=");
        let vdir = install.join("versions").join(version);
        assert!(
            vdir.join("project/project.toml").is_file(),
            "project normalised"
        );
        assert!(vdir.join("runtime").is_file(), "binary renamed to runtime");
        let current = std::fs::read_link(install.join("current")).unwrap();
        assert_eq!(current, vdir, "current symlink points at the new version");
    }

    /// Write PATH shims that make the script's restart branch reachable and
    /// controllable: `systemctl is-enabled` succeeds only when the `enabled`
    /// marker exists, and any `restart` fails. `sudo -n <cmd>` just runs
    /// `<cmd>`, so the sudo path reaches the same shim.
    fn write_systemctl_shims(install_dir: &std::path::Path) {
        let shim = install_dir.join(".shim");
        std::fs::create_dir_all(&shim).unwrap();
        std::fs::write(
            shim.join("systemctl"),
            "#!/bin/bash\n\
             for a in \"$@\"; do\n\
             case \"$a\" in\n\
             is-enabled) [ -f \"$(dirname \"$0\")/enabled\" ] && exit 0; exit 1 ;;\n\
             restart) echo \"Job for ia2.service failed\" >&2; exit 1 ;;\n\
             esac\n\
             done\n\
             exit 0\n",
        )
        .unwrap();
        std::fs::write(
            shim.join("sudo"),
            "#!/bin/bash\n[ \"$1\" = \"-n\" ] && shift\nexec \"$@\"\n",
        )
        .unwrap();
        // Force two deployments into the same timestamp: a rollback must
        // not point at a directory the failed deployment already overwrote.
        std::fs::write(shim.join("date"), "#!/bin/sh\necho 2026-09-17T00-00-00Z\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in ["systemctl", "sudo", "date"] {
                std::fs::set_permissions(shim.join(f), std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
        }
    }

    fn stage_project(tmp: &std::path::Path, marker: &str) -> std::path::PathBuf {
        let stage = tmp.join(format!("stage-{marker}"));
        std::fs::create_dir_all(stage.join("myproj")).unwrap();
        std::fs::write(
            stage.join("myproj/project.toml"),
            format!("name = \"myproj\"\nmarker = \"{marker}\"\n"),
        )
        .unwrap();
        std::fs::write(stage.join("ia2-runtime"), "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                stage.join("ia2-runtime"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        stage
    }

    /// Regression: the symlink swap happens BEFORE the restart, so a deploy
    /// whose restart failed used to leave `current` pointing at a version the
    /// box had never successfully started — while the error told the operator
    /// "the edge still runs the previous version". Neither that process
    /// claim nor the next restart target was guaranteed.
    #[test]
    fn a_failed_restart_rolls_the_current_symlink_back() {
        let tmp = tempfile::tempdir().unwrap();
        let install = tmp.path().join("ia2");
        std::fs::create_dir_all(&install).unwrap();
        write_systemctl_shims(&install);

        // First deploy: unit not enabled -> staged, nothing restarted, ok.
        let v1 = stage_project(tmp.path(), "v1");
        let script = remote_deploy_script(
            install.to_str().unwrap(),
            "myproj",
            Some("ia2-runtime"),
            None,
        );
        let out = run_deploy_script(&install, &script, &v1, &["myproj", "ia2-runtime"]);
        assert!(out.status.success(), "first deploy must succeed");
        // `readlink -f` in the script resolves symlinks, and on macOS a
        // tmpdir's /var is really /private/var — compare canonical paths.
        let good = std::fs::canonicalize(install.join("current")).unwrap();

        // Second deploy: unit now "enabled", every restart fails.
        std::fs::write(install.join(".shim/enabled"), "").unwrap();
        let v2 = stage_project(tmp.path(), "v2");
        let script = remote_deploy_script(
            install.to_str().unwrap(),
            "myproj",
            Some("ia2-runtime"),
            None,
        );
        let out = run_deploy_script(&install, &script, &v2, &["myproj", "ia2-runtime"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(3),
            "a failed restart fails the deploy: {stderr}"
        );
        assert!(stderr.contains("ROLLED_BACK="), "{stderr}");
        assert!(
            stderr.contains("Job for ia2.service failed"),
            "the restart's own reason must survive, not be swallowed: {stderr}"
        );
        assert_eq!(
            std::fs::canonicalize(install.join("current")).unwrap(),
            good,
            "current must point back at the version this box actually started"
        );
        // The new files are still on disk for a human to inspect / re-swap.
        let staged = String::from_utf8_lossy(&out.stdout);
        let version = staged
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix("VERSION="))
            .expect("VERSION= is still printed before the restart");
        assert_ne!(
            install
                .join("versions")
                .join(version)
                .canonicalize()
                .unwrap(),
            good
        );
        let prior = std::fs::read_to_string(good.join("project/project.toml")).unwrap();
        assert!(
            prior.contains("marker = \"v1\""),
            "rollback payload was overwritten: {prior}"
        );
        assert!(
            install
                .join("versions")
                .join(version)
                .join("runtime")
                .is_file(),
            "the rolled-back version stays staged"
        );
    }

    #[test]
    fn first_install_restart_failure_removes_unconfirmed_current() {
        let tmp = tempfile::tempdir().unwrap();
        let install = tmp.path().join("ia2");
        std::fs::create_dir_all(&install).unwrap();
        write_systemctl_shims(&install);
        std::fs::write(install.join(".shim/enabled"), "").unwrap();
        let stage = stage_project(tmp.path(), "first");
        let script = remote_deploy_script(
            install.to_str().unwrap(),
            "myproj",
            Some("ia2-runtime"),
            None,
        );
        let out = run_deploy_script(&install, &script, &stage, &["myproj", "ia2-runtime"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(3), "{stderr}");
        assert!(std::fs::symlink_metadata(install.join("current")).is_err());
        assert!(
            !stderr.contains("still runs the previous version"),
            "{stderr}"
        );
        assert!(stderr.contains("runtime state is unconfirmed"), "{stderr}");
        assert!(std::fs::read_dir(install.join("versions"))
            .unwrap()
            .next()
            .is_some());
    }

    #[test]
    fn deploy_script_fails_loudly_without_runtime_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let install = tmp.path().join("ia2");
        std::fs::create_dir_all(&install).unwrap();
        let stage = tmp.path().join("stage");
        std::fs::create_dir_all(stage.join("myproj")).unwrap();
        std::fs::write(stage.join("myproj/project.toml"), "name = \"myproj\"\n").unwrap();

        // No binary in the stream and no prior `current` to carry from.
        let script = remote_deploy_script(install.to_str().unwrap(), "myproj", None, None);
        let out = run_deploy_script(&install, &script, &stage, &["myproj"]);
        assert_eq!(out.status.code(), Some(2), "documented exit for no-runtime");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("no runtime binary"),
            "expected the no-runtime message, got:\n{stderr}"
        );
        // And crucially: `current` was never flipped.
        assert!(!install.join("current").exists());
    }

    #[test]
    fn sh_squote_neutralizes_command_substitution() {
        assert_eq!(sh_squote("/opt/ia2"), "'/opt/ia2'");
        // `$(…)` and backticks are inert inside single quotes.
        assert_eq!(sh_squote("/opt/$(reboot)"), "'/opt/$(reboot)'");
        // an embedded single quote is closed, escaped, reopened.
        assert_eq!(sh_squote("a'b"), r"'a'\''b'");
    }

    #[test]
    fn deploy_script_keeps_metachars_single_quoted() {
        let s = remote_deploy_script(
            "/opt/$(reboot)",
            "proj$(touch /tmp/x)",
            Some("rt`whoami`"),
            Some("dist$(id)"),
        );
        // Dangerous values appear only inside single-quoted words, so the
        // remote shell treats them as literals rather than evaluating them.
        assert!(s.contains("INSTALL_DIR='/opt/$(reboot)'"), "{s}");
        assert!(s.contains("PROJECT='proj$(touch /tmp/x)'"), "{s}");
        assert!(s.contains(r"'rt`whoami`'"), "{s}");
        assert!(s.contains("'dist$(id)'"), "{s}");
        // The pre-fix bug: the value inside a double-quoted assignment.
        assert!(!s.contains(r#"INSTALL_DIR="/opt/$(reboot)""#), "{s}");
    }

    #[test]
    fn bsdtar_gets_metadata_suppression_flags() {
        // The exact set measured to yield a pax-record-free archive on
        // macOS bsdtar 3.5.3 (see host_tar_metadata_flags).
        assert_eq!(
            tar_metadata_flags("bsdtar 3.5.3 - libarchive 3.7.4 zlib/1.2.12"),
            ["--no-xattrs", "--no-fflags", "--no-mac-metadata"]
        );
    }

    #[test]
    fn gnu_and_unknown_tars_get_no_flags() {
        // GNU tar stores no xattrs/fflags unless asked — and rejects the
        // bsdtar-only flags as unknown options.
        assert!(tar_metadata_flags("tar (GNU tar) 1.35").is_empty());
        // A failed probe must not break packing.
        assert!(tar_metadata_flags("").is_empty());
    }

    #[test]
    fn deploy_log_drops_pax_keyword_warnings_only() {
        let noisy = "tar: Ignoring unknown extended header keyword 'LIBARCHIVE.xattr.com.apple.provenance'\n\
                     tar: Ignoring unknown extended header keyword 'SCHILY.fflags'\n\
                     real error: disk full";
        assert_eq!(strip_pax_keyword_warnings(noisy), "real error: disk full");
        // All-noise stderr collapses to empty, which keeps the
        // `--stderr--` section out of the report entirely.
        assert_eq!(
            strip_pax_keyword_warnings(
                "tar: Ignoring unknown extended header keyword 'LIBARCHIVE.xattr.com.apple.FinderInfo'\n"
            ),
            ""
        );
    }

    #[test]
    fn deploy_script_normalises_web_assets_and_carries_forward() {
        let s = remote_deploy_script("/opt/ia2", "proj", None, Some("dist"));
        // Bundled `dist/` is renamed to the fixed `web/` the systemd
        // unit's --static-dir points at.
        assert!(s.contains(r#"mv "$DEST/"'dist' "$DEST/web""#), "{s}");
        // A deploy without web assets keeps the previous version's panel.
        assert!(
            s.contains(r#"cp -R "$INSTALL_DIR/current/web" "$DEST/web""#),
            "{s}"
        );
        // Already-named `web` needs no rename step.
        let s2 = remote_deploy_script("/opt/ia2", "proj", None, Some("web"));
        assert!(!s2.contains(r#""$DEST/"'web' "$DEST/web""#), "{s2}");
    }

    /// A runtime that answers /health while one of its buses is down must
    /// NOT probe as plain "reachable and fine" — that reads as a healthy
    /// edge in the IDE badge and in `cs probe`, while inputs are frozen
    /// and outputs are being dropped.
    #[test]
    fn probe_surfaces_a_down_fieldbus_on_a_reachable_edge() {
        let probe = probe_from_health_body(
            r#"{"status":"ok","uptime_secs":704,"scan_count":351667,
                "fieldbus_healthy":false,
                "devices":[{"name":"coupler","healthy":false},
                           {"name":"servo","healthy":true}]}"#,
        );
        assert!(probe.reachable, "the runtime did answer");
        assert_eq!(probe.fieldbus_healthy, Some(false));
        assert_eq!(probe.unhealthy_devices, vec!["coupler".to_string()]);
        assert!(probe.error.is_none());
    }

    /// A runtime whose program died still answers `/health` with
    /// `status: "ok"`. The probe must carry the fault through, or the IDE
    /// badge and `cs probe` call a dead edge running.
    #[test]
    fn probe_carries_the_fault_of_a_reachable_edge() {
        let probe = probe_from_health_body(
            r#"{"status":"ok","uptime_secs":90,"scan_count":0,
                "fieldbus_healthy":true,"devices":[],"watchdog_tripped":false,
                "fault":"VM trap in main_inst: DivideByZero"}"#,
        );
        assert!(probe.reachable, "the runtime did answer");
        assert_eq!(
            probe.fault.as_deref(),
            Some("VM trap in main_inst: DivideByZero")
        );
        assert!(probe.error.is_none());
    }

    /// A runtime whose feeding task has ended still answers `/health` with
    /// `status: "ok"` and a climbing-or-frozen scan count. The probe must
    /// carry the tap state through, or the badge and `cs probe` call an edge
    /// that records no history and evaluates no alarms healthy.
    #[test]
    fn probe_carries_the_snapshot_tap_of_a_reachable_edge() {
        let probe = probe_from_health_body(
            r#"{"status":"ok","uptime_secs":90,"scan_count":4,
                "fieldbus_healthy":true,"devices":[],"watchdog_tripped":false,
                "fault":null,
                "snapshot_tap":{"running":false,"faults":3,
                                "last_fault":"the alarms stage panicked on scan 4: boom"}}"#,
        );
        assert!(probe.reachable, "the runtime did answer");
        let tap = probe.snapshot_tap.expect("carried through");
        assert!(!tap.running);
        assert_eq!(tap.faults, 3);
        assert_eq!(
            tap.last_fault.as_deref(),
            Some("the alarms stage panicked on scan 4: boom")
        );
        assert!(probe.error.is_none());
    }

    #[test]
    fn probe_carries_the_supervision_policy_and_reads_an_older_edge_as_none() {
        let probe = probe_from_health_body(
            r#"{"status":"ok","uptime_secs":9,"scan_count":4,
                "supervision":{"on_loss":"safe_state","grace_secs":45}}"#,
        );
        let policy = probe.supervision.expect("carried through");
        assert_eq!(
            (policy.on_loss.as_str(), policy.grace_secs),
            ("safe_state", 45)
        );
        assert_eq!(
            (policy.lost_for_ms, policy.latch_in_ms),
            (None, None),
            "a runtime that predates the countdown fields reports none"
        );
        let older = probe_from_health_body(r#"{"status":"ok","uptime_secs":9,"scan_count":4}"#);
        assert_eq!(older.supervision, None, "unknown, not report");
    }

    /// While alarm evaluation is lost the edge says for how long and how long is
    /// left before `safe_state` acts; the probe carries both through, so `cs
    /// probe` and the IDE can show them.
    #[test]
    fn probe_carries_how_long_alarms_have_been_lost_and_when_the_policy_acts() {
        let probe = probe_from_health_body(
            r#"{"status":"ok","uptime_secs":9,"scan_count":4,
                "supervision":{"on_loss":"safe_state","grace_secs":45,
                               "lost_for_ms":12400,"latch_in_ms":32600}}"#,
        );
        let policy = probe.supervision.expect("carried through");
        assert_eq!(policy.lost_for_ms, Some(12_400));
        assert_eq!(policy.latch_in_ms, Some(32_600));
        let calm = probe_from_health_body(
            r#"{"status":"ok","uptime_secs":9,"scan_count":4,
                "supervision":{"on_loss":"report","grace_secs":30,
                               "lost_for_ms":null,"latch_in_ms":null}}"#,
        );
        let policy = calm.supervision.expect("carried through");
        assert_eq!((policy.lost_for_ms, policy.latch_in_ms), (None, None));
    }

    /// A newer runtime may report a mode this server has never heard of. The
    /// probe must still read the edge, not call the whole thing unreachable.
    #[test]
    fn probe_survives_a_supervision_mode_it_does_not_know() {
        let probe = probe_from_health_body(
            r#"{"status":"ok","uptime_secs":9,"scan_count":4,
                "supervision":{"on_loss":"a_future_mode","grace_secs":10,"extra":true}}"#,
        );
        assert!(probe.reachable, "{probe:?}");
        assert_eq!(probe.supervision.unwrap().on_loss, "a_future_mode");
    }

    #[test]
    fn probe_reports_a_fully_healthy_edge_cleanly() {
        let probe = probe_from_health_body(
            r#"{"status":"ok","uptime_secs":10,"scan_count":20,
                "fieldbus_healthy":true,
                "devices":[{"name":"servo","healthy":true}]}"#,
        );
        assert_eq!(probe.fieldbus_healthy, Some(true));
        assert!(probe.unhealthy_devices.is_empty());
    }

    /// An edge running a build that predates per-device health still probes
    /// fine; the health fields degrade to "unknown" rather than failing the
    /// parse and reporting the whole edge unreachable.
    #[test]
    fn probe_tolerates_a_runtime_without_health_fields() {
        let probe = probe_from_health_body(r#"{"status":"ok","uptime_secs":5,"scan_count":9}"#);
        assert!(probe.reachable);
        assert_eq!(probe.scan_count, Some(9));
        assert_eq!(probe.fieldbus_healthy, None, "unknown, not false");
        assert!(probe.unhealthy_devices.is_empty());
        assert_eq!(probe.fault, None);
        assert_eq!(probe.snapshot_tap, None, "unknown, not stopped");
    }

    /// The proxy must carry the edge's own status through — a
    /// governance 403 stays a 403 with the edge's error text verbatim
    /// (ADR-0002: all four write paths report policy denials the same
    /// way), and a 409 stays a 409 (api.md: "All return 409 when
    /// nothing is running" includes the proxy path).
    #[test]
    fn edge_runtime_response_keeps_status_and_body() {
        assert_eq!(
            parse_edge_runtime_response("{\"ok\":true,\"value\":10}\n200").unwrap(),
            serde_json::json!({ "ok": true, "value": 10 })
        );
        assert_eq!(
            parse_edge_runtime_response("write to 'x' rejected by project governance\n403"),
            Err(EdgeRuntimeError::Status {
                code: 403,
                body: "write to 'x' rejected by project governance".into(),
            })
        );
        assert_eq!(
            parse_edge_runtime_response("scan loop has stopped\n409"),
            Err(EdgeRuntimeError::Status {
                code: 409,
                body: "scan loop has stopped".into(),
            })
        );
        // Multi-line error bodies survive intact — only the trailing
        // status marker is consumed.
        assert_eq!(
            parse_edge_runtime_response("line one\nline two\n500"),
            Err(EdgeRuntimeError::Status {
                code: 500,
                body: "line one\nline two".into(),
            })
        );
    }

    #[test]
    fn edge_runtime_response_without_status_marker_is_a_transport_fault() {
        let err = parse_edge_runtime_response("<html>502 Bad Gateway</html>").unwrap_err();
        assert!(matches!(err, EdgeRuntimeError::Transport(m) if m.contains("status marker")));
        // 2xx with a non-JSON body is also a fault of the hop, not a
        // fabricated edge answer.
        let err = parse_edge_runtime_response("not json\n200").unwrap_err();
        assert!(matches!(err, EdgeRuntimeError::Transport(m) if m.contains("unexpected")));
    }

    /// A declared origin is sanitized for the remote shell, never
    /// silently dropped — otherwise the edge audit records "anonymous"
    /// for a write the server-side overlay attributed to a label.
    #[test]
    fn origin_header_is_sanitized_not_dropped() {
        assert_eq!(origin_header_arg(Some("gui")), "-H 'x-ia2-origin: gui' ");
        assert_eq!(
            origin_header_arg(Some("a_b.c-d")),
            "-H 'x-ia2-origin: a_b.c-d' "
        );
        // Space and shell metacharacters are removed, label kept.
        assert_eq!(
            origin_header_arg(Some("my bridge")),
            "-H 'x-ia2-origin: mybridge' "
        );
        assert_eq!(
            origin_header_arg(Some("a'b$(reboot)`x`")),
            "-H 'x-ia2-origin: abrebootx' "
        );
        // Over-long labels are capped at 64 chars.
        let long = "x".repeat(70);
        assert_eq!(
            origin_header_arg(Some(&long)),
            format!("-H 'x-ia2-origin: {}' ", "x".repeat(64))
        );
        // Absent or empty-after-sanitize → no header at all.
        assert_eq!(origin_header_arg(None), "");
        assert_eq!(origin_header_arg(Some("")), "");
        assert_eq!(origin_header_arg(Some("!!!")), "");
    }

    #[test]
    fn probe_rejects_a_non_ok_runtime_and_garbage() {
        let bad = probe_from_health_body(r#"{"status":"degraded","uptime_secs":1,"scan_count":1}"#);
        assert!(!bad.reachable);
        assert!(bad.error.unwrap().contains("degraded"));

        let junk = probe_from_health_body("<html>502 Bad Gateway</html>");
        assert!(!junk.reachable);
        assert!(junk.error.unwrap().contains("unexpected body"));
    }

    /// Run the post-restart check against scripted `/status` reads (the
    /// last one repeats), in a short window so a check that never settles
    /// ends quickly.
    async fn watch(reads: Vec<Result<serde_json::Value, String>>) -> DeployHealth {
        watch_declaring(reads, &project::SupervisionConfig::default()).await
    }

    /// The same, for a project that declares `declared`.
    async fn watch_declaring(
        reads: Vec<Result<serde_json::Value, String>>,
        declared: &project::SupervisionConfig,
    ) -> DeployHealth {
        let mut queue = std::collections::VecDeque::from(reads);
        watch_restarted_program(
            move || {
                let read = if queue.len() > 1 {
                    queue.pop_front()
                } else {
                    queue.front().cloned()
                }
                .expect("at least one scripted read");
                async move { read }
            },
            Duration::from_millis(200),
            Duration::from_millis(5),
            declared,
        )
        .await
    }

    fn status(scan_count: u64, fault: Option<&str>) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({
            "scan_count": scan_count,
            "uptime_secs": 10,
            "runtime_id": "test-process",
            "mode": {"kind": "running"},
            "fault": fault,
            "watchdog_tripped": false,
            "device_health": [],
        }))
    }

    #[tokio::test]
    async fn a_restarted_program_that_scans_is_running() {
        let health = watch(vec![status(0, None), status(3, None), status(9, None)]).await;
        assert_eq!(health.state, DeployHealthState::Running, "{health:?}");
        assert!(health.detail.contains("9 scans"), "{health:?}");
        assert!(health.state.deploy_ok());
    }

    /// systemd reported the restart fine; the program trapped on its first
    /// scan. Deploy used to report success.
    #[tokio::test]
    async fn a_restarted_program_that_traps_is_faulted() {
        let trap = "VM trap in main_inst: DivideByZero";
        let health = watch(vec![status(0, None), status(0, Some(trap))]).await;
        assert_eq!(health.state, DeployHealthState::Faulted, "{health:?}");
        assert!(health.detail.contains(trap), "{health:?}");
        assert!(!health.state.deploy_ok());
    }

    /// One read showing scans is not enough: the confirming read a poll
    /// later catches a trap right after the first scans.
    #[tokio::test]
    async fn a_trap_after_the_first_scans_is_caught_by_the_confirming_read() {
        let trap = "VM trap in main_inst: DivideByZero";
        let health = watch(vec![status(2, None), status(2, Some(trap))]).await;
        assert_eq!(health.state, DeployHealthState::Faulted, "{health:?}");
    }

    /// A VM that never starts records no fault on an older edge build; it
    /// just never scans. That must not pass either.
    #[tokio::test]
    async fn a_restarted_program_that_never_scans_is_unconfirmed() {
        let health = watch(vec![status(0, None)]).await;
        assert_eq!(health.state, DeployHealthState::Unknown, "{health:?}");
        assert!(
            health.detail.contains("progress was not confirmed"),
            "{health:?}"
        );
        assert!(!health.state.deploy_ok());
    }

    #[tokio::test]
    async fn a_runtime_that_never_answers_is_unknown() {
        let health = watch(vec![Err("curl: connection refused".into())]).await;
        assert_eq!(health.state, DeployHealthState::Unknown, "{health:?}");
        assert!(health.detail.contains("connection refused"), "{health:?}");
    }

    #[tokio::test]
    async fn a_runtime_that_answers_late_is_waited_for() {
        let health = watch(vec![
            Err("curl: connection refused".into()),
            Err("curl: connection refused".into()),
            status(1, None),
            status(4, None),
        ])
        .await;
        assert_eq!(health.state, DeployHealthState::Running, "{health:?}");
    }

    /// The latch is one flag, set by the scan watchdog or by the project's
    /// supervision policy. A verdict that blames the watchdog for the latter
    /// sends the operator to the wrong fault.
    #[tokio::test]
    async fn a_latch_the_runtime_explains_is_faulted_with_its_own_reason() {
        let latched = Ok(serde_json::json!({
            "scan_count": 40, "fault": null, "watchdog_tripped": true, "device_health": [],
            "latch_reason": "alarm supervision lost for 30 s: the snapshot task has stopped",
        }));
        let health = watch(vec![latched]).await;
        assert_eq!(health.state, DeployHealthState::Faulted, "{health:?}");
        assert!(
            health.detail.contains("alarm supervision lost for 30 s"),
            "{health:?}"
        );
        assert!(!health.detail.contains("scan watchdog"), "{health:?}");
    }

    #[test]
    fn probe_carries_the_latch_reason_and_reads_an_older_edge_as_none() {
        let probe = probe_from_health_body(
            r#"{"status":"ok","uptime_secs":9,"scan_count":4,"watchdog_tripped":true,
                "latch_reason":"alarm supervision lost for 30 s: the snapshot task has stopped"}"#,
        );
        assert_eq!(probe.watchdog_tripped, Some(true));
        assert_eq!(
            probe.latch_reason.as_deref(),
            Some("alarm supervision lost for 30 s: the snapshot task has stopped")
        );
        let older = probe_from_health_body(
            r#"{"status":"ok","uptime_secs":9,"scan_count":4,"watchdog_tripped":true}"#,
        );
        assert_eq!(
            older.latch_reason, None,
            "unknown, and the old wording stands"
        );
    }

    #[tokio::test]
    async fn a_latched_watchdog_is_faulted() {
        let latched = Ok(serde_json::json!({
            "scan_count": 40, "fault": null, "watchdog_tripped": true, "device_health": [],
        }));
        let health = watch(vec![latched]).await;
        assert_eq!(health.state, DeployHealthState::Faulted, "{health:?}");
        assert!(health.detail.contains("watchdog"), "{health:?}");
    }

    /// A device still down runs the program all the same; it is reported,
    /// not failed.
    #[tokio::test]
    async fn a_down_device_is_reported_on_a_running_program() {
        let degraded = |n: u64| {
            Ok(serde_json::json!({
                "scan_count": n, "uptime_secs": 10, "runtime_id": "test-process",
                "mode": {"kind": "running"}, "fault": null, "watchdog_tripped": false,
                "device_health": [{"name": "coupler", "healthy": false},
                                  {"name": "servo", "healthy": true}],
            }))
        };
        let health = watch(vec![degraded(1), degraded(5)]).await;
        assert_eq!(health.state, DeployHealthState::Running, "{health:?}");
        assert_eq!(health.unhealthy_devices, vec!["coupler".to_string()]);
    }
    /// A stage that blocks neither panics nor lags: `running` stays true and
    /// `faults` stays 0. The runtime reports it as `stalled_for_ms`, and the
    /// check must treat it like a stopped task, since history, alarms and the
    /// scan count have stopped moving all the same.
    #[tokio::test]
    async fn a_snapshot_task_stuck_on_one_snapshot_fails_the_deploy() {
        let mut stuck = with_tap(12, true, 0, None).unwrap();
        stuck["snapshot_tap"]["stalled_for_ms"] = serde_json::json!(4200);
        let health = watch(vec![Ok(stuck)]).await;
        assert_eq!(health.state, DeployHealthState::Unmonitored, "{health:?}");
        assert!(
            health.detail.contains("stuck on one snapshot (4200 ms)"),
            "{health:?}"
        );
        assert!(!health.state.deploy_ok());
    }

    /// A running-mode `/status` read where the runtime reports the policy it
    /// is enforcing (`None` = a runtime that predates the field).
    fn with_policy(
        scan_count: u64,
        policy: Option<(&str, u64)>,
    ) -> Result<serde_json::Value, String> {
        let mut v = status(scan_count, None).unwrap();
        if let Some((on_loss, grace_secs)) = policy {
            v["supervision"] = serde_json::json!({"on_loss": on_loss, "grace_secs": grace_secs});
        }
        Ok(v)
    }

    fn declares(
        on_loss: project::SupervisionAction,
        grace_secs: u32,
    ) -> project::SupervisionConfig {
        project::SupervisionConfig {
            on_loss,
            grace_secs,
        }
    }

    /// The deploy carried an older runtime forward; the project declares
    /// `safe_state`; the older runtime ignores the table. Every other signal is
    /// green (reachable, scanning, no fault), and the protection the project
    /// asked for does not exist. The check must say so and fail the deploy.
    #[tokio::test]
    async fn a_declared_safe_state_that_an_older_runtime_ignores_fails_the_deploy() {
        use project::SupervisionAction::SafeState;
        let declared = declares(SafeState, 45);
        let health =
            watch_declaring(vec![with_policy(1, None), with_policy(5, None)], &declared).await;
        assert_eq!(health.state, DeployHealthState::Unprotected, "{health:?}");
        assert!(!health.state.deploy_ok());
        assert!(health.detail.contains("predates the setting"), "{health:?}");
        assert!(health.detail.contains("NOT in force"), "{health:?}");
        assert!(health.detail.contains("45 s"), "{health:?}");
    }

    #[tokio::test]
    async fn a_runtime_enforcing_a_different_policy_than_declared_fails_the_deploy() {
        use project::SupervisionAction::{Report, SafeState};
        for (declared, reported, expect) in [
            // reports report, declared safe_state
            (
                declares(SafeState, 45),
                ("report", 30),
                "reports on_loss = report",
            ),
            // right mode, wrong grace
            (declares(SafeState, 45), ("safe_state", 30), "grace 30 s"),
            // declared report but the runtime is enforcing safe_state
            (
                declares(Report, 30),
                ("safe_state", 30),
                "reports on_loss = safe_state",
            ),
        ] {
            let health = watch_declaring(
                vec![
                    with_policy(1, Some(reported)),
                    with_policy(5, Some(reported)),
                ],
                &declared,
            )
            .await;
            assert_eq!(
                health.state,
                DeployHealthState::Unprotected,
                "{declared:?} vs {reported:?}: {health:?}"
            );
            assert!(health.detail.contains(expect), "{health:?}");
        }
    }

    /// The check must not cry wolf: matching policies, a project that asks for
    /// nothing (`report`) on a runtime that reports nothing, and a grace that
    /// does not matter under `report`, all pass as before.
    #[tokio::test]
    async fn a_matching_or_unneeded_policy_does_not_change_the_verdict() {
        use project::SupervisionAction::{Report, SafeState};
        for (declared, reported) in [
            (declares(SafeState, 45), Some(("safe_state", 45))),
            (declares(Report, 30), Some(("report", 30))),
            (declares(Report, 30), Some(("report", 99))), // grace is irrelevant to report
            (declares(Report, 30), None),                 // older runtime, nothing to enforce
        ] {
            let health = watch_declaring(
                vec![with_policy(1, reported), with_policy(5, reported)],
                &declared,
            )
            .await;
            assert_eq!(
                health.state,
                DeployHealthState::Running,
                "{declared:?} vs {reported:?}: {health:?}"
            );
        }
    }

    /// A running-mode `/status` read with the runtime's `snapshot_tap`.
    fn with_tap(
        scan_count: u64,
        running: bool,
        faults: u64,
        last_fault: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let mut v = status(scan_count, None).unwrap();
        v["snapshot_tap"] =
            serde_json::json!({"running": running, "faults": faults, "last_fault": last_fault});
        Ok(v)
    }

    /// The task that feeds history and process alarms has ended. The program
    /// may well be scanning, but nothing is recorded, no alarm can be raised,
    /// and `scan_count` froze with the tap, so the old check could only say
    /// "unknown" without saying why. A runtime that cannot raise an alarm
    /// fails the deploy, and says so.
    #[tokio::test]
    async fn a_stopped_snapshot_task_fails_the_deploy_as_unmonitored() {
        let health = watch(vec![with_tap(
            12,
            false,
            2,
            Some("the alarms stage panicked on scan 12: boom"),
        )])
        .await;
        assert_eq!(health.state, DeployHealthState::Unmonitored, "{health:?}");
        assert!(!health.state.deploy_ok(), "{health:?}");
        assert!(
            health.detail.contains("no alarm is evaluated"),
            "{health:?}"
        );
        assert!(
            health.detail.contains("alarms stage panicked"),
            "{health:?}"
        );
    }

    /// Frozen counters alone stay "unknown"; the tap field is what names
    /// the cause, and it wins over the generic verdict.
    #[tokio::test]
    async fn a_stopped_snapshot_task_is_named_even_when_the_scan_count_is_frozen() {
        let health = watch(vec![
            with_tap(7, false, 0, None),
            with_tap(7, false, 0, None),
        ])
        .await;
        assert_eq!(health.state, DeployHealthState::Unmonitored, "{health:?}");
        assert!(!health.detail.contains("not confirmed by"), "{health:?}");
    }

    /// A tap that is still running but fails on every snapshot is the same
    /// loss of alarms. It shows as a fault count that rises between reads.
    #[tokio::test]
    async fn a_snapshot_task_whose_fault_count_is_rising_fails_the_deploy() {
        let health = watch(vec![
            with_tap(
                2,
                true,
                3,
                Some("the alarms stage panicked on scan 2: boom"),
            ),
            with_tap(
                6,
                true,
                9,
                Some("the alarms stage panicked on scan 6: boom"),
            ),
        ])
        .await;
        assert_eq!(health.state, DeployHealthState::Unmonitored, "{health:?}");
        assert!(health.detail.contains("3 -> 9 faults"), "{health:?}");
        assert!(health.detail.contains("scan 6"), "{health:?}");
    }

    /// A fault that happened once and has not recurred does not fail the
    /// deploy, but the verdict says it happened.
    #[tokio::test]
    async fn a_steady_fault_count_passes_with_the_fault_in_the_verdict() {
        let health = watch(vec![
            with_tap(
                2,
                true,
                1,
                Some("fell behind the scan loop: 4 snapshots were skipped"),
            ),
            with_tap(
                6,
                true,
                1,
                Some("fell behind the scan loop: 4 snapshots were skipped"),
            ),
            with_tap(
                9,
                true,
                1,
                Some("fell behind the scan loop: 4 snapshots were skipped"),
            ),
        ])
        .await;
        assert_eq!(health.state, DeployHealthState::Running, "{health:?}");
        assert!(health.detail.contains("recorded 1 fault"), "{health:?}");
        assert!(
            health.detail.contains("4 snapshots were skipped"),
            "{health:?}"
        );
    }

    /// A healthy tap, and a runtime build that predates the field, change
    /// nothing about the verdict.
    #[tokio::test]
    async fn a_healthy_or_missing_snapshot_tap_does_not_change_the_verdict() {
        let healthy = watch(vec![with_tap(1, true, 0, None), with_tap(5, true, 0, None)]).await;
        assert_eq!(healthy.state, DeployHealthState::Running, "{healthy:?}");
        assert!(!healthy.detail.contains("fault"), "{healthy:?}");

        let older = watch(vec![status(1, None), status(5, None)]).await;
        assert_eq!(older.state, DeployHealthState::Running, "{older:?}");
    }

    fn reading(n: u64, uptime: u64, id: &str, mode: &str) -> Result<serde_json::Value, String> {
        let mut v = status(n, None).unwrap();
        v["uptime_secs"] = uptime.into();
        v["runtime_id"] = id.into();
        v["mode"]["kind"] = mode.into();
        Ok(v)
    }

    #[tokio::test]
    async fn paused_and_single_step_programs_do_not_pass() {
        for mode in ["paused", "step"] {
            let h = watch(vec![reading(3, 10, "a", mode)]).await;
            assert_eq!(h.state, DeployHealthState::NotRunning, "{h:?}");
            assert!(!h.state.deploy_ok());
        }
    }

    #[tokio::test]
    async fn a_frozen_positive_counter_is_unknown_not_running() {
        let h = watch(vec![status(3, None)]).await;
        assert_eq!(h.state, DeployHealthState::Unknown, "{h:?}");
        assert!(!h.state.deploy_ok());
    }

    #[tokio::test]
    async fn restart_must_begin_a_fresh_observation() {
        // A restarted process may already have a greater count by the next poll.
        for (count, uptime, id) in [(2, 0, "a"), (900, 0, "a"), (900, 10, "b")] {
            let h = watch(vec![
                reading(800, 10, "a", "running"),
                reading(count, uptime, id, "running"),
            ])
            .await;
            assert_eq!(h.state, DeployHealthState::Unknown, "{h:?}");
        }
        let h = watch(vec![
            reading(800, 10, "a", "running"),
            reading(2, 0, "b", "running"),
            reading(5, 1, "b", "running"),
        ])
        .await;
        assert_eq!(h.state, DeployHealthState::Running, "{h:?}");
    }

    #[tokio::test]
    async fn equal_counts_between_long_period_scans_are_waited_for() {
        let mut reads = vec![status(3, None); 6];
        for v in reads.iter_mut().flatten() {
            v["scan_period_ms"] = 5000.into();
        }
        reads.push(status(4, None));
        assert_eq!(watch(reads).await.state, DeployHealthState::Running);
    }

    #[tokio::test]
    async fn missing_status_fields_are_not_assumed_healthy() {
        let h = watch(vec![Ok(serde_json::json!({"scan_count": 10}))]).await;
        assert_eq!(h.state, DeployHealthState::Unknown);
        // Older builds without the new identity remain compatible using uptime.
        let mut a = status(3, None).unwrap();
        let mut b = status(4, None).unwrap();
        a.as_object_mut().unwrap().remove("runtime_id");
        b.as_object_mut().unwrap().remove("runtime_id");
        assert_eq!(
            watch(vec![Ok(a), Ok(b)]).await.state,
            DeployHealthState::Running
        );
    }

    #[tokio::test]
    async fn a_read_error_breaks_the_progress_observation() {
        let h = watch(vec![
            status(3, None),
            Err("connection lost".into()),
            status(4, None),
        ])
        .await;
        assert_eq!(h.state, DeployHealthState::Unknown);
    }

    #[tokio::test]
    async fn deadline_cancels_a_hanging_read() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let h = tokio::time::timeout(
            Duration::from_millis(500),
            watch_restarted_program(
                || {
                    let guard = Dropped(dropped.clone());
                    async move {
                        let _guard = guard;
                        std::future::pending::<Result<serde_json::Value, String>>().await
                    }
                },
                Duration::from_millis(10),
                Duration::from_millis(1),
                &project::SupervisionConfig::default(),
            ),
        )
        .await
        .expect("the health deadline must bound the read");
        assert_eq!(h.state, DeployHealthState::Unknown);
        assert!(h.detail.contains("deadline"));
        assert!(
            dropped.load(Ordering::SeqCst),
            "pending read must be cancelled"
        );
    }

    #[tokio::test]
    async fn deadline_also_bounds_the_poll_sleep() {
        let h = tokio::time::timeout(
            Duration::from_millis(500),
            watch_restarted_program(
                || async { status(0, None) },
                Duration::from_millis(10),
                Duration::from_secs(60),
                &project::SupervisionConfig::default(),
            ),
        )
        .await
        .expect("poll sleep must not extend the deadline");
        assert_eq!(h.state, DeployHealthState::Unknown);
    }
}
