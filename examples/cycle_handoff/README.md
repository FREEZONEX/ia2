# Offline cycle handoff fixture

Two scalar output fields on `_sim`; no drive enable, init SDO or physical interface.
Tasks and the simulated bus run at 50 ms. The independent bus thread uses the
production handoff state machine. The 2 ms timing contract is tested separately
with a deterministic clock, without claiming that a developer host is real-time.

Open a copy in an isolated local IA2 server, then run both scenarios inside one
`cs agent run` session, passing that server and project explicitly:

```sh
cs --server "$SRV" --project cycle_handoff sim run scenarios/continuity.toml --keep-running
cs --server "$SRV" --project cycle_handoff get runtime/status
cs --server "$SRV" --project cycle_handoff sim run scenarios/late_frame.toml --no-run
cs --server "$SRV" --project cycle_handoff get runtime/status
cs --server "$SRV" --project cycle_handoff stop
```

The second scenario intentionally leaves `watchdog_tripped` latched. The paired
output alarm covers PLC-side arithmetic; exact frame pairing/sequence is covered
by the adapter tests and CLI e2e, not inferred from occasional variable snapshots.
See [the contract](../../docs/scan-cycle-handoff.md).
