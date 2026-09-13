import { encodeForWrite } from "@/lib/write-encoding"
import type { AlarmState } from "@/types/generated/AlarmState"
import type { AttachInfo } from "@/types/generated/AttachInfo"
import type { AttachmentStatus } from "@/types/generated/AttachmentStatus"
import type { CheckDiagnostic } from "@/types/generated/CheckDiagnostic"
import type { DemoSlaveSnapshot } from "@/types/generated/DemoSlaveSnapshot"
import type { DeployReport } from "@/types/generated/DeployReport"
import type { Device } from "@/types/generated/Device"
import type { Edge } from "@/types/generated/Edge"
import type { EdgeProbe } from "@/types/generated/EdgeProbe"
import type { FsListing } from "@/types/generated/FsListing"
import type { HistoryResponse } from "@/types/generated/HistoryResponse"
import type { IoMap } from "@/types/generated/IoMap"
import type { ProjectInfo } from "@/types/generated/ProjectInfo"
import type { MigrationResponse } from "@/types/generated/MigrationResponse"
import type { Pou } from "@/types/generated/Pou"
import type { PouLanguage } from "@/types/generated/PouLanguage"
import type { PouType } from "@/types/generated/PouType"
import type { OpenProjectsList } from "@/types/generated/OpenProjectsList"
import type { ProjectListing } from "@/types/generated/ProjectListing"
import type { ProjectPous } from "@/types/generated/ProjectPous"
import type { ProjectTree } from "@/types/generated/ProjectTree"
import type { OpcuaBrowseNode } from "@/types/generated/OpcuaBrowseNode"
import type { Protocol } from "@/types/generated/Protocol"
import type { RunResponse } from "@/types/generated/RunResponse"
import type { RuntimeStatus } from "@/types/generated/RuntimeStatus"
import type { Tasks } from "@/types/generated/Tasks"
import type { VariableInfo } from "@/types/generated/VariableInfo"

async function jsonOrThrow<T>(res: Response, label: string): Promise<T> {
  if (!res.ok) {
    const text = await res.text().catch(() => "")
    throw new Error(`${label} → ${res.status} ${text}`.trim())
  }
  return res.json() as Promise<T>
}

// ---------- Multi-project routing --------------------------------
//
// Every window in the IDE picks its target project from the URL
// `?project=<name>` search parameter. The server-side routes take
// the project from an `X-IA2-Project` request header (with a fallback
// to "the active project" so legacy single-window flows still work).
//
// Two helpers wire this together:
//
//   - `currentProject()` reads the URL fresh each call (no module-
//     level cache; tabs that change their `?project=` via History
//     API see the new value next request).
//   - `apiFetch()` is a thin wrapper around `fetch` that attaches
//     the header when a project is known. All API functions below
//     use it instead of bare `fetch` so the routing is consistent.
//
// We deliberately do NOT prefix the URL with the project name — keeps
// URLs flat and lets the server treat the header as a per-request
// concern, not a routing concern. (URL paths still look like
// `/api/pous/main`.)

/** Read the active project's name out of the URL's `?project=` search
 * param. Returns `null` if absent — callers (and the server) fall back
 * to whatever the server is treating as "active". */
export function currentProject(): string | null {
  if (typeof window === "undefined") return null
  const params = new URLSearchParams(window.location.search)
  const name = params.get("project")
  return name && name.length > 0 ? name : null
}

/** `fetch` with the `X-IA2-Project` header injected automatically.
 * Use this instead of bare `fetch` for every API call. Also declares
 * `X-IA2-Origin: gui` so the server can tell the human at the console
 * apart from unattributed writers on mutating routes (ADR-0002). */
export async function apiFetch(input: string, init?: RequestInit): Promise<Response> {
  // Merge with any caller-supplied headers; the caller wins if they
  // explicitly set X-IA2-Project (unusual, but supported).
  const headers = new Headers(init?.headers)
  if (!headers.has("X-IA2-Origin")) {
    headers.set("X-IA2-Origin", "gui")
  }
  const project = currentProject()
  if (project && !headers.has("X-IA2-Project")) {
    headers.set("X-IA2-Project", encodeURIComponent(project))
    headers.set("X-IA2-Project-Encoding", "percent")
  }
  return fetch(input, { ...init, headers })
}

// ---------- Project lifecycle ----------

/** Returns null when no project is open (server replies 409). */
export async function fetchProject(): Promise<ProjectTree | null> {
  const res = await apiFetch(`/api/project`)
  if (res.status === 409) return null
  return jsonOrThrow<ProjectTree>(res, "GET /api/project")
}

export async function fetchProjects(): Promise<ProjectListing[]> {
  return jsonOrThrow(await apiFetch(`/api/projects`), "GET /api/projects")
}

/** Browse a server-side directory for the Open-project folder picker.
 * Pass no path to start at the default projects dir. */
export async function browseFs(path?: string): Promise<FsListing> {
  const q = path ? `?path=${encodeURIComponent(path)}` : ""
  return jsonOrThrow(await apiFetch(`/api/fs/browse${q}`), `GET /api/fs/browse${q}`)
}

/** List projects currently OPEN on the server (multi-window picker
 * source). Distinct from `fetchProjects` which scans disk. */
export async function fetchOpenProjects(): Promise<OpenProjectsList> {
  return jsonOrThrow(
    await apiFetch(`/api/projects/open-list`),
    "GET /api/projects/open-list",
  )
}

export async function createProject(name: string): Promise<ProjectInfo> {
  return jsonOrThrow(
    await apiFetch(`/api/projects`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name }),
    }),
    "POST /api/projects",
  )
}

export async function openProject(
  path: string,
  opts?: { signal?: AbortSignal },
): Promise<ProjectInfo> {
  return jsonOrThrow(
    await apiFetch(`/api/projects/open`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path }),
      signal: opts?.signal,
    }),
    "POST /api/projects/open",
  )
}

export async function closeProject(): Promise<RunResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/projects/close`, { method: "POST" }),
    "POST /api/projects/close",
  )
}

// ---------- POUs (`.st` files holding 1+ IEC declarations) ----------

export async function fetchPou(path: string): Promise<Pou> {
  return jsonOrThrow(
    await apiFetch(`/api/pous/${encodeURIComponent(path)}`),
    `GET /api/pous/${path}`,
  )
}

export async function createPou(
  path: string,
  type_: PouType,
  language: PouLanguage = "st",
): Promise<Pou> {
  return jsonOrThrow(
    await apiFetch(`/api/pous`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path, type: type_, language }),
    }),
    "POST /api/pous",
  )
}

export async function savePou(
  path: string,
  source: string,
): Promise<RunResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/pous/${encodeURIComponent(path)}`, {
      method: "PUT",
      headers: { "Content-Type": "text/plain" },
      body: source,
    }),
    `PUT /api/pous/${path}`,
  )
}

export async function deletePou(path: string): Promise<RunResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/pous/${encodeURIComponent(path)}`, {
      method: "DELETE",
    }),
    `DELETE /api/pous/${path}`,
  )
}

export async function createPouFolder(path: string): Promise<RunResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/pous/folders`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path }),
    }),
    "POST /api/pous/folders",
  )
}

export async function fetchPouVariables(path: string): Promise<VariableInfo[]> {
  return jsonOrThrow(
    await apiFetch(`/api/pous/${encodeURIComponent(path)}/variables`),
    `GET /api/pous/${path}/variables`,
  )
}

// ---------- Devices ----------

export async function fetchDevice(name: string): Promise<Device> {
  return jsonOrThrow(
    await apiFetch(`/api/devices/${encodeURIComponent(name)}`),
    `GET /api/devices/${name}`,
  )
}

export async function createDevice(
  name: string,
  protocol: Protocol,
): Promise<Device> {
  return jsonOrThrow(
    await apiFetch(`/api/devices`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name, protocol }),
    }),
    "POST /api/devices",
  )
}

export async function updateDevice(
  name: string,
  device: Device,
): Promise<RunResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/devices/${encodeURIComponent(name)}`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(device),
    }),
    `PUT /api/devices/${name}`,
  )
}

export async function deleteDevice(name: string): Promise<RunResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/devices/${encodeURIComponent(name)}`, {
      method: "DELETE",
    }),
    `DELETE /api/devices/${name}`,
  )
}

/** Live-browse one level of an OPC UA device's address space.
 *  `nodeId` empty/undefined = ObjectsFolder. */
export async function opcuaBrowse(
  name: string,
  nodeId?: string,
): Promise<OpcuaBrowseNode[]> {
  return jsonOrThrow(
    await apiFetch(`/api/devices/${encodeURIComponent(name)}/opcua-browse`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ node_id: nodeId ?? null }),
    }),
    `POST /api/devices/${name}/opcua-browse`,
  )
}

export async function createDeviceFolder(path: string): Promise<RunResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/devices/folders`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path }),
    }),
    "POST /api/devices/folders",
  )
}

// ---------- Tasks (project-level scheduling) ----------

export async function fetchTasks(): Promise<Tasks> {
  return jsonOrThrow(await apiFetch(`/api/tasks`), "GET /api/tasks")
}

export async function updateTasks(tasks: Tasks): Promise<RunResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/tasks`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(tasks),
    }),
    "PUT /api/tasks",
  )
}

export async function migrateTasks(): Promise<MigrationResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/project/migrate-tasks`, { method: "POST" }),
    "POST /api/project/migrate-tasks",
  )
}

export async function fetchProjectPous(): Promise<ProjectPous> {
  return jsonOrThrow(
    await apiFetch(`/api/project/pous`),
    "GET /api/project/pous",
  )
}

// ---------- IO Mapping ----------

export async function fetchIomap(): Promise<IoMap> {
  return jsonOrThrow(await apiFetch(`/api/iomap`), "GET /api/iomap")
}

export async function updateIomap(iomap: IoMap): Promise<RunResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/iomap`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(iomap),
    }),
    "PUT /api/iomap",
  )
}

// ---------- Runtime ----------

/**
 * Symbol extraction — variables + FB instances visible to the POU.
 * Drives Monaco's hover & variable completion providers in
 * STEditor; the graphical editors mostly use their own JSON-derived
 * variable lists, but this endpoint is the language-agnostic
 * supply.
 *
 * Same content-type / language conventions as `checkProgram`.
 */
export async function fetchSymbols(
  source: string,
  language: "st" | "ld" | "fbd" | "sfc" = "st",
): Promise<import("@/types/generated/VariableInfo").VariableInfo[]> {
  const url =
    language === "st"
      ? `/api/symbols`
      : `/api/symbols?language=${language}`
  const contentType =
    language === "st" ? "text/plain" : "application/json"
  return jsonOrThrow(
    await apiFetch(url, {
      method: "POST",
      headers: { "Content-Type": contentType },
      body: source,
    }),
    "POST /api/symbols",
  )
}

export async function checkProgram(
  source: string,
  language: "st" | "ld" | "fbd" | "sfc" = "st",
  path?: string,
): Promise<CheckDiagnostic[]> {
  // ST source is plain text; LD / FBD source is JSON. Different
  // Content-Type plus a `?language=` query so the bridge knows what
  // shape to expect before running ironplc. Without the query the
  // server defaults to ST for back-compat with older clients.
  //
  // The check runs against the whole open project (sibling files'
  // FUNCTION_BLOCKs resolve); `path` names the slug this buffer was
  // loaded from so its on-disk copy doesn't double-declare.
  const params = new URLSearchParams()
  if (language !== "st") params.set("language", language)
  if (path) params.set("path", path)
  const qs = params.toString()
  const url = qs ? `/api/check?${qs}` : `/api/check`
  const contentType =
    language === "st" ? "text/plain" : "application/json"
  return jsonOrThrow(
    await apiFetch(url, {
      method: "POST",
      headers: { "Content-Type": contentType },
      body: source,
    }),
    "POST /api/check",
  )
}

// ---------- Libraries ----------

export async function fetchLibraries(): Promise<
  import("@/types/generated/LibrarySummary").LibrarySummary[]
> {
  return jsonOrThrow(await apiFetch(`/api/library`), "GET /api/library")
}

/** Import registry blocks (empty `blocks` = the whole library).
 *  Re-importing overwrites — that's the update path. */
export async function importLibrary(
  library: string,
  blocks: string[] = [],
): Promise<import("@/types/generated/ImportLibraryResponse").ImportLibraryResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/library/import`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ library, blocks }),
    }),
    "POST /api/library/import",
  )
}

export async function removeLibrary(name: string): Promise<RunResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/library/${encodeURIComponent(name)}`, {
      method: "DELETE",
    }),
    `DELETE /api/library/${name}`,
  )
}

/**
 * Run a project.
 *
 * - No args → use the project's `tasks.toml` schedule (every PROGRAM
 *   instance declared there). The "production" path.
 * - `(program, file_path)` → ad-hoc one-shot: spawn a synthetic single-
 *   PROGRAM schedule on a default 100 ms task. When `file_path` is also
 *   set, the compile input is limited to that file alone — Monitor only
 *   sees the running PROGRAM's variables (no bleed from other POUs).
 */
export async function runProgram(
  program?: string,
  file_path?: string,
): Promise<RunResponse> {
  const body = program ? (file_path ? { program, file_path } : { program }) : {}
  return jsonOrThrow(
    await apiFetch(`/api/run`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }),
    "POST /api/run",
  )
}

export async function stopProgram(): Promise<RunResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/stop`, { method: "POST" }),
    "POST /api/stop",
  )
}

export function eventsUrl(): string {
  return `/api/events`
}

/** Snapshot of "what is the runtime doing right now" — same struct the
 *  agent docs / GET endpoint surface. The IDE uses this on mount and
 *  after detach to recover `running_info` (the page-reload case where
 *  no SSE `started` event will fire for an already-running program). */
export async function fetchRuntimeStatus(): Promise<RuntimeStatus> {
  return jsonOrThrow(
    await apiFetch(`/api/runtime/status`),
    "GET /api/runtime/status",
  )
}

// ---------- Runtime historian + alarms ----------

/** Downsampled history for one or more variables. Timestamps share the
 *  live snapshot's time base (scan-relative micros), so seeded points
 *  land on the same axis the SSE feed pushes onto — see
 *  `historyToSamples` / `seedTimedBuffer`. `stepMs` picks the bucket
 *  size (1000 = the native 1 Hz store); `fromUs`/`toUs` default to 0,
 *  which the runtime reads as "the whole retained window". */
export async function fetchRuntimeHistory(
  vars: string[],
  opts?: { fromUs?: bigint | number; toUs?: bigint | number; stepMs?: number },
): Promise<HistoryResponse> {
  const params = new URLSearchParams({
    vars: vars.join(","),
    from_us: String(opts?.fromUs ?? 0),
    to_us: String(opts?.toUs ?? 0),
    step_ms: String(opts?.stepMs ?? 1000),
  })
  return jsonOrThrow(
    await apiFetch(`/api/runtime/history?${params.toString()}`),
    "GET /api/runtime/history",
  )
}

/** Current alarm states, sorted standing-first by the runtime. */
export async function fetchRuntimeAlarms(): Promise<AlarmState[]> {
  return jsonOrThrow(
    await apiFetch(`/api/runtime/alarms`),
    "GET /api/runtime/alarms",
  )
}

/** Acknowledge one alarm; returns its updated state (404 = unknown id). */
export async function ackRuntimeAlarm(id: string): Promise<AlarmState> {
  return jsonOrThrow(
    await apiFetch(`/api/runtime/alarms/${encodeURIComponent(id)}/ack`, {
      method: "POST",
    }),
    `POST /api/runtime/alarms/${id}/ack`,
  )
}

/** Write a single variable on the running bridge.
 *
 *  The runtime's write primitive is `write_variable(VarIndex, i32)`,
 *  so the payload is always an i32. This helper handles the mapping
 *  from JS `number` + IEC type name:
 *
 *    - BOOL                                     → 0 or 1
 *    - SINT/INT/DINT/USINT/UINT/UDINT/BYTE/...  → truncated integer
 *    - REAL (32-bit float)                      → IEEE-754 bit pattern
 *      (interpret the bytes of the float as an i32 so the VM sees
 *      the right f32 in the slot)
 *    - LREAL / 64-bit ints                      → not supported via
 *      this endpoint yet; caller should warn or skip.
 *
 *  Throws if the runtime isn't running or the variable doesn't exist.
 */
export async function writeVariable(
  name: string,
  value: number,
  typeName: string = "",
  pulseMs?: number,
): Promise<void> {
  const i32 = encodeForWrite(value, typeName)
  await jsonOrThrow(
    await apiFetch(`/api/runtime/variables/${encodeURIComponent(name)}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(
        pulseMs != null ? { value: i32, pulse_ms: pulseMs } : { value: i32 },
      ),
    }),
    `POST /api/runtime/variables/${name}`,
  )
}

// ---------- Runtime debug controls ----------

/** Halt the scan loop. IO is frozen; the program does not advance.
 *  Variable writes and forces still apply while paused. */
export async function pauseRuntime(): Promise<void> {
  await jsonOrThrow(
    await apiFetch(`/api/runtime/pause`, { method: "POST" }),
    "POST /api/runtime/pause",
  )
}

/** Resume continuous scanning. */
export async function resumeRuntime(): Promise<void> {
  await jsonOrThrow(
    await apiFetch(`/api/runtime/resume`, { method: "POST" }),
    "POST /api/runtime/resume",
  )
}

/** Run `cycles` scan rounds then auto-pause (default 1). */
export async function stepRuntime(cycles: number = 1): Promise<void> {
  await jsonOrThrow(
    await apiFetch(`/api/runtime/step`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ cycles }),
    }),
    "POST /api/runtime/step",
  )
}

/** Pin a variable to a value across scans (force). The IEC-style i32
 *  encoding is the same as writeVariable. */
export async function forceVariable(
  name: string,
  value: number,
  typeName: string = "",
): Promise<void> {
  const i32 = encodeForWrite(value, typeName)
  await jsonOrThrow(
    await apiFetch(`/api/runtime/forces/${encodeURIComponent(name)}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ value: i32 }),
    }),
    `POST /api/runtime/forces/${name}`,
  )
}

/** Release a forced variable. Idempotent — no-op if not currently forced. */
export async function unforceVariable(name: string): Promise<void> {
  await jsonOrThrow(
    await apiFetch(`/api/runtime/forces/${encodeURIComponent(name)}`, {
      method: "DELETE",
    }),
    `DELETE /api/runtime/forces/${name}`,
  )
}

// encodeForWrite lives in `lib/write-encoding.ts` — shared with the
// standalone HMI panel, which writes through the edge runtime instead
// of this API layer.

export async function fetchDemoSlaveSnapshot(): Promise<DemoSlaveSnapshot> {
  return jsonOrThrow(await apiFetch(`/api/_demo/slave`), "GET /api/_demo/slave")
}

// ---------- Edges ----------

export async function fetchEdge(name: string): Promise<Edge> {
  return jsonOrThrow(
    await apiFetch(`/api/edges/${encodeURIComponent(name)}`),
    `GET /api/edges/${name}`,
  )
}

export async function createEdge(name: string, host: string): Promise<Edge> {
  return jsonOrThrow(
    await apiFetch(`/api/edges`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name, host }),
    }),
    "POST /api/edges",
  )
}

export async function updateEdge(name: string, edge: Edge): Promise<RunResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/edges/${encodeURIComponent(name)}`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(edge),
    }),
    `PUT /api/edges/${name}`,
  )
}

export async function deleteEdge(name: string): Promise<RunResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/edges/${encodeURIComponent(name)}`, { method: "DELETE" }),
    `DELETE /api/edges/${name}`,
  )
}

export async function createEdgeFolder(path: string): Promise<RunResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/edges/folders`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path }),
    }),
    "POST /api/edges/folders",
  )
}

export async function probeEdge(name: string): Promise<EdgeProbe> {
  return jsonOrThrow(
    await apiFetch(`/api/edges/${encodeURIComponent(name)}/probe`),
    `GET /api/edges/${name}/probe`,
  )
}

export async function deployEdge(name: string): Promise<DeployReport> {
  return jsonOrThrow(
    await apiFetch(`/api/edges/${encodeURIComponent(name)}/deploy`, {
      method: "POST",
    }),
    `POST /api/edges/${name}/deploy`,
  )
}

export async function attachEdge(name: string): Promise<AttachInfo> {
  return jsonOrThrow(
    await apiFetch(`/api/edges/${encodeURIComponent(name)}/attach`, {
      method: "POST",
    }),
    `POST /api/edges/${name}/attach`,
  )
}

export async function detachEdge(name: string): Promise<RunResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/edges/${encodeURIComponent(name)}/detach`, {
      method: "POST",
    }),
    `POST /api/edges/${name}/detach`,
  )
}

// --- Edge introspection: logs / discover / system ---
// Lightweight inline types (mirror the runtime's wire shapes). These could
// be replaced by ts-rs-generated bindings later.

export interface EdgeLogs {
  lines: string[]
}

/** Recent edge-runtime log lines (discovery, bus health, connect errors). */
export async function fetchEdgeLogs(name: string, tail = 300): Promise<EdgeLogs> {
  return jsonOrThrow(
    await apiFetch(`/api/edges/${encodeURIComponent(name)}/logs?tail=${tail}`),
    `GET /api/edges/${name}/logs`,
  )
}

export interface DiscoveredSlave {
  index: number
  name: string
  vendor_id: number
  product_id: number
  input_bytes: number
  output_bytes: number
}

export interface DeviceReport {
  name: string
  protocol: string
  connected: boolean
  error: string | null
  slaves: DiscoveredSlave[]
}

/** Per-device connect status + discovered EtherCAT topology. */
export async function discoverEdge(name: string): Promise<DeviceReport[]> {
  return jsonOrThrow(
    await apiFetch(`/api/edges/${encodeURIComponent(name)}/discover`),
    `GET /api/edges/${name}/discover`,
  )
}

export interface EdgeNic {
  name: string
  mac: string
  operstate: string
  carrier: boolean
}

export interface EdgeSystem {
  arch: string
  os: string
  nics: EdgeNic[]
  serial_ports: string[]
}

/** Edge interfaces / serial ports / arch — for authoring device configs. */
export async function fetchEdgeSystem(name: string): Promise<EdgeSystem> {
  return jsonOrThrow(
    await apiFetch(`/api/edges/${encodeURIComponent(name)}/system`),
    `GET /api/edges/${name}/system`,
  )
}

export interface EdgeRuntimeMode {
  kind: string
  remaining?: number
}
export interface EdgeForce {
  name: string
  value: number
}
export interface EdgeSnapshotVar {
  name: string
  type_name: string
  value: string
}
export interface EdgeStatus {
  project: string
  scan_count: number
  uptime_secs: number
  mode: EdgeRuntimeMode
  forces: EdgeForce[]
  last_snapshot: { scan_count: number; vars: EdgeSnapshotVar[] } | null
}

/** Edge runtime status: debug mode + forces + last snapshot (live vars). */
export async function fetchEdgeStatus(name: string): Promise<EdgeStatus> {
  return jsonOrThrow(
    await apiFetch(`/api/edges/${encodeURIComponent(name)}/status`),
    `GET /api/edges/${name}/status`,
  )
}

/** Online-debug control op (pause/resume/step/write/force/unforce). */
export async function edgeRuntimeOp(
  name: string,
  op: "pause" | "resume" | "step" | "write" | "force" | "unforce",
  body?: Record<string, unknown>,
): Promise<unknown> {
  return jsonOrThrow(
    await apiFetch(`/api/edges/${encodeURIComponent(name)}/runtime/${op}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body ?? {}),
    }),
    `POST /api/edges/${name}/runtime/${op}`,
  )
}

export async function fetchAttachment(name: string): Promise<AttachmentStatus> {
  return jsonOrThrow(
    await apiFetch(`/api/edges/${encodeURIComponent(name)}/attachment`),
    `GET /api/edges/${name}/attachment`,
  )
}

// ============================================================
//  HMI screens
// ============================================================

import type { HmiDoc } from "@/types/generated/HmiDoc"
import type { HmiIssue } from "@/types/generated/HmiIssue"
import type { HmiListEntry } from "@/types/generated/HmiListEntry"
import type { HmiOp } from "@/types/generated/HmiOp"
import type { HmiOpsResponse } from "@/types/generated/HmiOpsResponse"
import type { HmiSymbolInfo } from "@/types/generated/HmiSymbolInfo"

export async function fetchHmis(): Promise<HmiListEntry[]> {
  return jsonOrThrow(await apiFetch(`/api/hmi`), "GET /api/hmi")
}

export async function fetchHmi(path: string): Promise<HmiDoc> {
  return jsonOrThrow(
    await apiFetch(`/api/hmi/${encodeURIComponent(path)}`),
    "GET /api/hmi/{path}",
  )
}

export async function createHmi(
  path: string,
  title?: string,
): Promise<HmiDoc> {
  return jsonOrThrow(
    await apiFetch(`/api/hmi`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path, title }),
    }),
    "POST /api/hmi",
  )
}

export async function saveHmi(
  path: string,
  doc: HmiDoc,
): Promise<HmiIssue[]> {
  return jsonOrThrow(
    await apiFetch(`/api/hmi/${encodeURIComponent(path)}`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(doc),
    }),
    "PUT /api/hmi/{path}",
  )
}

export async function deleteHmi(path: string): Promise<void> {
  await jsonOrThrow(
    await apiFetch(`/api/hmi/${encodeURIComponent(path)}`, {
      method: "DELETE",
    }),
    "DELETE /api/hmi/{path}",
  )
}

export async function hmiOps(
  path: string,
  ops: HmiOp[],
): Promise<HmiOpsResponse> {
  return jsonOrThrow(
    await apiFetch(`/api/hmi/${encodeURIComponent(path)}/ops`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ ops }),
    }),
    "POST /api/hmi/{path}/ops",
  )
}

export async function generateHmi(
  path: string,
  opts?: { force?: boolean; title?: string },
): Promise<HmiDoc> {
  return jsonOrThrow(
    await apiFetch(`/api/hmi/${encodeURIComponent(path)}/generate`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(opts ?? {}),
    }),
    "POST /api/hmi/{path}/generate",
  )
}

export async function checkHmi(path: string): Promise<HmiIssue[]> {
  return jsonOrThrow(
    await apiFetch(`/api/hmi/${encodeURIComponent(path)}/check`),
    "GET /api/hmi/{path}/check",
  )
}

export async function fetchHmiSymbols(): Promise<HmiSymbolInfo[]> {
  return jsonOrThrow(await apiFetch(`/api/hmi-symbols`), "GET /api/hmi-symbols")
}

import type { ProjectVariables } from "@/types/generated/ProjectVariables"

/** Every variable declared in any POU, with its file + direction — the
 *  HMI editor's binding autocomplete source. */
export async function fetchProjectVariables(): Promise<ProjectVariables> {
  return jsonOrThrow(
    await apiFetch(`/api/project/variables`),
    "GET /api/project/variables",
  )
}
