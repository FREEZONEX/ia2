# watchdog_latch — bench fixture for the scan-watchdog latch

Exercises the scan-watchdog latch. The offline regression
`runtime::tests::watchdog_trip_latches_outputs_off_until_restart` checks that
the normal output phase cannot overwrite configured safe values after a
trip. The bench procedure observes a drive's statusword separately; an
adapter acknowledgement alone does not establish wire or physical state.

## Two ways to make it trip

The watchdog fires on a real timing condition — a scan body that cannot
finish inside its interval. Two ways to create it, one per environment:

- **sim** — `scenarios/latch.toml` uses the scenario DSL's `inject` step
  (`POST /api/runtime/inject-scan-stall`): the runtime stalls the next N
  scans by a chosen wall-clock amount, and the watchdog trips through
  its real overrun path. Deterministic and host-speed independent:

  ```bash
  cs sim run scenarios/latch.toml
  ```

  `scenarios/acknowledgement.toml` adds a healthy-before-trip control and a
  continued-latch check. The CLI end-to-end regression runs it with `_sim`
  and checks `/api/runtime/status`: `watchdog_tripped` is true, `servo` is in
  `failsafe_status.acknowledged`, and pending/unconfirmed lists are empty.
  This confirms an in-memory adapter response only. Rejection and panic
  cases are covered by the bridge's local-adapter regressions.

- **bench** — force the CPU burn live: `cs runtime force burn_n 200000
  --edge <edge>`. Every scan then overruns for real with no HTTP in the
  loop once armed. `burn_n` ships as 0 so the project deploys healthy;
  the trip is an explicit action, never a startup race.

## Safety — this fixture cannot move an axis

`control_word` is only ever driven to `16#0006` (Shutdown: Enable voltage +
Quick stop, Switch on cleared). That parks a CiA402 drive at
**Ready to switch on** — energised, not enabled. Operation Enabled is never
commanded and `target_position` is deliberately left unmapped.

The observable is the **drive's statusword**, not motion. Do not edit the
program to command `16#000F`.

## What you are watching

| Phase | controlword on the wire | Expected statusword |
|---|---|---|
| Before the trip | `0x0006` | `0x1631` — Ready to switch on |
| Watchdog trips, failsafe zeroes the PDI | `0x0000` | bit 6 set — **Switch on disabled** |
| **Fixed build** | stays `0x0000` | **stays** in Switch on disabled |
| **Pre-fix build** | back to `0x0006` next scan | **returns** to `0x1631` |

The whole verdict is whether the statusword *stays* or *bounces back*.

## Running it

Offline first (no hardware, proves the project loads):

```bash
cargo run -p server &
cs api POST /api/projects/open -d '{"path":"'$PWD'"}'
cs api POST /api/project/validate     # [] == clean
```

On the bench, point the device at the real NIC first:

```bash
# devices/servo.toml
nic = "enp2s0"        # was "_sim"
```

Then follow `docs/bench/watchdog-latch-runbook.md`.

## Tuning `burn_n` (bench only)

`burn_n` (default 0 = harmless) sets the burn length and is forced at
runtime on the bench. Raise it until scans actually overrun — the check
is that `scan_count` stops tracking wall clock:

```bash
cs get runtime/status     # scan_count should grow far slower than uptime_secs × 500
```

On a host fast enough to finish 200000 iterations inside 2 ms the watchdog
will never trip and the test is vacuous — always confirm the overrun before
trusting the result.
