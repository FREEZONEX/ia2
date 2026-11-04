# R08/R10: pulse ownership and active valve faults

This change fixes two reproducible violations of existing behavior. It does
not introduce a general ban on automatic recovery, a new operator approval
step, a command TTL, or a requirement for a fresh start/open edge.

## R08: an older pulse reset overwrites a newer write

Previously, every `write_with_pulse` spawned an independent delayed write of
zero. A pulse at t=0 with a 100 ms duration could therefore clear a newer
pulse written at t=50 with a 200 ms duration, truncating it at t=100 instead
of t=250. The old timer could also erase a later ordinary write.

Each accepted pulse now registers an owner against the resolved VM slot in
the scan thread. A later successful pulse, ordinary write (including MQTT),
or force supersedes that owner. Bare and instance-qualified aliases refer to
the same slot; different PROGRAM instances retain separate owners. Rejected
writes leave the current pulse's reset active.

The delayed reset submits its owner to the same command queue. Ownership
validation and writing zero happen together without an intervening await,
so a newer accepted command cannot be overwritten by a stale reset. A reset
that still owns its slot continues to bypass write-governance bounds and
runs while paused. During a device outage it clears the VM, reports the
undelivered device, and logs the failure to deliver; ordinary output
synchronization sends the current value when the link returns. VM success
does not establish that the disconnected field device has cleared.

Six new regressions in
`crates/ironplc-bridge/src/runtime_pulse_tests.rs` cover:

- Overlapping pulses and final zero under a controlled Tokio clock,
  including pause and bare/qualified aliases.
- An ordinary later write surviving an older reset.
- Both command queue orders, same-value writes, and force/unforce.
- Independent PROGRAM instances and rejected writes retaining the valid
  reset, including a governance minimum greater than zero.
- A stopped run's reset failing without affecting its replacement.
- Undelivered reset results, VM zero during an outage, and sending zero
  after normal link recovery.

The existing `pulse_reset_bypasses_governance_and_still_resets_to_zero` test
also remains in place.

## R10: reset clears an active dual-limit valve fault

Previously, `FB_VALVE` could observe both limits TRUE, latch `fault`, and
then clear it later in the same scan when `reset` was TRUE and `fault_in`
was FALSE. With `cmd_open` still TRUE, the block could open its output and
report status 2 despite the continuing dual-limit contradiction.

Reset now requires both the external fault to be clear and the dual-limit
contradiction to be absent. Fault-close priority remains in effect. Once the
cause clears, reset can still accept a continuously held open request; no
new opening edge is required. Travel-timeout reset behavior is unchanged.
The process-control library version is **0.3.2** so imported projects can
identify the update; existing vendored copies require a library update.

`valve_reset_cannot_clear_active_dual_limits` in
`crates/cli/tests/sim_e2e.rs` copies the actual library FB into a temporary
project, starts an isolated loopback server, runs `cs project check`, and
executes the 30-step `library/process-control/scenarios/valve-reset.toml`:

- Sustained dual limits with reset and the open request held TRUE.
- A latched fault remaining after its cause clears until reset is given.
- Normal recovery accepting the held open request.
- A new dual-limit fault closing an already-open valve with reset held.
- Recovery when the contradiction clears while reset stays held.
- External fault and close-command priority.

## Reproduction and validation

The pulse regressions first failed against the original independent timers:
the old reset replaced the newer pulse's `1` and ordinary write's `7` with
`0`. The final valve scenario was also checked with the original reset
condition restored: it failed to observe `valve_fault = TRUE`. Restoring the
fix made the same scenario pass.

Run the focused checks with the server built from the same checkout:

```bash
cargo build -p server
cargo test -p ironplc-bridge pulse_ -- --nocapture
cargo test -p ia2-cli --test sim_e2e valve_reset_cannot_clear_active_dual_limits -- --nocapture
```

The repository quality gate also includes skill adaptation, formatting,
workspace Clippy with warnings denied, the full Rust workspace tests, the
server/CLI/runtime/LSP release build, and the Web build and tests. These are
software checks; no physical valve or fieldbus acceptance is claimed.
