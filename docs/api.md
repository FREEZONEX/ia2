# IA2 HTTP API

**Audience:** AI agents (Claude Code, Cursor, Codex) and humans curling for diagnosis.

**Address:** `http://127.0.0.1:3001` (the IDE backend). The edge runtime binary
serves a smaller subset on its own port — see `docs/edge-deploy.md`.

**Auth:** none (localhost-only). Remote access via SSH port-forward.

**Conventions:**
- Resources are plural nouns under `/api/<resource>`.
- Names with `/` (folder-nested POUs, devices, edges) are URL-encoded — the
  `%2F`-encoded form decodes back to `/` inside the path param. E.g.,
  `GET /api/pous/pid_loops%2Ftemperature`.
- All bodies are JSON unless noted. POU sources are `text/plain`.
- Select an open project with `X-IA2-Project`. To send Unicode names,
  percent-encode the UTF-8 bytes and add `X-IA2-Project-Encoding: percent`
  (for example, `demo%20line` selects `demo line`). The IDE and CLI apply
  this encoding automatically. Without the encoding header, legacy values
  remain literal: `a%20b` selects the name `a%20b`. Unsupported encodings
  and malformed encoded values return HTTP 400 instead of falling back
  to the active project.
- Errors are HTTP status + a human-readable body. 4xx for client errors,
  5xx for server bugs.
- Generated TypeScript types live under `apps/web/src/types/generated/` and
  are the source of truth for request/response shapes.

---

## Document versions and concurrent edits

`GET /api/pous/{path}`, `/api/hmi/{path}`, `/api/devices/{name}`,
`/api/edges/{name}`, `/api/iomap`, `/api/tasks`, `/api/alarms`, and
`/api/northbound` return a quoted strong `ETag`, derived from backing-file
bytes (including whether an optional config file exists). Keep that version
with the document you read. On the corresponding whole-document `PUT`, send
`If-Match: "<version>"`. If another writer has changed the file, the server
returns **412 Precondition Failed** without writing; re-read the document
and reapply the edit. Weak `If-Match` tags never authorize a write.
`If-None-Match: *` allows creation only when the backing file is absent.

The comparison and write hold the same project lock, so concurrent API
writers cannot both replace one version. Hashes survive restarts and detect
edits made outside the server. Direct filesystem writers do not take this
lock and can still race during the final check/write interval; use the API
when co-editing. This is conflict detection, not automatic merging.

Successful whole-document PUTs return the resulting document version in
`X-IA2-Version`; named-document creation POSTs return `ETag`. The separate
PUT header respects RFC 9110 §9.3.4 because JSON writes can be normalized
into TOML; it is usable as the next `If-Match` without a racy follow-up GET.
JSON response bodies are unchanged. Legacy raw HTTP
and `cs api` calls without precondition headers retain their unconditional
behavior. `cs set` is deliberately stricter: replacements require the
original `--if-match <ETag|@FILE>` or explicit `--force`. `cs get --etag-file
FILE` preserves stdout and saves that read's version separately. Named
resources created by `cs set` need neither flag; the CLI uses the creation
response's version for its initial write. Single-document configs always
have a GET representation (defaults when absent), so read their version
before setting them. HTTP 412 remains CLI exit **2**.

Clients must associate versions with individual edit buffers, not a global
latest-version cache: another agent/tab's new GET must never validate an
older buffer. A conflict must preserve the local edits and stop Save/Run.

## Health & lifecycle

| Method | Path | Purpose | Notes |
|---|---|---|---|
| `GET` | `/health` | Liveness. Returns `HealthStatus`. | Convenience root path |
| `GET` | `/api/health` | Same as `/health` under the `/api` namespace | For agents that scope to `/api` |
| `GET` | `/api/projects` | List discoverable projects. Returns `ProjectListing[]`. | |
| `POST` | `/api/projects` | Create a new project. Body: `CreateProjectRequest`. | |
| `POST` | `/api/projects/open` | Open an existing project. Body: `OpenProjectRequest { path }`. | |
| `POST` | `/api/projects/close` | Close the currently-open project. | |
| `GET` | `/api/projects/open-list` | Every project the server currently has open + which is the active fallback. Returns `OpenProjectsList`. | Multi-window IDE picker |
| `GET` | `/api/fs/browse?path=` | List the sub-directories of `path` (default `~/Documents/IA2`) for the Open-project folder picker — directories only, dotfiles hidden, each flagged `is_project` (has a `project.toml`). Returns `FsListing`. | A browser has no native OS folder dialog |
| `GET` | `/api/project` | Full project tree (applications, devices, edges, iomap, tasks, folder lists). Returns `ProjectTree` or `null` when no project is open. | |
| `POST` | `/api/project/migrate-tasks` | One-shot migrate inline-CONFIGURATION blocks in POU files into `tasks.toml`. Idempotent. Returns `MigrationResponse`. | Legacy projects only |
| `POST` | `/api/project/validate` | Compile and statically validate the I/O map without starting devices. Returns `Vec<CheckDiagnostic>` (empty = ok). Recognizes configured EtherCAT gear routes: seven read/write parameters and two read-only feedback channels; unknown names, feedback writes, gear/PDO name collisions on referenced devices, and an Output mapping onto the PDO bytes an in-cycle gear engine writes each scan are errors (collisions previously surfaced only when the device connected; the engine-owned bytes were previously accepted and then silently discarded every cycle). Unreferenced devices still need connect-time validation. | Pre-flight check before Run/Deploy |

`/api/project/validate` also lints `alarms.toml`. An alarm whose `variable`
names nothing the project declares is an **error** (`alarms-validate`): the
engine matches snapshot names exactly and skips an unmatched definition
quietly, so such a definition never evaluates and `GET /alarms` reports it as
a calm, never-raised, already-acknowledged entry — a green line claiming
coverage that does not exist. Both spellings a snapshot can carry are accepted
— bare, and `instance.variable` for a name more than one PROGRAM instance
declares. A bare name that is in fact *shared* is NOT caught: the snapshot
qualifies it and the alarm is dead anyway, but distinguishing that needs
per-instance variable sets, which static extraction does not carry.

It also lints each **device document against itself** (`device-validate`, all
errors). Two findings, both of which used to pass silently on Modbus, OPC UA
and CANopen (EtherCAT already refused the first at connect): a channel name
declared more than once — every adapter keys its channel table by name, so the
repeat overwrites and the earlier channel stops existing while mappings onto
that name move to the survivor, and `iomap-validate` compounds it by resolving
the same name to the *first* match while the adapter keeps the *last*; and an
OPC UA or CANopen `failsafe` on a channel whose `access` is not `write`, which
the failsafe sweep skips, so a configured safe state can never be applied. The
duplicate-name half is also refused by the adapters themselves at connect,
because the edge runtime runs no project validation at all.

## POUs

A POU is one IEC declaration (PROGRAM / FUNCTION_BLOCK / FUNCTION). A
single `.st` file may declare multiple POUs; the file is the unit on
disk, and the tree (in `/api/project`) shows each declaration as its
own node. The URL identifier in these routes is the **file path** —
slash-separated under `pous/`, no `.st` extension.

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/api/pous` | Create a POU file. Body: `CreatePouRequest { path, type, language }`. `type` is `program` / `function_block` / `function`; `language` is `st` / `ld` / `fbd` / `sfc` (picks the on-disk extension). |
| `POST` | `/api/pous/folders` | Create a folder under `pous/`. Body: `CreateFolderRequest { path }`. |
| `DELETE` | `/api/pous/folders/{path}` | Delete an empty folder. |
| `GET` | `/api/pous/{path}` | Read a POU file. Returns `Pou { path, source, declarations: PouDecl[] }`. |
| `PUT` | `/api/pous/{path}` | Write POU source. Body is raw `text/plain`. |
| `DELETE` | `/api/pous/{path}` | Delete a POU file (and every declaration inside it). |
| `GET` | `/api/pous/{path}/variables` | Variables declared in the file. Returns `VariableInfo[]`. |

## Libraries & device catalog

First-class FB libraries (vendored into `pous/lib/<name>/`) and the
read-only device-template catalog used to pre-fill devices from a bus scan.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/api/hmi` | List the project's HMI screens: `HmiListEntry[] { path, title, level }`. |
| `POST` | `/api/hmi` | Create an empty screen. Body `{ path, title? }`; returns the fresh `HmiDoc`. Emits SSE `hmi` mutation. |
| `GET` | `/api/hmi/{path}` | Read one screen as `HmiDoc` (slug percent-encoded into one segment, like `/api/pous/{path}`). |
| `PUT` | `/api/hmi/{path}` | Replace the whole document (`cs set hmi/…`), with optional `If-Match` / `If-None-Match` preconditions described above. No header means a blind overwrite; the IDE canvas, drags included, uses incremental `/ops` instead. Rejects structural errors; returns remaining warnings as `HmiIssue[]`. Emits SSE `hmi` mutation with empty `touched`. |
| `DELETE` | `/api/hmi/{path}` | Delete the screen. Emits SSE `hmi` mutation (`hmi_deleted`). |
| `POST` | `/api/hmi/{path}/ops` | THE incremental authoring surface: body `{ ops: HmiOp[] }` (`add_node` / `update_node` / `remove_node` / `set_meta`), applied atomically. Returns `{ touched, issues }`; the SSE `hmi` mutation carries the same `touched` node ids so every open canvas spawn-animates exactly the elements this batch placed. |
| `GET` | `/api/hmi/{path}/check` | Structural validation plus variable-existence warnings against the project's POUs. Returns `HmiIssue[]`. |
| `POST` | `/api/hmi/{path}/generate` | Deterministic first-pass screen from project truth (alarmbar, per-POU sections, BOOL→indicator, numeric→value, `*_sp`→setpoint input, one trend). Body `{ force?, title? }`; 409 if the screen exists and `force` is absent. Returns the generated `HmiDoc`. |
| `GET` | `/api/hmi-symbols` | The built-in symbol catalog (`HmiSymbolInfo[]`: name, bindable keys, props, default size) — an agent's palette reference. |
| `GET` | `/api/library` | Registry libraries + per-project import state. Returns `LibrarySummary[]` (name, version, `imported_version`, `imported_files`). |
| `POST` | `/api/library/import` | Vendor blocks into the project. Body: `ImportLibraryRequest { library, blocks?[] }` (empty `blocks` = all; re-import overwrites = the update path). Returns `ImportLibraryResponse { library, version, imported[] }`. |
| `DELETE` | `/api/library/{name}` | Drop `pous/lib/<name>/` and the project.toml entry. Idempotent. |
| `GET` | `/api/device-catalog` | Validated device templates from `<library-dir>/devices/`. Returns `CatalogEntry[]`. |
| `GET` | `/api/device-catalog/match?vendor_id=&product_id=` | Resolve a discovered slave's identity to a catalog template (pre-fill a device from an EtherCAT scan instead of hand-typing PDI offsets). Returns `CatalogEntry`; 404 when the identity isn't catalogued. |

## Devices

Modbus channel `access` is `read` or `write` (default `write` for existing
configs). `read` blocks both normal writes and failsafe zeroing, including
holding registers used for measurements. `input_register` and
`discrete_input` are always read-only regardless of `access`. Mark input-only
holding registers/coils explicitly `read`; register kind alone is not permission.

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/api/devices` | Create a device. Body: `CreateDeviceRequest { name, protocol }`. |
| `POST` | `/api/devices/folders` | Create a folder under `devices/`. |
| `DELETE` | `/api/devices/folders/{path}` | Delete an empty folder. |
| `GET` | `/api/devices/{name}` | Read a device. Returns `Device`. |
| `PUT` | `/api/devices/{name}` | Update full device config. Body: `Device`. |
| `DELETE` | `/api/devices/{name}` | Delete. |
| `GET` | `/api/devices/{name}/describe` | Deterministic per-device agent reference file (ADR-0002), derived from project truth — no timestamps, identical state → identical output. Returns `DeviceDescription { device, protocol, config, bindings, alarms, write_mode, write_rules }`: the device config as JSON with every credential-bearing key (case-insensitive pattern set: `password`, `passwd`, `secret`, `token`, `api_key`/`apikey`, `credential`, `private_key`) redacted to `***`, the iomap bindings onto this device sorted by variable (with their `unit`/`min`/`max`/`description` metadata), the alarm definitions reading those variables, and the project's write mode plus the governance rules naming them. CLI: `cs get devices/<n>/describe`. |
| `POST` | `/api/devices/{name}/esi-assemble` | Assemble a modular EtherCAT coupler's channels from its ESI file + the modules it reports. Body: `EsiAssembleRequest { detected: u32[] }` (module idents in slot order). Requires the device to be EtherCAT with `bringup = esi_modular`; the assembled channels **replace** the device's channel list. Returns the updated `Device`. |
| `POST` | `/api/devices/{name}/opcua-browse` | Live-browse one level of an OPC UA device's address space using its own endpoint/auth config. Body: `OpcuaBrowseRequest { node_id? }` (omitted = ObjectsFolder). Returns `OpcuaBrowseNode[]` — NodeId, display name, node class, and for Variables the UA data type plus the channel `data_type` that fits. Backs the editor's NodeId picker; CLI: `cs api POST /api/devices/<n>/opcua-browse`. |

`describe` returns an error if an existing iomap, alarms, or governance
configuration cannot be read or parsed; it never substitutes empty data
or cached governance. Absent optional iomap/alarms files still mean none.

## Edges (deploy targets)

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/api/edges` | Create an edge. Body: `CreateEdgeRequest { name, host }`. |
| `POST` | `/api/edges/folders` | Create a folder under `edges/`. |
| `DELETE` | `/api/edges/folders/{path}` | Delete an empty folder. |
| `GET` | `/api/edges/{name}` | Read an edge. Returns `Edge`. |
| `PUT` | `/api/edges/{name}` | Update an edge. Body: `Edge`. |
| `DELETE` | `/api/edges/{name}` | Delete. Also tears down any open attach tunnel. |
| `GET` | `/api/edges/{name}/probe` | SSH+curl the edge's runtime `/health`. Returns `EdgeProbe { reachable, scan_count, uptime_secs, runtime_version, fieldbus_healthy, unhealthy_devices, watchdog_tripped, fault, error }`. `reachable` only means the runtime answered — check `fieldbus_healthy` for whether its buses are up, and `unhealthy_devices` for which ones are not. `fieldbus_healthy` is `null` when unreachable or when the edge runs a build predating per-device health. `watchdog_tripped: true` means the edge latched its outputs off after losing the scan deadline — it stays reachable and fieldbus-healthy while driving nothing, so a health gate must read this too. `fault` carries why the edge's program stopped (a VM trap, say): such an edge is reachable while running nothing, so a gate must read it as well. It is `null` while the program runs, when unreachable, or when the edge's `/health` predates the field (its `/status` still reports `fault`). |
| `GET` | `/api/edges/{name}/logs?tail=N` | Tail the edge runtime's journald logs over ssh (`tail` clamped to 2000, default 200). Returns JSON. |
| `GET` | `/api/edges/{name}/discover` | Per-device connect status + discovered EtherCAT topology from the edge, so PDO maps can be authored against the real bus. Returns JSON. |
| `GET` | `/api/edges/{name}/system` | Edge interfaces / serial ports / arch — for authoring device configs against real edge facts. Returns JSON. |
| `GET` | `/api/edges/{name}/status` | Proxy the edge runtime's `/status` (project + scan count + debug mode/forces + last snapshot). Returns JSON. |
| `GET` | `/api/edges/{name}/audit` | Proxy the edge runtime's `/audit` write ring (who claimed to write what: self-declared origin, op, requested/applied values, outcome — see the edge table below). Backs `cs get edges/<n>/audit`. Returns JSON. |
| `POST` | `/api/edges/{name}/runtime/{op}` | Proxy an online-debug op to the *deployed* edge runtime over ssh. `op` ∈ {`pause`,`resume`,`step`,`write`,`force`,`unforce`}; body forwarded as the remote payload (e.g. `{cycles}` for step, `{name,value}` for write/force). The edge's own answer passes through with its original status and body verbatim — a governance denial on the edge is a 403 here too, a stopped runtime a 409; only ssh/transport faults are this server's own 500s. |
| `POST` | `/api/edges/{name}/deploy` | Tar project + runtime binary + web assets (when this server has a `--static-dir`; they land at `current/web` for the edge's HMI panel), ssh to edge, atomic symlink swap, restart unit. Returns `DeployReport`. |
| `POST` | `/api/edges/{name}/attach` | Open `ssh -N -L` tunnel to the edge runtime port. Returns `AttachInfo { local_port }`. |
| `POST` | `/api/edges/{name}/detach` | Close the tunnel. |
| `GET` | `/api/edges/{name}/attachment` | Current attachment state. Returns `AttachmentStatus { attached, local_port }`. |

## IO Mapping

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/api/iomap` | Read iomap.toml. Returns `IoMap`. |
| `PUT` | `/api/iomap` | Replace iomap.toml. Body: `IoMap`. |

## Tasks (project-level scheduling)

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/api/tasks` | Read tasks.toml. Returns `Tasks { tasks: [], programs: [] }`. |
| `PUT` | `/api/tasks` | Replace tasks.toml. Body: `Tasks`. |

## Northbound (edge → platform publishing)

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/api/northbound` | Read the edge's northbound (MQTT → supOS/Tier0) publishing config. Returns `NorthboundConfig`. |
| `PUT` | `/api/northbound` | Replace the northbound config. Body: `NorthboundConfig`. |

## Compile, run, observe

Every `CheckDiagnostic` includes `context` and `related` arrays, even
when empty, so clients can consume diagnostics without missing-field checks.

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/api/check` | Compile-check ONE source string (no project required). Body: `text/plain`. Returns `CheckDiagnostic[]`. | Fast feedback for editor squiggles
| `POST` | `/api/symbols?language=st\|ld\|fbd\|sfc` | Extract declared variables from one source string (any language; default `st`). Body: `text/plain`. Returns `VariableInfo[]`. | Backs the editor's binding picker
| `POST` | `/api/run` | Compile the whole project + spawn the bridge. Body: `{}` or `RunRequest`. | Reads `tasks.toml` to decide what runs
| `POST` | `/api/stop` | Stop the running program (cooperative; scan loop drains). |
| `GET` | `/api/runtime/status` | Synchronous overview of the runtime. Returns `RuntimeStatus { running, project, program_instances, devices, device_health, watchdog_tripped, scan_period_ms, scan_count, last_snapshot_us, last_error, running_info, mode, forces }`; `last_error` carries why a run died — a VM trap, a VM that failed to start, or a panic — (also emitted as SSE `error` + `stopped`), `null` after a clean run. `devices` lists the project's configured device names, while `device_health` reports whether each one's transport is actually live — a `false` entry means that device's inputs are frozen at last-known values and its outputs are dropped, even though `running` stays `true` and the scan count keeps climbing. `watchdog_tripped` is `true` once the scan watchdog latched: the program lost real-time guarantees, every output was zeroed and the output phase stays off until a restart — check it before reading any variable value as plant state, because the VM keeps computing and `scan_count` keeps climbing after a trip. `scan_period_ms` is the fastest task's interval — the cadence at which `scan_count` can advance — so a client judging "is this live data current?" can size its window from the project's own rate instead of a fixed guess: `interval_ms` has no upper bound, and on a 5 s-cycle project a 4 s-old value is the newest one that exists. It says nothing about how long a scan actually takes; the overrun watchdog owns that. `scan_overruns` counts missed scan deadlines since the program started (summed over units) and `consecutive_scan_overruns` is the longest streak any unit is currently on — the achieved cadence, which `scan_period_ms` cannot describe. The overrun log line fires once per unit and then suppresses itself, so before these there was no way to tell a runtime quietly missing every fourth deadline from one meeting them all; `watchdog_tripped` only covers the terminal case. A total that climbs between two polls means it is missing them now. `scan_period_ms`, `scan_overruns` and `consecutive_scan_overruns` are `null` when nothing is running. | One-shot, agent-friendly
| `GET` | `/api/runtime/snapshot` | Latest `VarSnapshot` or `null`. | No SSE needed for one-off queries
| `POST` | `/api/runtime/variables/{name}` | Write a variable while running. Body: `WriteVariableRequest { value: <i32-coerceable> }`. Returns the new value. Subject to the project's `[governance]` (project.toml): in `allowlist` mode a write to an unlisted variable is 403, and a rule's `min`/`max` clamp the written value (the response echoes the clamped value). A write no honest clamp exists for is denied 403 instead: `NaN` to a min/max-ruled REAL, or a rule range containing no representable value of the variable's type. The response also carries `undelivered_device`: when the variable has an Output mapping onto a device whose transport is down, the write is still applied (a Stop that flushes when the link returns beats one never commanded) and this names that device, so a caller must not report a bare success. `null` on a healthy link or an internal variable — and it is transport health only, never proof that the value took effect. | Critical for debugging closed loops
| `GET` | `/api/events` | SSE stream of `AppEvent` (`snapshot` / `started` / `stopped` / `error`). | For long-running IDE clients
| `GET` | `/api/project/variables` | Flat list of every variable across every POU in the project. Returns `ProjectVariables { variables: [...] }`. | Cross-POU index for agents
| `GET` | `/api/project/pous` | Every IEC POU declared anywhere in the project (parser-driven). Returns `ProjectPous { pous: [{ application, name, kind }] }` — `kind` ∈ `program` / `function_block` / `function`. | Source of truth for "what's schedulable" — multi-POU files (one .st declaring PROGRAM + FB + FUNCTION) are correctly enumerated, unlike `application.kind` which is a heuristic |

## Runtime debug control

Online debugging of the locally-running program (the IDE-side bridge). For
the *deployed* edge runtime, proxy the same ops through
`POST /api/edges/{name}/runtime/{op}`. All return `409` when nothing is running.

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/api/runtime/pause` | Freeze the scan loop (last outputs hold). Returns `ModeResponse { mode }`. |
| `POST` | `/api/runtime/resume` | Resume free-running. Returns `ModeResponse`. |
| `POST` | `/api/runtime/step` | Advance N cycles while paused. Body: `StepRequest { cycles }` (default 1). Returns `ModeResponse`. |
| `GET` | `/api/runtime/forces` | List currently-forced variables. Returns `ForceEntry[]` (`[]` when not running). |
| `POST` | `/api/runtime/forces/{name}` | Pin a variable every cycle until released. Body: `ForceRequest { value }`. Returns `ForceResponse { name, value }`; 404 unknown variable, 409 if stopped. **Precedence:** the force is applied after the input read and *before* the program runs, so it beats the bus but loses to the program — a variable the program assigns every scan (most outputs) is overwritten by that assignment and the forced value never reaches the field. Force is for variables the program only reads. |
| `DELETE` | `/api/runtime/forces/{name}` | Release a forced variable. Idempotent (200 even if it wasn't forced). |
| `POST` | `/api/runtime/inject-scan-stall` | Fault injection (test primitive): stall the next `scans` scans by `stall_ms` each so the scan watchdog trips through its real overrun path. Body: `{ stall_ms, scans? }` (`scans` defaults to threshold + 1). Backs the scenario DSL's `inject` step; on a live plant this deliberately drives the runtime into latched failsafe — only a program restart recovers. 409 if stopped. |

## Runtime history & alarms

Snapshots include `device_health: [{ name, healthy }]` and, on each mapped
input variable, `input: { device, channel, stale }`. An unavailable device,
failed input read, or mapping awaiting its first successful read makes that
input stale. `value`/`bits` remain the VM's last value, **not a fresh field
measurement**. Recovery clears `stale` only after a successful input read;
recovering a link while paused is insufficient. Internal and output variables
have no `input` field; this is not transitive provenance through PLC logic.
Transport health does not claim a per-channel acquisition timestamp.

Every configured device gets a built-in high-severity alarm
`__device/<device-name>` after 1 s of continuously observed unhealthy state,
including failed startup connections. No `alarms.toml` entry is required.
Recovery returns it, but an unacknowledged occurrence remains standing until
acknowledged. URL-encode the entire id for the existing ack route (including
the slash). The `__device/` prefix is reserved. These alarms share the existing
state/journal/ack machinery on server and edge; they do not change PLC logic
or output safety policy. Process alarms watching stale inputs neither raise
nor clear, and their pending debounce resets until valid input returns.

History points add optional `stale: true` when any sample in the bucket was
stale. Consumers must show a gap/unknown interval, not a fresh flat line;
the edge's persisted history retains this flag across restarts.

Served by the shared monitor layer (`ironplc_bridge::monitor`): an
in-memory 1 Hz historian (~2 h window; the edge runtime persists the
same rings to disk) and the alarm engine over the project's
`alarms.toml`. Alarm DEFINITIONS are project config (`/api/alarms`
below, same get→edit→set shape as iomap); alarm STATE lives here.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/api/runtime/history` | Downsampled history. Query: `vars=a,b` (empty = all), `from_us`, `to_us` (0 = open), `step_ms` (default 1000). Returns `HistoryResponse { series: [{ name, points: [{ t_us, min, max, v }] }], oldest_us, sample_interval_us }`. |
| `GET` | `/api/runtime/alarms` | Live alarm states, standing-first then severity. Returns `AlarmState[]` (`active`, `acked`, `raised_at_us`, `value_at_raise`, `count`). |
| `POST` | `/api/runtime/alarms/{id}/ack` | Acknowledge one alarm. Returns the updated `AlarmState`; 404 unknown id. |
| `GET` | `/api/runtime/alarms-journal` | Most-recent-first event journal (`raised` / `acked` / `returned`). Query: `limit` (default 100, ring-capped at 1000). Returns `AlarmJournalEntry[]`. |
| `GET` | `/api/alarms` | Read the project's alarm definitions (`alarms.toml`). Returns `AlarmConfig { alarms: AlarmDef[] }`. |
| `PUT` | `/api/alarms` | Replace the alarm definitions. Rejects duplicate ids and numeric conditions without a `limit`. Applies on the NEXT run. |

## Agent activity (takeover overlay)

Drives the IDE's "an agent is operating" overlay. See
`crates/server/src/events.rs` for the protocol. Read-only `cs` commands
don't call these.

**Attribution beyond `cs` (ADR-0002).** Mutating requests may declare
`X-IA2-Origin: <operator>` (`gui`, `cs`, `hmi`, `mqtt`, …). The web IDE
sends `gui`, the `cs` CLI sends `cs`, the operator panel sends `hmi`.
The label is self-declared — a convention, not authentication: the
server trusts it as given, so a request that *claims* `gui` is treated
as the console. It is sanitized before use (kept charset
`[A-Za-z0-9._-]`, capped at 64 chars; a label that sanitizes to
nothing counts as absent; a mangled label is cleaned up, never
silently dropped). On every mutating runtime route
(`/api/runtime/variables/{name}`, `/api/runtime/forces/{name}`,
pause/resume/step, alarm ack, `/api/run`, `/api/stop`,
`/api/runtime/inject-scan-stall`, and the
`/api/edges/{name}/runtime/{op}` proxy) the server auto-attributes by
origin: `gui` is suppressed (the IDE user drives the banner UI
itself); `cs` refreshes the open agent session's liveness and is
suppressed exactly while that session is open, flashing
`<op> — cs (no session)` otherwise; any other declared label flashes
`<op> — <origin> (self-declared)` — always, even during an active
session; no usable label flashes `<op> (unattributed)`. The guarantee
is exactly as strong as the convention: unlabelled writers and
self-labelled non-`gui` writers always surface in the overlay (`cs`
folding into the session banner only while an agent session is open) —
but a writer that falsely claims to be the IDE does not. What no label
can dodge is the record: the edge proxy forwards the (sanitized)
origin header to the runtime, where every write — whatever it claimed
— lands in the runtime's audit ring (`GET /audit`, below).

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/api/agent/heartbeat` | Transient one-off ping. Body: `AgentHeartbeatRequest { command, session? }`. Overlay flashes on, then ages out. |
| `POST` | `/api/agent/session/start` | Open an explicit takeover session (overlay stays on with `label`). Body: `{ id, label }`. Returns `AgentSessionResponse`. A fresh start replaces any open session. |
| `POST` | `/api/agent/session/end` | Close a session. Body: `{ id? }` (omit to force-end whatever's open — the IDE's "kick agent" button). Returns `RunResponse { ok }` (`ok=false` if nothing matched). |

## Bridges

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/api/lsp` | WebSocket upgrade. Bridges to a freshly-spawned ironplc LSP process (JSON-RPC). | Frame format = bare JSON-RPC bodies — proxy adds/strips Content-Length headers for stdio |

## Internal / debug aids

These are intentionally prefixed `_` so they're easy to spot. Stable API
contract but only useful when wiring up demos or when the runtime hasn't
been pointed at real hardware yet.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/api/_demo/slave` | Peek the in-process demo Modbus slave's first 32 entries per address space. Returns `DemoSlaveSnapshot`. |
| `PUT` | `/api/_demo/slave/{kind}/{addr}` | Inject a value into the demo slave (e.g., to simulate a discrete-input edge). `kind` ∈ {`coil`, `discrete_input`, `holding_register`, `input_register`}; body: `{ value: bool | u16 }`. |

---

# Coverage

Coverage is TEST-ENFORCED, not dated prose: `crates/server/tests/
api_doc_coverage.rs` fails the build when any `.route()` mounted in
`crates/server/src/main.rs` (or `crates/runtime/src/main.rs`, for the Edge
runtime table below) has no row here. When
you add a route, add its row here in the same change; the generated TypeScript
types under `apps/web/src/types/generated/` remain the source of truth for
shapes.

Notable capabilities, mapped to the agent-use-case checklist (see
`MEMORY/principles.md`):

- ✅ Whole-project compile-check → **POST /api/project/validate**
- ✅ One-shot latest snapshot (no SSE required) → **GET /api/runtime/snapshot**
- ✅ Runtime overview without curl-ing both `/health` and the SSE stream → **GET /api/runtime/status**
- ✅ Write a variable while running (debug agents) → **POST /api/runtime/variables/{name}**
- ✅ Inject input signals into demo slave → **PUT /api/_demo/slave/{kind}/{addr}**
- ✅ Delete a folder under applications / devices / edges → **DELETE /api/.../folders/{path}**
- ✅ Cross-POU variable index → **GET /api/project/variables**
- ✅ Cross-POU declaration index (real schedulable POU names) → **GET /api/project/pous**
- ✅ Health-under-/api alias → **GET /api/health**

# Redundancies (kept on purpose)

- `/health` + `/api/health` — `/health` is the convenience root for monitoring
  tooling; `/api/health` is the agent-friendly mirror. Trivial cost.
- `/api/check` + `/api/project/validate` — different scopes: `check` is "compile
  this string of source" (used by the editor while typing), `validate` is
  "compile the whole project" (used by agents before Run/Deploy).

# Edge runtime API (separate process)

The headless `ia2-runtime` binary (running on the edge) exposes the runtime
slice of the same surface — liveness/status, log tailing, discovery, and the
full online-debug set — bound to `127.0.0.1` only. The IDE proxies most of
these through `/api/edges/{name}/…` (and `/api/edges/{name}/runtime/{op}` for
the debug ops).

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/health` | Liveness: status, uptime, scan count, plus `fieldbus_healthy` (true only when every configured device's transport is live), a per-device `devices` health breakdown, and `watchdog_tripped` (the scan watchdog latched the outputs off — a probe that only checks `status: "ok"` would otherwise call a latched runtime healthy, since it keeps serving HTTP and keeps scanning). `fault` is the same message as `/status`'s: why the program stopped, `null` while it runs. `status` stays `"ok"` for a faulted runtime — it only says the process answers. |
| `GET` | `/status` | Project + PROGRAM instances + device list + scan count + last snapshot + debug mode + forces, plus `device_health` (per-device transport health; a `false` entry means that device's inputs are frozen at last-known values), `watchdog_tripped` (the scan watchdog latched: outputs were zeroed and stay off until this process restarts — check it before reading `last_snapshot` as plant state, because the VM keeps computing after a trip) and `fault` (why the scan loop died — a VM trap, a VM that failed to start, or a panic — `null` while running or after a requested stop). `scan_period_ms` is the fastest task's interval — the cadence at which `scan_count` can advance — so a client judging "is this live data current?" can size its window from the project's own rate instead of a fixed guess: `interval_ms` has no upper bound, and on a 5 s-cycle project a 4 s-old value is the newest one that exists. It says nothing about how long a scan actually takes. `scan_overruns` counts missed scan deadlines since the program started (summed over units) and `consecutive_scan_overruns` is the longest streak any unit is currently on — the achieved cadence, which `scan_period_ms` cannot describe. The overrun log line fires once per unit and then suppresses itself, so before these there was no way to tell a runtime quietly missing every fourth deadline from one meeting them all; `watchdog_tripped` only covers the terminal case. A total that climbs between two polls means it is missing them now. `build_commit` and `build_rustc` are stamped into the binary at compile time — the source commit (suffixed `-dirty`) and the compiler — so a deployed runtime can say what built it; either is `unknown` when the build could not see a git tree and none was supplied. |
| `GET` | `/events` | SSE stream of `VarSnapshot` (bare — no `AppEvent` wrapper). |
| `GET` | `/logs?tail=N` | The most recent `tail` (default 200) captured log lines. What `cs get edges/<n>/logs` pulls over the tunnel. |
| `GET` | `/logs/stream` | SSE stream of log lines as they're emitted (no backlog; pair with `/logs` for history). |
| `GET` | `/discover` | Per-device connect reports + discovered EtherCAT topology. Powers `cs get edges/<n>/scan`. |
| `GET` | `/system` | Edge NICs / serial ports / arch, for authoring device configs against real edge facts. Powers `cs get edges/<n>/system`. |
| `POST` | `/pause` | Freeze the scan loop (last outputs hold). Returns `{ mode }`. |
| `POST` | `/resume` | Resume free-running. Returns `{ mode }`. |
| `POST` | `/step` | Advance N cycles while paused. Body: `{ cycles }`. Returns `{ mode }`. |
| `POST` | `/write` | One-shot write of a variable. Body: `{ name, value }`. Returns the applied value. Subject to the deployed project's `[governance]` (project.toml): `write_mode = "allowlist"` rejects writes to variables with no matching `[[governance.rules]]` entry (403), and a rule's `min`/`max` clamp the value (the response echoes the clamped value — a clamp is also logged and lands in `/audit` as `result: "clamped"` with both `requested` and `applied`, never silent). A write no honest clamp exists for is denied 403 instead: `NaN` to a min/max-ruled REAL, or a rule range containing no representable value of the variable's type. The response also carries `undelivered_device`: when the variable has an Output mapping onto a device whose transport is down, the write is still applied (a Stop that flushes when the link returns beats one never commanded) and this names that device, so a caller must not report a bare success. `null` on a healthy link or an internal variable — and it is transport health only, never proof that the value took effect. |
| `POST` | `/force` | Pin a variable every cycle until released. Body: `{ name, value }`. Same precedence as the server route: applied before the program runs, so a program-written variable is overwritten by the program and the force never reaches the field. Force is the debug override: it **bypasses governance** by explicit decision (ADR-0002) and is never exported on any northbound surface. |
| `POST` | `/unforce` | Release a forced variable. Body: `{ name }`. |
| `POST` | `/inject-scan-stall` | Fault injection (test primitive): same contract as the server's `/api/runtime/inject-scan-stall` — stall `scans` scans by `stall_ms` each and trip the watchdog for real. Body: `{ stall_ms, scans? }`. |
| `GET` | `/audit` | The write-audit ring (bounded at 256, oldest first): every `/write`·`/force`·`/unforce` and MQTT northbound write with `ts_unix_secs`, `origin` (`X-IA2-Origin` header, `mqtt`, or `anonymous`), `op`, `name`, bit-packed `requested` and `applied` values (an `unforce` entry carries neither — there is no value to release to), and `result` — `ok`, `clamped` (governance altered the value; `requested`/`applied` show both sides), or the error text (denied writes are recorded too, with the denial reason; `applied` is then absent). In-memory only — cleared when the runtime process restarts (a redeploy restarts it), and under steady write traffic the 256-entry window covers only the recent past; `/history` is the surface that survives restarts. Also readable through the IDE as `GET /api/edges/{name}/audit` / `cs get edges/<n>/audit`. |
| `GET` | `/history` | Downsampled history, same query/response shape as `/api/runtime/history`. Backed by JSONL segments under the edge's state dir — survives restarts AND deploys (state/ sits beside `current`). |
| `GET` | `/alarms` | Live `AlarmState[]` for the deployed `alarms.toml`, standing-first. `/status` carries the `alarms_standing` count on the panel's existing poll. |
| `POST` | `/alarms/{id}/ack` | Acknowledge one alarm (operator action from the panel). 404 unknown id. |
| `GET` | `/alarms-journal` | Most-recent-first alarm event journal (`raised` / `acked` / `returned`). Query: `limit`. |
| `POST` | `/stop` | Request graceful shutdown. |
| `GET` | `/api/hmi` | HMI screens deployed with the project: `[{ path, title, level }]` (same row shape as the IDE route). Read-only — screens are edited in the IDE and arrive via deploy. |
| `GET` | `/api/hmi/{path}` | One screen's full `HmiDoc` JSON. 404 if the deployed project has no such screen. |

With `--static-dir` pointing at the built web assets the runtime also serves the
standalone operator panel: `GET /hmi` lists the deployed screens, `GET /hmi/{rest}`
renders one (`/` redirects to `/hmi`). The panel is a separate vite entry
(`hmi.html`) that talks only to this runtime's own surface — `/events` for live
values, `/write` for confirmed actions, `/status` for the fault strip — so an
operator client needs nothing but a browser and this port.

Access from the dev machine: open an `ssh -N -L <local>:127.0.0.1:<runtime_port> <edge>`
tunnel (see `/api/edges/{name}/attach`) and hit `http://127.0.0.1:<local>/...`.
