# Simulated axis capture

The fixture uses only the `_sim` EtherCAT adapter. The PLC counts requests
and keeps the command disabled. The PLC scenario verifies that distinction.

With the IA2 IDE server running on loopback, execute from the repository root.

```sh
cs --server http://127.0.0.1:3001 project check examples/axis_capture_sim
cs --server http://127.0.0.1:3001 project open examples/axis_capture_sim
cs --server http://127.0.0.1:3001 --project axis_capture_sim sim run examples/axis_capture_sim/scenarios/pre_enable_abort.toml
```

To observe the same program through the edge API, launch a standalone local
runtime with its own state directory.

```sh
./target/debug/ia2-runtime --project-dir examples/axis_capture_sim \
  --bind 127.0.0.1:13001 --state-dir target/axis-capture-sim-state
cs --server http://127.0.0.1:13001 api GET /axis-capture/bus
```

Prepare a start request with the returned runtime ID and bus generation,
slave 0, `counts_per_revolution: 600`, and explicit
`max_samples` and `duration_ms`. During the recording, use a local test write
to set `trial_request` and read the PLC scan and request counters from
`/status`. The fixture increments `requests_consumed` and leaves
`trial_starts` at zero. Follow the [capture API](../../docs/api.md#edge-axis-capture)
to freeze the record, download it and export that evidence to V1. The V1
endpoint should return `pre_enable_abort` and `metrics: null`.
