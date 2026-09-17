//! HTTP route handlers.
//!
//! Grouped by concern (project lifecycle / POUs / devices / iomap / runtime
//! / health) but kept in one file because the layer is still small.

use std::convert::Infallible;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        FromRequestParts, Path as AxumPath, Query, State,
    },
    http::request::Parts,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use futures_util::stream::Stream;
use futures_util::{SinkExt, StreamExt as FuturesStreamExt};
use ironplc_bridge::{CheckDiagnostic, DeviceSpec, VariableInfo};
use project::{
    default_projects_dir, load_last_opened, save_last_opened, Device, Edge, EthercatBringup, IoMap,
    MigrationReport, Pou, PouFile, PouLanguage, PouType, ProgramInstance, ProjectListing,
    ProjectManifest, ProjectStore, ProjectTree, Protocol, ProtocolConfig, Task, Tasks,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as TokioStreamExt;
use ts_rs::TS;

use crate::edges::{attach_edge, deploy_to_edge, probe_edge, AttachInfo, DeployReport, EdgeProbe};
use crate::error::ApiError;
use crate::events::{topic, AppEvent, MutationDetail};
use crate::state::RunningProgram;
use serde::Deserialize as SerdeDeserialize;

/// HTTP header name used by every multi-project-aware client. The web
/// app sets it from the `?project=` URL search param; CLI sets it
/// when `--project` is passed; both fall back to "header absent" =
/// "use the server's active project" for single-window workflows.
pub const PROJECT_HEADER: &str = "x-ia2-project";

/// Axum extractor that pulls the `X-IA2-Project` header off a
/// request. `None` means "header absent / empty"; the route handler
/// then falls back to the active project. Encoded headers carry an
/// explicit marker so legacy literal percent sequences remain literal.
/// Invalid selectors fail rather than silently targeting the active project.
#[derive(Debug, Clone, Default)]
pub struct ProjectName(pub Option<String>);

impl ProjectName {
    pub fn as_deref(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

impl<S> FromRequestParts<S> for ProjectName
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parse_project_header(&parts.headers)
    }
}

fn parse_project_header(headers: &axum::http::HeaderMap) -> Result<ProjectName, ApiError> {
    let invalid = || ApiError::BadRequest("invalid X-IA2-Project selector or encoding".into());
    let value = headers
        .get(PROJECT_HEADER)
        .map(|v| v.to_str().map(str::trim).map_err(|_| invalid()))
        .transpose()?;
    match headers.get("x-ia2-project-encoding") {
        None => Ok(ProjectName(
            value.filter(|v| !v.is_empty()).map(str::to_owned),
        )),
        Some(encoding) => {
            if encoding.to_str().map_err(|_| invalid())? != "percent" {
                return Err(invalid());
            }
            let value = value.filter(|v| !v.is_empty()).ok_or_else(invalid)?;
            let bytes = value.as_bytes();
            for (i, byte) in bytes.iter().enumerate() {
                if *byte == b'%'
                    && !bytes
                        .get(i + 1..i + 3)
                        .is_some_and(|pair| pair.iter().all(u8::is_ascii_hexdigit))
                {
                    return Err(invalid());
                }
            }
            let decoded = percent_encoding::percent_decode_str(value)
                .decode_utf8()
                .map_err(|_| invalid())?;
            Ok(ProjectName(Some(decoded.into_owned())))
        }
    }
}

#[derive(SerdeDeserialize, Debug, Default)]
pub struct AgentHeartbeatRequest {
    /// Short label for *what* the agent is currently doing — usually
    /// the `cs` subcommand path (e.g. "pou create"). Optional so a
    /// minimal "I'm alive" beacon works too.
    #[serde(default)]
    pub command: Option<String>,
    /// Stable identifier for this CLI run. The CLI generates a UUID
    /// once at process start and reuses it for every subsequent
    /// command, letting the frontend tell apart a single agent doing
    /// many things from multiple concurrent agents.
    #[serde(default)]
    pub session: Option<String>,
}

/// Heartbeat from an external agent (typically `cs` CLI). Updates the
/// "agent active" state used by the IDE's takeover overlay. Returns
/// instantly; the actual SSE event broadcast happens inside
/// `record_agent_heartbeat` on the leading edge, and inside the
/// `agent_watchdog` task on the trailing edge.
///
/// Returns the canonical `{ok: true}` to keep the wire-shape
/// consistent with other agent-friendly endpoints.
pub async fn agent_heartbeat(
    State(state): State<AppState>,
    Json(req): Json<AgentHeartbeatRequest>,
) -> Json<RunResponse> {
    state.record_agent_heartbeat(req.command, req.session);
    Json(RunResponse { ok: true })
}

#[derive(SerdeDeserialize, Debug)]
pub struct StartAgentSessionRequest {
    /// Client-generated unique id. `end` must pass the same id back
    /// — a stale `cs agent leave` from an old terminal won't kick a
    /// fresh agent that's already taken over.
    pub id: String,
    /// Human-readable label rendered in the IDE banner.
    /// "rebuilding tank controller" / "running tests" / etc.
    pub label: String,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct AgentSessionResponse {
    pub ok: bool,
    /// Echo back the started session so the client knows the
    /// authoritative server-side state (in case the request was
    /// re-tried and an older session was replaced).
    pub session: crate::state::AgentSession,
}

/// Open an explicit agent takeover session. The IDE's overlay
/// banner stays on with the supplied label until the matching
/// `/api/agent/session/end` arrives — replaces the old "every
/// command blinks the overlay" pattern with explicit enter/leave.
///
/// Policy: a fresh `start` replaces any existing session (single
/// human, single agent at a time — strict-locking with 409 errors
/// would be more frustrating than useful here).
pub async fn start_agent_session(
    State(state): State<AppState>,
    Json(req): Json<StartAgentSessionRequest>,
) -> Json<AgentSessionResponse> {
    let session = state.start_agent_session(req.id, req.label);
    Json(AgentSessionResponse { ok: true, session })
}

#[derive(SerdeDeserialize, Debug, Default)]
pub struct EndAgentSessionRequest {
    /// Session id to end. Server only closes if this matches the
    /// currently-open session. Omit (or send `null`) to force-end
    /// whatever session is open — the IDE's "kick agent" button
    /// uses this empty-body form.
    #[serde(default)]
    pub id: Option<String>,
}

/// Close an agent takeover session. Idempotent. Returns `{ok}`
/// indicating whether a session was actually closed (false if the
/// id didn't match or no session was open). Same shape as `stop`
/// so the CLI can treat them uniformly.
pub async fn end_agent_session(
    State(state): State<AppState>,
    body: Option<Json<EndAgentSessionRequest>>,
) -> Json<RunResponse> {
    let id = body.and_then(|Json(b)| b.id);
    let ok = state.end_agent_session(id.as_deref());
    Json(RunResponse { ok })
}

use crate::state::{AppState, RunningInfo};

// ============================================================
//  Response types (TS-exported)
// ============================================================

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct DemoSlaveSnapshot {
    pub coils: Vec<bool>,
    pub discrete_inputs: Vec<bool>,
    pub holding_registers: Vec<u16>,
    pub input_registers: Vec<u16>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct HealthStatus {
    pub status: String,
    pub uptime_secs: u64,
    pub project_open: bool,
    pub program_running: bool,
    /// Where the bundled demo Modbus TCP slave is listening, or empty
    /// when disabled (DEMO_MODBUS_ADDR="").
    pub demo_modbus_addr: String,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct RunResponse {
    pub ok: bool,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct ProjectInfo {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Deserialize, TS)]
#[ts(export)]
pub struct CreateProjectRequest {
    pub name: String,
}

#[derive(Debug, Deserialize, TS)]
#[ts(export)]
pub struct OpenProjectRequest {
    pub path: String,
}

#[derive(Debug, Deserialize, TS)]
#[ts(export)]
pub struct CreatePouRequest {
    /// Project-relative slash-path under `pous/`, without `.st`. The
    /// leaf is also used as the IEC POU identifier in the seeded source.
    pub path: String,
    #[serde(rename = "type")]
    pub type_: PouType,
    pub language: PouLanguage,
}

#[derive(Debug, Deserialize, TS)]
#[ts(export)]
pub struct CreateDeviceRequest {
    pub name: String,
    pub protocol: Protocol,
}

#[derive(Debug, Deserialize, TS)]
#[ts(export)]
pub struct CreateFolderRequest {
    /// Forward-slash relative path under `applications/` or `devices/`,
    /// e.g. `"pid_loops"` or `"actuators/valves"`. Each segment must be
    /// non-empty, not start with '.', and contain no backslashes/colons.
    pub path: String,
}

#[derive(Debug, Deserialize, TS)]
#[ts(export)]
pub struct CreateEdgeRequest {
    pub name: String,
    /// SSH host or ~/.ssh/config alias. Anything ssh(1) would accept.
    pub host: String,
}

/// Body for /api/run. Two modes:
///
/// - `{}` (empty) — uses the project's `tasks.toml` as the schedule.
///   Runs every PROGRAM instance declared there. Used by the Tasks
///   pane's Run button and any agent that wants the production schedule.
///
/// - `{ "program": "<iec_name>" }` — ad-hoc single-PROGRAM run. The
///   server synthesizes a minimal schedule (one default task + one
///   instance of the named PROGRAM) without touching tasks.toml.
///   Used by the ProgramPane's Run button so "click Run while looking
///   at cascade_pid" runs cascade_pid, period — matching engineer
///   intuition even when tasks.toml schedules something else.
#[derive(Debug, Default, Deserialize, TS)]
#[ts(export)]
pub struct RunRequest {
    /// IEC POU identifier of the PROGRAM to run in ad-hoc mode. None =
    /// fall back to tasks.toml.
    #[serde(default)]
    pub program: Option<String>,
    /// POU file path containing `program`. When set together with
    /// `program`, the compile input is limited to this file alone —
    /// other POU files are not concatenated, so ironplc's debug section
    /// (and therefore the Monitor pane) only sees the running PROGRAM's
    /// variables, not those of unrelated PROGRAMs in other files.
    ///
    /// Ignored when `program` is None.
    #[serde(default)]
    pub file_path: Option<String>,
}

// ============================================================
//  Health
// ============================================================

pub async fn health(State(state): State<AppState>) -> Json<HealthStatus> {
    let elapsed = state.start_time.elapsed().as_secs();
    let project_open = !state.projects.lock().is_empty();
    let program_running = state.program.lock().is_some();
    Json(HealthStatus {
        status: "ok".into(),
        uptime_secs: elapsed,
        project_open,
        program_running,
        demo_modbus_addr: state.demo_modbus_addr.clone(),
    })
}

/// /api/health alias — same payload as /health. Exists so agents that
/// scope to /api/* don't have to special-case liveness.
pub async fn api_health(State(state): State<AppState>) -> Json<HealthStatus> {
    health(State(state)).await
}

// ============================================================
//  Project lifecycle
// ============================================================

pub async fn list_projects() -> Json<Vec<ProjectListing>> {
    Json(scan_projects())
}

/// One sub-directory in a filesystem-browse listing.
#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct FsEntry {
    pub name: String,
    pub path: String,
    /// `true` when this directory is itself an IA2 project (contains
    /// `project.toml`), so the UI can offer to open it directly.
    pub is_project: bool,
}

/// A browsable directory: where we are, the parent to go up to, and the
/// child directories. Used by the "Open project" folder picker — a browser
/// can't surface a native OS path dialog, so the local server (which has
/// filesystem access) lists directories for the UI to navigate.
#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct FsListing {
    pub path: String,
    pub parent: Option<String>,
    /// `true` when `path` itself is an IA2 project directory.
    pub is_project: bool,
    pub entries: Vec<FsEntry>,
}

#[derive(Debug, Deserialize)]
pub struct BrowseQuery {
    /// Absolute directory to list. Empty/missing → the default projects
    /// directory (`~/Documents/IA2`).
    pub path: Option<String>,
}

/// List the sub-directories of `path` (default: the projects dir) for the
/// Open-project folder picker. Directories only — never file contents — and
/// dotfiles are hidden. A non-existent / non-directory path falls back to
/// the projects dir so the picker always lands somewhere sane.
pub async fn fs_browse(Query(q): Query<BrowseQuery>) -> Json<FsListing> {
    let dir = match q.path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => {
            let pb = PathBuf::from(p);
            if pb.is_dir() {
                pb
            } else {
                default_projects_dir()
            }
        }
        None => default_projects_dir(),
    };

    let parent = dir
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty());

    let mut entries = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue; // hide dotfiles
            }
            let is_project = p.join("project.toml").exists();
            entries.push(FsEntry {
                name,
                path: p.to_string_lossy().to_string(),
                is_project,
            });
        }
    }
    // Project dirs first, then alphabetical.
    entries.sort_by(|a, b| {
        b.is_project
            .cmp(&a.is_project)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Json(FsListing {
        is_project: dir.join("project.toml").exists(),
        path: dir.to_string_lossy().to_string(),
        parent,
        entries,
    })
}

pub async fn create_project(
    State(state): State<AppState>,
    Json(req): Json<CreateProjectRequest>,
) -> Result<Json<ProjectInfo>, ApiError> {
    let root = default_projects_dir().join(&req.name);
    let store = ProjectStore::create(root, &req.name)?;
    let info = ProjectInfo {
        name: store.name().into(),
        path: store.root().display().to_string(),
    };
    save_last_opened(store.root());
    let name_for_event = info.name.clone();
    state.projects.lock().insert_and_activate(store);
    save_open_projects(&state);
    state.emit_mutation(
        name_for_event,
        topic::PROJECT_META,
        MutationDetail::ProjectCreated {
            name: info.name.clone(),
            path: info.path.clone(),
        },
    );
    Ok(Json(info))
}

pub async fn open_project(
    State(state): State<AppState>,
    Json(req): Json<OpenProjectRequest>,
) -> Result<Json<ProjectInfo>, ApiError> {
    let path = resolve_user_path(&req.path);
    let store = ProjectStore::open(path)?;
    let info = ProjectInfo {
        name: store.name().into(),
        path: store.root().display().to_string(),
    };
    save_last_opened(store.root());
    let name_for_event = info.name.clone();
    // Adding doesn't displace other open projects — multi-window IDE
    // clients can now have several projects open simultaneously, each
    // pinned to one window via `X-IA2-Project`. The newly-opened one
    // becomes the active fallback for header-less requests.
    state.projects.lock().insert_and_activate(store);
    save_open_projects(&state);
    state.emit_mutation(
        name_for_event,
        topic::PROJECT_META,
        MutationDetail::ProjectOpened {
            name: info.name.clone(),
            path: info.path.clone(),
        },
    );
    Ok(Json(info))
}

pub async fn close_project(
    State(state): State<AppState>,
    project: ProjectName,
) -> Result<Json<RunResponse>, ApiError> {
    // Resolve which project to close — either the named one in the
    // header or the active fallback. Errors out (NoProject) if no
    // project is open. After resolving, stop the runtime if it
    // belongs to that project.
    let target = resolve_project_name(&state, &project)?;
    {
        let mut prog = state.program.lock();
        if let Some(rp) = prog.as_ref() {
            if rp.project_name == target {
                if let Some(rp) = prog.take() {
                    rp.handle.stop();
                }
            }
        }
    }
    // Tear down any ssh tunnels attached to this project's edges.
    state.attachments.detach_all_for_project(&target);
    let removed = state.projects.lock().remove(&target);
    if !removed {
        return Err(ApiError::NoProject);
    }
    save_open_projects(&state);
    // Wipe runtime caches if the program we stopped belonged to this
    // project — otherwise leave them so the other window can still
    // see its own values.
    let running_for_target = state
        .program
        .lock()
        .as_ref()
        .map(|rp| rp.project_name.clone())
        == Some(target.clone());
    if running_for_target {
        *state.last_snapshot.lock() = None;
        *state.last_error.lock() = None;
        *state.running_info.lock() = None;
        state.historian.clear();
        *state.alarms.lock() = ironplc_bridge::monitor::AlarmEngine::default();
    }
    state.emit_mutation(target, topic::PROJECT_META, MutationDetail::ProjectClosed);
    Ok(Json(RunResponse { ok: true }))
}

/// GET /api/projects/open — list every project the server currently
/// has open, plus which one is active. Distinct from
/// `/api/projects` (the disk-scan listing); this is the in-memory
/// set the multi-window IDE picks from.
pub async fn list_open_projects(State(state): State<AppState>) -> Json<OpenProjectsList> {
    let guard = state.projects.lock();
    let active = guard.active_name().map(str::to_string);
    let projects = guard
        .iter()
        .map(|store| OpenProjectInfo {
            name: store.name().to_string(),
            path: store.root().display().to_string(),
        })
        .collect();
    Json(OpenProjectsList { projects, active })
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct OpenProjectInfo {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct OpenProjectsList {
    pub projects: Vec<OpenProjectInfo>,
    /// Server's current "active fallback" project — the one a
    /// header-less request lands on. May be absent if nothing is
    /// open. Frontends with no `?project=` in their URL load this
    /// one by default.
    pub active: Option<String>,
}

pub async fn project_tree(
    State(state): State<AppState>,
    project: ProjectName,
) -> Result<Json<ProjectTree>, ApiError> {
    with_project(&state, &project, |store| {
        // The skeleton has each POU file's raw source. We parse each here
        // to surface declarations to the frontend (the store stays parser-
        // free; the bridge owns the parser).
        let skel = store.tree_skeleton()?;
        let pous: Vec<PouFile> = skel
            .pous
            .into_iter()
            .map(|f| PouFile {
                path: f.path,
                declarations: ironplc_bridge::extract_pou_declarations(&f.source, f.language),
            })
            .collect();
        Ok(ProjectTree {
            name: skel.name,
            path: skel.path,
            pous,
            pou_folders: skel.pou_folders,
            devices: skel.devices,
            device_folders: skel.device_folders,
            edges: skel.edges,
            edge_folders: skel.edge_folders,
            iomap: skel.iomap,
            tasks: skel.tasks,
        })
    })
    .map(Json)
}

// ============================================================
//  POUs (files holding 1+ IEC POU declarations)
// ============================================================
//
// Identifier convention: `path` = slash-separated location under
// `pous/`, no `.st` extension. Same shape as the old
// `/api/applications/{name}` route — only the noun changes.

/// Read a POU file: raw source + parsed declarations (PROGRAM/FB/FN found
/// inside). The IDE editor uses the source verbatim; agents and the Tasks
/// pane use the declaration list to drive scheduling.
pub async fn get_pou(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(path): AxumPath<String>,
) -> Result<Json<Pou>, ApiError> {
    with_project(&state, &project, |store| {
        let language = store.pou_file_language(&path)?;
        let source = store.read_pou_source(&path)?;
        Ok(Pou {
            path: path.clone(),
            declarations: ironplc_bridge::extract_pou_declarations(&source, language),
            source,
        })
    })
    .map(Json)
}

pub async fn create_pou(
    State(state): State<AppState>,
    project: ProjectName,
    Json(req): Json<CreatePouRequest>,
) -> Result<Json<Pou>, ApiError> {
    reject_library_path(&req.path)?;
    let created_path = req.path.clone();
    let pou = with_project(&state, &project, |store| {
        let language = req.language;
        let source = store.create_pou_file(&req.path, req.type_, language)?;
        Ok(Pou {
            path: req.path,
            declarations: ironplc_bridge::extract_pou_declarations(&source, language),
            source,
        })
    })?;
    emit_mutation(
        &state,
        &project,
        topic::PROJECT,
        MutationDetail::PouCreated { path: created_path },
    );
    Ok(Json(pou))
}

pub async fn save_pou(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(path): AxumPath<String>,
    body: String,
) -> Result<Json<RunResponse>, ApiError> {
    reject_library_path(&path)?;
    with_project(&state, &project, |store| {
        store.write_pou_source(&path, &body).map_err(Into::into)
    })?;
    // Fire two topics: the per-POU one (specific editor refetches its
    // own source) AND the project-wide one (declarations may have
    // changed, so the tree should re-derive symbols).
    emit_mutation(
        &state,
        &project,
        topic::pou(&path),
        MutationDetail::PouUpdated { path: path.clone() },
    );
    emit_mutation(
        &state,
        &project,
        topic::PROJECT,
        MutationDetail::PouUpdated { path },
    );
    Ok(Json(RunResponse { ok: true }))
}

pub async fn delete_pou(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(path): AxumPath<String>,
) -> Result<Json<RunResponse>, ApiError> {
    reject_library_path(&path)?;
    with_project(&state, &project, |store| {
        store.delete_pou_file(&path).map_err(Into::into)
    })?;
    emit_mutation(
        &state,
        &project,
        topic::PROJECT,
        MutationDetail::PouDeleted { path },
    );
    Ok(Json(RunResponse { ok: true }))
}

pub async fn create_pou_folder(
    State(state): State<AppState>,
    project: ProjectName,
    Json(req): Json<CreateFolderRequest>,
) -> Result<Json<RunResponse>, ApiError> {
    reject_library_path(&req.path)?;
    with_project(&state, &project, |store| {
        store.create_pou_folder(&req.path).map_err(Into::into)
    })?;
    emit_mutation(
        &state,
        &project,
        topic::PROJECT,
        MutationDetail::PouFolderCreated { path: req.path },
    );
    Ok(Json(RunResponse { ok: true }))
}

pub async fn delete_pou_folder(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(path): AxumPath<String>,
) -> Result<Json<RunResponse>, ApiError> {
    reject_library_path(&path)?;
    with_project(&state, &project, |store| {
        store.delete_pou_folder(&path).map_err(Into::into)
    })?;
    emit_mutation(
        &state,
        &project,
        topic::PROJECT,
        MutationDetail::PouFolderDeleted { path },
    );
    Ok(Json(RunResponse { ok: true }))
}

// ============================================================
//  Libraries
// ============================================================

/// The `pous/lib/**` subtree only mutates through /api/library — the
/// generic POU CRUD refuses it so an imported block can't silently
/// drift from its registry source. Reading (GET) stays open; "I want
/// to change FB_PID" = copy it out of `lib/` and own the copy.
fn reject_library_path(path: &str) -> Result<(), ApiError> {
    if project::is_library_slug(path) {
        return Err(ApiError::Conflict(
            "library files are read-only — import / update / remove them via /api/library, \
             or copy the POU out of lib/ to make it editable"
                .into(),
        ));
    }
    Ok(())
}

/// GET /api/library — every library in the server's registry, each
/// decorated with its imported state in the addressed project (when
/// one is open; browse works without).
pub async fn list_libraries(
    State(state): State<AppState>,
    project: ProjectName,
) -> Json<Vec<crate::library::LibrarySummary>> {
    let registry = crate::library::scan(state.library_dir.as_deref());
    let mut out: Vec<_> = registry.iter().map(|l| l.to_summary()).collect();
    // No open project (agent browsing a fresh server) → plain listing.
    let _ = with_project(&state, &project, |store| {
        let versions = store.read_libraries()?;
        let slugs = store.list_pou_paths()?;
        for summary in &mut out {
            summary.imported_version = versions.get(&summary.name).cloned();
            let prefix = format!("{}/{}/", project::LIBRARY_SLUG_PREFIX, summary.name);
            summary.imported_files = slugs
                .iter()
                .filter_map(|s| s.strip_prefix(&prefix))
                .filter(|rest| !rest.contains('/'))
                .map(str::to_string)
                .collect();
        }
        Ok(())
    });
    Json(out)
}

#[derive(Debug, Deserialize, TS)]
#[ts(export)]
pub struct ImportLibraryRequest {
    /// Registry library name, e.g. `process-control`.
    pub library: String,
    /// Bare block file names to import (`fb_pid.st`). Empty / omitted
    /// imports every block in the library. Re-importing overwrites —
    /// that's the update path.
    #[serde(default)]
    pub blocks: Vec<String>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct ImportLibraryResponse {
    pub library: String,
    pub version: String,
    /// Block files written under `pous/lib/<library>/`.
    pub imported: Vec<String>,
}

/// POST /api/library/import — vendor registry blocks into the project.
pub async fn import_library(
    State(state): State<AppState>,
    project: ProjectName,
    Json(req): Json<ImportLibraryRequest>,
) -> Result<Json<ImportLibraryResponse>, ApiError> {
    let registry = crate::library::scan(state.library_dir.as_deref());
    let lib = registry
        .into_iter()
        .find(|l| l.name == req.library)
        .ok_or_else(|| ApiError::NotFound(format!("library '{}' not in registry", req.library)))?;

    let chosen: Vec<&crate::library::RegistryBlockFile> = if req.blocks.is_empty() {
        lib.blocks.iter().collect()
    } else {
        req.blocks
            .iter()
            .map(|want| {
                lib.blocks.iter().find(|b| &b.file == want).ok_or_else(|| {
                    ApiError::BadRequest(format!(
                        "library '{}' has no block file '{want}'",
                        lib.name
                    ))
                })
            })
            .collect::<Result<_, _>>()?
    };

    // Read every source up-front so an unreadable registry file fails
    // the request before the project is half-written.
    let mut files = Vec::with_capacity(chosen.len());
    for block in &chosen {
        let content = fs::read_to_string(&block.path).map_err(|e| {
            ApiError::Internal(format!("reading registry block '{}': {e}", block.file))
        })?;
        files.push((block.file.clone(), content));
    }

    let imported = with_project(&state, &project, |store| {
        for (file, content) in &files {
            store.write_library_file(&lib.name, file, content)?;
        }
        store.set_library_entry(&lib.name, &lib.version)?;
        Ok(files.iter().map(|(f, _)| f.clone()).collect::<Vec<_>>())
    })?;
    emit_mutation(
        &state,
        &project,
        topic::PROJECT,
        MutationDetail::LibraryImported {
            name: lib.name.clone(),
        },
    );
    Ok(Json(ImportLibraryResponse {
        library: lib.name,
        version: lib.version,
        imported,
    }))
}

/// GET /api/device-catalog — validated device templates from
/// `<library-dir>/devices/` (see library/devices/README.md).
pub async fn list_device_catalog(
    State(state): State<AppState>,
) -> Json<Vec<crate::catalog::CatalogEntry>> {
    Json(crate::catalog::scan(state.library_dir.as_deref()))
}

#[derive(Debug, Deserialize)]
pub struct CatalogMatchQuery {
    pub vendor_id: u32,
    pub product_id: u32,
}

/// GET /api/device-catalog/match?vendor_id=&product_id= — resolve a
/// discovered slave's identity to a catalog template, so the IDE /
/// agent can pre-fill a device from a bus scan instead of hand-typing
/// PDI offsets. 404 when the identity isn't in the catalog.
pub async fn match_device_catalog(
    State(state): State<AppState>,
    Query(q): Query<CatalogMatchQuery>,
) -> Result<Json<crate::catalog::CatalogEntry>, ApiError> {
    let entries = crate::catalog::scan(state.library_dir.as_deref());
    crate::catalog::match_identity(&entries, q.vendor_id, q.product_id)
        .cloned()
        .map(Json)
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "no catalog entry for vendor_id={} product_id={}",
                q.vendor_id, q.product_id
            ))
        })
}

/// DELETE /api/library/{name} — drop `pous/lib/<name>/` and the
/// project.toml entry. Idempotent on the directory.
pub async fn remove_library(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<RunResponse>, ApiError> {
    with_project(&state, &project, |store| {
        store.remove_library_dir(&name)?;
        store.remove_library_entry(&name).map_err(Into::into)
    })?;
    emit_mutation(
        &state,
        &project,
        topic::PROJECT,
        MutationDetail::LibraryRemoved { name },
    );
    Ok(Json(RunResponse { ok: true }))
}

/// Variables declared inside any POU in this file. Empty list if parse
/// fails — handy for mid-typing editor calls.
pub async fn pou_variables(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(path): AxumPath<String>,
) -> Result<Json<Vec<VariableInfo>>, ApiError> {
    let source = with_project(&state, &project, |store| {
        store.read_pou_source(&path).map_err(Into::into)
    })?;
    Ok(Json(ironplc_bridge::extract_variables(&source)))
}

// ============================================================
//  Devices
// ============================================================

// ============================================================
//  Device describe — deterministic per-device agent reference
// ============================================================

/// One bound variable in the device manifest: the iomap row plus the
/// optional engineering metadata declared on it.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct DescribedBinding {
    pub application: String,
    pub variable: String,
    pub direction: project::Direction,
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub description: Option<String>,
}

/// The deterministic per-device manifest — IA2's scheme-agnostic answer
/// to "auto-generated agent reference file" (ADR-0002 Decision 2).
/// Derived from project truth only; no timestamps, so identical project
/// state produces byte-identical output.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct DeviceDescription {
    pub device: String,
    pub protocol: String,
    /// Device config as JSON with every credential-pattern key (see
    /// `SECRET_KEY_PATTERNS`) redacted.
    #[ts(type = "Record<string, unknown>")]
    pub config: serde_json::Value,
    /// Iomap bindings onto this device, sorted by variable.
    pub bindings: Vec<DescribedBinding>,
    /// Alarm definitions reading this device's bound variables (bare or
    /// instance-qualified, matching the snapshot names the alarm engine
    /// sees), by id.
    pub alarms: Vec<project::AlarmDef>,
    /// The project's write mode plus the governance rules that name any
    /// of this device's bound variables (bare or instance-qualified).
    pub write_mode: project::WriteMode,
    pub write_rules: Vec<project::WriteRule>,
}

/// Credential key patterns for `redact_secrets`: an object key that
/// case-insensitively CONTAINS one of these has its value replaced.
/// New protocol configs MUST keep their credential keys inside this
/// pattern set (`password`, `mqtt_token`, `client_secret`, …) — a
/// credential field whose name matches none of these leaks through
/// `/describe` into agent context.
const SECRET_KEY_PATTERNS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "credential",
    "api_key",
    "apikey",
    "private_key",
];

/// Redact secrets recursively: any object key matching
/// `SECRET_KEY_PATTERNS` has its value replaced with a visible
/// placeholder — never silent omission. The manifest is meant to be
/// pasted into agent context — credentials never leave the box.
fn redact_secrets(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                let key = k.to_ascii_lowercase();
                if SECRET_KEY_PATTERNS.iter().any(|p| key.contains(p)) {
                    *val = serde_json::Value::String("***".into());
                } else {
                    redact_secrets(val);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_secrets),
        _ => {}
    }
}

fn build_device_description(
    store: &project::ProjectStore,
    name: &str,
) -> Result<DeviceDescription, ApiError> {
    let device = store.read_device(name)?;
    let mut config = serde_json::to_value(&device.config)
        .map_err(|e| ApiError::Internal(format!("device config serializes: {e}")))?;
    redact_secrets(&mut config);

    let iomap = store.read_iomap()?;
    let mut bindings: Vec<DescribedBinding> = iomap
        .mappings
        .iter()
        .filter(|m| m.device == name)
        .map(|m| DescribedBinding {
            application: m.application.clone(),
            variable: m.variable.clone(),
            direction: m.direction,
            channel: m.channel.clone(),
            unit: m.unit.clone(),
            min: m.min,
            max: m.max,
            description: m.description.clone(),
        })
        .collect();
    bindings.sort_by(|a, b| {
        a.variable
            .cmp(&b.variable)
            .then(a.application.cmp(&b.application))
            .then(a.channel.cmp(&b.channel))
    });

    // Alarm defs read SNAPSHOT names, which are instance-qualified
    // (`main.level`) when a variable name is shared across program
    // units — accept both forms, like the write_rules filter below.
    // Matching stays exact-case to mirror the alarm engine's exact
    // snapshot-name matching.
    let bound: std::collections::HashSet<&str> =
        bindings.iter().map(|b| b.variable.as_str()).collect();
    let bound_qualified: std::collections::HashSet<String> = bindings
        .iter()
        .map(|b| format!("{}.{}", b.application, b.variable))
        .collect();
    let mut alarms: Vec<project::AlarmDef> = store
        .read_alarms()?
        .alarms
        .into_iter()
        .filter(|a| bound.contains(a.variable.as_str()) || bound_qualified.contains(&a.variable))
        .collect();
    alarms.sort_by(|a, b| a.id.cmp(&b.id));

    // Invalid configuration must not look like an empty or stale policy.
    // Describe and the next run use the same on-disk governance.
    let governance = fresh_governance(store)?;
    let mut write_rules: Vec<project::WriteRule> = governance
        .rules
        .iter()
        .filter(|r| {
            let key = r.variable.to_lowercase();
            bindings.iter().any(|b| {
                key == b.variable.to_lowercase()
                    || key
                        == format!(
                            "{}.{}",
                            b.application.to_lowercase(),
                            b.variable.to_lowercase()
                        )
            })
        })
        .cloned()
        .collect();
    write_rules.sort_by(|a, b| a.variable.cmp(&b.variable));

    Ok(DeviceDescription {
        device: device.name.clone(),
        protocol: format!("{:?}", device.config.protocol()).to_lowercase(),
        config,
        bindings,
        alarms,
        write_mode: governance.write_mode,
        write_rules,
    })
}

pub async fn describe_device(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<DeviceDescription>, ApiError> {
    with_project(&state, &project, |store| {
        build_device_description(store, &name)
    })
    .map(Json)
}

pub async fn get_device(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Device>, ApiError> {
    with_project(&state, &project, |store| {
        store.read_device(&name).map_err(Into::into)
    })
    .map(Json)
}

pub async fn create_device(
    State(state): State<AppState>,
    project: ProjectName,
    Json(req): Json<CreateDeviceRequest>,
) -> Result<Json<Device>, ApiError> {
    let device = with_project(&state, &project, |store| {
        store
            .create_device(&req.name, req.protocol)
            .map_err(Into::into)
    })?;
    emit_mutation(
        &state,
        &project,
        topic::DEVICES,
        MutationDetail::DeviceUpserted {
            name: device.name.clone(),
        },
    );
    Ok(Json(device))
}

pub async fn update_device(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
    Json(device): Json<Device>,
) -> Result<Json<RunResponse>, ApiError> {
    if device.name != name {
        return Err(ApiError::BadRequest(format!(
            "path name '{name}' does not match body name '{}'",
            device.name
        )));
    }
    with_project(&state, &project, |store| {
        store.write_device(&device).map_err(Into::into)
    })?;
    emit_mutation(
        &state,
        &project,
        topic::DEVICES,
        MutationDetail::DeviceUpserted { name: name.clone() },
    );
    emit_mutation(
        &state,
        &project,
        topic::device(&name),
        MutationDetail::DeviceUpserted { name },
    );
    Ok(Json(RunResponse { ok: true }))
}

/// Body for `POST /api/devices/{name}/esi-assemble`: the module idents the
/// coupler reports (object `0xF050`), in slot order. Hex or decimal on the
/// CLI side; plain `u32` here.
#[derive(Debug, Deserialize)]
pub struct EsiAssembleRequest {
    pub detected: Vec<u32>,
}

/// Assemble a modular EtherCAT coupler's channels from its ESI file +
/// detected modules, and write them onto the device.
///
/// The device must be EtherCAT with `bringup = esi_modular`; its `esi_path`
/// is read from the project directory, the process image is assembled (see
/// the `esi` crate), and the resulting channels REPLACE the device's
/// channel list — the ESI is authoritative for a modular coupler, so the
/// channels are derived, not hand-entered. Returns the updated device.
pub async fn esi_assemble_device(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
    Json(req): Json<EsiAssembleRequest>,
) -> Result<Json<Device>, ApiError> {
    let updated = with_project(&state, &project, |store| {
        let mut device = store.read_device(&name)?;

        let cfg = match &device.config {
            ProtocolConfig::Ethercat(c) => c,
            _ => {
                return Err(ApiError::BadRequest(format!(
                    "device '{name}' is not an EtherCAT device"
                )))
            }
        };
        let esi_path = match &cfg.bringup {
            EthercatBringup::EsiModular { esi_path } => esi_path.clone(),
            EthercatBringup::Auto => {
                return Err(ApiError::BadRequest(format!(
                    "device '{name}' bring-up is `auto`; set it to `esi_modular` first"
                )))
            }
        };
        if esi_path.trim().is_empty() {
            return Err(ApiError::BadRequest(format!(
                "device '{name}' has no ESI file path set"
            )));
        }

        // Resolve the project-relative ESI path under the project root,
        // rejecting traversal outside it.
        let full = store.root().join(&esi_path);
        if !full.starts_with(store.root()) {
            return Err(ApiError::BadRequest(format!(
                "ESI path '{esi_path}' escapes the project directory"
            )));
        }
        let xml = std::fs::read_to_string(&full)
            .map_err(|e| ApiError::BadRequest(format!("cannot read ESI file '{esi_path}': {e}")))?;

        let channels = iomap_ethercat::assemble_channels(&xml, &req.detected)
            .map_err(|e| ApiError::BadRequest(format!("ESI assembly failed: {e}")))?;

        if let ProtocolConfig::Ethercat(c) = &mut device.config {
            c.channels = channels;
        }
        store.write_device(&device)?;
        Ok(device)
    })?;

    emit_mutation(
        &state,
        &project,
        topic::DEVICES,
        MutationDetail::DeviceUpserted { name: name.clone() },
    );
    emit_mutation(
        &state,
        &project,
        topic::device(&name),
        MutationDetail::DeviceUpserted { name },
    );
    Ok(Json(updated))
}

#[derive(Debug, Deserialize, TS)]
#[ts(export)]
pub struct OpcuaBrowseRequest {
    /// NodeId to browse under; omitted/empty = ObjectsFolder.
    #[serde(default)]
    pub node_id: Option<String>,
}

/// One row of an address-space browse — `OpcuaDeviceEditor`'s picker and
/// `cs device opcua-browse` both consume this.
#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct OpcuaBrowseNode {
    pub node_id: String,
    pub display_name: String,
    /// `Object` / `Variable` / `Method` / …
    pub node_class: String,
    /// UA scalar type name for Variables (`Int32`, `Double`, …) when
    /// the server reports a standard one.
    pub data_type: Option<String>,
    /// The channel `data_type` that fits — one-click adoption into an
    /// `OpcuaChannel`.
    pub suggested_type: Option<project::OpcuaDataType>,
}

/// Live-browse the device's OPC UA server so users pick NodeIds from
/// the actual address space instead of retyping them from UaExpert.
/// Connects with the device's own endpoint/auth config, browses one
/// level, disconnects.
pub async fn opcua_browse_device(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
    Json(req): Json<OpcuaBrowseRequest>,
) -> Result<Json<Vec<OpcuaBrowseNode>>, ApiError> {
    let config = with_project(&state, &project, |store| {
        let device = store.read_device(&name)?;
        match device.config {
            ProtocolConfig::Opcua(c) => Ok(c),
            _ => Err(ApiError::BadRequest(format!(
                "device '{name}' is not an OPC UA device"
            ))),
        }
    })?;
    let from = req.node_id.as_deref().filter(|s| !s.trim().is_empty());
    let nodes = iomap_opcua::browse_endpoint(&config, from)
        .await
        .map_err(|e| ApiError::BadRequest(format!("browse failed: {e}")))?;
    Ok(Json(
        nodes
            .into_iter()
            .map(|n| OpcuaBrowseNode {
                node_id: n.node_id,
                display_name: n.display_name,
                node_class: n.node_class,
                data_type: n.data_type,
                suggested_type: n.suggested_channel_type,
            })
            .collect(),
    ))
}

pub async fn delete_device(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<RunResponse>, ApiError> {
    with_project(&state, &project, |store| {
        store.delete_device(&name).map_err(Into::into)
    })?;
    emit_mutation(
        &state,
        &project,
        topic::DEVICES,
        MutationDetail::DeviceDeleted { name },
    );
    Ok(Json(RunResponse { ok: true }))
}

pub async fn create_device_folder(
    State(state): State<AppState>,
    project: ProjectName,
    Json(req): Json<CreateFolderRequest>,
) -> Result<Json<RunResponse>, ApiError> {
    with_project(&state, &project, |store| {
        store.create_device_folder(&req.path).map_err(Into::into)
    })?;
    emit_mutation(
        &state,
        &project,
        topic::DEVICES,
        MutationDetail::DeviceFolderCreated { path: req.path },
    );
    Ok(Json(RunResponse { ok: true }))
}

// ============================================================
//  Edges (deploy targets)
// ============================================================

pub async fn get_edge(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Edge>, ApiError> {
    with_project(&state, &project, |store| {
        store.read_edge(&name).map_err(Into::into)
    })
    .map(Json)
}

pub async fn create_edge(
    State(state): State<AppState>,
    project: ProjectName,
    Json(req): Json<CreateEdgeRequest>,
) -> Result<Json<Edge>, ApiError> {
    let edge = with_project(&state, &project, |store| {
        store.create_edge(&req.name, &req.host).map_err(Into::into)
    })?;
    emit_mutation(
        &state,
        &project,
        topic::EDGES,
        MutationDetail::EdgeUpserted {
            name: edge.name.clone(),
        },
    );
    Ok(Json(edge))
}

pub async fn update_edge(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
    Json(edge): Json<Edge>,
) -> Result<Json<RunResponse>, ApiError> {
    if edge.name != name {
        return Err(ApiError::BadRequest(format!(
            "path name '{name}' does not match body name '{}'",
            edge.name
        )));
    }
    with_project(&state, &project, |store| {
        store.write_edge(&edge).map_err(Into::into)
    })?;
    emit_mutation(
        &state,
        &project,
        topic::EDGES,
        MutationDetail::EdgeUpserted { name: name.clone() },
    );
    emit_mutation(
        &state,
        &project,
        topic::edge(&name),
        MutationDetail::EdgeUpserted { name },
    );
    Ok(Json(RunResponse { ok: true }))
}

pub async fn delete_edge(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<RunResponse>, ApiError> {
    with_project(&state, &project, |store| {
        store.delete_edge(&name).map_err(Into::into)
    })?;
    let project_name = resolve_project_name(&state, &project)?;
    // Drop any active attachment for this edge so we don't leak ssh procs.
    state.attachments.detach(&project_name, &name);
    emit_mutation(
        &state,
        &project,
        topic::EDGES,
        MutationDetail::EdgeDeleted { name },
    );
    Ok(Json(RunResponse { ok: true }))
}

pub async fn create_edge_folder(
    State(state): State<AppState>,
    project: ProjectName,
    Json(req): Json<CreateFolderRequest>,
) -> Result<Json<RunResponse>, ApiError> {
    with_project(&state, &project, |store| {
        store.create_edge_folder(&req.path).map_err(Into::into)
    })?;
    emit_mutation(
        &state,
        &project,
        topic::EDGES,
        MutationDetail::EdgeFolderCreated { path: req.path },
    );
    Ok(Json(RunResponse { ok: true }))
}

pub async fn probe_edge_route(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<EdgeProbe>, ApiError> {
    let edge = with_project(&state, &project, |store| {
        store.read_edge(&name).map_err(Into::into)
    })?;
    Ok(Json(probe_edge(&edge).await))
}

#[derive(serde::Deserialize)]
pub struct LogsQuery {
    pub tail: Option<usize>,
}

/// Pull the last `tail` (default 200, capped 2000) log lines from the
/// edge runtime over ssh. Returns the runtime's `{ "lines": [...] }`
/// body so the IDE/CLI can show edge-side truth (EtherCAT discovery,
/// bus health, connect failures) that otherwise only hits journald.
pub async fn logs_edge_route(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
    axum::extract::Query(q): axum::extract::Query<LogsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let edge = with_project(&state, &project, |store| {
        store.read_edge(&name).map_err(Into::into)
    })?;
    let tail = q.tail.unwrap_or(200).min(2000);
    crate::edges::fetch_edge_json(&edge, &format!("/logs?tail={tail}"))
        .await
        .map(Json)
        .map_err(ApiError::Internal)
}

/// Pull per-device connect status + discovered EtherCAT topology from
/// the edge runtime over ssh. Backs `cs get edges/<n>/scan` / the IDE's discovery
/// pane so PDO maps can be authored against the real bus.
pub async fn discover_edge_route(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let edge = with_project(&state, &project, |store| {
        store.read_edge(&name).map_err(Into::into)
    })?;
    crate::edges::fetch_edge_json(&edge, "/discover")
        .await
        .map(Json)
        .map_err(ApiError::Internal)
}

/// Edge interfaces / serial ports / arch (for authoring device configs
/// against real edge facts). Backs `cs get edges/<n>/system`.
pub async fn system_edge_route(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let edge = with_project(&state, &project, |store| {
        store.read_edge(&name).map_err(Into::into)
    })?;
    crate::edges::fetch_edge_json(&edge, "/system")
        .await
        .map(Json)
        .map_err(ApiError::Internal)
}

/// Edge runtime `/status` (project + scan count + debug mode/forces +
/// last snapshot with variable types). Backs `cs runtime --edge` status
/// reads and force/write value-packing.
pub async fn status_edge_route(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let edge = with_project(&state, &project, |store| {
        store.read_edge(&name).map_err(Into::into)
    })?;
    crate::edges::fetch_edge_json(&edge, "/status")
        .await
        .map(Json)
        .map_err(ApiError::Internal)
}

pub async fn deploy_edge_route(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<DeployReport>, ApiError> {
    let (edge, project_dir) = with_project(&state, &project, |store| {
        let edge = store.read_edge(&name).map_err(crate::error::project_err)?;
        Ok((edge, store.root().to_path_buf()))
    })?;
    let runtime_binary = find_runtime_binary();
    match deploy_to_edge(
        &edge,
        &project_dir,
        runtime_binary.as_deref(),
        state.web_dist.as_deref(),
    )
    .await
    {
        Ok(report) => Ok(Json(report)),
        // A remote script failure is a deploy OUTCOME, not an infra
        // fault: return it as ok:false with the full log so `cs deploy`
        // can print the story and exit 1 (its documented remote-failure
        // code). 5xx stays reserved for local/transport faults.
        Err(crate::edges::DeployError::Remote(code, log)) => Ok(Json(DeployReport {
            ok: false,
            version: String::new(),
            log: format!("remote deploy script exited with status {code}\n{log}"),
            warning: None,
        })),
        Err(e) => Err(ApiError::Internal(e.to_string())),
    }
}

pub async fn attach_edge_route(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<AttachInfo>, ApiError> {
    let edge = with_project(&state, &project, |store| {
        store.read_edge(&name).map_err(Into::into)
    })?;
    let project_name = resolve_project_name(&state, &project)?;
    let info = attach_edge(&project_name, &edge, &state.attachments)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    emit_mutation(
        &state,
        &project,
        topic::edge(&name),
        MutationDetail::EdgeAttached {
            name,
            local_port: info.local_port,
        },
    );
    Ok(Json(info))
}

pub async fn detach_edge_route(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
) -> Json<RunResponse> {
    let project_name = match resolve_project_name(&state, &project) {
        Ok(n) => n,
        Err(_) => {
            // No project resolved — nothing to detach scoped to one
            // project. Silent no-op keeps the API idempotent.
            return Json(RunResponse { ok: true });
        }
    };
    state.attachments.detach(&project_name, &name);
    emit_mutation(
        &state,
        &project,
        topic::edge(&name),
        MutationDetail::EdgeDetached { name },
    );
    Json(RunResponse { ok: true })
}

/// Convenience: the IDE keeps showing the local /api/events stream by
/// default; switching to an attached edge means changing the SSE source
/// URL on the client. Rather than build a streaming proxy, we tell the
/// client where to point — `attach` already returns the local port.
///
/// This endpoint lets the UI ask "is anything attached right now, and at
/// what port?" on page load (so a detach across reload is visible).
#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct AttachmentStatus {
    pub attached: bool,
    pub local_port: Option<u16>,
}

pub async fn attachment_status(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
) -> Json<AttachmentStatus> {
    let project_name = resolve_project_name(&state, &project).ok();
    let port = project_name
        .as_deref()
        .and_then(|pname| state.attachments.current_port(pname, &name));
    Json(AttachmentStatus {
        attached: port.is_some(),
        local_port: port,
    })
}

/// Look for a freshly-built runtime binary on the dev machine. Heuristic:
/// release first, then debug, then env var override. Returns None if no
/// binary is found — deploy falls back to "reuse current/runtime".
fn find_runtime_binary() -> Option<std::path::PathBuf> {
    // Explicit override wins — but only if it's a Linux ELF (see below).
    if let Ok(p) = std::env::var("IA2_RUNTIME_BIN") {
        let p = std::path::PathBuf::from(p);
        if p.exists() {
            if is_linux_elf(&p) {
                return Some(p);
            }
            tracing::warn!(
                path = %p.display(),
                "IA2_RUNTIME_BIN is not a Linux ELF — not shipping it; the edge will reuse its \
                 existing runtime binary"
            );
        }
    }
    let exe = std::env::current_exe().ok()?;
    let parent = exe.parent()?.to_path_buf();
    // Sibling of `server` in the target dir — but ONLY ship a Linux ELF.
    // Edges are Linux; on a macOS dev machine the local `ia2-runtime` is a
    // Mach-O binary that would brick the edge on restart (exec format
    // error). When no suitable binary is found we ship nothing and the
    // deploy reuses the edge's existing `current/runtime` (carry-forward).
    [
        parent.join("ia2-runtime"),
        parent.parent()?.join("release").join("ia2-runtime"),
    ]
    .into_iter()
    .find(|c| c.exists() && is_linux_elf(c))
}

/// True if `path` begins with the ELF magic (`\x7fELF`). We refuse to ship
/// a non-ELF (e.g. macOS Mach-O) runtime to a Linux edge.
fn is_linux_elf(path: &std::path::Path) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .map(|_| magic == [0x7f, b'E', b'L', b'F'])
        .unwrap_or(false)
}

// ============================================================
//  IO Mapping
// ============================================================

pub async fn get_iomap(
    State(state): State<AppState>,
    project: ProjectName,
) -> Result<Json<IoMap>, ApiError> {
    with_project(&state, &project, |store| {
        store.read_iomap().map_err(Into::into)
    })
    .map(Json)
}

pub async fn put_iomap(
    State(state): State<AppState>,
    project: ProjectName,
    Json(iomap): Json<IoMap>,
) -> Result<Json<RunResponse>, ApiError> {
    with_project(&state, &project, |store| {
        store.write_iomap(&iomap).map_err(Into::into)
    })?;
    emit_mutation(&state, &project, topic::IOMAP, MutationDetail::IoMapChanged);
    Ok(Json(RunResponse { ok: true }))
}

// ============================================================
//  Northbound (edge -> platform publishing config)
// ============================================================

pub async fn get_northbound(
    State(state): State<AppState>,
    project: ProjectName,
) -> Result<Json<project::NorthboundConfig>, ApiError> {
    with_project(&state, &project, |store| {
        store.read_northbound().map_err(Into::into)
    })
    .map(Json)
}

pub async fn put_northbound(
    State(state): State<AppState>,
    project: ProjectName,
    Json(config): Json<project::NorthboundConfig>,
) -> Result<Json<RunResponse>, ApiError> {
    with_project(&state, &project, |store| {
        store.write_northbound(&config).map_err(Into::into)
    })?;
    Ok(Json(RunResponse { ok: true }))
}

// ============================================================
//  Alarms (alarms.toml — declarative alarm definitions)
// ============================================================

pub async fn get_alarms(
    State(state): State<AppState>,
    project: ProjectName,
) -> Result<Json<project::AlarmConfig>, ApiError> {
    with_project(&state, &project, |store| {
        store.read_alarms().map_err(Into::into)
    })
    .map(Json)
}

/// Replace alarms.toml. Rejects duplicate ids and numeric conditions
/// without a limit — the engine would otherwise treat a missing limit
/// as 0.0 and "work" misleadingly. Definitions apply on the NEXT run
/// (the engine is rebuilt by /api/run).
pub async fn put_alarms(
    State(state): State<AppState>,
    project: ProjectName,
    Json(config): Json<project::AlarmConfig>,
) -> Result<Json<RunResponse>, ApiError> {
    let mut seen = std::collections::HashSet::new();
    for def in &config.alarms {
        if !seen.insert(def.id.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "duplicate alarm id '{}'",
                def.id
            )));
        }
        let needs_limit = !matches!(
            def.condition,
            project::AlarmCondition::IsTrue | project::AlarmCondition::IsFalse
        );
        if needs_limit && def.limit.is_none() {
            return Err(ApiError::BadRequest(format!(
                "alarm '{}': condition {:?} needs a `limit`",
                def.id, def.condition
            )));
        }
    }
    with_project(&state, &project, |store| {
        store.write_alarms(&config).map_err(Into::into)
    })?;
    Ok(Json(RunResponse { ok: true }))
}

// ============================================================
//  Tasks (project-level scheduling)
// ============================================================

pub async fn get_tasks(
    State(state): State<AppState>,
    project: ProjectName,
) -> Result<Json<Tasks>, ApiError> {
    with_project(&state, &project, |store| {
        Ok(store.read_tasks()?.unwrap_or_default())
    })
    .map(Json)
}

pub async fn put_tasks(
    State(state): State<AppState>,
    project: ProjectName,
    Json(tasks): Json<Tasks>,
) -> Result<Json<RunResponse>, ApiError> {
    with_project(&state, &project, |store| {
        store.write_tasks(&tasks).map_err(Into::into)
    })?;
    emit_mutation(&state, &project, topic::TASKS, MutationDetail::TasksChanged);
    Ok(Json(RunResponse { ok: true }))
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct MigrationResponse {
    pub migrated: bool,
    pub tasks_count: usize,
    pub programs_count: usize,
    pub pous_modified: Vec<String>,
}

/// Promote a legacy project (inline CONFIGURATION blocks in each POU) to
/// the new project-level `tasks.toml`. Idempotent — running on an
/// already-migrated project is a no-op.
pub async fn migrate_tasks(
    State(state): State<AppState>,
    project: ProjectName,
) -> Result<Json<MigrationResponse>, ApiError> {
    let report = with_project(&state, &project, |store| {
        store.migrate_tasks().map_err(Into::into)
    })?;
    let resp = match report {
        MigrationReport::Skipped => MigrationResponse {
            migrated: false,
            tasks_count: 0,
            programs_count: 0,
            pous_modified: vec![],
        },
        MigrationReport::Migrated {
            tasks_count,
            programs_count,
            pous_modified,
        } => MigrationResponse {
            migrated: true,
            tasks_count,
            programs_count,
            pous_modified,
        },
    };
    if resp.migrated {
        // Migration touches both task config and POU sources, so
        // invalidate the broad project key too. The targeted topics
        // keep individual panes responsive.
        emit_mutation(
            &state,
            &project,
            topic::TASKS,
            MutationDetail::TasksMigrated,
        );
        emit_mutation(
            &state,
            &project,
            topic::PROJECT,
            MutationDetail::TasksMigrated,
        );
    }
    Ok(Json(resp))
}

// ============================================================
//  Compile / run / stream
// ============================================================

/// Query parameters for `POST /api/check`. `language` defaults to ST
/// for back-compat with callers that don't know about LD yet (they POST
/// a plain text/plain body and get ST-mode behaviour).
#[derive(Debug, Default, Deserialize)]
pub struct CheckQuery {
    #[serde(default)]
    pub language: Option<String>,
    /// Store slug the checked buffer was loaded from (e.g.
    /// `lib/process-control/fb_pid`). Lets the project-aware check
    /// exclude the buffer's own on-disk copy from the analysis context
    /// so it doesn't double-declare its POUs.
    #[serde(default)]
    pub path: Option<String>,
}

/// Symbol extraction for the editor's hover / completion providers
/// and the `cs symbols` CLI subcommand. Takes the raw source on the
/// wire (so unsaved edits work) + a `language` query param identical
/// to `/api/check`.
///
/// Returns `[]` on parse failure — the front-end falls back to its
/// hard-coded keyword / FB-type lists.
pub async fn symbols(
    Query(q): Query<CheckQuery>,
    body: String,
) -> Json<Vec<ironplc_bridge::VariableInfo>> {
    let language = match q
        .language
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("ld") => PouLanguage::Ld,
        Some("fbd") => PouLanguage::Fbd,
        Some("sfc") => PouLanguage::Sfc,
        _ => PouLanguage::St,
    };
    Json(ironplc_bridge::extract_symbols(&body, language))
}

pub async fn check(
    State(state): State<AppState>,
    project: ProjectName,
    Query(q): Query<CheckQuery>,
    body: String,
) -> Json<Vec<CheckDiagnostic>> {
    // The frontend posts `?language=ld` when the editor source on the
    // wire is `.ld.json`. Anything else (or absent) is treated as ST,
    // which is the historical behaviour. Keeping `check` and
    // `check_pou_source` as one HTTP route means the editor doesn't
    // need a second LSP-style channel.
    let language = match q
        .language
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("ld") => PouLanguage::Ld,
        Some("fbd") => PouLanguage::Fbd,
        Some("sfc") => PouLanguage::Sfc,
        _ => PouLanguage::St,
    };
    // Check against the open project when there is one, so FBs declared
    // in sibling files resolve (the editor-squiggle counterpart of the
    // project compile). No open project — agents checking a loose
    // snippet — falls back to the isolated check. Either way the parse
    // runs on the blocking pool, unlocked: this fires on every editor
    // debounce and re-parses all sibling POUs.
    let store = owned_store(&state, &project).ok();
    let path = q.path.clone();
    let diags = tokio::task::spawn_blocking(move || match &store {
        Some(store) => {
            ironplc_bridge::check_pou_in_project(store, &body, language, path.as_deref())
        }
        None => ironplc_bridge::check_pou_source(&body, language),
    })
    .await
    .unwrap_or_default();
    Json(diags)
}

/// Compile the whole project (every POU + tasks.toml-synthesized
/// CONFIGURATION) without spawning. Returns diagnostics from the parser
/// + analyzer + codegen pipeline. Empty list = clean; safe to Run.
///
/// Agent use-case: validate before Deploy. Cheaper than POST /api/run
/// because no bridge thread or devices are touched.
pub async fn validate_project(
    State(state): State<AppState>,
    project: ProjectName,
) -> Result<Json<Vec<CheckDiagnostic>>, ApiError> {
    // Whole-project compile — run it unlocked on the blocking pool, same
    // rationale as the run handler.
    let store = owned_store(&state, &project)?;
    tokio::task::spawn_blocking(move || -> Result<Vec<CheckDiagnostic>, ApiError> {
        let store = &store;
        let diag = |code: &str, message: String| CheckDiagnostic {
            severity: "error".into(),
            code: code.into(),
            message,
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 1,
            context: vec![],
            related: vec![],
            explanation: None,
            ld_location: None,
            fbd_location: None,
            sfc_location: None,
        };
        let tasks = store.read_tasks()?.unwrap_or_default();
        let mut out = Vec::new();
        // ADR-0001 gate — same bridge implementation the run path
        // enforces, surfaced at validate time so agents see it before a
        // run attempt.
        if let Err(msg) = ironplc_bridge::reject_shared_globals(store, &tasks) {
            out.push(diag("project-validate-globals", msg));
        }
        // Compile exactly what the run path would execute: one container
        // per PROGRAM instance. Convert any error into a single
        // CheckDiagnostic for the agent — full per-line diagnostics live
        // in /api/check for POU-level editing. The failure surface is:
        //  - missing tasks.toml programs / unknown PROGRAM names
        //  - parser / analyzer / codegen diagnostics
        //  - file read failures
        if let Err(e) = ironplc_bridge::compile_project_units(store, &tasks) {
            out.push(diag("project-validate", e.to_string()));
        }
        // Static iomap lint: device exists, channel exists, direction
        // matches, no duplicate writers. Same data the spawn path would
        // trip over at runtime — surfaced here so a bad mapping fails
        // validate, not the first scan.
        let iomap = store.read_iomap()?;
        let devices = store.list_devices()?;
        // Static device lint, BEFORE the iomap lint reasons about those same
        // documents: a device that declares one channel name twice makes the
        // two disagree — the linter resolves the name to the first match, the
        // adapter keeps the last — so report the broken document first.
        for issue in project::validate_devices(&devices) {
            out.push(diag(
                "device-validate",
                format!("device '{}': {}", issue.device, issue.message),
            ));
        }
        for issue in project::validate_iomap(&iomap, &devices) {
            let code = match issue.severity {
                project::IomapIssueSeverity::Error => "iomap-validate",
                project::IomapIssueSeverity::Warning => "iomap-validate-warning",
            };
            let mut d = diag(
                code,
                format!("mapping #{}: {}", issue.mapping_index + 1, issue.message),
            );
            if issue.severity == project::IomapIssueSeverity::Warning {
                d.severity = "warning".into();
            }
            out.push(d);
        }
        // Static alarm lint: does each definition watch a variable that
        // exists? The engine skips an unmatched one quietly, so without this
        // a typo'd or renamed variable leaves a calm, never-raised entry in
        // /alarms that claims coverage it does not have.
        {
            let alarms = store.read_alarms()?;
            let mut sources = Vec::new();
            for path in store.list_pou_paths()? {
                sources.push(store.read_pou_source(&path)?);
            }
            let declared_names: Vec<String> = sources
                .iter()
                .flat_map(|src| ironplc_bridge::extract_variables(src))
                .map(|v| v.name)
                .collect();
            let declared: std::collections::HashSet<&str> =
                declared_names.iter().map(String::as_str).collect();
            let instances: Vec<String> =
                tasks.programs.iter().map(|p| p.instance.clone()).collect();
            for issue in project::validate_alarms(&alarms.alarms, &declared, &instances) {
                out.push(diag(
                    "alarms-validate",
                    format!("alarm #{}: {}", issue.alarm_index + 1, issue.message),
                ));
            }
        }
        // HMI screens fold into the same diagnostics story (the
        // docs/hmi-design.md contract): structural issues, unknown
        // variables, read-only write targets, dangling nav targets. A
        // screen that no longer parses is itself a finding — screens are
        // plain files a human can break by hand.
        for slug in store.list_hmis()? {
            match store.read_hmi(&slug) {
                Ok(doc) => {
                    for issue in crate::hmi_routes::check_hmi_doc(store, &doc)? {
                        let code = if issue.severity == "error" {
                            "hmi-validate"
                        } else {
                            "hmi-validate-warning"
                        };
                        let message = match issue.node_id.as_deref() {
                            Some(n) => format!("hmi '{slug}' [{n}]: {}", issue.message),
                            None => format!("hmi '{slug}': {}", issue.message),
                        };
                        let mut d = diag(code, message);
                        d.severity = issue.severity;
                        out.push(d);
                    }
                }
                Err(e) => out.push(diag("hmi-validate", e.to_string())),
            }
        }
        Ok(out)
    })
    .await
    .map_err(|e| ApiError::Internal(format!("validate task failed: {e}")))?
    .map(Json)
}

// ============================================================
//  Cross-POU declaration index (real schedulable POU names)
// ============================================================

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct PouInProject {
    /// File the declaration lives in — project-relative POU path
    /// (e.g. `pid_loops/temperature_pid`).
    pub file_path: String,
    /// IEC POU identifier — what `PROGRAM <inst> WITH <task> : <name>`
    /// references in a CONFIGURATION block.
    pub name: String,
    #[serde(rename = "type")]
    pub type_: PouType,
    pub language: PouLanguage,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct ProjectPous {
    pub pous: Vec<PouInProject>,
}

/// Every IEC POU declared anywhere in the project, parser-driven so it
/// reflects what's actually schedulable. A single `.st` file may declare
/// multiple POUs (FB + PROGRAM + FUNCTION side by side); each appears
/// as its own entry here. Agents and the Tasks pane both use this to
/// populate the "PROGRAM to schedule" dropdown.
pub async fn project_pous(
    State(state): State<AppState>,
    project: ProjectName,
) -> Result<Json<ProjectPous>, ApiError> {
    with_project(&state, &project, |store| {
        let paths = store.list_pou_paths()?;
        let mut out = Vec::new();
        for path in paths {
            let language = store.pou_file_language(&path)?;
            let source = store.read_pou_source(&path)?;
            for d in ironplc_bridge::extract_pou_declarations(&source, language) {
                out.push(PouInProject {
                    file_path: path.clone(),
                    name: d.name,
                    type_: d.type_,
                    language: d.language,
                });
            }
        }
        Ok(ProjectPous { pous: out })
    })
    .map(Json)
}

// ============================================================
//  Cross-POU variable index
// ============================================================

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct ProjectVariable {
    /// POU file the variable was declared in (`pous/<file_path>.st`).
    pub file_path: String,
    pub name: String,
    pub type_name: String,
    pub direction: String,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct ProjectVariables {
    pub variables: Vec<ProjectVariable>,
}

/// All variables declared anywhere in the project, qualified by their
/// owning POU. Used by debug agents to answer "which POU declares
/// `counter`?" / "list every BOOL variable" without round-tripping
/// per-POU.
pub async fn project_variables(
    State(state): State<AppState>,
    project: ProjectName,
) -> Result<Json<ProjectVariables>, ApiError> {
    with_project(&state, &project, |store| {
        let paths = store.list_pou_paths()?;
        let mut out = Vec::new();
        for path in paths {
            let source = store.read_pou_source(&path)?;
            for v in ironplc_bridge::extract_variables(&source) {
                out.push(ProjectVariable {
                    file_path: path.clone(),
                    name: v.name,
                    type_name: v.type_name,
                    direction: v.direction,
                });
            }
        }
        Ok(ProjectVariables { variables: out })
    })
    .map(Json)
}

/// Governance for THIS run, re-read from disk like tasks/iomap/alarms.
/// The registry's store caches the manifest at project-open, so using
/// `store.governance()` here would make a `[governance]` edit invisible
/// until the project is re-opened — failing OPEN when an agent tightens
/// open → allowlist between runs (ADR-0002).
fn fresh_governance(store: &ProjectStore) -> Result<project::WriteGovernance, ApiError> {
    store.read_governance().map_err(|e| match e {
        // An invalid [governance] table is the caller's project.toml to
        // fix — 400, not a 500 the CLI would report as infrastructure.
        e @ project::StoreError::TomlDe(..) => ApiError::BadRequest(e.to_string()),
        e => e.into(),
    })
}

pub async fn run(
    State(state): State<AppState>,
    project: ProjectName,
    headers: axum::http::HeaderMap,
    body: Option<Json<RunRequest>>,
) -> Result<Json<RunResponse>, ApiError> {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    // Starting a program IS driving the plant — an unattributed run
    // flashes the takeover overlay like any other mutating route.
    state.maybe_attribute_external(
        crate::runtime_routes::origin_of(&headers).as_deref(),
        "run".into(),
    );

    // Resolve which project this run targets up front. Multi-project
    // servers route `/api/run` to whichever project the X-IA2-Project
    // header names (falling back to the active one). We capture the
    // name now so the RunningProgram entry can label what's running.
    let project_name = resolve_project_name(&state, &project)?;

    // Three modes (matched in the handler so the bridge stays simple):
    //  - `program: None`             → compile_project_units (reads
    //                                   tasks.toml; one container per
    //                                   PROGRAM instance)
    //  - `program` + `file_path`     → compile_isolated_in_project_full
    //                                   (that file's PROGRAM + siblings as
    //                                   type context)
    //  - `program` only              → compile_project_units with a
    //                                   synthetic single-instance schedule;
    //                                   tasks.toml untouched on disk
    // Clone the store OUT of the registry lock, then compile unlocked on a
    // blocking thread. A full parse→analyze→codegen over every POU takes
    // long enough that holding the global projects mutex for it would
    // stall every other window's requests — and it would pin a tokio
    // worker with CPU-bound work. The store is just {root, manifest};
    // sources are read from disk on demand inside the compile.
    let store = {
        let mut projects_guard = state.projects.lock();
        // Locate the project in the registry; mark it active as a
        // side-effect so subsequent header-less requests follow this
        // window. Error if the named project isn't open.
        if !projects_guard.set_active(&project_name) {
            return Err(ApiError::NoProject);
        }
        projects_guard
            .get(&project_name)
            .ok_or(ApiError::NoProject)?
            .clone()
    };
    let req_program = req.program.clone();
    let req_file_path = req.file_path.clone();
    let (units, device_specs, mappings, alarm_defs, governance) =
        tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
            let store = &store;
            let req = RunRequest {
                program: req_program,
                file_path: req_file_path,
            };

            // Determine the effective Tasks for this run — tasks.toml when
            // running the whole project, synthesised single-program when
            // doing an ad-hoc run from a specific PROGRAM.
            let effective_tasks = match req.program.as_deref() {
                None => store.read_tasks()?.unwrap_or_default(),
                Some(name) => single_program_tasks(name),
            };

            // ADR-0001 gate — one authoritative implementation in the bridge,
            // shared with /api/project/validate and the edge runtime.
            ironplc_bridge::reject_shared_globals(store, &effective_tasks)
                .map_err(ApiError::BadRequest)?;

            let units = match (req.program.as_deref(), req.file_path.as_deref()) {
                (None, _) => {
                    // Whole-project schedule: one unit per tasks.toml
                    // PROGRAM instance, each with its own container and
                    // its own task's interval/priority.
                    ironplc_bridge::compile_project_units(store, &effective_tasks)?
                }
                (Some(name), Some(file_path)) => {
                    // Ad-hoc isolated run: the named file's PROGRAM plus a
                    // single-PROGRAM CONFIGURATION. Sibling files that
                    // declare no PROGRAM ride along as type context, so
                    // library FUNCTION_BLOCKs resolve — while the debug
                    // section (and therefore Monitor) still shows exactly
                    // the variables of the file the user is looking at.
                    let tasks = single_program_tasks(name);
                    let (container, metadata) =
                        ironplc_bridge::compile_isolated_in_project_full(store, file_path, &tasks)?;
                    vec![adhoc_unit(&tasks, container, metadata.retain_vars)]
                }
                (Some(name), None) => {
                    // Ad-hoc but no file scope — same per-unit assembly as
                    // the scheduled path, driven by a synthetic
                    // single-PROGRAM schedule: the requested PROGRAM is
                    // hoisted to the front of its unit and foreign PROGRAM
                    // declarations are excluded. (This arm used to
                    // concatenate every POU file, and ironplc compiles the
                    // FIRST PROGRAM in source order — so whichever file
                    // sorted first ran, silently, instead of the requested
                    // one.)
                    let tasks = single_program_tasks(name);
                    ironplc_bridge::compile_project_units(store, &tasks)?
                }
            };
            let devices = store.list_devices()?;
            let iomap = store.read_iomap()?;
            let alarms = store.read_alarms().map(|c| c.alarms).unwrap_or_default();
            let specs = devices
                .into_iter()
                .map(|d| DeviceSpec {
                    name: d.name,
                    config: d.config,
                })
                .collect::<Vec<_>>();
            Ok((
                units,
                specs,
                iomap.mappings,
                alarms,
                // Per-run freshness, same as tasks/iomap/alarms above.
                fresh_governance(store)?,
            ))
        })
        .await
        .map_err(|e| ApiError::Internal(format!("compile task failed: {e}")))??;

    {
        let mut guard = state.program.lock();
        if let Some(old) = guard.take() {
            old.handle.stop();
        }
    }

    // Rebuild the alarm engine from this project's alarms.toml — the
    // run picks up definition edits without a server restart.
    *state.alarms.lock() = ironplc_bridge::monitor::AlarmEngine::new(alarm_defs);

    // IDE-side server runs are ephemeral — they're for the user
    // poking values, not for persisted plant state. We leave
    // `state_path = None`; only the headless `ia2-runtime` edge
    // binary points it at a real disk location.
    let handle = ironplc_bridge::spawn_units(units, device_specs, mappings, None, governance);
    let mut rx = handle.subscribe();
    let event_tx = state.event_tx.clone();
    let last_snapshot_cache = state.last_snapshot.clone();
    let historian = state.historian.clone();
    let alarm_engine = state.alarms.clone();
    // Fresh run wipes the prior error; if this run faults, the forwarder
    // below refills it when the snapshot stream closes.
    *state.last_error.lock() = None;

    let fault_state = state.clone();
    let fault_handle = handle.clone();
    let mut fault_rx = handle.fault_watch();
    tokio::spawn(async move {
        // Forward snapshots until the run ends. "Ends" cannot be seen on
        // the broadcast stream alone — this task and the program slot both
        // hold ProgramHandle clones whose snapshot sender keeps the channel
        // open — so select on the fault watch too: it fires when the scan
        // loop records a VM trap / panic, and closes (Err) when the scan
        // thread exits cleanly.
        loop {
            tokio::select! {
                res = rx.recv() => match res {
                    Ok(snap) => {
                        historian.note_snapshot(&snap);
                        alarm_engine
                            .lock()
                            .note_snapshot(&snap, ironplc_bridge::monitor::wall_now_us());
                        *last_snapshot_cache.lock() =
                            Some(snap.clone());
                        let _ = event_tx.send(AppEvent::Snapshot(snap));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                // `wait_for` (not `changed`) on purpose: it tests the
                // CURRENT value, so a trap that fires on the very first
                // scan — possibly before this task even started — still
                // resolves immediately. `changed()` would count that
                // early write as already-seen and sleep forever.
                res = fault_rx.wait_for(|f| f.is_some()) => {
                    let _ = res; // Ok = fault present; Err = clean exit
                    break;
                }
            }
        }
        // A clean stop leaves `fault()` empty (the stop handler already
        // cleared state and emitted Stopped). A VM trap / scan-thread
        // panic set it — and this task is the only one that notices, so
        // surface it: record last_error, release the dead program slot
        // (only if it is still ours — a newer run may have replaced it),
        // and emit Error + Stopped so /api/runtime/status and SSE watchers
        // see the fault instead of a forever-stale "running".
        if let Some(msg) = fault_handle.fault() {
            *fault_state.last_error.lock() = Some(msg.clone());
            {
                let mut guard = fault_state.program.lock();
                if guard
                    .as_ref()
                    .is_some_and(|rp| rp.handle.same_run(&fault_handle))
                {
                    guard.take();
                    *fault_state.running_info.lock() = None;
                }
            }
            let _ = fault_state.event_tx.send(AppEvent::Error(msg));
            let _ = fault_state.event_tx.send(AppEvent::Stopped);
        }
    });

    state.program.lock().replace(RunningProgram {
        project_name: project_name.clone(),
        handle,
    });

    // Record what kind of run this is so /api/runtime/status can label
    // the Monitor pane on a fresh page load (which would otherwise have
    // no way to know — the SSE `Started` event already fired).
    let info = match (req.program.as_deref(), req.file_path.as_deref()) {
        (Some(name), Some(file_path)) => Some(RunningInfo::Isolated {
            program: name.to_string(),
            file_path: file_path.to_string(),
        }),
        (Some(name), None) => Some(RunningInfo::Scheduled {
            programs: vec![name.to_string()],
        }),
        (None, _) => {
            // Whole-project schedule — pull the PROGRAM names from
            // tasks.toml so the IDE can render them, not the instance
            // names (instances are bookkeeping; PROGRAM names are what
            // humans recognise from the POU tree).
            let programs = state
                .projects
                .lock()
                .get(&project_name)
                .and_then(|s| s.read_tasks().ok().flatten())
                .map(|t| t.programs.into_iter().map(|p| p.program).collect())
                .unwrap_or_default();
            Some(RunningInfo::Scheduled { programs })
        }
    };
    *state.running_info.lock() = info;

    let _ = state.event_tx.send(AppEvent::Started);

    Ok(Json(RunResponse { ok: true }))
}

pub async fn stop(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Json<RunResponse> {
    // Global stop — only one program can be running at a time
    // (hardware constraint), so a single `/api/stop` always targets
    // it regardless of which window the request came from.
    state.maybe_attribute_external(
        crate::runtime_routes::origin_of(&headers).as_deref(),
        "stop".into(),
    );
    if let Some(rp) = state.program.lock().take() {
        rp.handle.stop();
    }
    *state.running_info.lock() = None;
    let _ = state.event_tx.send(AppEvent::Stopped);
    Json(RunResponse { ok: true })
}

/// First 32 entries of each address space in the in-process demo slave.
/// Useful for verifying that the scan loop wrote to / read from the bus.
pub async fn demo_slave(State(state): State<AppState>) -> Json<DemoSlaveSnapshot> {
    const N: usize = 32;
    let coils = state.demo_slave.coils().lock().unwrap()[..N].to_vec();
    let discrete_inputs = state.demo_slave.discrete_inputs().lock().unwrap()[..N].to_vec();
    let holding_registers = state.demo_slave.holding_registers().lock().unwrap()[..N].to_vec();
    let input_registers = state.demo_slave.input_registers().lock().unwrap()[..N].to_vec();
    Json(DemoSlaveSnapshot {
        coils,
        discrete_inputs,
        holding_registers,
        input_registers,
    })
}

pub async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Subscribe first, then snapshot — so a change that races the
    // snapshot lands in the live stream (a duplicate AgentActivity just
    // repaints identical state; missing one would leave the UI wrong).
    let rx = state.event_tx.subscribe();
    // Replay current agent-activity on connect. The stream is delta-only
    // and agent state has no REST read endpoint, so a reconnecting UI
    // would otherwise miss an in-progress takeover session until the
    // next change. (Runtime snapshot/status DO have REST reads, so the
    // frontend back-fills those on load — only this needs replaying.)
    let initial = Event::default()
        .json_data(AppEvent::AgentActivity(state.current_agent_activity()))
        .ok()
        .map(Ok);
    let live = TokioStreamExt::filter_map(BroadcastStream::new(rx), |res| match res {
        Ok(event) => Event::default().json_data(&event).ok().map(Ok),
        Err(_) => None,
    });
    let stream = FuturesStreamExt::chain(tokio_stream::iter(initial), live);
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

// ============================================================
//  Folder deletion (devices / edges) — POU folder delete lives with
//  the rest of the POU handlers above.
// ============================================================

pub async fn delete_device_folder(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(path): AxumPath<String>,
) -> Result<Json<RunResponse>, ApiError> {
    with_project(&state, &project, |store| {
        store.delete_device_folder(&path).map_err(Into::into)
    })?;
    emit_mutation(
        &state,
        &project,
        topic::DEVICES,
        MutationDetail::DeviceFolderDeleted { path },
    );
    Ok(Json(RunResponse { ok: true }))
}

pub async fn delete_edge_folder(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(path): AxumPath<String>,
) -> Result<Json<RunResponse>, ApiError> {
    with_project(&state, &project, |store| {
        store.delete_edge_folder(&path).map_err(Into::into)
    })?;
    emit_mutation(
        &state,
        &project,
        topic::EDGES,
        MutationDetail::EdgeFolderDeleted { path },
    );
    Ok(Json(RunResponse { ok: true }))
}

// ============================================================
//  Demo-slave poke — inject input signals
// ============================================================

#[derive(Debug, Deserialize, TS)]
#[ts(export)]
pub struct DemoSlavePoke {
    /// For coil / discrete_input, value is interpreted as boolean
    /// (non-zero = TRUE); for holding_register / input_register it's
    /// truncated to u16.
    pub value: i32,
}

/// Write a single address in the in-process demo Modbus slave. Useful
/// for simulating input signals during agent-driven testing — e.g.,
/// flip a discrete_input to test a fault path without driving Modbus
/// from real hardware. `kind` matches `ModbusChannelKind`.
pub async fn poke_demo_slave(
    State(state): State<AppState>,
    // The demo slave is a process-wide singleton — there's only one,
    // regardless of which project is open. We still accept the header
    // for transport symmetry but don't use it for routing.
    _project: ProjectName,
    AxumPath((kind, addr)): AxumPath<(String, u16)>,
    Json(req): Json<DemoSlavePoke>,
) -> Result<Json<RunResponse>, ApiError> {
    let addr = addr as usize;
    // Bind each Arc<Mutex<...>> to a named local so the temporary lives
    // through the .lock() borrow — otherwise the returned Arc is dropped
    // at the end of the same statement.
    match kind.as_str() {
        "coil" => {
            let arc = state.demo_slave.coils();
            let mut guard = arc.lock().unwrap();
            *guard
                .get_mut(addr)
                .ok_or_else(|| ApiError::BadRequest("address out of range".into()))? =
                req.value != 0;
        }
        "discrete_input" => {
            let arc = state.demo_slave.discrete_inputs();
            let mut guard = arc.lock().unwrap();
            *guard
                .get_mut(addr)
                .ok_or_else(|| ApiError::BadRequest("address out of range".into()))? =
                req.value != 0;
        }
        "holding_register" => {
            let arc = state.demo_slave.holding_registers();
            let mut guard = arc.lock().unwrap();
            *guard
                .get_mut(addr)
                .ok_or_else(|| ApiError::BadRequest("address out of range".into()))? =
                req.value as u16;
        }
        "input_register" => {
            let arc = state.demo_slave.input_registers();
            let mut guard = arc.lock().unwrap();
            *guard
                .get_mut(addr)
                .ok_or_else(|| ApiError::BadRequest("address out of range".into()))? =
                req.value as u16;
        }
        other => {
            return Err(ApiError::BadRequest(format!(
                "unknown kind '{other}' — use coil / discrete_input / holding_register / input_register"
            )));
        }
    }
    Ok(Json(RunResponse { ok: true }))
}

// ============================================================
//  LSP WebSocket bridge
// ============================================================

/// Ceiling on one LSP message body from the child. Semantic tokens for a
/// large POU are measured in kilobytes; 16 MiB is far above anything real and
/// far below "allocate whatever the stream claims".
const MAX_LSP_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Ceiling on one header line. `read_line` grows its buffer until a newline
/// arrives, so a child emitting none would grow it without limit.
const MAX_LSP_HEADER_BYTES: usize = 8 * 1024;

/// Upgrade to WebSocket and bridge to a freshly-spawned ironplc LSP
/// process. WS frames are LSP JSON-RPC bodies (no Content-Length header);
/// the proxy adds/strips headers when talking to stdio.
pub async fn lsp(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_lsp_ws)
}

fn lsp_launcher_path() -> PathBuf {
    if let Ok(p) = std::env::var("LSP_LAUNCHER") {
        return PathBuf::from(p);
    }
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop();
    path.push("lsp-launcher");
    path
}

async fn handle_lsp_ws(mut socket: WebSocket) {
    let cmd = lsp_launcher_path();
    let mut child = match Command::new(&cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(path = %cmd.display(), %e, "failed to spawn lsp-launcher");
            // Close with a reason instead of dropping the socket cold:
            // the browser's devtools then show WHY the LSP died (a
            // missing sidecar binary — i.e. an incomplete install)
            // rather than a bare disconnect. The editor keeps working
            // either way; diagnostics come from /api/check.
            let _ = socket
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 1011, // internal error
                    reason: format!(
                        "lsp-launcher not available at {} — reinstall (the \
                         binary ships alongside ia2-server)",
                        cmd.display()
                    )
                    .into(),
                })))
                .await;
            return;
        }
    };
    tracing::info!(path = %cmd.display(), "lsp-launcher spawned for ws client");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let (mut ws_tx, mut ws_rx) = FuturesStreamExt::split(socket);

    // WS → child stdin: add LSP Content-Length framing.
    let to_child = async move {
        while let Some(msg) = FuturesStreamExt::next(&mut ws_rx).await {
            let Ok(msg) = msg else { break };
            let text = match msg {
                Message::Text(t) => t.to_string(),
                Message::Close(_) => break,
                _ => continue,
            };
            let body = text.as_bytes();
            let header = format!("Content-Length: {}\r\n\r\n", body.len());
            if stdin.write_all(header.as_bytes()).await.is_err() {
                break;
            }
            if stdin.write_all(body).await.is_err() {
                break;
            }
            if stdin.flush().await.is_err() {
                break;
            }
        }
    };

    // child stdout → WS: strip LSP Content-Length framing.
    let from_child = async move {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_lsp_frame(&mut reader).await {
                Ok(Some(text)) => {
                    if ws_tx.send(Message::Text(text.into())).await.is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(why) => {
                    tracing::error!(%why, "lsp stdout stopped being LSP framing; closing the bridge");
                    return;
                }
            }
        }
    };

    tokio::select! {
        _ = to_child => {}
        _ = from_child => {}
    }
    let _ = child.kill().await;
}

/// Read one LSP frame from a child's stdout: headers, then exactly
/// `Content-Length` bytes of body.
///
/// `Ok(None)` is a clean EOF. `Err` means the stream stopped being LSP
/// framing and re-syncing it would be guesswork — the caller closes the
/// bridge rather than guess.
///
/// Both reads are bounded. The child is our own lsp-launcher, so this is not
/// a security boundary — it is blast-radius control. A desynced or buggy
/// child announcing a wild `Content-Length` would make `vec![0u8; len]`
/// allocate it verbatim, and THIS process is also the one holding a running
/// PLC program: an OOM abort here stops the plant, not just the editor's
/// autocomplete.
///
/// A header block with no `Content-Length`, and a body that is not UTF-8,
/// are both skipped rather than fatal — that is the pre-existing behaviour
/// and it costs one message, not the session.
async fn read_lsp_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<String>, String> {
    loop {
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = Vec::new();
            let n = match reader
                .take(MAX_LSP_HEADER_BYTES as u64)
                .read_until(b'\n', &mut line)
                .await
            {
                Ok(0) => return Ok(None),
                Ok(n) => n,
                Err(e) => return Err(format!("read: {e}")),
            };
            if n == MAX_LSP_HEADER_BYTES && !line.ends_with(b"\n") {
                return Err(format!("header line exceeded {MAX_LSP_HEADER_BYTES} bytes"));
            }
            let Ok(line) = String::from_utf8(line) else {
                return Err("header line was not UTF-8".into());
            };
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some(v) = trimmed.strip_prefix("Content-Length:") {
                content_length = v.trim().parse().ok();
            }
        }
        let Some(len) = content_length else {
            continue;
        };
        if len > MAX_LSP_BODY_BYTES {
            return Err(format!(
                "Content-Length {len} exceeds the {MAX_LSP_BODY_BYTES}-byte ceiling"
            ));
        }
        let mut body = vec![0u8; len];
        if let Err(e) = reader.read_exact(&mut body).await {
            return Err(format!("body read: {e}"));
        }
        match String::from_utf8(body) {
            Ok(text) => return Ok(Some(text)),
            Err(_) => continue,
        }
    }
}

// ============================================================
//  Helpers
// ============================================================

/// Resolve a `ProjectName` (parsed `X-IA2-Project` header) to the
/// project's store and pass it to `f`.
///
///   - `ProjectName(Some(name))` → look up the project by name. Errors
///     with `NoProject` if no such project is open. The found project
///     is also marked active, so subsequent requests without a header
///     in this same window continue to land on it (LRU behaviour).
///   - `ProjectName(None)` → use the server's currently-active project
///     (the last-touched one). Errors with `NoProject` if no project
///     is open at all.
///
/// This is the **only** project-state access shape the route handlers
/// should use. Direct `state.projects.lock()` is reserved for
/// lifecycle handlers (create / open / close / list).
/// Resolve a user-typed project path into an absolute one. Handles the
/// shapes people actually type in the "Open project" dialog (and that
/// shells don't pre-expand when the path is quoted):
///
/// - leading `~` or `~/...` expands to the user's home directory
/// - a relative path is joined onto the home dir
/// - an already-absolute path passes through untouched
///
/// Leading/trailing whitespace (from copy-paste) is trimmed. We do not
/// canonicalize: the path may not exist yet, and `ProjectStore::open`
/// gives a clearer "no project.toml here" error than a canonicalize
/// failure would.
fn resolve_user_path(raw: &str) -> PathBuf {
    // NOTE: home comes from `project::home_dir()` (→ dirs::home_dir,
    // getpwuid-backed), NOT `$HOME` — a service-manager-launched server
    // (launchd/systemd) may have no `$HOME` in its environment, and the
    // env-var path would make tilde/relative paths silently fail to
    // resolve there.
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix('~') {
        // `~`, `~/x`, or `~x` (treat `~x` as `~/x` — nobody means a
        // different user's home in this dialog).
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        if let Some(home) = project::home_dir() {
            return home.join(rest);
        }
    }
    let p = PathBuf::from(trimmed);
    if p.is_absolute() {
        p
    } else if let Some(home) = project::home_dir() {
        // Relative path → anchor to home, where projects live by
        // default (~/Documents/IA2/...). Better than CWD, which for a
        // GUI-launched server is `/`.
        home.join(p)
    } else {
        p
    }
}

/// Clone the addressed project's store out of the registry lock. For
/// handlers that go on to do CPU-bound work (compile / whole-project
/// check): the clone is `{root, manifest}`-cheap, and compiling unlocked
/// on `spawn_blocking` keeps the global registry responsive to every
/// other window. Prefer `with_project` for quick metadata reads.
fn owned_store(state: &AppState, project: &ProjectName) -> Result<ProjectStore, ApiError> {
    with_project(state, project, |store| Ok(store.clone()))
}

pub(crate) fn with_project<T>(
    state: &AppState,
    project: &ProjectName,
    f: impl FnOnce(&ProjectStore) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let mut guard = state.projects.lock();
    let store = match project.as_deref() {
        Some(name) => {
            if !guard.set_active(name) {
                return Err(ApiError::BadRequest(format!(
                    "project '{name}' is not open on this server",
                )));
            }
            guard.get(name).ok_or(ApiError::NoProject)?
        }
        None => guard.active().ok_or(ApiError::NoProject)?,
    };
    f(store)
}

/// Convenience: resolve the project name and broadcast a mutation
/// event tagged with it. Designed for callsites that have already
/// succeeded at a `with_project` call (so the project is known to
/// exist) — silently no-ops if resolution fails, since by that point
/// the response has already been written and there's nowhere to
/// surface an error. Replaces the 2-arg `state.emit_mutation` so
/// every event on the wire carries its project tag.
fn emit_mutation(
    state: &AppState,
    project: &ProjectName,
    topic: impl Into<String>,
    detail: MutationDetail,
) {
    if let Ok(name) = resolve_project_name(state, project) {
        state.emit_mutation(name, topic, detail);
    }
}

/// Pull a clone of the active-project's name (resolved per
/// `ProjectName`) out of the registry. Used by mutation-emission
/// callsites that need to tag the event with `project: …` but don't
/// need the full store.
fn resolve_project_name(state: &AppState, project: &ProjectName) -> Result<String, ApiError> {
    let guard = state.projects.lock();
    match project.as_deref() {
        Some(name) => {
            if guard.get(name).is_some() {
                Ok(name.to_string())
            } else {
                Err(ApiError::BadRequest(format!(
                    "project '{name}' is not open on this server",
                )))
            }
        }
        None => guard
            .active_name()
            .map(str::to_string)
            .ok_or(ApiError::NoProject),
    }
}

/// Write the current set of open projects to disk so a server
/// restart re-opens them. Best-effort — a failure here is logged and
/// swallowed (the server's state is the source of truth at runtime).
///
/// Path: `<projects_dir>/.ia2-open-projects.json`, sibling of the
/// projects so the file travels with the user's workspace.
pub fn save_open_projects(state: &AppState) {
    let path = open_projects_state_path();
    let guard = state.projects.lock();
    let paths: Vec<String> = guard
        .iter()
        .map(|store| store.root().display().to_string())
        .collect();
    let payload = OpenProjectsPersistence {
        paths,
        active: guard.active_name().map(str::to_string),
    };
    drop(guard);
    if let Err(e) = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        project::write_atomic(&path, serde_json::to_vec_pretty(&payload)?)
    })() {
        tracing::warn!(?path, %e, "failed to persist open-projects list");
    }
}

/// Restore the open-projects set saved by `save_open_projects`. Any
/// path that no longer exists on disk is silently dropped — the
/// expected outcome when a user deletes a project directory between
/// sessions.
pub fn load_open_projects(state: &AppState) {
    let path = open_projects_state_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(?path, %e, "open-projects file unreadable; starting empty");
            return;
        }
    };
    let payload: OpenProjectsPersistence = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(?path, %e, "open-projects file is corrupt; starting empty");
            return;
        }
    };
    let mut guard = state.projects.lock();
    for p in &payload.paths {
        match ProjectStore::open(PathBuf::from(p)) {
            Ok(store) => guard.insert_and_activate(store),
            Err(e) => tracing::warn!(path = %p, %e, "could not re-open project; skipping"),
        }
    }
    if let Some(name) = payload.active {
        // `insert_and_activate` left `active` on whichever was
        // restored last. Override with the persisted active so the
        // user lands on the same project they last viewed.
        guard.set_active(&name);
    }
    tracing::info!(open = guard.len(), "restored open projects");
}

fn open_projects_state_path() -> PathBuf {
    default_projects_dir().join(".ia2-open-projects.json")
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenProjectsPersistence {
    paths: Vec<String>,
    active: Option<String>,
}

/// Walk the default projects dir and surface anything that looks like a
/// project. Also includes the last-opened path if it lives elsewhere.
fn scan_projects() -> Vec<ProjectListing> {
    let last = load_last_opened();
    let mut out = Vec::new();
    let default_dir = default_projects_dir();
    if default_dir.exists() {
        if let Ok(entries) = fs::read_dir(&default_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                if let Some(listing) = listing_for(&path, last.as_deref()) {
                    out.push(listing);
                }
            }
        }
    }
    // Include the last-opened project even if it's outside the default dir.
    if let Some(ref last_path) = last {
        let already_listed = out.iter().any(|p| Path::new(&p.path) == last_path);
        if !already_listed {
            if let Some(listing) = listing_for(last_path, Some(last_path)) {
                out.push(listing);
            }
        }
    }
    out.sort_by(|a, b| {
        b.is_last_opened
            .cmp(&a.is_last_opened)
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

fn listing_for(path: &Path, last_opened: Option<&Path>) -> Option<ProjectListing> {
    let manifest_path = path.join("project.toml");
    if !manifest_path.exists() {
        return None;
    }
    let text = fs::read_to_string(&manifest_path).ok()?;
    let manifest: ProjectManifest = toml::from_str(&text).ok()?;
    Some(ProjectListing {
        name: manifest.name,
        path: path.display().to_string(),
        is_last_opened: last_opened == Some(path),
    })
}

/// Build the ad-hoc `Tasks` for "run just this PROGRAM once":
/// one 100-ms / priority-1 task hosting one instance of the named PROGRAM.
/// Used by the /api/run path's `program` override.
fn single_program_tasks(program_name: &str) -> Tasks {
    Tasks {
        tasks: vec![Task {
            name: "plc_task".into(),
            interval_ms: 100,
            priority: 1,
        }],
        programs: vec![ProgramInstance {
            instance: format!("{program_name}_inst"),
            program: program_name.into(),
            task: "plc_task".into(),
        }],
    }
}

/// Wrap an ad-hoc single-PROGRAM compile result into the one
/// `ProgramUnit` the bridge schedules. Instance name and cadence come
/// from the synthetic `single_program_tasks` schedule (always exactly
/// one task + one instance).
fn adhoc_unit(
    tasks: &Tasks,
    container: ironplc_bridge::Container,
    retain_vars: Vec<String>,
) -> ironplc_bridge::ProgramUnit {
    let task = &tasks.tasks[0];
    let instance = &tasks.programs[0];
    ironplc_bridge::ProgramUnit {
        instance: instance.instance.clone(),
        task_name: task.name.clone(),
        interval_ms: u64::from(task.interval_ms),
        priority: task.priority,
        container,
        retain_vars,
    }
}

#[cfg(test)]
mod path_resolution_tests {
    use super::resolve_user_path;
    use std::path::PathBuf;

    fn home() -> PathBuf {
        // Match production: resolve via project::home_dir (dirs-backed),
        // not $HOME — otherwise a CI runner where the two disagree would
        // fail the assertions for the wrong reason.
        project::home_dir().expect("home dir resolvable in test env")
    }

    #[test]
    fn absolute_path_passes_through() {
        assert_eq!(
            resolve_user_path("/Users/x/Documents/IA2/demo"),
            PathBuf::from("/Users/x/Documents/IA2/demo")
        );
    }

    #[test]
    fn tilde_slash_expands_to_home() {
        assert_eq!(
            resolve_user_path("~/Documents/IA2/demo"),
            home().join("Documents/IA2/demo")
        );
    }

    #[test]
    fn bare_tilde_is_home() {
        assert_eq!(resolve_user_path("~"), home().join(""));
    }

    #[test]
    fn relative_path_anchors_to_home() {
        assert_eq!(
            resolve_user_path("Documents/IA2/demo"),
            home().join("Documents/IA2/demo")
        );
    }

    #[test]
    fn whitespace_is_trimmed() {
        assert_eq!(
            resolve_user_path("  /abs/path  "),
            PathBuf::from("/abs/path")
        );
    }
}

#[cfg(test)]
mod device_describe_tests {
    use super::*;
    use project::ProjectStore;

    #[test]
    fn describe_rejects_broken_configuration_instead_of_stale_or_empty_data() {
        for file in ["project.toml", "iomap.toml", "alarms.toml"] {
            let dir = tempfile::tempdir().unwrap();
            let store = ProjectStore::create(dir.path().join("p"), "p").unwrap();
            store
                .create_device("plc1", project::Protocol::Modbus)
                .unwrap();
            std::fs::write(dir.path().join("p").join(file), "broken = [").unwrap();
            assert!(build_device_description(&store, "plc1").is_err(), "{file}");
        }
    }

    #[test]
    fn device_describe_is_deterministic_and_redacts() {
        let dir = tempfile::tempdir().unwrap();
        // Create only scaffolds the project dir; the store used below is
        // re-opened after the config files land.
        let _ = ProjectStore::create(dir.path().join("p"), "p").unwrap();
        let root = dir.path().join("p");
        std::fs::write(
            root.join("devices/plc1.toml"),
            "name = \"plc1\"\nprotocol = \"opcua\"\nendpoint_url = \"opc.tcp://127.0.0.1:4840\"\npoll_interval_ms = 100\n\n[auth]\nkind = \"user_password\"\nusername = \"u\"\npassword = \"s3cret\"\n\n[[channels]]\nname = \"ch0\"\nnode_id = \"ns=2;i=3\"\ndata_type = \"f64\"\naccess = \"write\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("iomap.toml"),
            "[[mappings]]\napplication = \"main\"\nvariable = \"level\"\ndirection = \"input\"\ndevice = \"plc1\"\nchannel = \"ch0\"\nunit = \"%\"\ndescription = \"Tank level\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("alarms.toml"),
            "[[alarms]]\nid = \"level_high\"\nvariable = \"level\"\ncondition = \"gt\"\nlimit = 90.0\nmessage = \"Tank level high\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("project.toml"),
            "name = \"p\"\n\n[governance]\nwrite_mode = \"allowlist\"\n\n[[governance.rules]]\nvariable = \"level\"\nmax = 100.0\n",
        )
        .unwrap();
        let store = ProjectStore::open(root).unwrap();

        let a = build_device_description(&store, "plc1").unwrap();
        let b = build_device_description(&store, "plc1").unwrap();
        let ja = serde_json::to_string(&a).unwrap();
        assert_eq!(ja, serde_json::to_string(&b).unwrap(), "deterministic");

        assert_eq!(a.protocol, "opcua");
        // Secret redacted everywhere it appears.
        assert!(!ja.contains("s3cret"), "password must be redacted: {ja}");
        assert!(ja.contains("***"));
        // Binding carries the metadata.
        assert_eq!(a.bindings.len(), 1);
        assert_eq!(a.bindings[0].unit.as_deref(), Some("%"));
        assert_eq!(a.bindings[0].description.as_deref(), Some("Tank level"));
        // Related alarm + applicable governance rule included.
        assert_eq!(a.alarms.len(), 1);
        assert_eq!(a.alarms[0].id, "level_high");
        assert_eq!(a.write_mode, project::WriteMode::Allowlist);
        assert_eq!(a.write_rules.len(), 1);
        assert_eq!(a.write_rules[0].max, Some(100.0));
        // Unknown device → the store error (404 path).
        assert!(build_device_description(&store, "nope").is_err());
    }

    /// Alarm defs read SNAPSHOT names, which are instance-qualified
    /// (`main.level`) when a variable name is shared across program
    /// units — describe must match those against its bindings the same
    /// two-key way as write_rules. A qualified alarm on a DIFFERENT
    /// application must not ride along.
    #[test]
    fn describe_matches_instance_qualified_alarm_variables() {
        let dir = tempfile::tempdir().unwrap();
        let _ = ProjectStore::create(dir.path().join("p"), "p").unwrap();
        let root = dir.path().join("p");
        std::fs::write(
            root.join("devices/plc1.toml"),
            "name = \"plc1\"\nprotocol = \"modbus\"\nslave_id = 1\npoll_interval_ms = 100\n\n[transport]\nkind = \"tcp\"\nhost = \"127.0.0.1\"\nport = 502\n\n[[channels]]\nname = \"ch0\"\nkind = \"holding_register\"\naddress = 0\ndata_type = \"u16\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("iomap.toml"),
            "[[mappings]]\napplication = \"main\"\nvariable = \"level\"\ndirection = \"input\"\ndevice = \"plc1\"\nchannel = \"ch0\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("alarms.toml"),
            "[[alarms]]\nid = \"level_high_qualified\"\nvariable = \"main.level\"\ncondition = \"gt\"\nlimit = 90.0\nmessage = \"Tank level high\"\n\n[[alarms]]\nid = \"foreign_level_high\"\nvariable = \"other.level\"\ncondition = \"gt\"\nlimit = 90.0\nmessage = \"Someone else's level\"\n",
        )
        .unwrap();
        let store = ProjectStore::open(root).unwrap();

        let d = build_device_description(&store, "plc1").unwrap();
        let ids: Vec<&str> = d.alarms.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["level_high_qualified"],
            "qualified alarm on the bound (application, variable) is in; \
             a foreign application's alarm is out"
        );
    }

    /// The redaction key set covers credential-ish keys beyond the
    /// literal `password` — a protocol config gaining a token/api-key
    /// field must not leak through describe. Placeholder stays visible,
    /// never silent omission; non-credential keys pass untouched.
    #[test]
    fn redact_secrets_covers_credential_key_patterns() {
        let mut v = serde_json::json!({
            "endpoint_url": "opc.tcp://127.0.0.1:4840",
            "auth": { "kind": "user_password", "username": "u", "password": "p" },
            "Token": "t0k3n",
            "api_key": "k1",
            "broker": {
                "client_secret": "sh",
                "tls": { "private_key": "-----BEGIN..." }
            },
            "channels": [ { "name": "ch0", "apikey": "k2" } ]
        });
        redact_secrets(&mut v);
        let redacted = serde_json::Value::String("***".into());
        assert_eq!(v["auth"]["password"], redacted);
        // Case-insensitive…
        assert_eq!(v["Token"], redacted);
        // …and substring: `client_secret` / `private_key` style names.
        assert_eq!(v["api_key"], redacted);
        assert_eq!(v["broker"]["client_secret"], redacted);
        assert_eq!(v["broker"]["tls"]["private_key"], redacted);
        // Arrays are walked too.
        assert_eq!(v["channels"][0]["apikey"], redacted);
        // Non-credential keys survive verbatim.
        assert_eq!(v["auth"]["username"], "u");
        assert_eq!(v["endpoint_url"], "opc.tcp://127.0.0.1:4840");
        assert_eq!(v["channels"][0]["name"], "ch0");
    }
}

#[cfg(test)]
mod governance_freshness_tests {
    use super::fresh_governance;
    use project::ProjectStore;

    /// `/api/run` must enforce the `[governance]` table as it is ON DISK
    /// at spawn time, not the copy cached when the project was opened —
    /// otherwise tightening open → allowlist between runs silently fails
    /// open. This pins the helper the run handler's spawn closure uses.
    #[test]
    fn run_governance_is_reread_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let _ = ProjectStore::create(dir.path().join("p"), "p").unwrap();
        let root = dir.path().join("p");
        let cached = ProjectStore::open(root.clone()).unwrap();
        assert_eq!(cached.governance().write_mode, project::WriteMode::Open);

        // An agent edits project.toml between runs (the documented way
        // to declare governance — there is no API that writes it).
        std::fs::write(
            root.join("project.toml"),
            "name = \"p\"\n\n[governance]\nwrite_mode = \"allowlist\"\n",
        )
        .unwrap();

        // The registry's cached copy is stale by design…
        assert_eq!(cached.governance().write_mode, project::WriteMode::Open);
        // …and the run path must not use it.
        let fresh = fresh_governance(&cached).unwrap();
        assert_eq!(fresh.write_mode, project::WriteMode::Allowlist);
    }
}

#[cfg(test)]
mod project_header_tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn encoded_unicode_and_literal_percent_are_distinct_from_legacy_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(PROJECT_HEADER, "a%20b".parse().unwrap());
        assert_eq!(
            parse_project_header(&headers).unwrap().as_deref(),
            Some("a%20b")
        );
        headers.insert("x-ia2-project-encoding", "percent".parse().unwrap());
        headers.insert(
            PROJECT_HEADER,
            "%E6%8E%A7%E5%88%B6%20%2520".parse().unwrap(),
        );
        assert_eq!(
            parse_project_header(&headers).unwrap().as_deref(),
            Some("控制 %20")
        );
    }

    #[test]
    fn malformed_explicit_selectors_do_not_fall_back_to_the_active_project() {
        for (name, encoding) in [
            ("%", "percent"),
            ("%GG", "percent"),
            ("%FF", "percent"),
            ("", "percent"),
            ("main", "other"),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(PROJECT_HEADER, name.parse().unwrap());
            headers.insert("x-ia2-project-encoding", encoding.parse().unwrap());
            assert!(matches!(
                parse_project_header(&headers),
                Err(ApiError::BadRequest(_))
            ));
        }
        assert!(parse_project_header(&HeaderMap::new())
            .unwrap()
            .as_deref()
            .is_none());
    }
}

#[cfg(test)]
mod lsp_frame_tests {
    use super::{read_lsp_frame, MAX_LSP_BODY_BYTES, MAX_LSP_HEADER_BYTES};
    use tokio::io::BufReader;

    async fn frames(input: &[u8]) -> Result<Vec<String>, String> {
        let mut r = BufReader::new(std::io::Cursor::new(input.to_vec()));
        let mut out = Vec::new();
        loop {
            match read_lsp_frame(&mut r).await? {
                Some(t) => out.push(t),
                None => return Ok(out),
            }
        }
    }

    #[tokio::test]
    async fn back_to_back_frames_are_split_on_content_length() {
        let got = frames(b"Content-Length: 2\r\n\r\n{}Content-Length: 5\r\n\r\n[1,2]")
            .await
            .expect("well-framed");
        assert_eq!(got, vec!["{}".to_string(), "[1,2]".to_string()]);
    }

    #[tokio::test]
    async fn a_clean_eof_ends_the_stream() {
        assert_eq!(frames(b"").await.unwrap(), Vec::<String>::new());
    }

    /// The lever this guard exists for: `vec![0u8; len]` would have
    /// allocated whatever the stream claimed, in the process that also holds
    /// the running PLC program.
    #[tokio::test]
    async fn an_implausible_content_length_is_refused_not_allocated() {
        let input = format!("Content-Length: {}\r\n\r\n", u32::MAX);
        let err = frames(input.as_bytes()).await.expect_err("must refuse");
        assert!(err.contains("exceeds the"), "{err}");
        assert!(err.contains(&MAX_LSP_BODY_BYTES.to_string()), "{err}");
    }

    /// A child that emits no newline would grow `read_line`'s buffer without
    /// limit — same allocation lever, different door.
    #[tokio::test]
    async fn a_header_line_without_a_newline_is_bounded() {
        let input = vec![b'x'; MAX_LSP_HEADER_BYTES + 10];
        let err = frames(&input).await.expect_err("must refuse");
        assert!(err.contains("header line exceeded"), "{err}");
    }

    /// Pre-existing tolerances kept: a header block with no Content-Length
    /// costs one message, not the session.
    #[tokio::test]
    async fn a_header_block_without_content_length_is_skipped() {
        let got = frames(b"X-Note: hi\r\n\r\nContent-Length: 2\r\n\r\n{}")
            .await
            .expect("skips, then reads");
        assert_eq!(got, vec!["{}".to_string()]);
    }

    #[tokio::test]
    async fn a_body_that_is_not_utf8_costs_one_message_not_the_session() {
        let mut input = b"Content-Length: 2\r\n\r\n".to_vec();
        input.extend_from_slice(&[0xff, 0xfe]);
        input.extend_from_slice(b"Content-Length: 2\r\n\r\n{}");
        let got = frames(&input).await.expect("skips the bad body");
        assert_eq!(got, vec!["{}".to_string()]);
    }
}
