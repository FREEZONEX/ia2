# `cs` CLI reference

The surface is bash-sized on purpose: **five meta-primitives** cover
every resource (present and future), and a short list of **domain
verbs** carries the semantics a generic verb shouldn't blur. If you
remember one thing: *resources are slash-paths, and `ls/get/set/rm`
work on all of them the same way.*

Global flags (valid on every command):

- `--server URL` — default `http://127.0.0.1:3001`.
- `--project NAME` — target one open project on a multi-project server
  (adds `X-IA2-Project` to every request, no exceptions).
- `--json` — machine output. Commands whose output is inherently JSON
  (`get`, `api`, `runtime snapshot`, …) emit JSON regardless.

Project selection over HTTP: the IDE and CLI send UTF-8 percent-encoded
`X-IA2-Project` with `X-IA2-Project-Encoding: percent`, including for
Chinese names. Raw HTTP clients can use the same pair. Without the
encoding header, values remain literal (`a%20b` is that exact project
name); malformed explicit encoding returns 400, never the active-project
fallback. The CLI encodes `--project` names automatically, preserving
Unicode and literal percent signs.

Exit codes (uniform, enforced): `0` success · `1` problems in YOUR
content (check diagnostics, failed probe, remote deploy failure, sim
expectation failed) · `2` bad request — usage errors AND HTTP 4xx, with
the server's reason printed verbatim on stderr · `≥3` infrastructure
(server down, 5xx). A 422 like ``missing field `application` `` reaches
you word-for-word — read stderr before retrying anything.

Heartbeat rule: MUTATING commands announce to the IDE overlay
(`set`, `rm`, `api` non-GET, `run`, `stop`, `deploy`, `runtime`
pause/step/force/write/ack, `hmi op/generate`, `library import`,
`project create/open/close`, `sim run`). Reads (`ls`, `get`, `check`,
`probe`, `runtime status/snapshot`, …) stay silent — querying isn't
operating.

## The quartet — any resource, four verbs

```
cs ls                          # resource-kind overview (start here)
cs ls pous|devices|edges|hmi|library|projects|device-catalog
cs get <path>                  # read one resource
cs set <path> [--from f|-]     # create-or-replace (upsert)
cs rm  <path>                  # delete (trailing / = folder)
```

Path grammar: first segment = resource kind; the rest is the resource's
own slash-path (the same one it has on disk and in the API). Nested
names are fine (`pous/lib/pid/fb_pid`).

| Path | get | set | notes |
|---|---|---|---|
| `pous/<slug>[.st\|.ld.json\|.fbd.json\|.sfc.json]` | prints RAW SOURCE (redirect to a file) | body = raw source via `--from f\|-`; creating a NEW POU needs the extension (that's where the language comes from) + optional `--type function_block` | `cs get pous/motor --json` for the parsed `{path, source, declarations}` |
| `pous/<slug>/variables` | declared variables of one POU | — | |
| `devices/<name>` | full JSON config | `--from cfg.json`; create needs `--protocol modbus\|ethercat\|opcua\|canopen` (or `protocol` in the body) | get → edit → set is THE device workflow |
| `edges/<name>` | edge config | `--from cfg.json`; create needs `--host user@box` | |
| `edges/<n>/probe·status·logs·scan·system·audit` | sub-reads over ssh | — | `--query tail=500` on logs; `audit` = the edge's write-audit ring (who claimed to write what — see 03) |
| `devices/<n>/describe` | deterministic agent reference file: config (passwords redacted), bindings + metadata, related alarms, governance rules | — | read-only; invalid on-disk config returns an error, never cached rules or empty data |
| `hmi/<slug>` | full screen document | `--from doc.json`; create takes `--title` | incremental edits: `cs hmi op` (below) |
| `iomap` · `tasks` · `northbound` · `alarms` | the single config doc | `--from f\|-` (whole-doc replace) | shapes in 06 / 09 |
| `library` | — (`cs ls library`) | — | `cs rm library/<name>` removes an import |
| `project` · `project/variables` · `project/pous` | tree / cross-POU indexes | — | |
| `runtime/status·snapshot·forces·history·alarms·alarms-journal` | live runtime reads | — | `--query vars=a,b`, `--query step_ms=500` on history |
| `hmi-symbols` | the HMI palette contract | — | |
| `pous/<dir>/` etc. (trailing slash) | — | creates a folder | `cs rm pous/<dir>/` deletes one |

## `cs api` — the escape hatch

Any endpoint in `docs/api.md`, no porcelain required:

```
cs api GET  /api/edges/pi/probe
cs api POST /api/edges/pi/attach
cs api POST /api/devices/rio/esi-assemble --from -    # {"detected":[16,17]} on stdin (decimal idents)
cs api POST /api/devices/dcs/opcua-browse --from -    # {"node_id":"ns=2;s=Line1"} (null = ObjectsFolder)
cs api POST /api/project/migrate-tasks
cs api GET  /api/edges/pi/logs --query tail=500
```

Full API parity is guaranteed by construction — if the GUI can do it,
`cs api` can. Prefer porcelain when it exists (better output + exit
semantics).

## Domain verbs

### Validate / inspect (offline where possible)

```
cs check pous/*.st motor.ld.json     # files check TOGETHER (cross-file FBs resolve)
cs check hmi/overview                # server-side screen check (structure + variables)
cs check P0002                       # a problem code prints its full explanation
cs check bad.st --explain            # human mode + explanations (JSON always carries them)
cs transpile motor.ld.json [--with-map]   # the ST a graphical POU compiles to
cs symbols motor.fbd.json [--name pid]    # declared variables / FB instances
cs project check [dir]               # strongest offline gate: full project compile
cs project info  [dir]               # offline orientation (POUs/devices/edges)
```

`project check` is the compile gate, not I/O-map validation. Open the project
on a local server, then use `cs api POST /api/project/validate` for compile
diagnostics **and** static device/channel/access checks, without starting
devices. This recognizes configured gear routes, including parameter Input
echoes, and rejects Output bindings to gear feedback (see 06).

### Run / debug (online)

```
cs run [--program NAME [--file path.st]]  # tasks.toml schedule, or one PROGRAM
cs stop
cs runtime status [--edge NAME]      # mode + forces (no variable values)
cs runtime snapshot [--vars a,b] [--edge NAME]   # LIVE VALUES — the read you want
cs runtime pause | resume | step [N] [--edge NAME]
cs runtime force <var> <value> [--edge NAME]     # pinned every scan; type-aware encoding
cs runtime unforce <var> [--edge NAME]
cs runtime write <var> <value> [--edge NAME]     # one-shot (program may overwrite)
cs runtime ack <alarm-id>            # acknowledge an alarm (see 09-sim-alarms.md)
```

Value encoding for force/write: human notation — `TRUE`/`FALSE`/`1`/`0`
for BOOL, `50.0` for REAL (the CLI bit-packs by the variable's live
type). Negative numbers after `--`: `cs runtime force setpoint -- -5`.

Governed projects (`[governance]` in `project.toml` — see 09): in
`allowlist` mode a write to an unlisted variable exits 2 with the
server's 403 reason on stderr, and a rule's `min`/`max` **clamp** the
value — the echoed value is the applied bound, which may differ from
what you asked for. A write that can't be honestly clamped (NaN to a
min/max-ruled REAL, or a rule range containing no representable value
of the variable's type) is denied like an unlisted one — exit 2, 403
reason on stderr (see 09). `force` is not governed (deliberate debug
bypass — 09 again).

Force precedence: the force is applied after the input read and before
the program runs, so it beats the bus but loses to the program — a
variable the program assigns every scan (most outputs) is overwritten by
that assignment and the forced value never reaches the field, while the
CLI still reports success. Force variables the program only *reads*
(setpoints, mode requests, jog commands); to override a program-written
output, give the program an override input it applies last. In a
governed project, remember force bypasses `[governance]` by design (see
09) — drive governed setpoints with `cs runtime write` so the clamps
apply, and keep force for commissioning/debug overrides.

### Simulate (prove behaviour before hardware)

```
cs sim run scenarios/fill.toml [--program NAME] [--trace out.jsonl] [--keep-running] [--no-run]
```

Exit 0 = every expectation held; 1 = a step failed (the report names
the step, the deadline, and the last observed value). Scenario
vocabulary + alarm/history workflow: `references/09-sim-alarms.md`.

### Deploy / edge

```
cs deploy <edge>        # tar → ssh → versioned extract → atomic swap → systemd restart
cs probe <edge>         # reachability; exit 0/1
```

`probe` distinguishes *reachable* from *working*. A runtime whose fieldbus
is down still answers `/health`, so it prints `⚠ … reachable` plus a
`fieldbus DEGRADED — N down (inputs frozen, outputs dropped): <names>`
line. **Exit code stays 0** — the edge IS reachable — so scripts that
gate on health must read `fieldbus_healthy` / `unhealthy_devices` from
`cs probe --json`, not the exit status. Same data on
`/api/runtime/status`'s `device_health` for a locally-running program.

`watchdog_tripped` is the third field such a gate must read, and the
nastiest: a latched runtime is reachable AND fieldbus-healthy AND its scan
count keeps climbing, while it drives nothing. `probe` prints
`WATCHDOG LATCHED` for it; only a restart clears it.

Deploy REFUSES to lie: a failed restart, broken tar stream, or missing
version stamp fails the deploy (`ok:false` + log). install_dir/systemd
drift surfaces as a structured `warning` field. Attach/detach live
streaming: `cs api POST /api/edges/<n>/attach` / `detach`.

### HMI authoring actions

```
cs hmi generate <slug> [--title T] [--force]   # deterministic baseline from project truth
cs hmi op <slug> --from ops.json               # incremental structured edits (animate live)
```

CRUD is the quartet (`cs ls hmi`, `cs get/set/rm hmi/<slug>`); palette
contract is `cs get hmi-symbols`. See `references/08-hmi.md`.

### Libraries

```
cs ls library                        # registry + import state
cs library import process-control [--blocks fb_pid.st,fb_ramp.st]
cs rm library/process-control
```

### Projects & sessions

```
cs ls projects                       # open projects; * marks the active fallback
cs project create <name>             # → ~/Documents/IA2/<name>/
cs project open <path> | close
cs agent run --label "..." -- bash -c '...'    # REQUIRED wrapper for multi-step work
cs agent enter --label "..." / cs agent leave  # script-managed session variant
```
