# Config shapes: devices, iomap, tasks

The JSON shapes below are the real content of this file. **One command pattern reaches all of them** — get → edit → set, whole-document:

```bash
cs get devices/<name> --etag-file device.etag   # read the full Device JSON
cs set devices/<name> --from - --if-match @device.etag  # replace the version you read
cs get iomap --etag-file iomap.etag
cs set iomap --from - --if-match @iomap.etag  # body starts at "mappings"
cs get tasks --etag-file tasks.etag
cs set tasks --from - --if-match @tasks.etag  # body starts at "tasks"
```

Field names are snake_case; a wrong shape 422s with the reason on stderr (exit 2) — read it before retrying. Two device actions have no porcelain and go through `cs api`:

Keep each version with its original content while editing. A new named device
needs no `--if-match`; an existing target needs that original version or an
explicit `--force` for a deliberate full replacement. On 412, re-read and
reapply the edit instead of supplying a fresh version for stale content.

```bash
echo '{"detected":[16,32,48]}'    | cs api POST /api/devices/<name>/esi-assemble --from -   # modular EtherCAT, decimal idents
echo '{"node_id":"ns=2;s=Line1"}' | cs api POST /api/devices/<name>/opcua-browse --from -   # omit node_id for ObjectsFolder
```

> **The device body is the full `Device`**: a top-level `"name"` (must equal `<name>`, else 400) **and** a `"protocol"` discriminator (`modbus` | `ethercat` | `opcua` | `canopen`), then that protocol's fields — exactly what `cs get devices/<name>` prints, so it round-trips. `iomap` / `tasks` have no envelope (bodies start at `mappings` / `tasks`). Alarm *definitions* live in `alarms.toml` on the same pattern (`cs get alarms --etag-file alarms.etag` / `cs set alarms --from - --if-match @alarms.etag`); shape + sim/alarm workflow are in `09-sim-alarms.md`.

---

## Modbus device

Transport is a **tagged union** on `kind`. Old flat-`host`/`port` projects still load (auto-upgraded to `kind:"tcp"`); always *write* the new shape.

### TCP
```json
{
  "name": "hmi", "protocol": "modbus",
  "transport": { "kind": "tcp", "host": "192.168.1.50", "port": 502 },
  "slave_id": 1, "poll_interval_ms": 100,
  "channels": [
    { "name": "estop", "kind": "discrete_input",   "address": 0 },
    { "name": "start", "kind": "discrete_input",   "address": 1 },
    { "name": "valve", "kind": "coil",             "address": 0 },
    { "name": "level", "kind": "holding_register", "address": 0 },
    { "name": "temp",  "kind": "input_register",   "address": 0 }
  ]
}
```

### RTU (serial)
```json
{
  "name": "flow_meter", "protocol": "modbus",
  "transport": { "kind": "rtu", "serial_device": "/dev/cu.usbserial-A1B2",
                 "baud_rate": 9600, "data_bits": "eight", "stop_bits": "one", "parity": "none" },
  "slave_id": 1, "poll_interval_ms": 200,
  "channels": [ { "name": "valve", "kind": "coil", "address": 0 } ]
}
```

- `serial_device`: macOS `/dev/cu.usbserial-*`, Linux `/dev/ttyUSB0`, Windows `COM3`.
- `data_bits` `five|six|seven|eight` (def `eight`) · `stop_bits` `one|two` (def `one`) · `parity` `none|even|odd` (def `none`). Defaults are 8-N-1, so `{ "kind":"rtu", "serial_device":"…", "baud_rate":9600 }` suffices.
- `rs485` (optional, **Linux only**): half-duplex direction control (`TIOCSRS485`) for RTS-gated USB-485 adapters — add it when the port opens but every request times out with baud/parity/slave/wiring correct. Shape `{ "rts_on_send": true, "rx_during_tx": false, "delay_rts_before_send_ms": 0, "delay_rts_after_send_ms": 0 }`; omit for auto-direction adapters. Still silent after enabling → suspect the adapter itself.

### Channel `kind`
| kind | Modbus fn | read | write |
|---|---|---|---|
| `coil` | 01/05 | ✓ | ✓ |
| `discrete_input` | 02 | ✓ | ✗ |
| `holding_register` | 03/06 | ✓ | ✓ |
| `input_register` | 04 | ✓ | ✗ |

`address` is 0-based. An iomap `direction: output` against a read-only channel is a type error.

### Register data types (direct-to-instrument)
Register channels take two optional fields (default `u16` / `hi_lo`):
```json
{ "name": "flow",  "kind": "input_register", "address": 2,  "data_type": "f32", "word_order": "hi_lo" },
{ "name": "total", "kind": "input_register", "address": 30, "data_type": "u32", "word_order": "lo_hi" }
```
- `data_type`: `u16` (def) | `i16` | `u32` | `i32` | `f32`. 32-bit types span **two consecutive registers** from `address`.
- `word_order` (32-bit only): `hi_lo` = ABCD (spec default) | `lo_hi` = CDAB (common on Chinese instruments). Float reads as garbage (1.18e-38) → flip this first. Coils/discretes ignore both.
- `access`: `write` (default) | `read`. The register KIND alone does not grant write permission — couplers routinely map read-only measurements into HOLDING registers (the NX6 maps its DI word and AIs there; see `examples/nx6_modbus`). Mark those `"read"`: writes to them are rejected up front, and the shutdown/trip **failsafe sweep** (which zeroes every writable coil + holding register — deliberate on a field bus, unlike OPC UA's opt-in) skips them instead of drawing an Illegal-data-address exception. `validate_iomap` errors on an Output binding to an `access="read"` channel and warns when an input-only writable register is still marked writable.

The adapter merges channels into contiguous **read spans** per function code and bulk-refreshes a mirror every `poll_interval_ms` — a few reads, not one per channel (scales to hundreds). Writes queue so the single RTU connection is never concurrent; a failed poll holds last-known and retries.

---

## EtherCAT device

```json
{
  "name": "servo_bus", "protocol": "ethercat", "nic": "_sim",
  "cycle_us": 1000, "dc_sync": "off", "dc_static_sync_iterations": 0,
  "slaves": [
    { "index": 0, "name": "EK1100", "vendor_id": 2, "product_id": 72100946 },
    { "index": 1, "name": "SV660N", "vendor_id": 1048576, "product_id": 786701, "dc_sync": "sync0",
      "init_sdo": [ { "index": 24672, "sub_index": 0, "value": 8, "bits": 8 } ] }
  ],
  "channels": [
    { "name": "do_0", "slave_index": 0, "direction": "rx_pdo", "pdo_index": 28672, "sub_index": 1,
      "bit_length": 1, "data_type": "bool", "pdi_byte_offset": 0, "pdi_bit_offset": 0 }
  ]
}
```

- `nic`: `"_sim"` (or `""`) → in-memory simulator, runs anywhere (macOS/CI). A real name (`"eth0"`) → real `ethercrab` master, **Linux + `CAP_NET_RAW` only**.
- `dc_sync` (def `"off"`): `"off"` = free-run (IO couplers). `"sync0"` = SYNC0 pulse at `cycle_us` — **servo drives (SV660N) need it to reach OP** or SAFE-OP→OP times out. `slaves[].dc_sync` overrides per-SubDevice for mixed buses (bus `"off"`, servo `"sync0"`); the bus goes DC if *any* slave is `"sync0"`, and `"off"` slaves free-run inside it.
- `dc_static_sync_iterations` (def `0`): init-time drift compensation (FRMW burst). `0` is right for short buses; on a non-RT host one lost frame aborts init with `Timeout(Pdu)`. Raise to `1000`–`10000` on long DC buses.
- `slaves[].init_sdo`: CoE writes applied in **PRE-OP on every connect**, in order, before PDO mapping — how non-persisting drives get set each power-up (SV660N needs `0x6060 = 8`; PDO remap goes here too). Each entry `{ index, sub_index, value, bits (8|16|32) }`, decimal (`24672` = `0x6060`). A failed write aborts init.
- `direction`: `tx_pdo` (slave→master = **input**) | `rx_pdo` (master→slave = **output**). `data_type`: `bool` `u8` `i8` `u16` `i16` `u32` `i32` `real`.
- `pdi_byte_offset`/`pdi_bit_offset`: the entry's spot in the process image — **required for real hardware** (from the ESI/datasheet; sim ignores them; `bit_length < 8` uses the bit offset). `pdo_index`/`sub_index` are documentation-only.
- **Capacity**: up to **128 subdevices / 4 KiB image** per device (a 1000-point project ≈ 660 B). One device = one NIC = one bus.

### Bring-up mode (`bringup`)
Default `{ "mode": "auto" }` discovers process data from runtime CoE (`0x1C12`/`0x1C13`) — fine for fixed-PDO servos and slices. **ESI-driven modular couplers** (assembled module PDOs never appear over runtime CoE) need `{ "mode": "esi_modular", "esi_path": "esi/coupler.xml" }`; drop the vendor ESI at `<project>/esi/coupler.xml`, then assemble channels from it + the detected module idents (decimal, from the `0xF050` scan) in slot order:

```bash
echo '{"detected":[16,32,48]}' | cs api POST /api/devices/coupler/esi-assemble --from -
```

That parses the ESI, concatenates each module's PDO entries into the I/O images, and **replaces** the device's `channels` (names `m<slot>_<entry>`, slot-namespaced) so iomap can bind them. Author and verify offline in `nic:"_sim"`; real-bus `esi_modular` cyclic I/O is validated against the physical coupler separately.

### In-cycle electronic gear (`[[gear]]`)
B-tier motion. Rather than computing a follower's `target_position` in the PLC scan, an EtherCAT device carries `[[gear]]` entries so the **cyclic loop** generates the follower target every bus cycle, SYNC0-aligned — removing the scan-plane phase jitter that dominates inter-axis sync error at speed. Authored in the device TOML (a table-array, not the JSON body):

```toml
[[gear]]
slave_index        = 1        # follower axis
target_pos_offset  = 0        # 0x607A i32 follower output PDI — loop-owned
actual_pos_offset  = 4        # 0x6064 i32 follower input PDI
status_word_offset = 8        # 0x6041 u16 — gates on Operation Enabled
master = { kind = "virtual" }  # software accumulator, +master_vel counts/cycle
# master = { kind = "axis", slave_index = 2, actual_pos_offset = 4 }   # or gear off a real leader
```

Nine virtual channels carry the slow plane and bind in `iomap` without
fabricated PDO entries. Seven parameters accept Output bindings and Input
echoes: `gear_engage`, `ratio_num`, `ratio_den`, `ratio_step`, `phase_ofs`,
`master_vel`, `gear_max_travel`. The two feedback channels, `gear_engaged`
and `gear_trip`, accept Input only. Names are overridable per gear. Static
validation and both adapters share this routing catalog; unknown names,
feedback writes, and names colliding with PDOs or other gear fields remain
errors. The schema's `ratio_apply_channel` / `ratio_ack_channel` names are
reserved but **not routed by the device facade**; do not bind them in iomap.
I/O diagnostics are per mapping row: a name collision makes the whole device
unroutable, so it marks **every mapping row naming that device** — gear rows
and plain PDO rows alike — and those rows report the collision instead of
their own channel error until it is fixed. Devices not referenced by iomap are
outside this check; a clean map does not replace connect-time device
validation.

**Safety is enforced inside the loop** (no slow-plane mistake bypasses it):
the engine shadows `actual_position` until Operation Enabled (no jump at
enable); engage is refused unless `max_travel > 0`; ratio/phase latch at the
engage edge (mid-run edits inert until re-engage); travel past `±max_travel`
clamps then trips to a hold that clears only when engage drops; enable loss
forces a re-arm. The loop **owns `target_position` — leave it unmapped in
`iomap`** (any PLC write is overwritten each cycle). Static validation now
enforces that: an Output mapping onto an RxPDO covering the engine's four
target bytes on the follower slave is an error, not a silent discard. Reading
those bytes is a separate matter — they are controller→device, so an Input
mapping onto them fails the ordinary direction rule. Sim example and
bounded-trip scenario: `examples/eg_gear_incycle`. Simulation does not prove
physical timing, PDO layout, or machine safety; those remain bench gates.

---

## IoMap

```json
{
  "mappings": [
    { "application": "main", "variable": "estop_in",   "device": "hmi", "channel": "estop", "direction": "input"  },
    { "application": "main", "variable": "valve_open", "device": "hmi", "channel": "valve", "direction": "output" }
  ]
}
```

**Five fields, all required:** `application` (POU the variable lives in — **skipping it is the #1 422**), `variable` (IEC name in that POU), `device` (a name from `cs ls devices`), `channel` (a channel on it), `direction` (`input` = channel→variable, read before run_round | `output` = variable→channel, written after). Bindings that name an unknown device/variable/channel warn-skip at run time (they don't fail the run); a wrong *shape* 422s the `set`.

**Four optional metadata fields** (omit them and old files round-trip byte-identical): `unit` (engineering unit, e.g. `"%"`), `min` / `max` (meaningful engineering range), `description` (natural-language meaning, e.g. `"Tank level setpoint"`). The runtime never interprets them; they feed generated surfaces — `cs hmi generate` inherits them (setpoint inputs get the range, value nodes get the unit, descriptions become labels) and `cs get devices/<n>/describe` exports them into the agent reference file. Nonsense metadata is caught at validate time: `/api/project/validate` (`cs api POST /api/project/validate`) errors on non-finite bounds or `min > max` (naming the mapping index) and warns when a range is attached to a boolean channel. Write-range *enforcement* is a separate mechanism (`[governance]` in `project.toml` — see `09-sim-alarms.md`).

---

## Tasks (tasks.toml)

```json
{
  "tasks":    [ { "name": "fast", "interval_ms": 50, "priority": 1 } ],
  "programs": [ { "instance": "main_inst", "program": "main", "task": "fast" } ]
}
```

- `tasks[].interval_ms` → `TASK fast(INTERVAL := T#50ms, PRIORITY := 1)` in the synthesized CONFIGURATION. Periodic only, and **the real scan-cadence knob** (the bridge throttles there; ironplc's own task timer is neutralized at load so it can never skip a scan the bridge counted).
- `programs[].program` is a **PROGRAM**-kind POU; `instance` names it; `task` references a `tasks[].name`. Several instances run round-robin (`cs run` runs them all); the one rejected shape is 2+ PROGRAMs sharing a `VAR_GLOBAL` (see `01-mental-model.md` fact 2).

---

## OPC UA device (southbound to an existing DCS)

When the site's DCS/PLC owns the physical I/O, IA2 sits **above** it: read PV tags, write SP/command tags; the DCS keeps base regulatory control and safety.

```json
{
  "name": "dcs", "protocol": "opcua",
  "endpoint_url": "opc.tcp://10.0.0.10:4840",
  "auth": { "kind": "anonymous" }, "poll_interval_ms": 500,
  "channels": [
    { "name": "ft0202_pv",  "node_id": "ns=2;s=FT0202.PV",  "data_type": "f64", "access": "read" },
    { "name": "fv0203_cmd", "node_id": "ns=2;s=FV0203.CMD", "data_type": "f64", "access": "write", "failsafe": 0.0 }
  ]
}
```

- `auth`: `{ "kind": "anonymous" }` or `{ "kind": "user_password", "username": "...", "password": "..." }`. Session security policy is None in v1 (trusted segments / DA-gateway hops).
- `node_id`: full NodeId — `ns=2;s=Tag.Path` or `ns=3;i=1042`. Don't retype from UaExpert: `cs api POST /api/devices/<name>/opcua-browse --from -` (`{"node_id":"…"}`, omit for ObjectsFolder) walks the live address space with type hints.
- `data_type`: `bool` `i16` `u16` `i32` `u32` `f32` `f64` (f64 narrows to REAL's f32). `access`: `read` polls into a mirror (ONE bulk Read per interval for ALL tags) | `write` gets a direct Write on output.
- `failsafe`: written on shutdown/trip. **Leave unset by default** — the DCS keeps authority; set it only for tags exclusively IA2's.
- **OPC DA** (classic COM/DCOM): not spoken — route through a DA→UA gateway (KEPServerEX, Matrikon) and point IA2 at its UA endpoint.

---

## CANopen device (CiA 301 node on a CAN bus)

IA2 is the master side of a point-to-point conversation with one node; per-channel transport picks the lane.

```json
{
  "name": "servo", "protocol": "canopen", "interface": "can0", "node_id": 34,
  "poll_interval_ms": 100, "heartbeat_timeout_ms": 3000, "start_on_connect": true,
  "channels": [
    { "name": "statusword",      "index": 24641, "sub_index": 0, "data_type": "u16", "access": "read",  "transport": { "kind": "tpdo", "slot": 1, "byte_offset": 0 } },
    { "name": "controlword",     "index": 24640, "sub_index": 0, "data_type": "u16", "access": "write", "transport": { "kind": "rpdo", "slot": 1, "byte_offset": 0 } },
    { "name": "target_velocity", "index": 24831, "sub_index": 0, "data_type": "i32", "access": "write", "transport": { "kind": "sdo" }, "failsafe": 0 }
  ]
}
```

- `interface`: SocketCAN name (`can0`) on a Linux edge, or `"_sim"` (default on creation). Ops bring the real one up: `ip link set can0 up type can bitrate 500000`.
- `index`/`sub_index`: object-dictionary address, decimal (`24641` = `0x6041`); editor renders hex.
- `transport`: `{"kind":"sdo"}` polls/writes via SDO at `poll_interval_ms` (config-rate lane — setpoints, parameters). `tpdo`/`rpdo` ride process data on the CiA 301 predefined COB-IDs (`slot` 1–4, `byte_offset` into the ≤8-byte frame) using the node's existing mapping. Objects >4 bytes (segmented SDO) unsupported — bind a scalar.
- `heartbeat_timeout_ms`: no heartbeat this long → unhealthy (inputs freeze at last-known); `0` disables (SDO failures flip health instead).
- A healthy heartbeat is not a channel measurement. Unreceived inputs remain stale, and after link loss each channel needs a new TPDO/SDO sample before it becomes fresh again; heartbeat recovery alone does not revive its old value.
- `start_on_connect`: NMT Start so a pre-operational node enters Operational (PDOs only run there). Leave on unless another master owns NMT.
- `failsafe`: as OPC UA — only `write` channels with a value are written on trip; the adapter never sends NMT Stop (other tools may share the bus).

---

## Northbound (northbound.toml — MQTT to supOS / Tier0)

How the **edge runtime** publishes live data up to the plant platform. MQTT only. `cs get northbound --etag-file northbound.etag` / `cs set northbound --from - --if-match @northbound.etag`, or edit `northbound.toml`; the edge applies it at startup (redeploy/restart to change).

```json
{
  "mqtt": {
    "enabled": true, "broker_host": "10.0.0.5", "broker_port": 1883,
    "client_id": "", "username": "", "password": "", "topic_prefix": "",
    "publish_interval_ms": 1000, "qos": 0, "allow_write": false
  }
}
```

- `topic_prefix` defaults to `ia2/<project>`; `client_id` to `ia2-<project>`.
- Topics: `<prefix>/status` (retained `online`/`offline` via LWT — the platform sees crashes), `<prefix>/snapshot` (periodic `{"ts_us":…,"scan":…,"values":{…}}` typed JSON), `<prefix>/write` (only when `allow_write`; `{"name":"sp_flow","value":12.5}` → one-shot write).
- `allow_write` is **off by default** — making the link a control path is an explicit decision. Writes are one-shot (program can overwrite next scan); latch setpoints in program logic.
