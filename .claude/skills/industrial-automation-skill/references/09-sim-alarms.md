# Simulation, alarms, and history

These three close the loop that matters most for an agent: after you
WRITE control logic, you can PROVE it behaves (sim), declare what
counts as abnormal (alarms), and answer "what happened at 03:00"
(history) — all before and after hardware, all through the same API.

## `cs sim run` — prove behaviour before hardware

The program runs on the server against the simulated device layer (the
demo Modbus slave, `_sim` EtherCAT/CANopen NICs, the fake OPC UA
server); a scenario file plays the plant. This is the step between
"`cs project check` compiles" and "deploy": generate → check → **sim**
→ deploy.

```bash
cs sim run scenarios/fill.toml                 # runs tasks.toml, plays the scenario, stops
cs sim run s.toml --program main               # one PROGRAM instead of the schedule
cs sim run s.toml --trace /tmp/trace.jsonl     # record every polled snapshot
cs sim run s.toml --no-run                     # attach to a run you already started
```

Scenario TOML — steps execute top to bottom, one key per step:

```toml
description = "inlet fills the tank; high alarm raises; no overflow"

[[steps]]
wait_ms = 300                                   # settle time

[[steps]]                                       # play the plant: write an input
set = { var = "inlet_cmd", value = true }       # bool/number/string; encoded by IEC type

[[steps]]                                       # assertion with a deadline (poll ~100ms)
expect = { var = "level", op = "gt", value = 20.0, within_ms = 6000 }

[[steps]]                                       # safety property over a window
expect_never = { var = "overflow", op = "is_true", during_ms = 1500 }

[[steps]]                                       # alarms are first-class assertables
expect_alarm = { id = "level_high", active = true, within_ms = 5000 }

[[steps]]                                       # fault injection: stall N scans so the
inject = { scan_stall_ms = 25 }                 # watchdog trips via its REAL overrun path

[[steps]]                                       # assert on the watchdog latch itself
expect_watchdog = { tripped = true, within_ms = 3000 }
```

Ops: `gt ge lt le eq ne is_true is_false` (`is_*` take no `value`).
Defaults: `within_ms`/`during_ms` 5000, `expect_alarm.active` true,
`inject.scans` threshold+1 (enough to trip), `expect_watchdog.tripped`
true. `expect_watchdog` takes `within_ms` (state REACHED by deadline) or
`during_ms` (state HOLDS for the whole window) — same split as expect vs
expect_never. A negative control before inject must use `during_ms`:
`{ tripped = false, during_ms = 800 }` — with within-semantics it would
pass on the first poll and guard nothing. `inject` is the only
vocabulary that can drive a TIMING fault deterministically — a CPU-burn
program depends on host speed and proves nothing on a fast machine
(reference scenario: `examples/watchdog_latch/scenarios/latch.toml`).
After a trip the VM keeps computing and scan_count keeps climbing —
that is exactly why a health gate must read `watchdog_tripped`, not
liveness.
Exit 0 = all held · 1 = a step failed, and the report names the step,
the deadline, and the LAST OBSERVED value ("expected level > 20 within
6000ms — last observed 3.5") — fix the logic, re-run. Execution stops
at the first failed step (later steps depend on earlier state).

The reference example lives at `examples/sim_smoke/` (tank integrator +
alarm + scenario); it also runs as a cargo e2e test.

`examples/write_governance/scenarios/clamp.toml` proves governed writes
above, below and inside the allowed range against a device-free program.
Scenario `set` uses the governed write path; assert the applied value,
which may differ from the requested value after clamping.

Pattern for closed-loop plants: the scenario IS the plant for simple
cases (set inputs, watch outputs). For richer dynamics, add a second
PROGRAM that computes sensor values from actuator state and schedule it
in tasks.toml alongside the program under test.

## An alarm that can never fire

The engine matches a definition's `variable` against snapshot names with `==`
and skips an unmatched one **quietly**. So a typo, or an ST rename that
`alarms.toml` did not follow, leaves an alarm that never evaluates — and
`GET /alarms` reports it as calm, never-raised and already-acknowledged. A
green line claiming coverage that is not there.

`POST /api/project/validate` now refuses it: an alarm watching a variable no
POU declares is an **error** (`alarms-validate`), not a warning. Run validate
after renaming anything an alarm watches.

Both spellings a snapshot can carry are accepted — bare, and
`instance.variable` for a name more than one PROGRAM instance declares. What
it does NOT catch: a bare name that is in fact shared, where the snapshot adds
the `instance.` prefix and the alarm is dead anyway. Static extraction has no
per-POU attribution, so that one still needs a run to notice.

## Alarms — declared in the project, evaluated by the runtime

Device loss also has a zero-config alarm: `__device/<device-name>` raises at
high severity after 1 s continuously unhealthy, including a device that never
connected. It uses the same alarm journal and acknowledgement commands;
recovery alone does not dismiss an unacknowledged occurrence. Do not define
ids beginning `__device/` in `alarms.toml` (reserved). These alerts do not change
the scan program or failsafe policy.

Inspect `snapshot.vars[].input` for mapped field inputs: `{device, channel,
stale}`. A stale value is last-known, not live; a link recovering while paused
does not clear it until input is read again. `snapshot.device_health` includes
all configured devices. Internal/output values do not inherit this metadata.
Process alarms neither raise nor clear on stale inputs; debounce restarts on
valid input. History buckets containing stale samples carry `stale: true`,
including persisted edge history: graph them as gaps, not healthy flat lines.

Definitions are project config (`alarms.toml`), authored like any other
doc — and YOU should author them alongside the control logic; a program
that can misbehave silently is only half-delivered:

```bash
cs get alarms                       # current definitions
cs set alarms --from -              # replace (applies on the NEXT run)
```

```json
{ "alarms": [ {
    "id": "level_high",              // unique — the ack/journal key
    "variable": "tank_level",        // snapshot variable
    "condition": "gt",               // gt|ge|lt|le|eq|ne|is_true|is_false
    "limit": 90.0,                   // numeric conditions only
    "deadband": 2.0,                 // hysteresis on CLEARING (stops chatter)
    "delay_ms": 500,                 // must hold this long before raising
    "severity": "high",              // info|warn|high|critical
    "message": "Tank level high — check outlet valve"   // actionable, not a restatement
} ] }
```

The server rejects duplicate ids and numeric conditions without a
`limit` (422 — the reason prints on stderr).

State machine (ISA-18.2 shaped): raise → `active,unacked`; `cs runtime
ack` → `active,acked`; condition clears (through the deadband) →
`returned`. **Cleared-but-unacked stays standing** — an alarm that
fired at 03:00 and self-cleared remains visible until a human (or you,
deliberately) acks it.

```bash
cs get runtime/alarms               # live states, standing-first
cs runtime ack level_high           # acknowledge
cs get runtime/alarms-journal       # raised/acked/returned, most recent first
```

On the edge the same surface is `/alarms`, `/alarms/{id}/ack`,
`/alarms-journal`, and `/status` carries `alarms_standing`. The HMI has
an `alarmlist` node (summary table + per-row ACK) — see 08-hmi.md.

## History — "what happened at 03:00" is one GET

A zero-config historian samples every numeric variable at 1 Hz
(~2 h window in memory; the EDGE runtime also persists JSONL segments
under its state dir, so history survives restarts AND deploys).

```bash
cs get runtime/history --query vars=level,flow --query step_ms=1000
cs get runtime/history --query vars=level --query from_us=0 --query to_us=0
```

Response: per-variable buckets `{t_us, min, max, v}` (v = last in
bucket) + `oldest_us` so you know where coverage starts. Time base is
the snapshot clock (scan-relative); alarm journal timestamps are
wall-clock — correlate via "now".

Post-event drill: `cs get runtime/alarms-journal` for WHEN and WHAT →
`cs get runtime/history --query vars=<alarm variable>` around it for
WHY → read the POU → fix → `cs sim run` to prove the fix.

---

## Write governance — who may write what, within which bounds

Declared in `project.toml` (no new config file), enforced in the scan thread so **every** write path is covered — IDE server, edge runtime HTTP, IDE→edge proxy, and MQTT northbound writes alike:

```toml
[governance]
write_mode = "allowlist"    # default "open" = legacy: everything writable

[[governance.rules]]
variable = "level_sp"       # monitor name: bare or instance.variable
min = 0.0
max = 100.0
```

- `open` (default, and what old projects parse to) applies no checks.
- `allowlist` rejects writes to any variable no rule names. HTTP writes (the IDE's `/api/runtime/variables/{name}`, the edge's `/write`, the HMI panel) get a **403** with the reason in the body; a denied MQTT northbound write is dropped and logged, with the denial recorded in the edge audit ring (`GET /audit`) — MQTT is a data path with no reply channel, so there is no 403 to receive. A rule without `min`/`max` allows any value; with them, out-of-range values are **clamped to the bound** and the response echoes the clamped value — nothing silent: clamps are logged everywhere, and on a deployed edge they land in the audit ring (the IDE server keeps no ring; its record is the takeover overlay plus the server log).
- Clamping happens in the written value's own domain: integer writes clamp to the integers inside `[min, max]`, REAL writes stay within the declared bounds even after float rounding. A write that *can't* be honestly clamped is **denied** instead of guessed at: `NaN` to a min/max-ruled REAL, a rule whose bounds don't form a valid range, or a range containing no writable value (e.g. `min = 10.2, max = 10.8` on an INT). Range rules are meaningful for REAL and signed-integer setpoints — bit-string types (DWORD, TIME) compare as the raw signed slot value, so don't put ranges on those.
- The table is **validated at project load** (server and deployed edge alike): unknown keys (`writemode`, `mim`), empty or duplicate `variable`s, non-finite bounds, and `min > max` fail the load with an error naming the offending rule. The one typo TOML can't catch is the *section name itself* — `[governence]` parses as an unrelated table and governance silently stays at its default `open`, so eyeball the section header after editing.
- **`force` bypasses governance by explicit decision** (ADR-0002): it stays the pure debug override and is never exported on any northbound surface. Mechanically, force only sticks on variables the program *reads* — which includes setpoints (see 02's force-precedence note) — but in a governed project, drive governed setpoints through writes so the clamps apply. Forcing a governed variable is a deliberate ungoverned override: it is attributed on the takeover overlay like any other mutating call, recorded in the edge audit ring, and belongs to commissioning/debug, not to normal operation.
- Sim scenarios can assert governance directly: a `set` step to an unlisted variable fails the step with the 403 reason, and a `set` past a rule's bound followed by an `expect` reads back the clamped bound (the `set` itself discards the response) — both deterministic offline proofs.
- Variable `unit`/`min`/`max`/`description` **metadata** (iomap.toml `[[mappings]]`, see `06-devices-iomap-tasks.md`) is documentation for generated surfaces; the `[governance]` rules here are the enforcement. Keep them consistent when both exist.
