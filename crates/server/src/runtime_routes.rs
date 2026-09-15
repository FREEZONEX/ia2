//! Runtime-monitor routes — the IDE server's half of the shared
//! online-debug surface. The SEMANTICS (pulse reset, force
//! bookkeeping, mode ops, wire shapes) live once in
//! `ironplc_bridge::monitor`; the edge runtime mounts the same ops on
//! its own router. This file is only the IDE-side axum glue.

use axum::extract::{Path as AxumPath, State};
use axum::Json;
use ironplc_bridge::{DeviceHealth, ProgramHandle, RuntimeMode, VarSnapshot};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::error::ApiError;
use crate::routes::ProjectName;
use crate::state::{AppState, RunningInfo};

// ============================================================
//  Runtime — synchronous queries + variable writes
// ============================================================

/// Most recent VarSnapshot from the running bridge, or `null` when
/// nothing has been snapshotted in the current session (no run, or
/// project was just closed). Lets agents poll one-shot without
/// subscribing to /api/events SSE.
pub async fn runtime_snapshot(State(state): State<AppState>) -> Json<Option<VarSnapshot>> {
    Json(state.last_snapshot.lock().clone())
}

// ============================================================
//  History + alarms — served by the shared monitor layer
// ============================================================

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    /// Comma-separated variable names; empty/missing = all recorded.
    #[serde(default)]
    pub vars: String,
    #[serde(default)]
    pub from_us: u64,
    /// 0 = no upper bound.
    #[serde(default)]
    pub to_us: u64,
    /// Bucket width; defaults to 1000 ms.
    #[serde(default = "default_step_ms")]
    pub step_ms: u64,
}

fn default_step_ms() -> u64 {
    1000
}

/// Downsampled history for the named variables (min/max/last buckets).
/// Backed by the in-memory historian on the IDE server; the edge
/// runtime serves the same shape from its persisted rings at /history.
pub async fn runtime_history(
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

/// Live alarm states (severity-major, standing first). Defined by the
/// project's alarms.toml; evaluated while a program runs.
pub async fn runtime_alarms(
    State(state): State<AppState>,
) -> Json<Vec<ironplc_bridge::monitor::AlarmState>> {
    Json(state.alarms.lock().states())
}

/// Acknowledge one alarm. 404 for an unknown id.
pub async fn runtime_alarm_ack(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ironplc_bridge::monitor::AlarmState>, ApiError> {
    // Masking a standing alarm is a mutating operator action —
    // attributed like every other mutating runtime route (ADR-0002).
    state.maybe_attribute_external(origin_of(&headers).as_deref(), format!("ack {id}"));
    let now_us = ironplc_bridge::monitor::wall_now_us();
    state
        .alarms
        .lock()
        .ack(&id, now_us)
        .map(Json)
        .map_err(ApiError::NotFound)
}

#[derive(Debug, Deserialize)]
pub struct JournalQuery {
    #[serde(default = "default_journal_limit")]
    pub limit: usize,
}

fn default_journal_limit() -> usize {
    100
}

/// Most-recent-first alarm journal (raised / acked / returned).
pub async fn runtime_alarm_journal(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<JournalQuery>,
) -> Json<Vec<ironplc_bridge::monitor::AlarmJournalEntry>> {
    Json(state.alarms.lock().journal(q.limit))
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct RuntimeStatus {
    pub running: bool,
    pub project: Option<String>,
    /// Program instances declared in tasks.toml (what's actually scheduled).
    pub program_instances: Vec<String>,
    /// Device names declared in the project's `devices/` files — configuration,
    /// not connectivity. See `device_health` for whether they are actually up.
    pub devices: Vec<String>,
    /// Live per-device fieldbus health from the running program: `healthy:
    /// false` means that device's inputs are frozen at last-known values and
    /// its outputs are being dropped, while the scan loop keeps running and
    /// `running` stays `true`. Empty when nothing is running.
    ///
    /// Without this, a UI built on `/api/runtime/status` cannot distinguish a
    /// healthy plant from one whose bus died — the variable values alone never
    /// reveal it (see `DeviceHealth`).
    pub device_health: Vec<DeviceHealth>,
    /// `true` once the scan watchdog has tripped: the program lost real-time
    /// guarantees, `enter_failsafe` zeroed every output and the output phase
    /// is latched off until an explicit restart.
    ///
    /// Read this before trusting any variable value as plant state. After a
    /// trip the VM keeps computing and `scan_count` keeps advancing, so a UI
    /// that only watches values sees a perfectly healthy plant while the bus
    /// is actually holding zeros. `running` also stays `true` — the scan loop
    /// deliberately keeps going so operators can still inspect live state.
    pub watchdog_tripped: bool,
    /// Fastest task interval in ms — the cadence at which `scan_count` can
    /// advance, since it is the max across units. A client judging "is this
    /// live data current?" needs this: `interval_ms` has no upper bound, and
    /// on a 5 s-cycle project a 4 s-old value is the newest one that exists,
    /// so a fixed staleness window would refuse every operator write between
    /// scans. It says nothing about how long a scan actually takes — the
    /// overrun watchdog owns that.
    /// `None` when nothing is running.
    pub scan_period_ms: Option<u32>,
    /// Scan count from the most recent snapshot; 0 before the first one.
    pub scan_count: u64,
    /// Timestamp_us of the most recent snapshot, or 0.
    pub last_snapshot_us: u64,
    pub last_error: Option<String>,
    /// What kind of run is active (isolated vs scheduled) and which
    /// PROGRAM(s) it covers. Populated from `AppState.running_info`
    /// which the /api/run handler writes when it starts a program;
    /// cleared on /api/stop and on close-project. `None` here just
    /// means nothing is currently running.
    pub running_info: Option<RunningInfo>,
    /// Current scan-loop mode (`running` / `paused` / `step{remaining}`).
    /// `None` when nothing's running.
    pub mode: Option<RuntimeMode>,
    /// Currently-forced variables in `name=value` pairs, sorted by name.
    /// Empty when no force is active. The shape matches the in-memory
    /// HashMap snapshot from `ProgramHandle::forces()`.
    pub forces: Vec<ForceEntry>,
}

pub use ironplc_bridge::monitor::ForceEntry;

/// One-shot overview of the runtime — designed for agents who want
/// "what's going on right now" without composing /health + /api/project
/// + the SSE stream.
///
/// Multi-project note: status is scoped to whichever project the
/// caller named in `X-IA2-Project` (falling back to active). The
/// `running_info` field reports the globally-running program even if
/// it doesn't belong to the queried project; the IDE renders this as
/// "running: <project>/<program>" so the user can see when their
/// window is observing a sibling project's run.
pub async fn runtime_status(
    State(state): State<AppState>,
    project: ProjectName,
) -> Json<RuntimeStatus> {
    let running = state.program.lock().is_some();
    // Project-scoped fields: pulled from whichever project the
    // header (or active fallback) names. None if no project matched.
    let (project_name, programs, devices) = {
        let guard = state.projects.lock();
        let store = match project.as_deref() {
            Some(name) => guard.get(name),
            None => guard.active(),
        };
        match store {
            Some(store) => {
                let tasks = store.read_tasks().ok().flatten().unwrap_or_default();
                let programs = tasks.programs.iter().map(|p| p.instance.clone()).collect();
                let devices = store
                    .list_devices()
                    .map(|ds| ds.iter().map(|d| d.name.clone()).collect())
                    .unwrap_or_default();
                (Some(store.name().to_string()), programs, devices)
            }
            None => (None, vec![], vec![]),
        }
    };
    let snap = state.last_snapshot.lock().clone();
    let last_error = state.last_error.lock().clone();
    let running_info = state.running_info.lock().clone();
    // Mode + forces come from the live ProgramHandle, when there is
    // one. Clone the handle out of the mutex briefly to avoid holding
    // the sync lock across the calls.
    let (mode, forces, device_health, watchdog_tripped, scan_period_ms) = {
        let guard = state.program.lock();
        match guard.as_ref() {
            Some(rp) => (
                Some(rp.handle.mode()),
                ironplc_bridge::monitor::force_entries(&rp.handle),
                rp.handle.device_health(),
                rp.handle.watchdog_tripped(),
                Some(rp.handle.scan_period_ms()),
            ),
            None => (None, vec![], vec![], false, None),
        }
    };
    Json(RuntimeStatus {
        running,
        project: project_name,
        program_instances: programs,
        devices,
        device_health,
        watchdog_tripped,
        scan_period_ms,
        scan_count: snap.as_ref().map(|s| s.scan_count).unwrap_or(0),
        last_snapshot_us: snap.as_ref().map(|s| s.timestamp_us).unwrap_or(0),
        last_error,
        running_info,
        mode,
        forces,
    })
}

#[derive(Debug, Deserialize, TS)]
#[ts(export)]
pub struct WriteVariableRequest {
    /// Raw i32 value to write — the VM's variable-write primitive is
    /// `write_variable(VarIndex, i32)`, so callers map their domain type
    /// to an i32 (BOOL → 0/1, USINT/UINT → numeric, etc.).
    pub value: i32,
    /// Momentary hold: after `pulse_ms` the SERVER writes 0 back. The
    /// reset guarantee lives here — not in a page timer — so a closed
    /// tab or suspended tablet can't leave a momentary command latched
    /// (the HMI pulse action's contract). Overlapping pulses on one
    /// variable are latest-write-wins.
    #[serde(default)]
    pub pulse_ms: Option<u32>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct WriteVariableResponse {
    pub name: String,
    pub value: i32,
    /// Set when the value landed in the VM but the device carrying this
    /// variable has a dead transport, so it is NOT reaching the field.
    /// The write is still applied — a Stop that arrives late when the link
    /// returns beats one that was never commanded — but a caller that
    /// reports a bare success here would be telling the operator the plant
    /// obeyed. `null` on an internal variable with no Output mapping, and
    /// on a healthy link, which is transport health only: it does not
    /// prove the value took effect.
    pub undelivered_device: Option<String>,
}

/// Poke a variable while the program is running. Applied between scan
/// rounds (so the next round's logic sees the new value). 404 if the
/// name doesn't resolve to any declared variable; 409 if no program is
/// running.
pub async fn write_runtime_variable(
    State(state): State<AppState>,
    _project: ProjectName,
    headers: axum::http::HeaderMap,
    AxumPath(name): AxumPath<String>,
    Json(req): Json<WriteVariableRequest>,
) -> Result<Json<WriteVariableResponse>, ApiError> {
    // The runtime is global (one PROGRAM at a time, hardware constraint);
    // the `X-IA2-Project` header is accepted for symmetry but not used
    // to select a program — clients can poll runtime_status to see which
    // project's program is actually running. Clone the handle out of the
    // mutex so we don't hold a sync lock across the .await below — see
    // the bridge::ProgramHandle docs.
    let handle: ProgramHandle = live_program(&state)?;
    state.maybe_attribute_external(origin_of(&headers).as_deref(), format!("write {name}"));
    // The momentary-pulse reset guarantee lives in the shared monitor
    // core — one implementation for IDE server and edge runtime.
    let outcome =
        ironplc_bridge::monitor::write_with_pulse(&handle, &name, req.value, req.pulse_ms).await?;
    Ok(Json(WriteVariableResponse {
        name,
        value: outcome.value,
        undelivered_device: outcome.undelivered_device,
    }))
}

// ============================================================
//  Debug control trio: pause / resume / step + force / unforce
//
//  All four endpoints share the same "look up the live handle, 409
//  if nothing running" pattern as `write_runtime_variable`. Mode
//  toggles are synchronous on the bridge side (just a mutex write);
//  force commands round-trip through the cmd channel so the scan
//  loop can validate the variable name before the ack comes back.
// ============================================================

pub use ironplc_bridge::monitor::{ModeResponse, StepRequest};

#[derive(Debug, Deserialize, TS)]
#[ts(export)]
pub struct ForceRequest {
    pub value: i32,
}

/// Origin of a mutating request for attribution (ADR-0002): the
/// sanitized `X-IA2-Origin` header value (`gui`, `cs`, `hmi`, `mqtt`,
/// …). The label is self-declared — NOT authentication — so it is
/// sanitized (`state::sanitize_origin`: charset + length cap) rather
/// than trusted or silently dropped. `None` means no usable label
/// arrived: an unattributed external writer, for which the server
/// flashes the takeover overlay. This is the server crate's ONE
/// header-reading implementation — route handlers must not re-read
/// the header themselves.
pub(crate) fn origin_of(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("x-ia2-origin")
        .and_then(|v| v.to_str().ok())
        .and_then(crate::state::sanitize_origin)
}

/// Look up the live program handle or return 409. Used by every
/// debug-control endpoint below. The handle is global (one PROGRAM
/// runs at a time, hardware constraint), so this helper doesn't take
/// a project name — callers that want to know *which* project owns
/// the running program use `/api/runtime/status.running_info`.
fn live_program(state: &AppState) -> Result<ProgramHandle, ApiError> {
    state
        .program
        .lock()
        .as_ref()
        .map(|rp| rp.handle.clone())
        .ok_or(ApiError::Conflict("no program running".into()))
}

pub async fn runtime_pause(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<ModeResponse>, ApiError> {
    let handle = live_program(&state)?;
    state.maybe_attribute_external(origin_of(&headers).as_deref(), "pause".into());
    Ok(Json(ironplc_bridge::monitor::pause(&handle)))
}

pub async fn runtime_resume(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<ModeResponse>, ApiError> {
    let handle = live_program(&state)?;
    state.maybe_attribute_external(origin_of(&headers).as_deref(), "resume".into());
    Ok(Json(ironplc_bridge::monitor::resume(&handle)))
}

pub async fn runtime_step(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: Option<Json<StepRequest>>,
) -> Result<Json<ModeResponse>, ApiError> {
    let handle = live_program(&state)?;
    let cycles = body.map(|Json(r)| r.cycles).unwrap_or(1);
    state.maybe_attribute_external(origin_of(&headers).as_deref(), format!("step {cycles}"));
    Ok(Json(ironplc_bridge::monitor::step(&handle, cycles)))
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct ForceResponse {
    pub name: String,
    pub value: i32,
}

/// Pin a variable's value. The scan loop applies it on every cycle
/// until `unforce_runtime_variable` is called. 404 if the variable
/// isn't declared in this POU; 409 if nothing's running.
pub async fn force_runtime_variable(
    State(state): State<AppState>,
    _project: ProjectName,
    headers: axum::http::HeaderMap,
    AxumPath(name): AxumPath<String>,
    Json(req): Json<ForceRequest>,
) -> Result<Json<ForceResponse>, ApiError> {
    let handle = live_program(&state)?;
    state.maybe_attribute_external(origin_of(&headers).as_deref(), format!("force {name}"));
    let value = handle.force_variable(&name, req.value).await?;
    Ok(Json(ForceResponse { name, value }))
}

/// Release a forced variable. No-op (200) if the variable wasn't
/// forced — convenient for idempotent agent retries.
pub async fn unforce_runtime_variable(
    State(state): State<AppState>,
    _project: ProjectName,
    headers: axum::http::HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let handle = live_program(&state)?;
    state.maybe_attribute_external(origin_of(&headers).as_deref(), format!("unforce {name}"));
    handle.unforce_variable(&name).await?;
    Ok(Json(serde_json::json!({ "name": name, "forced": false })))
}

/// Body for `POST /api/runtime/inject-scan-stall`.
#[derive(Deserialize)]
pub struct InjectScanStallRequest {
    /// Wall-clock stall per scan, in milliseconds. Pick something
    /// comfortably over the task interval or nothing overruns.
    pub stall_ms: u64,
    /// How many consecutive scans to stall. Defaults to one more than
    /// the watchdog threshold — enough to trip it.
    #[serde(default = "default_inject_scans")]
    pub scans: u32,
}

fn default_inject_scans() -> u32 {
    ironplc_bridge::WATCHDOG_OVERRUN_THRESHOLD + 1
}

/// Fault injection: deliberately stall the next N scans so the scan
/// watchdog trips through its real code path. This is the test
/// primitive behind the `inject` scenario step — the only way a
/// scenario can drive a timing fault deterministically. On a live
/// plant this drives the runtime into latched failsafe on purpose;
/// there is no undo other than restarting the program.
pub async fn inject_scan_stall(
    State(state): State<AppState>,
    _project: ProjectName,
    headers: axum::http::HeaderMap,
    Json(req): Json<InjectScanStallRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let handle = live_program(&state)?;
    // Deliberately latching the runtime into failsafe is the LAST
    // mutating route that may run in silence — attributed like the
    // rest (ADR-0002; api.md's "every mutating runtime route" claim).
    state.maybe_attribute_external(origin_of(&headers).as_deref(), "inject-scan-stall".into());
    handle.inject_scan_stall(req.stall_ms, req.scans).await?;
    Ok(Json(
        serde_json::json!({ "injected": { "stall_ms": req.stall_ms, "scans": req.scans } }),
    ))
}

/// List currently-forced variables. Returns `[]` when nothing's
/// running (rather than 409) — easier for clients to render.
pub async fn list_runtime_forces(State(state): State<AppState>) -> Json<Vec<ForceEntry>> {
    let forces = state
        .program
        .lock()
        .as_ref()
        .map(|rp| ironplc_bridge::monitor::force_entries(&rp.handle))
        .unwrap_or_default();
    Json(forces)
}

// ============================================================
//  Edge runtime proxy — the same debug ops, on a deployed edge
// ============================================================

/// Proxy an online-debug control op to the edge runtime over ssh. `op`
/// is whitelisted (it's interpolated into the remote curl command). The
/// request body (if any) is forwarded as the JSON payload — e.g.
/// `{cycles}` for step, `{name,value}` for write/force.
///
/// The edge's own answer passes through with its ORIGINAL status and
/// body (a governance denial on the edge is a 403 here too, with the
/// edge's error text verbatim — not a 500). ADR-0002's "unlisted
/// writes are 403" must hold on every one of the four write paths,
/// and the CLI's exit-code contract (4xx = caller-fixable, 5xx =
/// infra) depends on the status surviving the ssh hop. Only transport
/// failures (ssh, curl, an unparseable response) are this server's own
/// 500s.
pub async fn edge_runtime_route(
    State(state): State<AppState>,
    project: ProjectName,
    headers: axum::http::HeaderMap,
    AxumPath((name, op)): AxumPath<(String, String)>,
    body: Option<Json<serde_json::Value>>,
) -> Result<axum::response::Response, ApiError> {
    const ALLOWED: [&str; 6] = ["pause", "resume", "step", "write", "force", "unforce"];
    if !ALLOWED.contains(&op.as_str()) {
        return Err(ApiError::BadRequest(format!("unknown runtime op '{op}'")));
    }
    let origin = origin_of(&headers);
    // An unattributed writer reaching the plant THROUGH the IDE still
    // flashes the takeover overlay (ADR-0002).
    state.maybe_attribute_external(origin.as_deref(), format!("{op} on edge {name}"));
    let edge = crate::routes::with_project(&state, &project, |store| {
        store.read_edge(&name).map_err(Into::into)
    })?;
    let payload = body
        .map(|Json(v)| v)
        .unwrap_or_else(|| serde_json::json!({}));
    let result = crate::edges::proxy_edge_runtime_op(&edge, &op, &payload, origin.as_deref()).await;
    edge_proxy_response(result)
}

/// Map the typed proxy result onto the HTTP response: 2xx JSON is
/// re-served as JSON, a non-2xx edge answer is replayed status+body
/// verbatim, and only transport faults become this server's own 500.
/// Split out of the handler so the passthrough is unit-testable.
fn edge_proxy_response(
    result: Result<serde_json::Value, crate::edges::EdgeRuntimeError>,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::IntoResponse as _;
    match result {
        Ok(v) => Ok(Json(v).into_response()),
        Err(crate::edges::EdgeRuntimeError::Status { code, body }) => {
            let status = axum::http::StatusCode::from_u16(code)
                .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
            Ok((status, body).into_response())
        }
        Err(crate::edges::EdgeRuntimeError::Transport(e)) => Err(ApiError::Internal(e)),
    }
}

/// Proxy the edge runtime's write-audit ring (`GET /audit`). The ring
/// is the agent-facing "who wrote what" record and the edge binds
/// localhost-only — without this proxy (and the matching
/// `cs get edges/<n>/audit` path) the one resource built FOR
/// auditability would be the only edge read requiring a hand-rolled
/// ssh tunnel.
pub async fn edge_audit_route(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let edge = crate::routes::with_project(&state, &project, |store| {
        store.read_edge(&name).map_err(Into::into)
    })?;
    crate::edges::fetch_edge_json(&edge, "/audit")
        .await
        .map(Json)
        .map_err(ApiError::Internal)
}

#[cfg(test)]
mod edge_proxy_tests {
    use super::*;
    use crate::edges::EdgeRuntimeError;

    /// The edge's governance 403 must survive the proxy hop with its
    /// status and body intact — a 500 here would misreport a policy
    /// denial as an infrastructure failure (and flip the CLI's exit
    /// code from 2 to 3).
    #[tokio::test]
    async fn edge_4xx_passes_through_with_status_and_body_verbatim() {
        let denied = edge_proxy_response(Err(EdgeRuntimeError::Status {
            code: 403,
            body: "write to 'x' rejected by project governance".into(),
        }))
        .expect("a status answer is a response, not an ApiError");
        assert_eq!(denied.status(), axum::http::StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(denied.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(bytes, "write to 'x' rejected by project governance");

        let conflict = edge_proxy_response(Err(EdgeRuntimeError::Status {
            code: 409,
            body: "no program running".into(),
        }))
        .unwrap();
        assert_eq!(conflict.status(), axum::http::StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn edge_success_is_served_as_json() {
        let ok = edge_proxy_response(Ok(serde_json::json!({ "ok": true }))).unwrap();
        assert_eq!(ok.status(), axum::http::StatusCode::OK);
    }

    #[test]
    fn transport_failures_stay_this_servers_own_500() {
        let err = edge_proxy_response(Err(EdgeRuntimeError::Transport(
            "ssh to pi failed: connection refused".into(),
        )))
        .expect_err("transport faults map to ApiError");
        assert!(matches!(err, ApiError::Internal(msg) if msg.contains("ssh to pi failed")));
    }
}
