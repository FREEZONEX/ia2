# Tier0 control bridge reference

A local, file-backed reference for the `ia2.control/v0` contract. It accepts HMAC-authenticated setpoint requests, applies a pinned target registry, records write intent before sending to a loopback IA2 runtime, and publishes the recorded outcome. The bridge never reports `verified`; the laboratory read-side verifier derives that status from separate feedback samples.

This imports the previously separate reference implementation with its P0/P1, IR01/IR02, AR01 and IR03 repairs. It is a laboratory integration example, not a deployed Tier0 connector. The real Agent-facing control MCP and D11 event/sample authentication are still unimplemented. Until D11 is implemented, the verifier assumes trusted local trace and sample files. `verified` does not prove physical effect or that this command caused the observed value.

## IR03: recover result publication without repeating the write

Previously, a trace write failure after a durable terminal record escaped into request error handling. The valid request was quarantined, so replay no longer published its saved result.

The engine now keeps failed terminal publications pending. Request replay, startup and periodic housekeeping retry the same saved event without another runtime read, write, classification or journal append. Startup includes old quarantined requests and results outside the hot-index horizon. Genuine request-processing exceptions still quarantine the request. Journal failure still stops processing.

Trace writes handle short writes, remove an uncommitted suffix before retry, and preserve a torn-tail backup on restart. Existing complete lines must sync before their event IDs suppress publication. Consumers must deduplicate by event ID: a reader can observe an unsynced line before recovery. This is not an exactly-once delivery claim.

## Run the tests

Python 3.8+ standard library on macOS or Linux; no package installation is required. The recorded acceptance used Python 3.9.6. The process tests use loopback sockets, POSIX file locks and subprocess signals.

From the repository root:

```sh
python3 -I -B examples/tier0-control/tools/run_tests.py
```

The runner executes each suite in its own interpreter, writes logs under `evidence/unit/`, and fails if any suite fails. It covers 240 tests, including 14 IR03 regressions. Legacy IR03 tests use a small test-only reproduction of the previous publication semantics; they do not require an archived developer checkout. The original archived engine and process were also exercised during the independent repair acceptance.

For the full runtime/fake-Modbus suite, use the accepted `1c42df70d2581babbf79f0ba96726f61cb8a68f5` runtime listed in VALIDATION.md, then run from this directory. **Current `main` runtime compatibility is blocked:** it exposes `failsafe_status: null` before any failsafe attempt, which this reference's strict evidence schema rejects at startup. The schema adaptation is separate from IR03; do not disable preflight to bypass it.

```sh
python3 -I -B lab/run_cases.py --bin /path/to/accepted/ia2-runtime --out evidence/e2e \
  --cases A1,B1,B3,C1,A2,B2,B4,B6,B7,B8,B9,B11,C3,C4,C6,C8,E1,E2,E3,R1,R2,R3,F1,F2,PROD,P1F5,P1F6
python3 -I -B tests/check_p0_crash_evidence.py evidence/e2e
python3 -I -B lab/p0_bool_acceptance.py --bin /path/to/accepted/ia2-runtime
```

Use a new output directory for each integration run. Run integration suites serially: they own loopback ports 13501, 13502, 13503, 13511 and 13512. The runner exits nonzero for a FAIL or setup error and records source-tree and binary hashes. See [VALIDATION.md](VALIDATION.md) for the exact tested runtime revision and results; the historical R3 finding remains visible.

The F1 case checks the pinned archived read-only tool schema in `tests/fixtures/`. It does not audit a live read-only MCP deployment or its delivery bundle. Golden HTTP fixtures come from the earlier loopback fake-device experiment.

## Device-free simulator smoke

`sim/` is a small IA2 project and scenario for the underlying typed, governed setpoint API. It complements the Python fault tests; it does not test IR03 publication by itself. With a disposable loopback server at `http://127.0.0.1:23231`, run from the repository root:

```sh
./target/release/cs project check examples/tier0-control/sim
./target/release/cs --server http://127.0.0.1:23231 project open examples/tier0-control/sim
./target/release/cs --server http://127.0.0.1:23231 --project control_bridge_smoke agent run --label "Tier0 offline smoke" -- \
  ./target/release/cs --server http://127.0.0.1:23231 --project control_bridge_smoke sim run examples/tier0-control/sim/scenarios/setpoints.toml
```

The scenario verifies scan progress, a REAL governance clamp, a negative DINT and a BOOL write. It stops the program when finished; no devices, force operations or alarms are configured.

## Layout and packaging

- `bridge/`, `bridge_run.py`: request processing, journal, HTTP client and trace writer.
- `lab/`: fake runtime/device harness, sample collector, independent verifier and stand-in tool surface.
- `tests/`: regression tests, explicit fault hooks and small data fixtures.
- `contract/`: contract, design decisions and historical test definitions; references to earlier local reports are historical context.
- `tools/package_prod.py`: removes lab hook loading from a copy of the bridge.

```sh
python3 -I -B tools/package_prod.py prod
python3 -I -B prod/bridge_run.py --help
```

`package_prod.py` replaces the specified output directory. Its `prod` flavor only means fault hooks are absent; it does not mean hardware or production acceptance. The bridge CLI requires a registry and its SHA-256, a mode-0600 request-key file, loopback runtime address, project directory, journal, request file and trace path. Lab fixtures create their own test keys; no deployment keys or runtime binaries are included.

On macOS, a failing `F_FULLFSYNC` is propagated rather than downgraded to `fsync`. A filesystem that exposes but cannot support full sync will therefore refuse to proceed. Actual power-loss durability and hardware behavior remain bench work; this example supplies offline evidence only.
