# Tier0 control bridge reference

A local, file-backed reference for the `ia2.control/v0` contract. It accepts HMAC-authenticated setpoint requests, applies a pinned target registry, records write intent before sending to a loopback IA2 runtime, and publishes the recorded outcome. The bridge never reports `verified`; the laboratory read-side verifier derives that status from separate feedback samples.

This imports the previously separate reference implementation with its P0/P1, IR01/IR02, AR01 and IR03 repairs. It is a laboratory integration example, not a deployed Tier0 connector. The real Agent-facing control MCP and D11 event/sample authentication are still unimplemented. Until D11 is implemented, the verifier assumes trusted local trace and sample files. `verified` does not prove physical effect or that this command caused the observed value.

## IR03: recover result publication without repeating the write

Previously, a trace write failure after a durable terminal record escaped into request error handling. The valid request was quarantined, so replay no longer published its saved result.

The engine now keeps failed terminal publications pending. Request replay, startup and periodic housekeeping retry the same saved event without another runtime read, write, classification or journal append. Startup includes old quarantined requests and results outside the hot-index horizon. Genuine request-processing exceptions still quarantine the request. Journal failure still stops processing.

Trace writes handle short writes, remove an uncommitted suffix before retry, and preserve a torn-tail backup on restart. Existing complete lines must sync before their event IDs suppress publication. A complete line the bridge could not have written (non-ASCII bytes, JSON that is not an object, a non-string event ID, nesting too deep to parse) is skipped and its line number reported on stderr at startup; it neither suppresses an event nor supplies an anchor. Consumers must deduplicate by event ID: a reader can observe an unsynced line before recovery. This is not an exactly-once delivery claim.

## R3: a missing /audit entry is not proof that a write did not run

When the bridge's POST times out, the runtime may still apply the write. Loopback tests (R3T1, R3T1X) show this: the runtime had already read the request and queued the write when the bridge closed the connection. The write was applied with no `/audit` entry. The current runtime records an audit entry only after the queued write is acknowledged. When the client disconnects, the request handler is dropped.

The bridge's `reconcile_note` therefore reports only what `/audit` shows. `audit_entry` is `found`, `absent` or `unavailable` (the runtime restarted). `landed` is `true` only when an entry was found and `null` otherwise; the bridge never writes `landed: false`. A `landed: false` from an older bridge means `absent`. The action status stays `unknown`, and the bridge never resends. A runtime fix that records the audit entry even after a client disconnect is still pending; it is outside this example.

## Reconciliation: journal, /audit and the devices (WP6)

`tools/reconcile_report.py` compares three records that the bridge does not share code with: the bridge journal, an archive of the runtime's `/audit` taken about once a second (the ring holds 256 entries and a restart clears it), and the fake devices' own write logs. It sorts every journal intent into one class: `consistent`; `executed_unaudited` (no HTTP answer, the device got the value, the archive covered the write and has no entry: the R3T1 gap); `not_sent`; `no_effect_no_entry`; `uncheckable_restart`; `uncheckable_turnover`; or `unexplained`. It also lists audit entries that no intent explains (unknown writes) and device writes that no requested value, initial value, program-owned value or failsafe pass explains. The report is `CLEAN` only when there are no unknown writes, no unexplained intents, no unexplained device writes, and the journal chain verifies. It reads files only and never contacts the runtime.

```sh
python3 -I -B tools/reconcile_report.py --journal journal.jsonl --audit-archive audit-archive.jsonl.gz \
  --device-map device-map.json --device-log 1=slave-1.jsonl --device-log 2=slave-2.jsonl \
  --lifecycle lifecycle.jsonl --wire wire.jsonl --out reconcile.json
```

## Soak (WP9)

`lab/soak.py` runs the real bridge, a sandbox runtime and two fake devices for a fixed time. It sends a random mix of valid, out-of-range, unknown-key, forged, malformed, expired, mistyped, duplicate and burst requests. Every two to four minutes it injects one fault: a device link down or silent, a runtime kill and restart or `SIGSTOP`, a watchdog latch, a bridge kill, or one of the exact R1, R2 and R3T1 points through an armed hook (`lab/hooks_soak.py`). Once, near the middle, the `/audit` archiver pauses long enough for the ring to turn over. At the end it runs the reconciliation above and writes `soak-summary.json`:

```sh
python3 -I -B lab/soak.py --bin ../../target/release/ia2-runtime --out evidence/soak-<date> --minutes 120
```

It owns the same loopback ports as the cases; run it alone, or give it its own `IA2_LAB_PORT_BASE`. Freeze a copy of this directory if you plan to edit it during a long run: a restarted bridge loads whatever code is on disk.

## Run the tests

Python 3.8+ on macOS or Linux; no package installation is required. The example bundles the unmodified pure Python [Tomli 2.2.1 parser](https://pypi.org/project/tomli/2.2.1/) under `bridge/_vendor/tomli`, including its MIT license, source archive hash and per-file hashes in `SOURCE.json`. All other imports use the standard library. The recorded acceptance used Python 3.9.6. The process tests use loopback sockets, POSIX file locks and subprocess signals.

From the repository root:

```sh
python3 -I -B examples/tier0-control/tools/run_tests.py
python3 -I -B examples/tier0-control/tools/check_failsafe_controls.py
```

The first runner executes each suite in its own interpreter, writes logs under `evidence/unit/`, and fails if any suite fails. It covers 288 tests, including 14 IR03 regressions, 7 nullable-failsafe compatibility regressions, 6 D6 TOML/parser/packaging regressions, 9 R3 reconcile regressions, 4 trace-reload regressions and 22 vectors for the reconciliation program. Legacy IR03 tests use a small test-only reproduction of the previous publication semantics; they do not require an archived developer checkout. The original archived engine and process were also exercised during the independent repair acceptance. The second command verifies that three deliberately broken nullable-failsafe variants fail by assertion.

D6 parses `northbound.toml` using TOML semantics at startup and before each new write. A missing file, omitted `mqtt` table or omitted `mqtt.allow_write` uses the runtime's disabled default. An explicit flag must be the boolean `false`, including when `mqtt.enabled` is false; malformed TOML, an invalid `mqtt` table or a flag of another type fails closed. Quoted/escaped keys, dotted keys, inline tables and `#` inside strings cannot hide enabled writes. Parser diagnostics are not exposed because configuration may contain credentials. The standalone production package includes the same parser and license.

For the full runtime/fake-Modbus suite, build `ia2-runtime` with the repository's Rust toolchain (`cargo build --release -p ia2-runtime`), then run from this directory. The bridge supports current-runtime `failsafe_status: null` (no attempt yet), the older omitted extension, and a report containing exactly the three defined lists. A null report never overrides the other snapshot's report, the watchdog, latch, fault or device-health checks. Nonempty reports still block a write; malformed reports still fail closed.

```sh
python3 -I -B lab/run_cases.py --bin ../../target/release/ia2-runtime --out evidence/e2e \
  --cases A1,B1,B3,C1,A2,B2,B4,B6,B7,B8,B9,B11,C3,C4,C6,C8,E1,E2,E3,R1,R2,R3,R3T1,R3T1K,R3T1X,R3T2,F1,F2,PROD,P1F5,P1F6
python3 -I -B lab/run_cases.py --bin ../../target/release/ia2-runtime --out evidence/e2e-p1 \
  --cases A3,A4,B5,B10,C2,C5,C7,C10,R4,R5,F3,F4,F5
python3 -I -B tests/check_p0_crash_evidence.py evidence/e2e
python3 -I -B lab/p0_bool_acceptance.py --bin ../../target/release/ia2-runtime
```

Use a new output directory for each integration run. Run integration suites serially: they own loopback ports 13501, 13502, 13503, 13511 and 13512. Setting `IA2_LAB_PORT_BASE` (default 13500) moves the whole set, so a second lab can run beside the first on other ports. The runner exits nonzero for a FAIL or setup error and records source-tree and binary hashes. See [VALIDATION.md](VALIDATION.md) for the exact tested runtime revision and results; the historical R3 finding remains visible. The R3 timing cases inject a single 1 s scan stall, signal only the harness's own runtime child, and read the runtime socket's receive queue with `netstat` (`-an -p tcp` on macOS, where they were recorded; the Linux `-tan` form is untested). The recording proxy can hold the runtime's `/write` answer, or keep the runtime connection open after the bridge disconnects; neither control is used unless a case enables it.

The harness manages the deliberately paused runtime with `subprocess.Popen` and waits for its exit in a worker thread. This avoids a macOS/Python 3.14.7 `asyncio` child-watcher hang on `SIGSTOP`, reproduced during review. It does not change the bridge or runtime behavior, or turn R3's missing audit evidence into proof of non-execution.

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
- `tools/reconcile_report.py`: the three-way reconciliation (WP6); `lab/soak.py`: the soak (WP9).

```sh
python3 -I -B tools/package_prod.py prod
python3 -I -B prod/bridge_run.py --help
```

`package_prod.py` replaces the specified output directory. Its `prod` flavor only means fault hooks are absent; it does not mean hardware or production acceptance. The bridge CLI requires a registry and its SHA-256, a mode-0600 request-key file, loopback runtime address, project directory, journal, request file and trace path. Lab fixtures create their own test keys; no deployment keys or runtime binaries are included.

On macOS, a failing `F_FULLFSYNC` is propagated rather than downgraded to `fsync`. A filesystem that exposes but cannot support full sync will therefore refuse to proceed. Actual power-loss durability and hardware behavior remain bench work; this example supplies offline evidence only.
