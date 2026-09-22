# Troubleshooting + known limits

Each entry: the symptom you'll see, the cause, the fix.

## HTTP / CLI errors

### exit `2` with a server message on stderr
**Cause:** an HTTP 4xx. The CLI prints the server's reason **verbatim on stderr** and exits 2 — the message *is* the diagnosis, not a generic "bad request".
**Fix:** read it before changing anything. Examples: `missing field `application`` (an iomap mapping lacks the POU name) → add it; `duplicate alarm id "…"` (two `alarms.toml` entries share an id) → rename one. Usage errors (unknown flag, bad path) exit 2 the same way. (Exit 1 = problems in your content — diagnostics, failed probe, remote deploy failure, sim expectation failed; `≥3` = infrastructure, server down / 5xx.)

### `409` Conflict
**Cause:** a create-only or read-only path. Devices/edges don't hit this — `cs set` is upsert, so reconfiguring replaces in place. What does 409: `cs hmi generate <slug>` on a screen that already exists (curated by a human — pass `--force` only if you mean to overwrite), or writing a `pous/lib/<name>/…` block imported from a library (read-only through the generic POU routes — copy it out of `lib/` to own it).
**Fix:** for a rewrite you intend, read the document with `cs get <path> --etag-file version`, edit that content, then `cs set <path> --from - --if-match @version` (or `--force` for a deliberately replacing generate).

### `412` Precondition Failed / `set` asks for `--if-match`
**Cause:** another writer changed the document after your read, or no original-read version was supplied. This includes changes made directly on disk.
**Fix:** keep your local edit, re-read with `get --etag-file NEW_VERSION`, and reapply it to the new content. Never use a newer version to endorse the old body, and never use `--force` just to silence a conflict. A deliberate full replacement may use `--force`. New named resources need no flag; default config documents still need a version. See `02-cli-reference.md`.

### `422` on `cs set iomap` / `cs set devices/<n>` / `cs set tasks`
**Cause:** wrong JSON shape. For iomap, almost always a missing `application` field. For a device, a transport union missing its `kind`.
**Fix:** read the stderr reason first (see above), then `cs get iomap` (or `cs get devices/<n>`) to see the exact shape the server emits, edit *that*, set it back. Field names are snake_case.

### `404` on `cs set pous/<n>.st` / `cs get devices/<n>` / any project-scoped call
**Cause:** the project isn't open on the server, or `--project NAME` names a project that isn't in the open set.
**Fix:** `cs ls projects`. If absent, `cs project open /abs/path`. If multiple are open and you omitted `--project`, the server used its LRU active fallback — name it explicitly.

### `404` "not in registry" on `cs library import`
**Cause:** the server found no library directory to import from — *not* a bad library name.
**Fix:** the FB-library registry resolves the repo's `library/` when `ia2-server` runs from the checkout root, and the installed `~/.local/share/ia2/library` otherwise. So start the server from the repo root, pass `--library-dir /path/to/library`, or re-run `scripts/install-skill.sh` (it vendors the library to `~/.local/share/ia2/library`). Then `cs ls library` should list `process-control`.

### `400` "project 'X' is not open on this server"
**Cause:** you passed `--project X` but X isn't open.
**Fix:** `cs project open` it first, or pick a name from `cs ls projects`.

### CLI can't reach the server at all (`connection refused`)
**Cause:** no server on the expected port. `cs` defaults to `:3001`.
**Fix:** start one (`ia2-server --bind 127.0.0.1:3001 &`) or discover a non-default port (see `checklists/first-contact.md`) and pass it as `--server`.

### `cs` command works but the IDE shows nothing / a different project
**Cause:** the IDE window is scoped to a different `?project=` than the one your command hit.
**Fix:** the human's window shows project A; your `cs` hit project B (active fallback). Pass `--project A` to match what they're looking at, or have them switch the window's project picker.

## Run / compile errors

### "tasks.toml schedules N PROGRAMs but the project declares VAR_GLOBAL …"
**Cause:** multi-PROGRAM projects run one container per instance, so a `VAR_GLOBAL` can't be shared across them (each instance would get a private, diverging copy). This is the *only* multi-PROGRAM restriction — scheduling several PROGRAMs is otherwise supported, and they run round-robin.
**Fix:** move the shared state behind an I/O mapping or FUNCTION_BLOCK parameter, or reduce `tasks.programs` to a single PROGRAM. Both `cs run` and `cs project check` name the offending globals. See `01-mental-model.md` fact 2.

### `cs project check` fails with `P####`
**Cause:** an IEC source error.
**Fix:** `cs check --explain pous/FILE.st` (or `cs check P####` for a bare problem code) prints the full ironplc problem doc. Common ones: missing `;`, C-style `&&` instead of `AND`, bare number where a `T#…ms` time literal is expected, a second PROGRAM in the file. See `05-iec-61131.md` § quirks.

### "modbus connect failed" in the run log, but the run still starts
**Cause (TCP):** nothing listening at host:port. **(RTU):** the serial device doesn't exist / is busy / wrong permissions.
**Fix:** this is **non-fatal by design** — the device is skipped, the scan loop runs with that device's channels reading zero. For RTU on macOS, confirm the path with `ls /dev/cu.*`; the device often appears as `/dev/cu.usbserial-XXXX`. Permissions: the user (not root) usually owns USB-serial adapters on macOS; on Linux add the user to `dialout`.

### A variable in the Monitor pane won't change no matter what
**Cause:** it's force-pinned and someone forgot to `unforce`.
**Fix:** `cs runtime status --json` lists active forces. `cs runtime unforce NAME`. (This is why workflow recipe D always unforces at the end.)

### RETAIN value didn't persist across restart
**Cause:** the variable isn't in a `VAR RETAIN` block; or the block is inside a `FUNCTION_BLOCK`, which is not persisted at all; or the program was killed hard (not a clean stop) between the 5 s flush windows.
**Fix:** confirm the `VAR RETAIN` declaration sits in a `PROGRAM` or `VAR_GLOBAL` block — an FB-internal one is warned about at compile time and ignored (`grep` the runtime log for `VAR RETAIN inside a FUNCTION_BLOCK`). Note the flush cadence is 5 s + on clean stop — up to 5 s of change can be lost on an unclean kill. Values themselves are stored as raw 64-bit slots and do not truncate.

### The run stopped by itself — snapshots frozen, nothing obvious in the way
**Cause:** the program faulted. A VM trap (divide by zero, bad array index, …) in any scheduled instance stops the whole plant and zeroes outputs (failsafe), by design.
**Fix:** read the reason instead of re-running blind: `/api/runtime/status` reports `running: false` with `last_error` carrying the trap message (e.g. `VM trap in main_inst: DivideByZero`), and the SSE stream emitted `error` then `stopped` at the moment it died. On an edge, the runtime's own `/status` carries the same message in `fault`. Fix the arithmetic (guard divisors, clamp indices) and run again — the next `cs run` clears `last_error`.

### Ran `cs run --program X`, but the snapshot shows a different POU's variables (e.g. the template's `counter`/`blink`)
**Cause:** you're on a pre-2026-07 binary. The `--program`-without-`--file` path used to concatenate every POU file and ironplc compiles the *first* PROGRAM in that blob — so whichever file sorted first (usually the seeded `main.st`) ran instead of the one you asked for, while `running_info` echoed the requested name. Your program's variables then look "always 0" from an HMI or any by-name read, because they were never in the snapshot at all.
**Fix:** current binaries compile the named PROGRAM into its own hoisted unit, so the request and the snapshot agree. On an old edge you can't upgrade yet, pass the file too (`cs run --program X --file pous/x.st`) or schedule X in `tasks.toml` and use plain `cs run` — both paths always ran the right program. Diagnostic signature: `running_info` names your program but the snapshot's variable list belongs to another POU.

### FB VAR_OUTPUT reads as zero / run faults with `InvalidVariableIndex` the moment a user FUNCTION is added
**Cause:** you're on a pre-2026-07 binary. User FUNCTIONs and user FUNCTION_BLOCKs were numbered into overlapping function-id ranges, so a PROGRAM calling both executed FB bytecode where the FUNCTION should run — typically an immediate `VM trap: InvalidVariableIndex`, occasionally silent state corruption.
**Fix:** fixed in the vendored compiler (ADR-0001 patch #2); rebuild/redeploy. Until then the workaround is to avoid mixing user FUNCTIONs and user FBs in one compiled unit — inline the function body or express it as another FB.

### `cs check` rejects an FB that declares `VAR t : TON;` (or any FB-typed field)
**Cause:** nested FB instances inside a FUNCTION_BLOCK aren't supported by the vendored codegen — there is no per-instance storage for the inner block. The error names the field, its type, and the enclosing FB.
**Fix:** hoist the instance into the calling PROGRAM and feed its results to the FB through `VAR_INPUT`. See `05-iec-61131.md` § quirks.

## Overlay / session

### The takeover banner strobes (label changes every couple seconds)
**Cause:** you're running commands without a session — each is a transient heartbeat.
**Fix:** wrap the workflow in `cs agent run --label "…" -- bash -c '…'`. See `03-agent-sessions.md`.

### The banner is stuck on after your work finished
**Cause:** you used `cs agent enter` but never `cs agent leave` (or a script errored before leave).
**Fix:** `cs agent leave` (reads `IA2_AGENT_SESSION`), or the server's 30 s watchdog ends it, or the human clicks "End session". **Prefer `cs agent run`** — it always cleans up, even on Ctrl-C.

### The human clicked "End session" mid-workflow
**Cause:** they want control back.
**Fix:** stop. Don't reopen a session and keep going. Ask what they want.

## EtherCAT (real mode)

### `init_single_group: Timeout(Pdu)` and/or `failed to decode raw PDU data`
The NIC isn't dedicated to EtherCAT. On NetworkManager hosts the usual cause is NM still managing the port — its periodic DHCP/activation corrupts raw L2 frames and flaps the link. Set the interface `unmanaged` and confirm hardware offloads are off (see `docs/edge-deploy.md` → *Dedicate the NIC to EtherCAT*). Use a separate NIC from your SSH/management link.

### Bus discovers the SubDevice but never reaches OP; drive shows a comm fault (e.g. `EE`-class)
A servo can wedge in a half-configured state after an init that aborted mid-configuration. **Power-cycle the drive**, let its panel reach idle, then start the runtime once cleanly. Also confirm the drive has `dc_sync = "sync0"` (servo drives need DC SYNC0 to reach OP) — set it per-device, or per-SubDevice on a mixed servo + IO bus.

### `init sdo write … : object does not exist in the object directory`
An `init_sdo` entry targets a CoE object the drive doesn't have. A failed startup SDO aborts init by design (better than silently running a mis-configured drive) — drop or fix that entry. Example: the Inovance SV660N has no `0x6080` (max motor speed); cap torque via `0x6072` and limit speed in your program instead.

## OPC UA

- **`Failed to read own certificate/private key` ERROR lines on connect** — harmless noise. The UA library probes its PKI store even when the session uses SecurityPolicy None (IA2's v1 posture); no certificate is needed and the connection proceeds. Ignore unless you also see the connect itself failing.
- **`browse failed: opcua endpoint … unreachable`** — the endpoint in the device config must be reachable *from the IDE server machine* (browse connects live). Check URL/port, then whether the UA server allows anonymous None-policy sessions.
- **Tags read as 0 while connected** — usually a `node_id` typo (the mirror skips tags the server answers with a bad status) or a wrapped/nonscalar value on the server side. `cs api POST /api/devices/<name>/opcua-browse --from -` (`{"node_id":"…"}`, or `{}` for ObjectsFolder) shows the exact NodeIds; the adapter unwraps `Variant`/`DataValue` nesting but arrays/strings don't map to channels.

## CANopen

- **All values frozen at 0, device unhealthy after `heartbeat_timeout_ms`** — node absent or wrong `node_id`. On a real bus check `ip link` (interface up? bitrate right?) and `candump can0` for the node's 0x700+id heartbeat.
- **PDO channels stay stale despite healthy heartbeats** — the node may be pre-operational (PDOs only run in Operational), or its PDO mapping is wrong. Leave `start_on_connect` on, or start it from the vendor tool; verify the PDO slot/offsets match the node's actual mapping. No sample is not a zero measurement, and recovery requires new channel data, not just a heartbeat.
- **`needs a segmented SDO transfer (>4 bytes)`** — the object is a string/array/domain. Bind a scalar sub-object instead; segmented transfers are out of scope.
- **macOS: `SocketCAN requires a Linux edge`** — expected; real CAN interfaces exist on the Linux edge only. Use `interface = "_sim"` for development.

## Known limits (not bugs — design constraints today)

- **Multi-PROGRAM runs are supported** — one container per scheduled instance, round-robin on one scan thread. The sole restriction is no `VAR_GLOBAL` shared across instances (rejected with a clear error). See `01-mental-model.md` fact 2.
- **One running program per server.** Hardware (Modbus/EtherCAT bus) can have one master. Starting a program stops the previous, across all projects.
- **No `AT %IX0.0` located variables.** Bind via `iomap.toml`, not IEC direct addressing.
- **Real EtherCAT is Linux-only** (`CAP_NET_RAW`). On macOS use `nic: "_sim"`.
- **RETAIN works in `PROGRAM` / `VAR_GLOBAL` blocks only** — an FB-internal `VAR RETAIN` is ignored (compile-time warning). Values are raw 64-bit slots; nothing truncates.
- **No per-entry iomap/tasks/device-channel edits.** Whole-document get → edit → set.
- **WSTRING** is upstream-WIP; don't author WSTRING programs expecting them to run.
- **No FB instances inside a FUNCTION_BLOCK** (e.g. a TON declared in an FB's `VAR` block) — compile-time error; hoist the instance into the calling PROGRAM. See `05-iec-61131.md` § quirks.
