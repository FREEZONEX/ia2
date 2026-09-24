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
   and its outputs are dropped.

3. **Deploy**. Click `Deploy`. The IDE:
   - `tar`s your project directory + (if found) a freshly-built
     `ia2-runtime` from the dev machine
   - Pipes the tar into `ssh edge bash …` which extracts to
     `$INSTALL_DIR/versions/<UTC-timestamp>.<unique-suffix>/`, atomically swaps the
     `current` symlink, and `systemctl restart ia2`s
   - Streams the remote script's output back into the pane

   Deploy REFUSES to lie about the outcome:
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
     the old process. File rollback does not prove that it is running;
   - a missing `VERSION=` line from the remote script fails the deploy
     (script drift = state unknown);
   - a restart systemd accepted is not a running program: after the
     restart the deploy reads the runtime's `/status` until the program has
     scanned and a second read shows no fault. A fault (a VM trap, a VM
     that failed to start), a latched watchdog, or no scan within 30 s fails
     the deploy (`ok: false`, `health` says why). The new version stays
     installed and current — nothing is rolled back automatically; the log
     names the previous version for the manual rollback below. A device
     still down at the check is a `warning`, not a failure. When nothing was
     restarted (the unit is not enabled), `health` says it was not checked;
   - a `[governance]` table that is invalid (unknown key, `min > max`,
     non-finite bound) is one such refusal — governance is validated on
     load, never silently ignored;
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
finished, not that every physical output was confirmed safe. The scan
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
