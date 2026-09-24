//! Headless edge runtime for IA2.
//!
//! Single binary, no IDE: load a project from disk → compile the named POU
//! → spawn the ironplc-bridge scan loop → expose a tiny HTTP server on a
//! local port so the IDE (via SSH port-forward) can stream `VarSnapshot`s
//! back for online debugging.
//!
//! Bind is `127.0.0.1` by default — remote access must go through SSH
//! port-forward, not direct exposure. There's intentionally no auth on
//! this server; the security perimeter is "only reachable via ssh".

use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use futures_util::stream::Stream;
use ironplc_bridge::{
    DeviceHealth, DeviceReport, DeviceSpec, ProgramHandle, RuntimeMode, RuntimeWriteError,
    VarSnapshot, WriteOutcome,
};
use project::{ProjectStore, ProtocolConfig, StoreError};
use serde::Serialize;
use tokio::signal;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as _;
use tower_http::cors::CorsLayer;
use tracing_subscriber::EnvFilter;

mod northbound;

const DEFAULT_BIND: &str = "127.0.0.1:13001";

/// Upper bound on the graceful bridge drain (stop scan loop -> failsafe ->
/// join the EtherCAT cyclic thread). Comfortably under systemd's
/// `TimeoutStopSec=10s` so a clean exit always wins the race against
/// SIGKILL; if a wedged bus blows this budget we log and exit anyway,
/// falling back to the drive's own watchdog.
const BRIDGE_DRAIN_TIMEOUT: Duration = Duration::from_secs(7);

// ============================================================
//  Log capture — tee tracing output into a ring buffer + a live
//  broadcast so the monitor server can surface it (GET /logs and
//  /logs/stream). This is what makes edge-side truth (EtherCAT
//  discovery, bus health, device connect failures) visible to the
//  IDE / CLI over the SSH tunnel, instead of being trapped in
//  journald on the box.
// ============================================================

#[derive(Clone)]
struct LogCapture {
    buf: Arc<Mutex<VecDeque<String>>>,
    tx: broadcast::Sender<String>,
    cap: usize,
}

impl LogCapture {
    fn new(cap: usize) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            buf: Arc::new(Mutex::new(VecDeque::new())),
            tx,
            cap,
        }
    }

    fn push_line(&self, line: String) {
        {
            let mut b = self.buf.lock().expect("log buffer");
            while b.len() >= self.cap {
                b.pop_front();
            }
            b.push_back(line.clone());
        }
        // No subscribers is fine — ignore the send error.
        let _ = self.tx.send(line);
    }

    /// Most recent `n` captured lines, oldest-first.
    fn tail(&self, n: usize) -> Vec<String> {
        let b = self.buf.lock().expect("log buffer");
        let start = b.len().saturating_sub(n);
        b.iter().skip(start).cloned().collect()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter {
            cap: self.clone(),
            buf: Vec::new(),
        }
    }
}

/// One formatted event's bytes accumulate here; on Drop (end of that
/// event's write) we split into lines and push them to the capture.
/// `fmt` formats an event into one buffer then writes it once, so a
/// fresh writer per event maps cleanly to "one (or few) line(s)".
struct CaptureWriter {
    cap: LogCapture,
    buf: Vec<u8>,
}

impl std::io::Write for CaptureWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for CaptureWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let s = String::from_utf8_lossy(&self.buf);
        for line in s.split('\n') {
            let trimmed = line.trim_end();
            if !trimmed.is_empty() {
                self.cap.push_line(trimmed.to_string());
            }
        }
    }
}

/// Parsed CLI args. Manual parsing (no clap) — four flags, still simple.
struct Args {
    project_dir: PathBuf,
    bind: SocketAddr,
    /// Where to load/save RETAIN variable values. Defaults to
    /// `<project_dir>/../state/retain.json`, matching how
    /// `infra/install.sh` lays out a typical edge deployment
    /// (`/opt/ia2/state/retain.json` alongside `/opt/ia2/current/`).
    state_dir: PathBuf,
    /// Built web assets (the IDE's `dist/`). When set, the runtime
    /// serves the standalone HMI panel at `/hmi/{screen}` so operator
    /// clients on the plant network can open screens straight off the
    /// edge box — no IDE in the loop. When absent, the project's
    /// sibling `web/` is auto-served if it holds the built panel — see
    /// `sibling_web_dir`.
    static_dir: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    let mut project_dir: Option<PathBuf> = None;
    let mut bind = DEFAULT_BIND.to_string();
    let mut state_dir: Option<PathBuf> = None;
    let mut static_dir: Option<PathBuf> = None;

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--project-dir" => {
                project_dir = Some(PathBuf::from(
                    iter.next().context("--project-dir requires a value")?,
                ));
            }
            "--bind" => {
                bind = iter.next().context("--bind requires a value")?;
            }
            "--state-dir" => {
                state_dir = Some(PathBuf::from(
                    iter.next().context("--state-dir requires a value")?,
                ));
            }
            "--static-dir" => {
                static_dir = Some(PathBuf::from(
                    iter.next().context("--static-dir requires a value")?,
                ));
            }
            // Legacy flag from the pre-tasks-refactor builds. Accept but
            // ignore so existing systemd unit files keep launching.
            "--app" => {
                let _ = iter.next();
                tracing::warn!(
                    "--app is deprecated; the runtime now compiles the whole project. \
                     Update your systemd unit to drop this flag."
                );
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(anyhow!("unknown argument: {other}")),
        }
    }

    let project_dir = project_dir.context("--project-dir is required")?;
    let bind: SocketAddr = bind
        .parse()
        .with_context(|| format!("--bind '{bind}' is not a valid socket address"))?;
    // Default: the sibling `state/` directory of the project, so a
    // project at `/opt/ia2/current/project` gets state at
    // `/opt/ia2/state/`. Survives `current` symlink rotations on
    // redeploy because the path is anchored to the install root, not
    // the version dir.
    let state_dir = state_dir.unwrap_or_else(|| {
        project_dir
            .parent()
            .and_then(|p| p.parent())
            .map(|root| root.join("state"))
            .unwrap_or_else(|| project_dir.join("state"))
    });

    Ok(Args {
        project_dir,
        bind,
        state_dir,
        static_dir,
    })
}

fn print_help() {
    eprintln!(
        "ia2-runtime — headless edge runtime\n\n\
         USAGE:\n  \
         ia2-runtime --project-dir <path> [--bind <addr>] [--state-dir <path>] [--static-dir <path>]\n\n\
         FLAGS:\n  \
         --project-dir <path>   Path to the project directory (containing project.toml).\n  \
         --bind <addr>          Local socket for the monitor server (default {DEFAULT_BIND}).\n  \
         --state-dir <path>     Where to persist RETAIN variables (default: sibling 'state/' of\n                         \
         the install root). Survives version swaps; safe to back up.\n  \
         --static-dir <path>    Built web assets. When set, operator clients can open the\n                         \
         project's HMI screens at http://<edge>:<port>/hmi/<screen>.\n                         \
         Default: web/ next to the project dir, when it holds the\n                         \
         built panel (the deploy layout).\n\n\
         The runtime exposes:\n  \
         GET  /health           Liveness check.\n  \
         GET  /status           Project + runtime metadata + last-known scan count.\n  \
         GET  /events           Server-Sent Events stream of VarSnapshot updates.\n  \
         GET  /api/hmi          HMI screens deployed with the project (read-only).\n  \
         POST /stop             Request graceful shutdown.\n\n\
         What runs is determined by the project's tasks.toml — every PROGRAM\n\
         instance declared there is bound to its task and scheduled.\n"
    );
}

/// Deploy lays an edge box out as `current/project` + `current/web`
/// (see `docs/edge-deploy.md`), but systemd units installed before
/// `--static-dir` existed launch without the flag — so when it is
/// absent, the panel falls back to the project's sibling `web/`. Gated
/// on `hmi.html` — the vite panel entry every deployed bundle contains
/// — so a coincidental `web/` directory next to a dev project is never
/// exposed through ServeDir. An explicit `--static-dir` bypasses this.
fn sibling_web_dir(project_dir: &Path) -> Option<PathBuf> {
    let dir = project_dir.parent()?.join("web");
    dir.join("hmi.html").is_file().then_some(dir)
}

/// Shared state for the HTTP handlers.
#[derive(Clone)]
struct AppState {
    project_name: String,
    /// PROGRAM instances actually scheduled, derived from tasks.toml.
    /// Reported by /status so attached IDEs / operators can see what's
    /// running without reading the project off-disk.
    program_instances: Vec<String>,
    devices: Vec<String>,
    /// Wall-clock when the runtime started accepting requests.
    start_time: Instant,
    /// Sender side of the runtime's snapshot fan-out. SSE handlers
    /// subscribe; the bridge subscriber task publishes.
    snapshot_tx: broadcast::Sender<VarSnapshot>,
    /// Most recent snapshot, kept so /status can return the latest scan
    /// count without subscribing.
    latest: Arc<Mutex<Option<VarSnapshot>>>,
    /// Flips true when /stop is hit; the main loop watches this and exits.
    shutdown: Arc<tokio::sync::Notify>,
    /// Captured tracing output (ring buffer + live broadcast) backing
    /// GET /logs and /logs/stream.
    logs: LogCapture,
    /// Handle to the running scan loop — used by /discover to read the
    /// per-device connect reports (connected/failed + EtherCAT topology).
    handle: ProgramHandle,
    /// Read-only view of the deployed project — backs the /api/hmi
    /// routes so operator panels can load screens off the edge box.
    store: Arc<ProjectStore>,
    /// Persisted historian (JSONL segments under the state dir) —
    /// backs GET /history.
    historian: Arc<ironplc_bridge::monitor::Historian>,
    /// Alarm engine over the deployed alarms.toml — backs GET /alarms,
    /// POST /alarms/{id}/ack, GET /alarms-journal.
    alarms: Arc<Mutex<ironplc_bridge::monitor::AlarmEngine>>,
    /// Bounded ring of every write attempt (HTTP + MQTT northbound)
    /// with its origin — backs GET /audit (ADR-0002 attribution).
    audit: AuditLog,
}

#[tokio::main]
async fn main() -> Result<()> {
    let log_capture = init_logging();
    let args = parse_args()?;

    tracing::info!(
        project_dir = %args.project_dir.display(),
        bind = %args.bind,
        version = env!("CARGO_PKG_VERSION"),
        "ia2-runtime starting"
    );

    // ---- Load + compile the whole project ----
    let store = ProjectStore::open(args.project_dir.clone())
        .with_context(|| format!("opening project at {}", args.project_dir.display()))?;
    let project_name = store.name().to_string();
    let tasks = store
        .read_tasks()
        .context("reading tasks.toml")?
        .ok_or_else(|| {
            anyhow!(
                "tasks.toml missing from project — run the IDE's 'Migrate to tasks' \
                 once, or hand-author tasks.toml, then redeploy"
            )
        })?;
    let program_instances: Vec<String> =
        tasks.programs.iter().map(|p| p.instance.clone()).collect();
    let devices = store.list_devices().context("listing devices")?;
    let iomap = store.read_iomap().context("reading iomap")?;
    let device_names = devices.iter().map(|d| d.name.clone()).collect::<Vec<_>>();
    let device_specs: Vec<DeviceSpec> = devices
        .into_iter()
        .map(|d| DeviceSpec {
            name: d.name,
            config: d.config,
        })
        .collect();
    for spec in &device_specs {
        if let ProtocolConfig::Ethercat(cfg) = &spec.config {
            // iomap-ethercat treats nic="_sim" (or empty) as sim mode and
            // anything else as a real NIC name. We only warn for sim —
            // a real NIC means we'll attempt real fieldbus traffic, and
            // errors there will surface naturally as connect failures.
            if cfg.nic == "_sim" || cfg.nic.is_empty() {
                tracing::warn!(
                    device = %spec.name,
                    "ethercat device is in simulation mode (nic=\"_sim\") — no real fieldbus traffic"
                );
            } else {
                tracing::info!(
                    device = %spec.name,
                    nic = %cfg.nic,
                    "ethercat device configured for real bus"
                );
            }
        }
    }

    // ADR-0001 gate — the same bridge implementation the IDE server
    // enforces, so the rule and its message can't drift between the
    // desktop and edge surfaces.
    if let Err(msg) = ironplc_bridge::reject_shared_globals(&store, &tasks) {
        anyhow::bail!("{msg} Then redeploy.");
    }

    // One container + VM per scheduled PROGRAM instance; the bridge's
    // single scan thread rotates them at their own task intervals.
    let units =
        ironplc_bridge::compile_project_units(&store, &tasks).context("compiling project")?;
    let retain_var_count: usize = units.iter().map(|u| u.retain_vars.len()).sum();
    tracing::info!(
        devices = device_specs.len(),
        mappings = iomap.mappings.len(),
        tasks = tasks.tasks.len(),
        programs = units.len(),
        retain_vars = retain_var_count,
        "compiled"
    );

    // RETAIN state file lives under the configured state dir. The
    // bridge handles missing-file / bad-content gracefully, so we
    // don't pre-create anything here. Skip the path entirely if no
    // unit declares RETAIN vars — no file means no future confusion
    // about "what's in this state.json".
    let state_path = if retain_var_count == 0 {
        None
    } else {
        let p = args.state_dir.join("retain.json");
        tracing::info!(state_path = %p.display(), "RETAIN state file");
        // The documented promise is that retained values survive a deploy,
        // which holds only while the state dir sits OUTSIDE the versioned
        // project tree. The `../state` default lands there for an absolute
        // `--project-dir`; a relative one makes `.parent().parent()` run out
        // of components and fall back to `<project>/state`, which the next
        // symlink swap orphans. Say so rather than let a totalizer vanish on
        // the deploy after next.
        if p.starts_with(&args.project_dir) {
            tracing::warn!(
                state_path = %p.display(),
                project_dir = %args.project_dir.display(),
                "RETAIN state lives INSIDE the project directory: a redeploy \
                 that replaces the project will orphan it. Pass an absolute \
                 --project-dir, or point --state-dir outside the versioned tree."
            );
        }
        Some(p)
    };

    // ---- Spawn the bridge ----
    let handle: ProgramHandle = ironplc_bridge::spawn_units(
        units,
        device_specs,
        iomap.mappings,
        state_path,
        store.governance().clone(),
    );

    // Historian: persisted JSONL segments under the state dir so an
    // edge restart (or redeploy — state/ sits beside `current`) keeps
    // recent history for post-event investigation. Zero config.
    let historian = Arc::new(ironplc_bridge::monitor::Historian::new(
        ironplc_bridge::monitor::history::DEFAULT_SAMPLE_INTERVAL_US,
        ironplc_bridge::monitor::history::DEFAULT_CAPACITY,
        Some(args.state_dir.join("history")),
    ));

    // Alarm engine from the deployed project's alarms.toml.
    let alarms = match store.read_alarms() {
        Ok(cfg) => {
            if !cfg.alarms.is_empty() {
                tracing::info!(alarms = cfg.alarms.len(), "alarm definitions loaded");
            }
            Arc::new(Mutex::new(ironplc_bridge::monitor::AlarmEngine::new(
                cfg.alarms,
            )))
        }
        Err(e) => {
            tracing::warn!(%e, "alarms.toml unreadable; process alarms disabled, device health alarms remain enabled");
            Arc::new(Mutex::new(ironplc_bridge::monitor::AlarmEngine::default()))
        }
    };

    // Fan out the bridge's snapshots into a runtime-owned broadcast channel,
    // so we can keep the latest snapshot in shared state for /status and so
    // SSE clients can come and go without affecting the bridge subscriber.
    // The same tap feeds the historian and the alarm engine.
    let (snapshot_tx, _) = broadcast::channel::<VarSnapshot>(128);
    let latest: Arc<Mutex<Option<VarSnapshot>>> = Arc::new(Mutex::new(None));
    let shutdown = Arc::new(tokio::sync::Notify::new());

    {
        let snapshot_tx = snapshot_tx.clone();
        let latest = latest.clone();
        let historian = historian.clone();
        let alarms = alarms.clone();
        let mut rx = handle.subscribe();
        tokio::spawn(async move {
            while let Ok(snap) = rx.recv().await {
                historian.note_snapshot(&snap);
                alarms
                    .lock()
                    .expect("alarms mutex")
                    .note_snapshot(&snap, ironplc_bridge::monitor::wall_now_us());
                *latest.lock().expect("latest mutex") = Some(snap.clone());
                let _ = snapshot_tx.send(snap);
            }
        });
    }

    let store = Arc::new(store);
    let state = AppState {
        project_name,
        program_instances,
        devices: device_names,
        start_time: Instant::now(),
        snapshot_tx,
        latest,
        shutdown: shutdown.clone(),
        logs: log_capture,
        handle: handle.clone(),
        store: store.clone(),
        historian,
        alarms,
        audit: AuditLog::default(),
    };

    // ---- Northbound (MQTT -> supOS/Tier0) ----
    match store.read_northbound() {
        Ok(nb) => {
            if let Some(mqtt) = nb.mqtt.filter(|m| m.enabled) {
                northbound::spawn(northbound::NorthboundCtx {
                    config: mqtt,
                    project_name: state.project_name.clone(),
                    latest: state.latest.clone(),
                    handle: handle.clone(),
                    audit: state.audit.clone(),
                });
            } else {
                tracing::info!("northbound disabled (no enabled [mqtt] in northbound.toml)");
            }
        }
        Err(e) => tracing::warn!(%e, "northbound.toml unreadable; northbound disabled"),
    }

    // ---- HTTP server ----
    // Permissive CORS: the only path to this socket on a real edge box is
    // through an SSH port-forward we (the dev) opened. The audience here
    // is "whatever dev tool is on the other end of the tunnel" — making
    // the IDE / curl / browser direct EventSource all work is the goal.
    let app = Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/events", get(events))
        .route("/logs", get(logs))
        .route("/logs/stream", get(logs_stream))
        .route("/discover", get(discover))
        .route("/system", get(system))
        .route("/pause", post(rt_pause))
        .route("/resume", post(rt_resume))
        .route("/step", post(rt_step))
        .route("/write", post(rt_write))
        .route("/force", post(rt_force))
        .route("/unforce", post(rt_unforce))
        .route("/inject-scan-stall", post(rt_inject_scan_stall))
        .route("/audit", get(rt_audit))
        .route("/history", get(history))
        .route("/alarms", get(alarms_state))
        .route("/alarms/{id}/ack", post(alarms_ack))
        .route("/alarms-journal", get(alarms_journal))
        .route("/stop", post(stop_handler))
        .route("/api/hmi", get(hmi_list))
        .route("/api/hmi/{path}", get(hmi_get))
        .layer(CorsLayer::permissive())
        .with_state(state);

    // ---- Standalone HMI panel (optional) ----
    // With `--static-dir` pointing at the built web assets, operator
    // clients on the plant network open screens directly off this box:
    // `/hmi` lists the deployed screens, `/hmi/<screen>` renders one,
    // live against this runtime's own /events + /write. Assets resolve
    // through ServeDir; the panel shell is a separate vite entry
    // (`hmi.html`) so none of the IDE bundle loads on an edge client.
    // No flag: fall back to the deploy layout's sibling web/ so edges
    // whose unit predates `--static-dir` still serve the panel.
    let static_dir = args.static_dir.clone().or_else(|| {
        let dir = sibling_web_dir(&args.project_dir)?;
        tracing::info!(
            static_dir = %dir.display(),
            "no --static-dir; serving the HMI panel from web/ next to the project"
        );
        Some(dir)
    });
    let app = if let Some(dir) = &static_dir {
        if !dir.join("hmi.html").exists() {
            // Routes still mount (they answer 404 with a hint) — this
            // warn is for the journal, where "why is /hmi empty" gets
            // debugged. A deploy without web assets lands here.
            tracing::warn!(static_dir = %dir.display(), "no hmi.html under --static-dir; panel disabled until assets arrive");
        }
        let panel = dir.join("hmi.html");
        let serve_panel = move || {
            let panel = panel.clone();
            async move {
                match tokio::fs::read_to_string(&panel).await {
                    Ok(html) => axum::response::Html(html).into_response(),
                    Err(_) => (
                        StatusCode::NOT_FOUND,
                        "hmi.html missing from --static-dir (build the web app first)",
                    )
                        .into_response(),
                }
            }
        };
        app.route(
            "/",
            get(|| async { axum::response::Redirect::temporary("/hmi") }),
        )
        .route("/hmi", get(serve_panel.clone()))
        .route("/hmi/{*rest}", get(serve_panel))
        .fallback_service(tower_http::services::ServeDir::new(dir))
    } else {
        app
    };

    let listener = tokio::net::TcpListener::bind(args.bind)
        .await
        .with_context(|| format!("binding {}", args.bind))?;
    tracing::info!(addr = %args.bind, "monitor server listening");

    // Run the HTTP server as a background task. We deliberately do NOT gate
    // shutdown on it via `with_graceful_shutdown`: the long-lived SSE
    // streams (/events, /logs/stream) never end on their own, so graceful
    // shutdown would block on them — which previously stalled the whole
    // process past systemd's TimeoutStopSec and got us SIGKILLed *before*
    // the failsafe ran (so a connected servo only de-energized via its own
    // bus watchdog). Instead we own the shutdown sequence below: drive the
    // safety-critical bridge drain first, then drop the server.
    let server_task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(%e, "monitor server error");
        }
    });

    // Block until SIGTERM / SIGINT / POST /stop.
    wait_for_shutdown(shutdown).await;

    // ---- Safety-critical shutdown ----
    // Stop the scan loop, drive every device to failsafe (zero outputs —
    // for a servo that's controlword=0 / Disable Voltage), and JOIN the
    // EtherCAT cyclic thread so that zeroed frame is actually on the wire
    // before we exit. `handle.shutdown()` blocks until the scan thread has
    // done all of that. Bounded so a wedged bus can't hold us past the
    // service supervisor's kill timeout.
    tracing::info!(
        "shutdown signalled; draining bridge (stop scan loop -> failsafe -> join fieldbus)"
    );
    match tokio::time::timeout(BRIDGE_DRAIN_TIMEOUT, handle.shutdown()).await {
        Ok(()) => tracing::info!(
            "bridge drained; inspect failsafe/shutdown results above for unconfirmed outputs"
        ),
        Err(_) => tracing::error!(
            timeout_s = BRIDGE_DRAIN_TIMEOUT.as_secs(),
            "bridge drain timed out; exiting anyway (outputs fall back to the drive watchdog)"
        ),
    }

    // The server has done its job; stop accepting and drop any in-flight
    // SSE connections. Clients on the SSH tunnel handle the disconnect.
    server_task.abort();
    tracing::info!("ia2-runtime exiting");
    Ok(())
}

/// Composite shutdown signal: any of SIGTERM, SIGINT, or POST /stop.
async fn wait_for_shutdown(shutdown: Arc<tokio::sync::Notify>) {
    #[cfg(unix)]
    let term = async {
        let mut sig = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        sig.recv().await;
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = signal::ctrl_c() => tracing::info!("received Ctrl+C"),
        _ = term => tracing::info!("received SIGTERM"),
        _ = shutdown.notified() => tracing::info!("received /stop"),
    }
}

/// Set up tracing with two fmt layers under one env-filter: the usual
/// stdout/journald sink (kept untouched) plus a capture layer (ANSI
/// stripped) that tees into the returned `LogCapture` for /logs.
fn init_logging() -> LogCapture {
    use tracing_subscriber::prelude::*;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "ia2_runtime=info,ironplc_bridge=info,iomap_modbus=info,iomap_ethercat=info,info",
        )
    });
    let capture = LogCapture::new(2000);
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(capture.clone()),
        )
        .init();
    capture
}

// ============================================================
//  HTTP handlers
// ============================================================

#[derive(Serialize)]
struct Health {
    status: &'static str,
    uptime_secs: u64,
    scan_count: u64,
    /// `true` only when every configured device's transport is live.
    /// `false` means at least one device failed to connect or its
    /// background link is down — input variables from that device are
    /// frozen at last-known values, even though `scan_count` keeps
    /// climbing and `/status` keeps serving snapshots.
    fieldbus_healthy: bool,
    /// Per-device breakdown behind `fieldbus_healthy`.
    devices: Vec<DeviceHealth>,
    /// `true` once the scan watchdog latched the outputs off. A monitoring
    /// probe that only checks `status: "ok"` would otherwise call a latched
    /// runtime healthy — it is still serving HTTP and still scanning.
    watchdog_tripped: bool,
}

async fn health(State(state): State<AppState>) -> Json<Health> {
    let scan_count = state
        .latest
        .lock()
        .expect("latest")
        .as_ref()
        .map(|s| s.scan_count)
        .unwrap_or(0);
    let devices = state.handle.device_health();
    Json(Health {
        status: "ok",
        uptime_secs: state.start_time.elapsed().as_secs(),
        scan_count,
        fieldbus_healthy: devices.iter().all(|d| d.healthy),
        devices,
        watchdog_tripped: state.handle.watchdog_tripped(),
    })
}

use ironplc_bridge::monitor::{self, ForceEntry, ModeResponse};

#[cfg(test)]
mod write_response_tests {
    use super::write_response;
    use ironplc_bridge::WriteOutcome;

    #[test]
    fn the_body_carries_the_keys_the_panel_reads_by_name() {
        let delivered = write_response(
            "stop_cmd",
            &WriteOutcome {
                value: 1,
                undelivered_device: None,
            },
        );
        assert_eq!(delivered["ok"], true, "cs edge reads this");
        assert_eq!(delivered["name"], "stop_cmd");
        assert_eq!(delivered["value"], 1);
        assert!(
            delivered["undelivered_device"].is_null(),
            "a delivered write must say so explicitly, not omit the key"
        );

        let stranded = write_response(
            "stop_cmd",
            &WriteOutcome {
                value: 1,
                undelivered_device: Some("bus_a".into()),
            },
        );
        assert_eq!(
            stranded["value"], 1,
            "an undelivered write is still applied"
        );
        assert_eq!(
            stranded["undelivered_device"], "bus_a",
            "this exact key is what the panel reads; renaming it drops the warning silently"
        );
    }
}

#[cfg(test)]
mod build_stamp_tests {
    /// A provenance stamp that silently degrades to an empty string is worse
    /// than none — `/status` would serve a field that looks answered. Assert
    /// the shape, including the honest `unknown` fallback.
    #[test]
    fn the_build_stamp_is_present_and_shaped() {
        let commit = env!("IA2_BUILD_COMMIT");
        let rustc = env!("IA2_BUILD_RUSTC");
        assert!(!commit.is_empty(), "commit stamp must never be empty");
        assert!(!rustc.is_empty(), "rustc stamp must never be empty");
        assert!(
            rustc == "unknown" || rustc.starts_with("rustc "),
            "rustc stamp should be a version line or the honest fallback: {rustc}"
        );
        eprintln!("build stamp: commit={commit} rustc={rustc}");
    }
}

#[derive(Serialize)]
struct Status {
    version: &'static str,
    /// What built this binary: the source commit (suffixed `-dirty` when the
    /// tree had uncommitted changes) and the compiler version, stamped in at
    /// compile time by `build.rs`.
    ///
    /// A deployed runtime is a bare file on a box with nothing beside it
    /// recording its origin; without this, "which commit is the bench
    /// running?" has no answer months later. Either may be `unknown` when the
    /// build could not see a git tree and none was supplied — an absent
    /// provenance, not a guessed one.
    build_commit: &'static str,
    build_rustc: &'static str,
    project: String,
    /// PROGRAM instances scheduled by the project's tasks.toml.
    program_instances: Vec<String>,
    devices: Vec<String>,
    /// Live transport health per configured device. A `healthy: false`
    /// entry means that device's input variables in `last_snapshot` are
    /// frozen at last-known values (link down or connect failed).
    device_health: Vec<DeviceHealth>,
    /// `true` once the scan watchdog tripped: the program lost real-time
    /// guarantees, every output was zeroed and the output phase is latched
    /// off until this process restarts.
    ///
    /// Check this before reading `last_snapshot` as plant state. The VM keeps
    /// computing and `scan_count` keeps climbing after a trip, so a poller
    /// watching only values sees a healthy plant while the bus holds zeros.
    watchdog_tripped: bool,
    /// Fastest task interval in ms — the cadence at which `scan_count` can
    /// advance, since it is the max across units. A client judging "is this
    /// live data current?" needs this: `interval_ms` has no upper bound, and
    /// on a 5 s-cycle project a 4 s-old value is the newest one that exists,
    /// so a fixed staleness window would refuse every operator write between
    /// scans. It says nothing about how long a scan actually takes — the
    /// overrun watchdog owns that.
    scan_period_ms: u32,
    /// Scan deadlines missed since the program started, summed over units, and
    /// the longest streak any unit is currently on.
    ///
    /// The overrun log line fires ONCE per unit and then suppresses itself, so
    /// after it scrolls away nothing else answers "is this runtime still
    /// missing deadlines?". `watchdog_tripped` only covers the terminal case.
    /// A total that climbs between two polls means it is missing them now; the
    /// streak is the distance to the trip threshold.
    ///
    /// This is the ACHIEVED cadence. `scan_period_ms` is the configured one and
    /// says nothing about whether it is being met.
    scan_overruns: u64,
    consecutive_scan_overruns: u32,
    uptime_secs: u64,
    scan_count: u64,
    last_snapshot: Option<VarSnapshot>,
    /// Scan-loop execution mode (running / paused / step) — debug state
    /// so an attached IDE can show "paused" / step progress.
    mode: RuntimeMode,
    /// Currently-forced variables (name → pinned value).
    forces: Vec<ForceEntry>,
    /// Why the scan loop died, if it died: a VM trap, a VM that failed to
    /// start, or a panic. `null` while running or after a requested stop.
    /// Agents watching an edge poll this to tell "still scanning" from
    /// "faulted and halted" — without it a trapped program looks like a
    /// frozen snapshot.
    fault: Option<String>,
    /// Alarms needing operator attention (active or cleared-unacked).
    /// The full list is one GET /alarms away; the count rides on
    /// /status so panels see it on their existing poll.
    alarms_standing: usize,
}

async fn status(State(state): State<AppState>) -> Json<Status> {
    let last_snapshot = state.latest.lock().expect("latest").clone();
    let scan_count = last_snapshot.as_ref().map(|s| s.scan_count).unwrap_or(0);
    let alarms_standing = state
        .alarms
        .lock()
        .expect("alarms mutex")
        .states()
        .iter()
        .filter(|a| a.standing())
        .count();
    Json(Status {
        version: env!("CARGO_PKG_VERSION"),
        build_commit: env!("IA2_BUILD_COMMIT"),
        build_rustc: env!("IA2_BUILD_RUSTC"),
        project: state.project_name.clone(),
        program_instances: state.program_instances.clone(),
        devices: state.devices.clone(),
        device_health: state.handle.device_health(),
        watchdog_tripped: state.handle.watchdog_tripped(),
        scan_period_ms: state.handle.scan_period_ms(),
        scan_overruns: state.handle.scan_overruns(),
        consecutive_scan_overruns: state.handle.consecutive_scan_overruns(),
        uptime_secs: state.start_time.elapsed().as_secs(),
        scan_count,
        last_snapshot,
        mode: state.handle.mode(),
        forces: monitor::force_entries(&state.handle),
        fault: state.handle.fault(),
        alarms_standing,
    })
}

// ============================================================
//  History + alarms — same shapes as the IDE server's
//  /api/runtime/{history,alarms,alarms-journal}; served locally from
//  the edge's persisted historian and the deployed alarms.toml.
// ============================================================

#[derive(serde::Deserialize)]
struct HistoryQuery {
    #[serde(default)]
    vars: String,
    #[serde(default)]
    from_us: u64,
    #[serde(default)]
    to_us: u64,
    #[serde(default = "default_step_ms")]
    step_ms: u64,
}

fn default_step_ms() -> u64 {
    1000
}

async fn history(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<HistoryQuery>,
) -> Json<ironplc_bridge::monitor::HistoryResponse> {
    let vars: Vec<String> = q
        .vars
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    Json(state.historian.query(&vars, q.from_us, q.to_us, q.step_ms))
}

async fn alarms_state(
    State(state): State<AppState>,
) -> Json<Vec<ironplc_bridge::monitor::AlarmState>> {
    Json(state.alarms.lock().expect("alarms mutex").states())
}

async fn alarms_ack(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ironplc_bridge::monitor::AlarmState>, (StatusCode, String)> {
    let now_us = ironplc_bridge::monitor::wall_now_us();
    state
        .alarms
        .lock()
        .expect("alarms mutex")
        .ack(&id, now_us)
        .map(Json)
        .map_err(|e| (StatusCode::NOT_FOUND, e))
}

#[derive(serde::Deserialize)]
struct JournalQuery {
    #[serde(default = "default_journal_limit")]
    limit: usize,
}

fn default_journal_limit() -> usize {
    100
}

async fn alarms_journal(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<JournalQuery>,
) -> Json<Vec<ironplc_bridge::monitor::AlarmJournalEntry>> {
    Json(state.alarms.lock().expect("alarms mutex").journal(q.limit))
}

/// Same row shape as the IDE server's `HmiListEntry` (whose ts-rs export
/// the panel shell consumes) — keep the fields in sync.
#[derive(Serialize)]
struct HmiRow {
    path: String,
    title: String,
    level: u8,
}

/// Screens deployed with the project, for the panel's index page.
/// Unreadable documents are skipped, not fatal — one corrupt screen
/// shouldn't blank the whole panel.
async fn hmi_list(State(state): State<AppState>) -> Response {
    match state.store.list_hmis() {
        Ok(paths) => {
            let rows: Vec<HmiRow> = paths
                .into_iter()
                .filter_map(|p| {
                    let doc = state.store.read_hmi(&p).ok()?;
                    Some(HmiRow {
                        path: p,
                        title: doc.title,
                        level: doc.level,
                    })
                })
                .collect();
            Json(rows).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn hmi_get(State(state): State<AppState>, AxumPath(path): AxumPath<String>) -> Response {
    match state.store.read_hmi(&path) {
        Ok(doc) => Json(doc).into_response(),
        Err(StoreError::HmiNotFound(_)) => (
            StatusCode::NOT_FOUND,
            format!("no HMI screen '{path}' in the deployed project"),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // BroadcastStream surfaces both Ok(snap) and Err(Lagged); drop lagged
    // ticks rather than disconnect the client — the IDE just wants the
    // latest values, not lossless history.
    let rx = state.snapshot_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|res| match res {
        Ok(snap) => Event::default().json_data(&snap).ok().map(Ok),
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[derive(serde::Deserialize)]
struct LogQuery {
    /// How many recent lines to return (default 200).
    tail: Option<usize>,
}

#[derive(Serialize)]
struct LogsResponse {
    lines: Vec<String>,
}

/// One-shot: the most recent `tail` (default 200) captured log lines.
/// This is what `cs get edges/<n>/logs` pulls over the SSH tunnel — surfacing
/// EtherCAT discovery / bus-health / connect errors that previously
/// only existed in journald on the edge.
async fn logs(State(state): State<AppState>, Query(q): Query<LogQuery>) -> Json<LogsResponse> {
    let n = q.tail.unwrap_or(200);
    Json(LogsResponse {
        lines: state.logs.tail(n),
    })
}

/// SSE: live log lines as they're emitted. No backlog — pair with
/// GET /logs for history (same split as /events vs /status).
async fn logs_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.logs.tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|res| match res {
        Ok(line) => Some(Ok(Event::default().data(line))),
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// Per-device connect reports + discovered EtherCAT topology. Powers
/// `cs get edges/<n>/scan` — the IDE authors PDO maps against the real bus.
async fn discover(State(state): State<AppState>) -> Json<Vec<DeviceReport>> {
    Json(state.handle.device_reports())
}

#[derive(Serialize)]
struct Nic {
    name: String,
    mac: String,
    /// `up` / `down` / `unknown` from the kernel.
    operstate: String,
    /// Whether a link partner is detected (cable in + powered).
    carrier: bool,
}

#[derive(Serialize)]
struct SystemInfo {
    arch: String,
    os: String,
    /// Network interfaces — pick one for an EtherCAT device's `nic`.
    nics: Vec<Nic>,
    /// Serial device paths — pick one for a Modbus RTU device.
    serial_ports: Vec<String>,
}

/// Enumerate the edge's interfaces / serial ports / arch so the IDE can
/// author device configs against real facts (which NIC has carrier for
/// EtherCAT, which /dev/tty* exists for Modbus RTU) instead of guessing.
/// Non-privileged: reads /sys/class/net and lists /dev.
fn collect_system_info() -> SystemInfo {
    let mut nics = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let p = e.path();
            let read = |f: &str| std::fs::read_to_string(p.join(f)).unwrap_or_default();
            nics.push(Nic {
                mac: read("address").trim().to_string(),
                operstate: read("operstate").trim().to_string(),
                carrier: read("carrier").trim() == "1",
                name,
            });
        }
    }
    nics.sort_by(|a, b| a.name.cmp(&b.name));

    let mut serial_ports = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/dev") {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            // USB-serial adapters (the common Modbus RTU case) + macOS
            // callout devices. `ttyS*` (builtin UARTs) are skipped — they
            // exist as nodes even with no hardware and just add noise.
            if n.starts_with("ttyUSB") || n.starts_with("ttyACM") || n.starts_with("cu.") {
                serial_ports.push(format!("/dev/{n}"));
            }
        }
    }
    serial_ports.sort();

    SystemInfo {
        arch: std::env::consts::ARCH.to_string(),
        os: std::env::consts::OS.to_string(),
        nics,
        serial_ports,
    }
}

/// Edge interfaces / serial ports / arch — powers `cs get edges/<n>/system`.
async fn system() -> Json<SystemInfo> {
    Json(collect_system_info())
}

// ============================================================
//  Online debug control — pause / resume / step / write / force.
//  Mirrors the IDE-side /api/runtime/* surface so an attached edge is a
//  full debug target, not just observable. NOTE: write/force can drive
//  real outputs on a connected bus — same power (and responsibility) as
//  forcing a variable on the local runtime; the scan loop's failsafe
//  still zeroes outputs on stop.
// ============================================================

async fn rt_pause(State(state): State<AppState>) -> Json<ModeResponse> {
    Json(monitor::pause(&state.handle))
}

async fn rt_resume(State(state): State<AppState>) -> Json<ModeResponse> {
    Json(monitor::resume(&state.handle))
}

async fn rt_step(
    State(state): State<AppState>,
    Json(req): Json<monitor::StepRequest>,
) -> Json<ModeResponse> {
    Json(monitor::step(&state.handle, req.cycles))
}

#[derive(serde::Deserialize)]
struct VarWriteReq {
    name: String,
    /// Bit-packed value — the caller resolves the variable's type, same
    /// as the IDE-side runtime write/force surface.
    value: i32,
    /// Momentary hold: after `pulse_ms` the RUNTIME writes 0 back, so a
    /// dead operator tab can't leave a momentary command latched. Same
    /// contract as the IDE server's write route; latest-write-wins on
    /// overlap.
    #[serde(default)]
    pulse_ms: Option<u32>,
}

#[derive(serde::Deserialize)]
struct VarNameReq {
    name: String,
}

// ---- Write audit (ADR-0002 attribution on the runtime write path) ----
//
// Every write that reaches this process — HTTP /write·/force·/unforce,
// MQTT northbound — is recorded here with its origin (`X-IA2-Origin`
// header, `mqtt`, or `anonymous`). Bounded ring; `GET /audit` serves it.
// The edge binds localhost-only, so the audit is read over the same SSH
// tunnel the writes arrive through.

/// One recorded write attempt. Both sides of the attempt are stored —
/// what the caller ASKED for and what actually LANDED — with a
/// `result` distinguishing ok / clamped / the error text, so denied
/// (governance 403), failed, and clamped writes are as visible as
/// clean ones. An audit that only showed the applied value would let
/// "mqtt asked 150, governance wrote 100" read as "mqtt asked 100" —
/// lying by omission.
#[derive(Debug, Clone, Serialize)]
struct AuditEntry {
    /// Seconds since UNIX epoch (wall clock, like the historian).
    ts_unix_secs: u64,
    origin: String,
    op: String,
    name: String,
    /// What the caller asked to write, as bit-packed i32 (REAL =
    /// IEEE-754 bits). Absent for unforce, which carries no value.
    #[serde(skip_serializing_if = "Option::is_none")]
    requested: Option<i32>,
    /// What was actually written after governance. Present only when
    /// the write was applied; `applied != requested` ⇔ `result` is
    /// "clamped".
    #[serde(skip_serializing_if = "Option::is_none")]
    applied: Option<i32>,
    /// "ok", "clamped", or the error text verbatim (a governance
    /// denial records its reason here).
    result: String,
}

type AuditLog = Arc<Mutex<VecDeque<AuditEntry>>>;

const AUDIT_CAP: usize = 256;

fn record_audit(
    log: &AuditLog,
    origin: &str,
    op: &str,
    name: &str,
    requested: Option<i32>,
    applied: Option<i32>,
    result: &str,
) {
    let ts_unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut g = log.lock().expect("audit mutex");
    if g.len() >= AUDIT_CAP {
        g.pop_front();
    }
    g.push_back(AuditEntry {
        ts_unix_secs,
        origin: origin.to_string(),
        op: op.to_string(),
        name: name.to_string(),
        requested,
        applied,
        result: result.to_string(),
    });
}

/// Audit outcome for one write/force attempt: `(applied, result)`
/// derived from the requested value and what the runtime answered.
/// "clamped" is claimed exactly when governance altered the value —
/// the log must never say a write landed as asked when it did not,
/// nor the reverse.
pub(crate) fn audit_outcome(
    requested: i32,
    result: &Result<i32, RuntimeWriteError>,
) -> (Option<i32>, String) {
    match result {
        Ok(v) if *v == requested => (Some(*v), "ok".to_string()),
        Ok(v) => (Some(*v), "clamped".to_string()),
        Err(e) => (None, e.to_string()),
    }
}

/// Writes carry delivery state on top of the applied value. An undelivered
/// write IS an applied write — the ring records what the VM took — so the
/// distinction rides in the outcome text, where the trail shows the value
/// never reached that device. Forces use [`audit_outcome`] directly; they
/// pin a VM value and make no claim about the field.
pub(crate) fn write_audit_outcome(
    requested: i32,
    result: &Result<WriteOutcome, RuntimeWriteError>,
) -> (Option<i32>, String) {
    match result {
        Ok(o) => {
            let (applied, base) = audit_outcome(requested, &Ok::<i32, RuntimeWriteError>(o.value));
            match &o.undelivered_device {
                Some(device) => (
                    applied,
                    format!("{base}; not delivered to device '{device}' (link down)"),
                ),
                None => (applied, base),
            }
        }
        Err(e) => (None, e.to_string()),
    }
}

/// Origin of a mutating request for the audit ring: the `X-IA2-Origin`
/// header (`gui`, `cs`, …) or `anonymous` when absent. The header value
/// is only ever stored/printed, never interpolated into a command.
fn origin_of(headers: &axum::http::HeaderMap) -> &str {
    headers
        .get("x-ia2-origin")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or("anonymous")
}

fn write_err(e: RuntimeWriteError) -> (StatusCode, String) {
    match e {
        RuntimeWriteError::UnknownVariable(n) => {
            (StatusCode::NOT_FOUND, format!("unknown variable '{n}'"))
        }
        RuntimeWriteError::Disconnected => {
            (StatusCode::CONFLICT, "scan loop has stopped".to_string())
        }
        RuntimeWriteError::Vm(e) => (StatusCode::INTERNAL_SERVER_ERROR, e),
        RuntimeWriteError::GovernanceDenied(n) => (
            StatusCode::FORBIDDEN,
            format!("write to '{n}' rejected by project governance"),
        ),
    }
}

/// The `/write` success body.
///
/// Extracted so its KEYS are covered by a test. They are hand-written strings
/// here — unlike the IDE server's typed `WriteVariableResponse`, which the
/// compiler checks — and the operator panel reads them by name, so a typo
/// would compile, deploy, and silently drop the undelivered warning.
///
/// `ok` stays in the body: `cs edge` reads it (crates/cli/src/cmd/edge.rs).
fn write_response(name: &str, outcome: &WriteOutcome) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "name": name,
        "value": outcome.value,
        // Same contract as the IDE server's WriteVariableResponse: the write
        // was applied, and this names the device it cannot currently reach.
        "undelivered_device": outcome.undelivered_device,
    })
}

async fn rt_write(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<VarWriteReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // The momentary-pulse reset guarantee lives in the shared monitor
    // core — one implementation for IDE server and edge runtime.
    let origin = origin_of(&headers).to_string();
    let result = monitor::write_with_pulse(&state.handle, &req.name, req.value, req.pulse_ms).await;
    let (applied, outcome) = write_audit_outcome(req.value, &result);
    record_audit(
        &state.audit,
        &origin,
        "write",
        &req.name,
        Some(req.value),
        applied,
        &outcome,
    );
    let outcome = result.map_err(write_err)?;
    Ok(Json(write_response(&req.name, &outcome)))
}

async fn rt_force(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<VarWriteReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let origin = origin_of(&headers).to_string();
    let result = state.handle.force_variable(&req.name, req.value).await;
    // Forces bypass governance (ADR-0002), so `applied == requested`
    // on success — audit_outcome would still expose a regression that
    // ever clamped a force, as a "clamped" entry.
    let (applied, outcome) = audit_outcome(req.value, &result);
    record_audit(
        &state.audit,
        &origin,
        "force",
        &req.name,
        Some(req.value),
        applied,
        &outcome,
    );
    let v = result.map_err(write_err)?;
    Ok(Json(
        serde_json::json!({ "ok": true, "name": req.name, "value": v }),
    ))
}

async fn rt_unforce(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<VarNameReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let origin = origin_of(&headers).to_string();
    let result = state.handle.unforce_variable(&req.name).await;
    match &result {
        Ok(()) => record_audit(
            &state.audit,
            &origin,
            "unforce",
            &req.name,
            None,
            None,
            "ok",
        ),
        Err(e) => record_audit(
            &state.audit,
            &origin,
            "unforce",
            &req.name,
            None,
            None,
            &e.to_string(),
        ),
    }
    result.map_err(write_err)?;
    Ok(Json(serde_json::json!({ "ok": true, "name": req.name })))
}

#[derive(serde::Deserialize)]
struct InjectScanStallReq {
    stall_ms: u64,
    #[serde(default = "default_inject_scans")]
    scans: u32,
}

fn default_inject_scans() -> u32 {
    ironplc_bridge::WATCHDOG_OVERRUN_THRESHOLD + 1
}

/// Fault injection (test primitive): stall the next N scans so the
/// scan watchdog trips through its real path. Deliberately drives the
/// runtime into latched failsafe — bench rehearsals of the watchdog
/// runbook, not normal operation. Restart the service to recover.
async fn rt_inject_scan_stall(
    State(state): State<AppState>,
    Json(req): Json<InjectScanStallReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    state
        .handle
        .inject_scan_stall(req.stall_ms, req.scans)
        .await
        .map_err(write_err)?;
    Ok(Json(
        serde_json::json!({ "injected": { "stall_ms": req.stall_ms, "scans": req.scans } }),
    ))
}

/// The write-audit ring, oldest first. Every HTTP /write·/force·/unforce
/// and every MQTT northbound write lands here with its origin and
/// outcome — denied and failed writes included.
async fn rt_audit(State(state): State<AppState>) -> Json<Vec<AuditEntry>> {
    let entries: Vec<AuditEntry> = state
        .audit
        .lock()
        .expect("audit mutex")
        .iter()
        .cloned()
        .collect();
    Json(entries)
}

async fn stop_handler(State(state): State<AppState>) -> impl IntoResponse {
    state.shutdown.notify_waiters();
    Json(serde_json::json!({"ok": true}))
}

#[cfg(test)]
mod tests {
    use super::{
        audit_outcome, origin_of, record_audit, sibling_web_dir, write_err, AuditLog,
        RuntimeWriteError, StatusCode, AUDIT_CAP,
    };

    #[test]
    fn audit_ring_is_bounded_and_keeps_the_newest() {
        let log = AuditLog::default();
        for i in 0..(AUDIT_CAP + 10) {
            record_audit(
                &log,
                "anonymous",
                "write",
                &format!("v{i}"),
                Some(i as i32),
                Some(i as i32),
                "ok",
            );
        }
        let g = log.lock().unwrap();
        assert_eq!(g.len(), AUDIT_CAP);
        // Oldest entries were evicted: the first survivor is v10.
        assert_eq!(g.front().unwrap().name, "v10");
        assert_eq!(g.back().unwrap().name, format!("v{}", AUDIT_CAP + 9));
    }

    /// A clamped write records BOTH sides of the story — the audit
    /// must be able to distinguish "asked 150, governance wrote 100"
    /// from "asked 100" (docs/api.md `/audit` row).
    #[test]
    fn clamped_write_records_requested_and_applied() {
        let (applied, outcome) = audit_outcome(150, &Ok(100));
        assert_eq!((applied, outcome.as_str()), (Some(100), "clamped"));

        let log = AuditLog::default();
        record_audit(&log, "mqtt", "write", "sp", Some(150), applied, &outcome);
        let g = log.lock().unwrap();
        let e = g.back().unwrap();
        assert_eq!(e.requested, Some(150));
        assert_eq!(e.applied, Some(100));
        assert_eq!(e.result, "clamped");
    }

    /// An untouched write is plain "ok" — the log must not cry
    /// "clamped" when nothing was altered.
    #[test]
    fn unclamped_write_is_plain_ok() {
        let (applied, outcome) = audit_outcome(42, &Ok(42));
        assert_eq!((applied, outcome.as_str()), (Some(42), "ok"));
    }

    /// docs/api.md's "denied writes are recorded too" — a governance
    /// denial lands in the ring with the requested value, no applied
    /// value, and the denial reason verbatim.
    #[test]
    fn denied_write_is_recorded_with_requested_value_and_reason() {
        let result: Result<i32, RuntimeWriteError> = Err(RuntimeWriteError::GovernanceDenied(
            "sp (write_mode = allowlist, no rule allows it)".into(),
        ));
        let (applied, outcome) = audit_outcome(150, &result);
        assert_eq!(applied, None);

        let log = AuditLog::default();
        record_audit(&log, "curl", "write", "sp", Some(150), applied, &outcome);
        let g = log.lock().unwrap();
        let e = g.back().unwrap();
        assert_eq!(e.requested, Some(150));
        assert_eq!(e.applied, None);
        assert!(
            e.result.contains("allowlist"),
            "denial reason must be recorded verbatim, got: {}",
            e.result
        );
    }

    /// The status the whole governance feature promises: a policy
    /// denial is 403, matching the IDE server's mapping so all four
    /// write paths agree (ADR-0002).
    #[test]
    fn governance_denied_maps_to_403() {
        let (status, body) = write_err(RuntimeWriteError::GovernanceDenied("x".into()));
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("governance"));
        // And the neighbours stay where they were.
        assert_eq!(
            write_err(RuntimeWriteError::UnknownVariable("x".into())).0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            write_err(RuntimeWriteError::Disconnected).0,
            StatusCode::CONFLICT
        );
    }

    /// The audit origin taxonomy is an OPEN set: any non-empty header
    /// value is recorded verbatim; only a missing/empty header is
    /// "anonymous" (skill 03 / api.md teach exactly this).
    #[test]
    fn origin_of_records_declared_labels_verbatim() {
        let mut headers = axum::http::HeaderMap::new();
        assert_eq!(origin_of(&headers), "anonymous");
        headers.insert("x-ia2-origin", "my-bridge".parse().unwrap());
        assert_eq!(origin_of(&headers), "my-bridge");
        headers.insert("x-ia2-origin", "".parse().unwrap());
        assert_eq!(origin_of(&headers), "anonymous");
    }

    #[test]
    fn deploy_layout_sibling_web_is_picked_up() {
        // The exact layout remote_deploy_script creates: current/project
        // next to current/web with the built panel inside.
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let web = root.path().join("web");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&web).unwrap();
        std::fs::write(web.join("hmi.html"), "<!doctype html>").unwrap();
        assert_eq!(sibling_web_dir(&project), Some(web));
    }

    #[test]
    fn sibling_web_without_panel_marker_is_not_served() {
        // A directory merely named `web` next to a dev project must not
        // be exposed through ServeDir.
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(root.path().join("web")).unwrap();
        assert_eq!(sibling_web_dir(&project), None);
    }

    #[test]
    fn no_sibling_web_means_no_panel() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        assert_eq!(sibling_web_dir(&project), None);
    }
}
