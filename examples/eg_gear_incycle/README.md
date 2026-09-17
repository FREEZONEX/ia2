# eg_gear_incycle — in-cycle electronic gear (B-tier motion)

A follower servo whose `target_position` is generated **inside the EtherCAT
cyclic loop**, strictly SYNC0-aligned, by the runtime's gear engine — not in
the PLC scan plane. Runs under `nic = "_sim"` with **no hardware**.

## Why in-cycle

The stock ("A-tier") way to do an electronic gear is to compute the follower
target in the PLC program each scan (~500 Hz) and let the cyclic loop copy it
to the wire. The two planes are asynchronous, so up to one scan of phase
jitter sits between "target computed" and "frame sent". That jitter is
invisible on a slow single axis but becomes the dominant term in inter-axis
sync error at higher speed / for multi-axis gearing / for electronic CAM.

This example uses the `[[gear]]` device feature: the loop computes the target
every bus cycle from a master source, cycle-aligned. The PLC only feeds slow
parameters (ratio / engage / …) through named channels that the device routes
into a lock-free struct instead of PDI bytes, and reads engaged/trip back.
Equivalent to the TwinCAT NC / SoftMotion split.

A dual-SV660N A/B run (13 rpm, 2:1) was originally reported here as a
−29% mean sync-error improvement. That figure did not survive
re-analysis of its raw data: the analysis script was not retained, the
recomputed improvement is smaller than its own sensitivity to the
analysis window (it even changes sign), and the 100 ms snapshot poll
used to measure it is ~50× coarser than the 2 ms scan-plane effect
being claimed. The theoretical motivation stands (SYNC0-aligned
in-cycle gearing removes scan-plane phase jitter by construction), but
no measured improvement is claimed until an A/B is re-run with
in-runtime logging. At low speed the two paths measure equal — error is
dominated by the drives' own following lag either way.

## What this project shows

- A `[[gear]]` block (`devices/servo.toml`) with a **virtual master** (a
  software accumulator) driving one follower — so it runs with no second axis.
- The PLC program (`pous/main.st`) runs only the CiA402 enable chain and
  forwards gear parameters; it **never writes `target_position`** (the loop
  owns those bytes).
- The engine's fast-plane safety interlocks (all enforced in-loop, so no
  slow-plane mistake can bypass them): shadow-actual until Operation Enabled
  (zero-jump at enable), engage refused unless `max_travel > 0`, ratio/phase
  latched at the engage edge, overtravel hard-clamped to `±max_travel` then
  tripped, re-arm required after an enable loss, non-finite params rejected,
  and (for an axis master) the ±2^31 encoder wrap handled as a wrapping delta.

## Run it (sim, no hardware)

The sim models an *ideal* follower (always Operation Enabled, actual == last
commanded target), so virtual-master gearing is fully exercisable:

```bash
cs project open examples/eg_gear_incycle
cs --project eg_gear_incycle run

cs --project eg_gear_incycle runtime force max_travel 8388608   # 1-rev limit (counts)
cs --project eg_gear_incycle runtime force master_vel  200      # counts / 2 ms cycle
cs --project eg_gear_incycle runtime force ratio_num   2
cs --project eg_gear_incycle runtime force ratio_den   1
cs --project eg_gear_incycle runtime force ratio_step  0.02     # soft-engage ramp
cs --project eg_gear_incycle runtime force engage      1
```

`gear_engaged` goes true and the follower's `target_position` climbs at
~2× `master_vel`; after ~1 rev of travel `gear_trip` latches and it holds.
Force `engage 0` to clear the trip. Force `max_travel 0` and the engine
refuses to engage (locked-by-default). Try `ratio_step -0.1` or a NaN ratio
and nothing runs away — the engine sanitizes both.

## Offline channel-contract regression

With a local server running, validate and exercise the unmodified `_sim`
project in an agent session:

```bash
cs agent run --label "Offline gear channel contract" -- bash -e -c '
  cs project open examples/eg_gear_incycle
  cs --project eg_gear_incycle api POST /api/project/validate
  cs --project eg_gear_incycle sim run examples/eg_gear_incycle/scenarios/channel-contract.toml --trace gear-trace.jsonl
  cs --project eg_gear_incycle get runtime/status
  cs --project eg_gear_incycle get runtime/forces
  cs --project eg_gear_incycle project close
'
```

Validate must return `[]`; the generic `cs api` exit code alone only proves
the HTTP call succeeded. The scenario proves default lockout, engagement,
bounded-travel trip and disengage reset in an ideal simulated follower.
The runner stops the runtime and removes its forces on completion. This is
**offline regression evidence, not bench acceptance**. Never run the scenario
after changing the device NIC away from `_sim`.

The example keeps its 2 ms PLC task and gear cycle. Run on an otherwise idle
host: heavy builds can trip the scan watchdog and latch outputs off. The
scenario checks that watchdog explicitly; a trip is a failed run, not a gear
pass. Retain its log and rerun from a fresh runtime after removing the load.

The seven gear parameters are writable and readable echoes; `gear_engaged`
and `gear_trip` are read-only feedback. These routes are not PDO entries.
The scenario exercises the seven Output routes and two feedback Input routes;
parameter Input echoes are covered by Rust mapping and adapter tests.
The schema also reserves `ratio_apply_channel` / `ratio_ack_channel`, but
those two are not exposed by the device facade and cannot be mapped.

## To a real two-axis bench

Set `nic` to your EtherCAT NIC, add the master SV660N as `slave 1`, and change
the gear master to read its feedback:

```toml
master = { kind = "axis", slave_index = 1, actual_pos_offset = 4 }
```

Now the follower gears off the master's real `0x6064` each cycle. Everything
else (enable chain, parameters, interlocks) is identical.

## Files

`devices/servo.toml` (follower PDOs + `[[gear]]`) · `iomap.toml` (enable-chain
+ gear-parameter channels; `target_position` deliberately unmapped) ·
`pous/main.st` (CiA402 + parameter passthrough) · `tasks.toml` (2 ms task).
