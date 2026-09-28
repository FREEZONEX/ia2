# Workflow recipes

Pattern-match the user's intent to one of these. Each is a complete, copy-adaptable sequence. All assume `CS` and `SRV` are set (see `checklists/first-contact.md`) and that multi-step work is wrapped per `03-agent-sessions.md`.

---

## A. New project from scratch → running

```bash
cs agent run --label "New project: tank_ctrl" --server "$SRV" -- bash -c '
set -e
SRV="'"$SRV"'"

# 1. create (becomes the active project)
cs project create tank_ctrl --server "$SRV"

# 2. Deliberately replace the just-created template main PROGRAM.
cs --project tank_ctrl set pous/main.st --force --server "$SRV" --from - <<"ST"
PROGRAM main
  VAR level : INT := 0; setpoint : INT := 800; valve_open, pump_on : BOOL; END_VAR
  VAR RETAIN cycle_count : DINT := 0; END_VAR
  cycle_count := cycle_count + 1;
  IF valve_open AND level < 1000 THEN level := level + 20; END_IF;
  IF pump_on   AND level > 0    THEN level := level - 15; END_IF;
  IF    level >= setpoint + 50 THEN valve_open := FALSE; pump_on := TRUE;
  ELSIF level <= setpoint - 50 THEN valve_open := TRUE;  pump_on := FALSE; END_IF;
END_PROGRAM
ST

# 3. validate BEFORE running
cs project check ~/Documents/IA2/tank_ctrl

# 4. run one PROGRAM ad-hoc (no tasks.toml needed for this)
cs --project tank_ctrl run --program main --server "$SRV"
'
```

Then tell the user: "Monitor pane should now show `level` oscillating around 800, `valve_open`/`pump_on` toggling, `cycle_count` climbing."

---

## B. Add a device + wire it to program variables

Devices and iomap are JSON: `cs get devices/<n> --etag-file device.etag` → edit → `cs set devices/<n> --from - --if-match @device.etag`. The device body is the full `Device` (top-level `name` + `protocol`), exactly what `cs get devices/<n>` prints — so it round-trips. `cs set` is upsert: a body that already carries `protocol` creates the device outright, no separate create step. (Full shapes: `06-devices-iomap-tasks.md`.)

```bash
cs agent run --label "Wire HMI to tank_ctrl" --server "$SRV" -- bash -c '
set -e
SRV="'"$SRV"'"

# NEW device — the "protocol" in the body creates it; existing names require an original-read ETag
cs --project tank_ctrl set devices/hmi --server "$SRV" --from - <<"JSON"
{ "name": "hmi", "protocol": "modbus",
  "transport": { "kind": "tcp", "host": "127.0.0.1", "port": 5502 },
  "slave_id": 1, "poll_interval_ms": 100,
  "channels": [
    { "name": "estop",  "kind": "discrete_input",   "address": 0 },
    { "name": "valve",  "kind": "coil",             "address": 0 },
    { "name": "level",  "kind": "holding_register", "address": 0 } ] }
JSON

# iomap — read the original version before editing; preserve any existing mappings
cs --project tank_ctrl get iomap --etag-file iomap.etag --server "$SRV"
cs --project tank_ctrl set iomap --if-match @iomap.etag --server "$SRV" --from - <<"JSON"
{ "mappings": [
  { "application": "main", "variable": "valve_open", "device": "hmi", "channel": "valve", "direction": "output" },
  { "application": "main", "variable": "level",      "device": "hmi", "channel": "level", "direction": "output" } ] }
JSON

cs project check ~/Documents/IA2/tank_ctrl
'
```

---

## C. Configure tasks.toml + run the full schedule

`cs run` (no `--program`) runs the whole tasks.toml — every scheduled PROGRAM, each in its own container, round-robin on one scan thread. The only rejected shape is 2+ PROGRAMs that *also* share a `VAR_GLOBAL` (globals aren't shared across instances). See `01-mental-model.md` fact 2.

```bash
cs --project tank_ctrl get tasks --etag-file tasks.etag --server "$SRV"
cs --project tank_ctrl set tasks --if-match @tasks.etag --server "$SRV" --from - <<'JSON'
{ "tasks":    [ { "name": "fast", "interval_ms": 50, "priority": 1 } ],
  "programs": [ { "instance": "main_inst", "program": "main", "task": "fast" } ] }
JSON
cs --project tank_ctrl run --server "$SRV"   # whole schedule
```

---

## D. Debug session (force / pause / step)

```bash
cs agent run --label "Debug fill logic" --server "$SRV" -- bash -c '
set +e
SRV="'"$SRV"'"
cs --project tank_ctrl run --program main --server "$SRV"; sleep 3
cs --project tank_ctrl runtime snapshot --vars level,setpoint,valve_open --server "$SRV"  # LIVE values (status has none)
cs --project tank_ctrl runtime force setpoint 200 --server "$SRV"; sleep 3   # tank drains
cs --project tank_ctrl runtime pause  --server "$SRV"; sleep 1              # freeze
cs --project tank_ctrl runtime step 20 --server "$SRV"; sleep 2            # advance exactly 20
cs --project tank_ctrl runtime resume --server "$SRV"; sleep 2
cs --project tank_ctrl runtime unforce setpoint --server "$SRV"           # release — IMPORTANT
cs --project tank_ctrl runtime status --server "$SRV"                     # confirm no leftover forces (mode + forces)
# after the fact — the 1 Hz historian (~2 h) and any standing alarms:
cs --project tank_ctrl get runtime/history --query vars=level,setpoint --query step_ms=1000 --server "$SRV"
cs --project tank_ctrl get runtime/alarms --server "$SRV"
# cs --project tank_ctrl runtime ack <alarm-id> --server "$SRV"          # ack one you deliberately caused
'
```

Always `unforce` what you `force`. A leftover force is invisible until someone wonders why a value won't change. And remember the split: `runtime status` is *mode + forces only* — watch numbers move with `runtime snapshot`, and reconstruct the trend afterward with `get runtime/history`.

---

## E. RTU (real serial hardware)

Switch a Modbus device to RTU by setting its transport. macOS device paths look like `/dev/cu.usbserial-XXXX`; Linux `/dev/ttyUSB0`; Windows `COM3`.

```bash
cs --project tank_ctrl get devices/hmi --etag-file hmi.etag --server "$SRV"
cs --project tank_ctrl set devices/hmi --if-match @hmi.etag --server "$SRV" --from - <<'JSON'
{ "name": "hmi", "protocol": "modbus",
  "transport": { "kind": "rtu", "serial_device": "/dev/cu.usbserial-A1B2",
                 "baud_rate": 9600, "data_bits": "eight", "stop_bits": "one", "parity": "none" },
  "slave_id": 1, "poll_interval_ms": 200,
  "channels": [ { "name": "valve", "kind": "coil", "address": 0 } ] }
JSON
```

RTU is slow — keep `poll_interval_ms` ≥ 200 at 9600 baud. A missing serial device fails the device connect gracefully (logged warning, scan loop continues with that device skipped); it does NOT crash the run.

---

## F. Deploy to an edge controller

```bash
cs --project tank_ctrl set edges/field_pi --host pi@plc.local --server "$SRV"  # create (or put "host" in a --from body)
cs --project tank_ctrl get edges/field_pi --server "$SRV"     # check install_dir / runtime_port
cs deploy field_pi --server "$SRV"                            # tar → ssh → versioned swap → restart
cs probe  field_pi --server "$SRV"                            # confirm the runtime came up and runs (✓, not ⚠)
```

Deploy ships the project **and** the `ia2-runtime` binary — but only if a **Linux ELF** for the edge's arch is present in `target/` (the deploy guards against shipping a wrong-arch/host binary, e.g. a macOS build); otherwise it carries forward the runtime already on the box. So cross-compile `ia2-runtime` for the edge's arch yourself before a binary-bearing deploy — there's no CI building artifacts. The edge runs headless; RETAIN state lives in `<install_dir>/state/retain.json` on the box.

**Deploy fails closed** (`ok:false`, exit 1): a remote systemd restart that doesn't come back, a broken tar stream, or a missing `VERSION=` stamp all fail the deploy and print the remote log — it will not report success on a half-applied push. install_dir/systemd *drift* surfaces as a structured `warning` on an otherwise-successful deploy (the project shipped, but the box's layout isn't what the config expects — read it).

---

## G. Drive / debug a deployed edge runtime (`--edge`)

`cs runtime …` and the introspection commands take `--edge <name>` to hit a *deployed* edge instead of the local server — same surface as the web **Edge → Debug** tab, so the pokes render in the IDE's agent-takeover overlay.

```bash
cs --project tank_ctrl get edges/field_pi/scan --server "$SRV"                  # connect status + discovered EtherCAT topology
cs --project tank_ctrl get edges/field_pi/logs --query tail=80 --server "$SRV"  # OP transitions, bus errors
cs --project tank_ctrl runtime snapshot --edge field_pi --server "$SRV"         # LIVE values from the box (status = mode+forces only)
cs --project tank_ctrl runtime force speed 500 --edge field_pi --server "$SRV"  # setpoint poke (negatives: ... force speed --edge field_pi -- -500)
cs --project tank_ctrl runtime unforce speed --edge field_pi --server "$SRV"    # release — IMPORTANT
```

**Transport:** each `--edge` call is one-shot `ssh host curl 127.0.0.1:<runtime_port>/<verb>` — a fresh SSH handshake per call (see `02-cli-reference.md`). Fine for occasional pokes; a poor fit for anything resembling a control loop.

**`force --edge` is a debug override, not a setpoint channel — know when *not* to use it:**
- Driving hardware for more than a quick *supervised* poke via `force` is a smell. For a real, repeatable setpoint make the variable an **iomap-bound input** (HMI register / recipe) or compute it in **POU logic** (e.g. a motion profile), and change it through the normal data path. Keep `force` for "pin this for a moment to see what happens."
- For **unattended / throughput / tight-loop** work, run the loop *on the box* (one persistent ssh, local `curl`s) rather than per-call `cs --edge`. The `cs --edge` path earns its hops only when a human is watching the IDE overlay and you want the action on the same audited path the GUI uses.
- It drives **real outputs** on a live bus. Treat a motion variable as you would at the panel, and pair every `force` with `unforce` (`checklists/handoff.md`).

> Worked example — spinning a real Inovance SV660N in CSP: a CiA-402 state machine in the POU did the enable sequence + `target_position := target_position + speed` ramp; the agent only `force --edge`-ed the internal `speed` knob (then `unforce` to stop). The POU did the control; force just injected the setpoint. Fine for a supervised demo — but for a product feature you'd bind `speed` to a real input rather than debug-force it.

---

## H. Multi-project work

When more than one project is open, **every** command needs `--project`. Check first:

```bash
cs ls projects --server "$SRV"           # see what's open, which is active (*)
cs --project bottling get pous/main.st --etag-file bottling.etag
cs --project bottling set pous/main.st --from - --if-match @bottling.etag ...
cs --project mixer get pous/main.st --etag-file mixer.etag
cs --project mixer set pous/main.st --from - --if-match @mixer.etag ...   # different window, different project, no cross-talk
```

Only one program runs at a time across the whole server. If `bottling` is running and you `cs --project mixer run`, the bottling program stops. Tell the user before doing that.

---

## I. Prove behaviour with a scenario (before hardware)

Don't hand-drive `force`/`snapshot` to convince yourself the logic is right — write it down as a scenario and let `cs sim run` assert it. The scenario *is* the plant: `set` steps write inputs, `expect`/`expect_never`/`expect_alarm` poll for a condition against a deadline. Exit 0 = every expectation held; exit 1 = a step failed, and the report names the step, the deadline, and the last observed value. CI-ready and re-runnable.

```bash
cs agent run --label "Prove fill logic" --server "$SRV" -- bash -c '
set -e
SRV="'"$SRV"'"
cs --project tank_ctrl project check ~/Documents/IA2/tank_ctrl   # compile-clean first
cat > ~/Documents/IA2/tank_ctrl/scenarios/fill.toml <<"TOML"
# ... set/expect/expect_never/expect_alarm steps — vocabulary in 09-sim-alarms.md
TOML
cs --project tank_ctrl sim run scenarios/fill.toml --server "$SRV"        # exit 0 = all held
'
```

`cs sim run` starts the program against the sim device layer, plays the file top to bottom, then stops it (`--keep-running` to leave it up, `--no-run` to attach to a run you already started, `--program NAME` for one PROGRAM, `--trace out.jsonl` to record every polled snapshot). The full step vocabulary and the alarm/history workflow live in `references/09-sim-alarms.md` — read it before writing a scenario.
