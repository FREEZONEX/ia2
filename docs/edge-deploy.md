# Edge deployment

How to ship an IA2 project to a Linux edge box and use the IDE
for online debugging against it.

## What this is

The IDE has three top-level concepts:

| | What it represents |
| --- | --- |
| **POUs** | Your ST source files. One `.st` file can hold one or more PROGRAM / FUNCTION_BLOCK / FUNCTION declarations. |
| **Devices** | Things your program talks to over a bus or network (Modbus, EtherCAT, OPC UA) |
| **Edges** | Linux boxes where the program runs in production |

Edges are deploy targets, not runtime peers. The IDE never opens a network
port to one directly — every interaction goes through SSH. You add an
`~/.ssh/config` entry for the edge, list it in the project, click Deploy,
and the IDE bundles the project + a runtime binary, scp's it over,
atomically swaps the live version, and restarts the systemd unit.

The runtime claims its HTTP listening socket before starting device connections
or creating its state directory. If the address cannot be bound (for example,
another instance already holds the port), it exits with a `binding ...` error
without touching devices. Binding successfully is not proof of healthy devices;
check `/health` and `/status` after startup.

Pause stops program scans, not adapter workers or physical motion. Variable
writes still update the VM while paused; force commands register overrides
that apply on the next scan. Output synchronization resumes with `resume` or
`step`. Stopping the program runs the failsafe pass; pause does not.

## One-time edge setup

You need a Linux box reachable over SSH with key-based auth (no password
prompts — the IDE runs `ssh -o BatchMode=yes`).

1. **Get a runtime binary for the edge's architecture**. From a Linux dev
   machine of the same arch:
   ```sh
   cargo build --release -p ia2-runtime
   # binary lands at target/release/ia2-runtime
   ```
   For cross-arch (e.g. ARM64 edge from x86_64 dev), use `cross`:
   ```sh
   cargo install cross --git https://github.com/cross-rs/cross
   cross build --release -p ia2-runtime \
     --target aarch64-unknown-linux-gnu
   # binary lands at target/aarch64-unknown-linux-gnu/release/ia2-runtime
   ```
   The dependency tree is NOT pure Rust: `ring` (C + assembly) arrives via
   `rumqttc` → `tokio-rustls` → `rustls` for the MQTT northbound's TLS. So a
   cross build needs a C toolchain for the target, which is the reason to use
   `cross` (it supplies one in a container) rather than plain
   `cargo build --target`. A bare `rustup target add` plus `cargo build
   --target x86_64-unknown-linux-musl` on macOS fails in `ring`'s build script
   with `failed to find tool "x86_64-linux-musl-gcc"` — verified 2026-09-16.
   Check with `cargo tree -p ia2-runtime -i cc` before assuming otherwise.

2. **Bootstrap the edge**. From your dev machine:
   ```sh
   scp infra/ia2.service edge:/tmp/
   scp infra/install.sh             edge:/tmp/
   scp target/.../release/ia2-runtime edge:/tmp/
   ssh edge "sudo INSTALL_DIR=/opt/ia2 \
                 RUNTIME_BIN=/tmp/ia2-runtime \
                 UNIT_FILE=/tmp/ia2.service \
                 bash /tmp/install.sh"
   ```
   Verify with `ssh edge systemctl status ia2`. It should be
   *enabled, not yet started*.

3. **Optional: smoke-start the stub**. Confirms the binary itself runs:
   ```sh
   ssh edge "sudo systemctl start ia2 && \
             curl -s http://127.0.0.1:13001/health"
   # → {"status":"ok","uptime_secs":2,"scan_count":15}
   ```

## In the IDE

1. **Add the edge to your project**. Click `+` next to "Edges" in the
   tree. Name it (free-form, no spaces), and point `Host` at your SSH
   alias. The IDE will run literally `ssh <host>`, so any
   `~/.ssh/config` entry works — including jump hosts, custom keys, etc.

2. **Wait for the probe**. The edge pane auto-probes every 10 s and on
   open. Green `running` badge = the runtime's `/health` came back ok
   **and** every configured fieldbus device is connected. An ochre
   `degraded` badge means the runtime is up and scanning but at least one
   bus is down — hover it for the device names. That state is easy to
   misread from the outside: the scan count keeps climbing and values keep
   updating, but the down device's inputs are frozen at last-known values
   and its outputs are dropped. A red `faulted` badge means the runtime
   answers but its program stopped (a VM trap, say) — hover it, or read
   the status grid, for the reason. A red `outputs locked` badge means the
   outputs are latched off, by the scan watchdog or by the project's
   alarm-supervision policy (the badge says which): every output is zeroed and
   held off until the program is restarted. An ochre `degraded` badge also covers a runtime
   whose snapshot task has ended or is stuck on one snapshot: the program and
   the buses are fine, but history is not recording and process alarms are not
   being evaluated (`/health`'s `snapshot_tap.running` is `false`, or
   `stalled_for_ms` is set; restart the runtime). The
   badge only judges that persistent state; whether `snapshot_tap.faults` is
   still climbing is for `cs probe` and a second look.

   ST scans are also bounded to 10,000,000 opcodes or one second (a shorter
   explicit container watchdog wins). The opcode ceiling is independent of
   the host and is normally the first one a runaway meets; reaching it takes
   tens of milliseconds on a fast CPU and longer on a slow edge board, so
   the time ceiling is only the backstop. Time is sampled every 256
   opcodes; this is not a hard real-time interrupt. A loop exceeding either
   ceiling ends the program with `VM execution budget exceeded in <instance>:
   instruction limit (10000000 opcodes per scan)` (or `time limit (<n> ms per
   scan)`), visible in
   `/health`'s `fault`, then attempts device failsafe/shutdown. Unlike the
   five-overrun latch, this does not keep the VM computing. Partial scan
   outputs and final RETAIN state are discarded. A normal stop (deploy,
   SIGTERM) lets the scan in flight finish and writes the final RETAIN
   checkpoint; only a scan still running 250 ms after the stop request is
   discarded as hung, with no fault and no final RETAIN write. Fault exit
   stops reconnect attempts while keeping delivered devices' I/O tasks alive
   through drain. Stopping while a device is still mid-connect delays the
   scan thread's exit by at most 2 s; the late adapter is failsafed by its
   own worker, and if that did not finish in time the closing log line says
   the outcome is unconfirmed rather than "exited cleanly".

3. **Deploy**. Click `Deploy`. The IDE:
   - `tar`s your project directory + (if found) a freshly-built
     `ia2-runtime` from the dev machine
   - Pipes the tar into `ssh edge bash …` which extracts to
     `$INSTALL_DIR/versions/<UTC-timestamp>.<unique-suffix>/`, atomically swaps the
     `current` symlink, and `systemctl restart ia2`s
   - Streams the remote script's output back into the pane

   Deploy REFUSES to lie about the outcome:
   - deploy first copies regular project files into a private temporary snapshot.
     The shared runtime pre-check and upload both consume that same copy, so
     edits after the check cannot alter the upload. The snapshot is cleaned up
     after success, failure or cancellation. Symlinks and special files are
     refused before upload; replace them with regular project files;
   - a project the edge runtime would refuse at startup is refused before
     upload, and nothing on the edge changes: deploy first runs the
     runtime's own start path on it (open the project, which validates
     `[governance]`; require `tasks.toml`; the multi-PROGRAM `VAR_GLOBAL`
     rule; compile every scheduled PROGRAM). The report is `ok: false`
     with an empty `version` and the reason in `log`. It compiles with
     this server's compiler — the edge's too when the deploy ships a
     runtime binary built from the same tree. A deploy that ships none
     keeps the edge's own binary, whose compiler may differ; the
     post-restart check below stays the authority;
   - a broken tar stream or a local tar failure fails the deploy (no
     silently-truncated uploads);
   - a failed `systemctl restart` fails the deploy (`ok: false` + the
     log). The prior `current` link is restored, or removed on a failed first
     install; staged files remain for inspection. Unique version directories
     keep same-second deployments from overwriting the rollback payload.
     Runtime state is unconfirmed: a failed restart may already have stopped
     the old process. File rollback does not prove that it is running.
     Whether an automatic restart that systemd had already scheduled can
     later start the version the link was restored to (R11) is **pending real
     systemd verification**; a device-free, opt-in fixture and its execution
     instructions are in [R11 offline verification](recovery-r11-offline.md).
     Preparing it does not change deploy behavior or establish a reproduction;
   - a missing `VERSION=` line from the remote script fails the deploy
     (script drift = state unknown);
   - a restart systemd accepted is not a running program: the check requires
     continuous mode and advancing positive scan counts in consecutive `/status`
     reads. Counter/uptime resets and changed `runtime_id` restart the observation;
     unchanged counts are waited for, since long-cycle tasks may not advance each
     second. New runtimes provide a per-process `runtime_id`; older ones fall back
     to counter/uptime evidence, which cannot exclude every fast restart;
   - the entire check, including SSH reads and poll sleeps, has a 30 s deadline.
     A fault/watchdog is `faulted`, paused/single-step mode is `not_running`, and
     failure to confirm progress before the deadline is `unknown`. A runtime whose
     snapshot task has stopped, is stuck on one snapshot (`snapshot_tap.stalled_for_ms`),
     or whose `snapshot_tap.faults` rose between two reads, is `unmonitored`: nothing is recorded, no process alarm can be raised,
     and `scan_count` freezes with the task, so progress cannot be confirmed
     either. A fault count that is steady does not change the verdict but is
     named in it. `faulted`, `not_running`, `unmonitored` and `unknown` all fail
     the deploy (`ok:false`). **Unknown does not prove stopped or physically safe.**
     A project that declares `[supervision] on_loss = "safe_state"` is also held
     to it: the check compares the declaration with the `supervision` the
     runtime reports it is enforcing, and `unprotected` (also `ok:false`) means
     they differ, typically because the edge runs an older runtime that ignores
     the table.
     A task with a period longer than the observation window may remain unknown;
   - the new version stays installed and current. No automatic rollback or
     restart of the previous program occurs after a failed health check. Inspect
     runtime and plant state before choosing manual recovery; the log names the
     previous version. A down device on a running program remains a warning.
     If nothing was restarted or the install directory differs from the service,
     `not_checked` retains installation success (`ok:true`), while the IDE shows
     program state unconfirmed, never green live;
   - invalid `[governance]` (unknown keys, `min > max`, non-finite bounds)
     is refused during snapshot preflight, before upload;
   - install_dir vs systemd-unit drift stays a deploy-level `warning`
     field in the report (structured, plus a WARNING line in the log) —
     the files land, but the service will not see them until you
     reconcile `install_dir` with the unit's `INSTALL_DIR`.

4. **Attach for live debugging**. Click `Attach`. The IDE opens an
   `ssh -N -L 127.0.0.1:<random>:127.0.0.1:<edge_runtime_port>` tunnel
   and switches the MonitorPane / VariablesPanel SSE source over to it.
   The same charts and pills you use locally now reflect the running
   program on the edge. Click `Detach` to go back to local mode.

## Layout on the edge

After deploy, an edge box looks like:

```
/opt/ia2/
├── current → versions/2026-05-12T08-30-00Z/       (atomic symlink)
├── versions/
│   ├── 2026-05-12T08-30-00Z/       latest
│   │   ├── runtime                  binary
│   │   ├── project/                 project.toml + pous/ + devices/ + tasks.toml + iomap.toml + hmi/
│   │   └── web/                     built web assets — the runtime's --static-dir serves the
│   │                                standalone HMI panel (/hmi) from here
│   ├── 2026-05-12T07-15-00Z/       previous (kept for rollback)
│   └── _initial/                   install.sh stub
└── state/                       retain.json + historian segments — sibling of `current`,
                                 so a symlink swap never touches it
```

## Upgrading pre-HMI edges

Edges bootstrapped before the HMI release run a unit whose `ExecStart`
has no `--static-dir`. The runtime auto-detects `current/web` next to
the project when the flag is absent, so such a box starts serving the
panel as soon as a deploy has landed both the web assets and a runtime
binary that knows the fallback — no unit edit required. To adopt the
current unit anyway:

```sh
scp infra/ia2.service edge:/tmp/
ssh edge "sudo install -m 0644 /tmp/ia2.service /etc/systemd/system/ia2.service && \
          sudo systemctl daemon-reload && sudo systemctl restart ia2"
```

Do **not** re-run `install.sh` on a live edge: it repoints `current` at
the `_initial` stub, knocking the deployed project off the box until
the next deploy.

## Rollback

There's no Rollback button (yet). Manually:
```sh
ssh edge
sudo ls /opt/ia2/versions/   # find the previous timestamp
sudo ln -sfn /opt/ia2/versions/<prev> /opt/ia2/.current.new
sudo mv -Tf /opt/ia2/.current.new /opt/ia2/current
sudo systemctl restart ia2
```

The Deploy code uses the same symlink-swap recipe; doing it by hand for
rollback is just "point `current` at an older version and restart".

## Security notes

- The runtime binds **127.0.0.1** on the edge — only ever reachable via
  the SSH tunnel the IDE sets up. Don't poke a hole in the firewall to
  expose `:13001` directly.
- A forwarded port is also reachable by every web page open in the browser on
  the dev machine, so the runtime refuses any request that names a foreign
  `Origin` or (while bound to loopback) a non-loopback `Host`, and answers a
  cross-origin page only for the IDE's `GET /events`. Requests without an
  `Origin` (curl, `cs`, the IDE server) are unaffected. Details in the **Auth**
  notes of `docs/api.md` (an operator panel behind a reverse proxy needs
  `--allow-origin <its public origin>` on the unit's `ExecStart`). Don't paste
  the forwarded address into pages you don't trust; the guard is the backstop,
  not the plan.
- The systemd unit grants `CAP_NET_RAW` so that EtherCAT (when wired)
  works. If you only use Modbus, you can drop it and run as a dedicated
  user; see the comments in `ia2.service`.
- Credentials are **not stored** in the project. The IDE's only auth
  mechanism is whatever `ssh` resolves via your agent / `~/.ssh/config`.
- Hardening is on (`PrivateTmp`, `ProtectSystem=strict`, `NoNewPrivileges`).
  If you find a legitimate access blocked, loosen carefully — these are
  there to limit what a misbehaving runtime can touch.

## Shutdown and failsafe evidence

A completed bridge drain means the failsafe and teardown **attempts**
finished for the scan thread; a reconnect worker beyond its two-second
grace is still unconfirmed. The server waits for that worker too before
allowing another run; the edge process keeps its existing bounded exit.
Neither proves that every physical output was confirmed safe. The scan
loop's final log includes `failsafe_failed` and `shutdown_failed` device
counts; inspect the preceding device/channel errors whenever either is
nonzero. Even zero counts do not replace physical readback or a hardware
safety circuit.

For Modbus, `protocol: modbus exception: …` means the slave replied but
rejected that write; the failsafe sweep continues to the remaining
writable channels and reports that some outputs are unconfirmed.
`transport: …` means the link failed (or timed out), so the sweep aborts
the remaining writes to keep shutdown bounded. A later transport failure
takes precedence over an earlier protocol rejection. Neither case is
reported as “outputs in failsafe”.

## EtherCAT mode selection

`iomap-ethercat` picks between two implementations based on the device
config's `nic` field:

| `nic` value | Behaviour |
| --- | --- |
| `"_sim"` (or empty) | In-memory PDO buffer. Output channels echo what the program writes; inputs start at zero. Used for macOS dev, CI, and demo. |
| anything else (e.g. `"eth0"`) | Real `ethercrab::MainDevice` on that NIC. Walks the bus, transitions to OP, runs a cyclic exchange on its own thread. Requires Linux + `CAP_NET_RAW` (already set in `ia2.service`). |

For real-mode channels, you must fill in `pdi_byte_offset` (and
`pdi_bit_offset` for sub-byte digital I/O) — the byte/bit position of
this PDO entry within the SubDevice's input or output PDI region. The
device editor surfaces these alongside the CoE `pdo_index` / `sub_index`
fields. They default to 0 for back-compat with sim-only configs.

On link loss, recovery first counts responding slaves with one read-only
BRD(Type). If that count differs from the last successful walk, it keeps
the existing capped backoff without resetting/configuring the surviving
slaves. A matching count still requires the full walk, configured identity
checks, and OP transition; it does not mark the bus healthy by itself.
The `reinits` heartbeat field counts recovery attempts, including census
deferrals, not just full bus walks. See
[reconnect cadence acceptance](bench/ethercat-reconnect-cadence.md) for
the offline evidence and the remaining hardware timing check.

### Dedicate the NIC to EtherCAT

EtherCAT is raw Layer-2 with no IP. The interface must be left alone by
the OS network stack and have hardware offloads off — otherwise frames
get corrupted and you'll see `init_single_group: Timeout(Pdu)` at startup
and `failed to decode raw PDU data` mid-run.

On a NetworkManager host (most Ubuntu/Debian edges) this is the common
gotcha: NM keeps the EtherCAT port "managed", and its periodic
DHCP/activation puts non-EtherCAT traffic on the wire and flaps the link
out from under the master. **Set the port unmanaged:**

```sh
# one-off (until reboot or NM restart)
sudo nmcli device set enp2s0 managed no
# persistent
printf '[keyfile]\nunmanaged-devices=interface-name:enp2s0\n' \
  | sudo tee /etc/NetworkManager/conf.d/99-ethercat.conf
sudo systemctl reload NetworkManager
sudo ip link set enp2s0 up      # raw L2 needs the link up — no IP
```

Also disable the NIC's hardware offloads — checksum / segmentation
offload mangles raw L2 frames:

```sh
sudo ethtool -K enp2s0 rx off tx off gso off gro off lro off tso off
```

(Some may report `[fixed]` and can't be changed — that's fine; verify
with `ethtool -k enp2s0`.) Use a **separate NIC** for EtherCAT from the
one carrying your SSH / management traffic.

## Caveats

- Field-input quality travels with each snapshot: mapped input variables carry
  `input: { device, channel, stale }`, and the snapshot carries `device_health`.
  Keep last-known values distinct from fresh measurements. MQTT snapshots keep
  the existing `values` object and add `inputs` (variable-name → the same quality
  object) and `device_health`; consumers must inspect quality before using a value.
  This does not trace dependencies through PLC calculations.
- Sustained device loss automatically raises `__device/<name>` after 1 s,
  even with no `alarms.toml`. Recovery returns the alarm; acknowledgement is
  still required for an unacknowledged occurrence. The existing `/alarms`,
  `/alarms-journal`, and encoded-id ack endpoint expose it. Process alarms do
  not evaluate stale field inputs, and persisted history marks stale buckets.

### Alarm supervision

The alarm engine, the history and the live feed all hang off one task in the
edge runtime. When it stops, is stuck inside one snapshot, or keeps failing,
the alarm list is quiet because nothing is looking, which on a panel reads as
"nothing is wrong". The runtime reports that state (`snapshot_tap` in
`/health` and `/status`, `cs probe`, the edge badge, the operator panel's
`ALARMS OFF`, and a failed `unmonitored` post-restart check), and the project
says what the runtime does about it in `alarms.toml`:

```toml
[supervision]
on_loss = "report"      # default: report it and keep running
# on_loss = "safe_state"  # latch the outputs off if alarm evaluation stays lost
grace_secs = 30         # 1..=3600; ignored for "report"
```

- **`report` is the default, and stops nothing.** Whether stopping is the safe
  direction depends on the plant (an unplanned stop can itself be the
  hazard), so the runtime does not decide it. Choose `safe_state` only where
  losing alarm evaluation is the more dangerous of the two.
- **`safe_state`** latches the outputs off once alarm evaluation has been lost
  for `grace_secs` in a row, exactly like a scan-watchdog trip: every device
  goes into failsafe and no further output is written, while the loop keeps
  scanning so the live state stays visible. `/health` and `/status` say why
  (`latch_reason`). It is never undone automatically and never done by
  restarting the process: a person restarts the program to re-arm.
  A loss that clears inside the grace starts the clock over.
- **Deploy refuses an `alarms.toml` it cannot read**, and a `[supervision]`
  table with an unknown key (`on_los`), an unknown `on_loss` or a `grace_secs`
  outside 1..=3600: a misspelt protective setting must not deploy and protect
  nothing. (An unreadable `alarms.toml` used to deploy, with the runtime
  logging a warning and running without alarms.)
- **The runtime refuses to start on one too.** It reads the project through the
  same loader as the deploy preflight, so an `alarms.toml` it cannot read makes
  `ia2-runtime` exit at startup with the reason, instead of running without
  alarms as it used to. The case that can surface this is an edge whose project
  was deployed by an older server and whose runtime is then upgraded. The
  service retries a few times (`Restart=on-failure`, `StartLimitBurst=5` in 60 s)
  and then stays `failed`, with the reason in the journal; the program does not
  run, and what the outputs do meanwhile is the I/O side's own failsafe, as for
  any stopped runtime. Fix the file and redeploy.
- **Logging is not proportional to time.** Losing alarm evaluation is logged
  at ERROR the moment it is seen, with the policy in the line; reminders then
  back off (1, 2, 4, 8 ... minutes into the episode, at most hourly), and
  recovery is one INFO line. A task that flaps is announced at the 1st, 2nd,
  4th, 8th ... episode. The state itself is always in `snapshot_tap`.
- **The runtime says which policy it is enforcing** (`supervision` on `/health`
  and `/status`, and in `cs probe` beside any alarm-evaluation warning). It is
  what the process read at startup, so a later edit of `alarms.toml` shows up
  only after a restart. A runtime built before `[supervision]` ignores the table
  and omits the key.
- **While it is lost, the same object says how long and how long is left**
  (`lost_for_ms`, and under `safe_state` `latch_in_ms` until a latch is requested).
  `cs probe` prints both on the policy line. While the loss continues,
  `lost_for_ms` remains available under `report` and after a latch; only
  `latch_in_ms` is `null` under `report`, once a latch is requested, or when
  outputs are already latched (`latch_reason` explains a completed latch).
  Both are `null` when nothing is lost or the supervisor has not looked for
  several seconds, so an old countdown is never shown as a live one.
- **It is also a standing alarm.** While alarm evaluation is lost, `GET /alarms`
  lists `__supervision/snapshot_tap` (Critical) beside the process alarms,
  `alarms_standing` counts it, the journal records it, and after it returns it
  stays standing until an operator acknowledges it, so the alarm list is not
  quiet in exactly the case it must not be. The supervisor raises it, not a
  snapshot, so it works when snapshots have stopped. If the alarm engine's own
  lock is wedged it cannot be listed (that is logged, and the policy, the log,
  `/health`, the panel and MQTT carry on). The id is reserved like `__device/`.
- **The northbound MQTT feed follows it.** While alarm evaluation is lost the
  runtime's latest snapshot is frozen, so publishing it would hand the platform
  the same old values every interval next to a retained `online`. Instead the
  retained `<prefix>/status` becomes `degraded` (once, logged) and no snapshot is
  published until it recovers, when `online` is asserted again; a broker
  reconnect re-asserts whichever is current.
- Edge runtime only: the IDE server's local run has no such task to supervise.

- **No EtherCAT hardware on the dev machine**: leave `nic = "_sim"`. The
  IDE will let you configure PDOs and the bridge will respond in sim
  mode. On the edge, configure the real NIC.
- **Retained / persistent variables** ARE preserved across deploys:
  `VAR RETAIN` state lives in a `state/` directory that sits BESIDE the
  versioned `current` symlink (`$INSTALL_DIR/state/`), so a symlink swap
  never touches it (schema-2 lossless 64-bit slots; see
  `crates/ironplc-bridge/src/retain.rs` and the flush interval in the
  bridge). Deleting `$INSTALL_DIR/state/` is the manual cold-start.
- **Hot patch / online change** (Codesys-style in-place code update) is
  not implemented. Deploy is stop → swap → start. Plan downtime.
- **Real-time**: stock Linux gives soft-RT only; scan jitter is in the
  millisecond range. A 2 ms cycle with DC SYNC0 on a dedicated NIC is
  demonstrated on real hardware (mean 500 scans/s over 89 s — see
  `docs/bench/ethercat-2ms-dc-sync.md`); sub-ms hard real-time control
  is not.
- **DC distributed clocks**: supported via `dc_sync = "sync0"` (per device,
  with an optional per-SubDevice override for mixed servo + IO buses) —
  servo drives need it to reach OP. Startup CoE writes go through
  `init_sdo` (e.g. `0x6060 = 8` for CSP). CiA 402 CSP motion, including
  electronic gearing, has been run on real hardware.
- **ESI modular couplers**: offline ESI parsing and channel assembly *are*
  shipped. Set `bringup = { mode = "esi_modular", esi_path = "esi/coupler.xml" }`,
  then `cs api POST /api/devices/<device>/esi-assemble` (body
  `{"detected":[…]}`) builds the channel list
  from the coupler's ESI plus its reported modules (tracking byte/bit offsets)
  and replaces the device's channels. What remains hardware-gated is the
  real-bus cyclic bring-up for these couplers (master-programmed
  SyncManager/FMMU + logical-RW exchange), tracked as issue #11; author and
  verify the layout in `nic = "_sim"` meanwhile. Fixed-PDO servos and slices
  (`bringup = auto`, the default) still take hand-authored `pdi_byte_offset`s —
  read them off the connect-time PDO-mapping log, where the runtime dumps each
  `0x1C12`/`0x1C13` entry with its object index and byte offset.
