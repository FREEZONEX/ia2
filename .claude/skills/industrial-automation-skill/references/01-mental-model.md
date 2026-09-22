# Mental model

Three layers. Memorise them — every other reference assumes this picture.

```
┌──────────────────────────────────────────────────────────────────┐
│  Client(s)                                                       │
│                                                                  │
│   Browser tab (web SPA)                                          │
│   ────────────────────────────────                               │
│   vite dev :3000, or served by the server via --static-dir       │
│   URL-scoped per ?project — one tab per project window           │
│                                                                  │
│   `cs` CLI (this is what you use)                                │
│   ─────────────────────────────────                              │
│   binary at target/release/cs (or in $PATH)                      │
│   talks HTTP only; defaults to http://127.0.0.1:3001             │
└──────────────────────────────────────────────────────────────────┘
                              │ HTTP + SSE
                              ▼
┌──────────────────────────────────────────────────────────────────┐
│  IA2 server (axum, Rust)                                         │
│                                                                  │
│   ProjectRegistry           Program (singleton)                  │
│   ───────────────────       ─────────────────                    │
│   N open projects           one project at a time                │
│   `X-IA2-Project` header    (hardware constraint — Modbus,       │
│   selects which              EtherCAT bus can have only one      │
│                              master per process)                 │
│                                                                  │
│   Agent activity            Event broadcaster                    │
│   ───────────────────       ─────────────────                    │
│   Session OR heartbeat      Mutation events tagged with          │
│   drives takeover overlay   project name; web filters            │
└──────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────────┐
│  ironplc-bridge (vendored ironplc + scan loop)                   │
│                                                                  │
│   compile_project_full() → (Container, ProgramMetadata)          │
│   spawn_with_options()   → ProgramHandle                         │
│     - scan loop ticks at tasks.toml interval (default 100ms)     │
│     - RETAIN vars persisted to <project>/state/retain.json       │
│     - panic / VM trap / stop → failsafe on all devices           │
└──────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────────┐
│  IoDevice adapters (iomap-*)                                     │
│                                                                  │
│   Modbus TCP  ── tokio-modbus over TcpStream                     │
│   Modbus RTU  ── tokio-modbus over tokio_serial::SerialStream    │
│   EtherCAT    ── ethercrab on a dedicated OS thread              │
│                                                                  │
│   Trait: read_channel / write_channel / enter_failsafe           │
└──────────────────────────────────────────────────────────────────┘
```

## Five facts you must remember

### 1. One server hosts N projects; a header picks which

Projects live in `~/Documents/IA2/<name>/`. The server holds a `ProjectRegistry` keyed by name. Every CLI request carries `X-IA2-Project: <name>` (set by the global `--project` flag, valid on every command); missing header → server falls back to its LRU "active" project. **Always pass `--project` if `cs ls projects` shows more than one.**

### 2. One running program per server; a run may schedule several PROGRAMs

There is exactly one running-program slot in server state, and it belongs to one project at a time — starting a run stops whatever ran before, across all projects. That singleton is a *hardware* constraint, not a language one: a Modbus or EtherCAT bus has one master per process, so two scan loops can't drive the field at once. `/api/runtime/status` reports which project owns the slot ("running: foo/main").

Within that one run, though, `tasks.toml` may schedule **several PROGRAM instances**, and they all execute. The bridge compiles one container plus one VM per instance and round-robin schedules them on the single scan thread: each instance fires on its own task interval, and priority then declaration order breaks same-tick ties. Snapshots merge across instances (a name shared by two renders as `instance.variable`; single-instance projects keep bare names), and each `iomap` entry routes to its `application` instance. Extra PROGRAMs are **not** silently dropped, and scheduling two is **not** an error.

The one thing multi-PROGRAM projects cannot do is share a `VAR_GLOBAL` across instances — separate containers isolate the address spaces, so each instance would get its own diverging copy. Both `/api/run` and `/api/project/validate` refuse a project that schedules 2+ PROGRAMs while also declaring `VAR_GLOBAL`, naming the offending globals; move the shared state behind an I/O mapping or FUNCTION_BLOCK parameter, or schedule a single PROGRAM. See ADR-0001 (`docs/adr/0001-ironplc-ia2-boundary.md`) for the round-robin design.

### 3. The scan loop is real and drives real hardware

`spawn_with_options()` starts a dedicated `std::thread` running a tokio current-thread runtime. Each scan:
- Input phase: every iomap'd `direction: input` reads its channel
- Force phase: pinned values overwrite VM state
- VM `run_round(now_us)` executes the bytecode
- Output phase: every iomap'd `direction: output` writes its channel
- Sleep until `next_scan_at` (cadence from `tasks.toml interval_ms`)

The default interval is 100 ms. Faster than 10 ms taxes the snapshot fan-out. RTU at 9600 baud is slow enough that 200-500 ms cadence is the realistic floor.

### 4. The agent overlay has two modes

- **Session mode** (preferred): `cs agent run --label "X"` opens an explicit session. Banner stays on with `X` as text. Server's watchdog only ends the session after 30 s of no heartbeat (crash recovery).
- **Transient heartbeat mode** (back-compat): a single `cs` command sends one heartbeat; the overlay flashes on with the command name; ages out after 3 s. **Always prefer session mode for multi-step work.**

### 5. Mutations are project-scoped on the wire

Every `MutationEvent` carries a `project: String` field. Web clients filter SSE events to those matching their URL `?project=`. So window A editing project `foo` doesn't make window B (showing project `bar`) re-fetch its tree.

## Where things live on disk

```
~/Documents/IA2/
  bottling_line/                       ← one project
    project.toml                       ← manifest (name, version)
    pous/
      main.st                          ← Structured Text PROGRAM/FB/FUNCTION
      conveyor.ld.json                 ← LD as JSON (graphical)
      reactor.fbd.json                 ← FBD as JSON
      batch_state.sfc.json             ← SFC as JSON
    devices/
      hmi_plc.toml                     ← per-device config (modbus / ethercat / opcua / canopen)
    edges/
      field_pi.toml                    ← deploy targets (SSH host etc.)
    iomap.toml                         ← variable ↔ device.channel bindings
    tasks.toml                         ← PROGRAM instance ↔ task schedule
  
  .ia2-open-projects.json              ← server's "open this on startup" list
  state/                               ← per-project, sibling of project dir
    bottling_line/
      retain.json                      ← VAR RETAIN persisted values
```

## Where you talk to the server

| Goal | HTTP | CLI |
|---|---|---|
| Health check | `GET /health` | `cs api GET /health` (or `cs ls projects`) |
| List open projects | `GET /api/projects/open-list` | `cs ls projects` |
| Open a project | `POST /api/projects/open {path}` | `cs project open PATH` |
| Project tree | `GET /api/project` (with header) | `cs get project` (online) · `cs project info PATH` (offline dir) |
| Save POU source | `PUT /api/pous/{path}` | `cs set pous/NAME.st --from - --if-match @version` |
| Validate full project | `POST /api/project/validate` | `cs project check PATH` |
| Start running | `POST /api/run` | `cs run [--program X]` |
| Pause / step / resume | `POST /api/runtime/{action}` | `cs runtime pause/step/resume` |
| Force a variable | `POST /api/runtime/forces/{name}` | `cs runtime force NAME VALUE` (negatives: `cs runtime force NAME -- -N`) |
| One-shot write / unforce | `POST /api/runtime/variables/{name}` · `DELETE /api/runtime/forces/{name}` | `cs runtime write NAME VALUE` · `cs runtime unforce NAME` |
| **Live values** | `GET /api/runtime/snapshot` | `cs runtime snapshot [--vars a,b]` |
| Mode + forces only | `GET /api/runtime/status` | `cs runtime status` |
| Historian (1 Hz, ~2 h) | `GET /api/runtime/history` | `cs get runtime/history --query vars=a,b` |
| Alarms — live / ack | `GET /api/runtime/alarms` · `POST /api/runtime/alarms/{id}/ack` | `cs get runtime/alarms` · `cs runtime ack ID` |
| IoMap / Tasks / Northbound / Alarm defs | `GET·PUT /api/iomap` (etc.) | `cs get iomap --etag-file version` · `cs set iomap --from - --if-match @version` (same for `tasks` / `northbound` / `alarms`) |
| Edge introspection | `GET /api/edges/{name}/{logs,scan,system,status,probe}` | `cs get edges/NAME/{logs,scan,system,status}` · `cs probe NAME` |
| Drive an edge runtime | `POST /api/edges/{name}/runtime/{op}` | `cs runtime <op> --edge NAME` |
| SSE event stream | `GET /api/events` | (SSE — see `02-cli-reference.md`) |
| Start agent session | `POST /api/agent/session/start` | `cs agent enter` / `cs agent run -- ...` |
| End agent session | `POST /api/agent/session/end` | `cs agent leave` / auto on `cs agent run` exit |

**`cs runtime status` is mode + forces only — it carries no variable values.** Live values come from `cs runtime snapshot` (a separate read). Don't reach for `status` to see a number change; reach for `snapshot`.

**The monitor surface is one shared layer.** `status` · `snapshot` · `pause`/`step`/`resume` · `force`/`write`/`unforce` · `history` · `alarms`/`ack` are the *same* read-and-debug API whether they run against the IDE server's local runtime or a deployed box (`--edge NAME`, or point `--server` at the box's forwarded port). Same verbs, same JSON shapes, same audited takeover overlay — the edge runtime serves `/history` and `/alarms` identically; only the transport differs (local HTTP vs one-shot ssh+curl per `--edge` call).
