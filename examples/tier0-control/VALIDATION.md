# Validation — 2026-10-28 UTC+8

IR03 result publication recovery, nullable-failsafe compatibility and the D6 TOML guard repair are validated. The prior `failsafe_status: null` startup blocker is resolved. A null report means there is no failsafe-attempt report; it does not override any other safety evidence. Pending, acknowledged or unconfirmed entries still inhibit writes, and malformed reports still fail closed.

| Check | Latest result |
|---|---|
| Isolated Python suites in this checkout | 253 tests passed, including 14 IR03, 7 nullable-failsafe and 6 D6 regressions; D6 also passed on Python 3.14.8 |
| Full runtime/fake-device suite on runtime built at `047e7c17ffda1c52ac3d0f68726d9a15889a3cba` (runtime sources identical to main) | 26 PASS, 1 known R3 FINDING; all 395 assertions passed; no setup errors |
| Crash recovery assertions per action | Passed |
| BOOL fake-device checks on current runtime | 21 passed |
| Nullable-failsafe mutation controls | All 3 detected by assertion failures, no test errors |
| Device-free project compilation and `cs sim run` | Repeated after D6 repair: all 7 steps passed; program stopped, no forces or alarms |
| Repository agent-adaptation check and formatting check | Passed again after D6 repair |
| Clippy, server build, full Rust workspace tests, release builds, Web build and Web tests | All rerun successfully after D6 repair; 8589 Rust tests passed (105 ignored), 423 Web tests passed |
| D6 regression against previous guard | 18 assertion failures, no errors; repaired guard passed |
| D6 with actual runtime, source and production packages | Both enabled-write fixtures refused with exit 3 in both packages; both disabled-write controls started |

The current runtime SHA-256 is `52160ab8ba63cd16852c66b5647f0a16844bdc45f410c48da2c589c2cc2e74de`. The runtime reports build commit `047e7c17ffda`; its Rust sources and locks are unchanged by this repair. Every integration verdict matches the final bridge/lab source-tree hash `4b79f02cbca51474192941cf17496ca49f2a121451a66158e7ceea2225cb0e2b`. The machine-readable suite results and file hashes are in [validation-summary.json](validation-summary.json). Raw logs and generated projects remain under the gitignored `evidence/` directory.

The D6 review found that literal text matching missed escaped TOML keys and treated `#` inside strings as comments. The guard now parses the actual `mqtt.allow_write` field with bundled, unmodified Tomli 2.2.1. Missing file/table/field follows the runtime false default; explicit values must be boolean false. Invalid TOML, table types and flag types fail closed, and errors do not expose configuration values. Regression vectors cover quoted/escaped/dotted keys, inline tables, basic/literal/multiline strings, duplicate keys and invalid types. Process tests cover startup before any runtime reads, configuration changes before new requests, replay without another send, and the standalone production package including the original parser license and file hashes. The same six tests pass on Python 3.9.6 and 3.14.8.

The actual-runtime D6 checks used a device-free project with a loopback MQTT address and no broker. They confirm that IA2 interprets both bypass fixtures as `allow_write=true` while the repaired bridge refuses them; they do not claim MQTT message delivery or physical effects. The source-tree hash now recursively includes the bundled parser. D6 raw evidence is under `evidence/d6-fix-20261028T223623p0800/`, with process evidence under `evidence/process-d6/`. The prior failing review and compatibility results remain historical evidence in the JSON summary.

The nullable-failsafe tests first reproduced 44 assertion failures on the previous code, then passed after repair. They cover startup, pre-write checks, post-write checks, recovery and replay, mixed null/omitted/empty reports, nonempty reports on either endpoint, all three pre-write snapshots, malformed reports and missing required fields. The three mutation controls reintroduce null rejection, ignore another endpoint's nonempty report, or accept malformed falsy values; each is detected.

The earlier pinned runtime `1c42df70d2581babbf79f0ba96726f61cb8a68f5` also passed the 27-case integration set (26 PASS plus the same R3 FINDING), all 395 assertions, crash checks and 21 BOOL checks before the nullable-failsafe follow-up. Its binary SHA-256 is `cf64e6a28d375496fb39664588cde25287d40c70e9bf3f23f0e857acfb7f4b27`. Historical results, including the pre-fix current-main startup failures, are retained separately in the JSON summary rather than presented as current failures.

During independent IR03 acceptance, the actual archived pre-IR03 engine/process generated a terminal followed by a trace failure and request quarantine. The repaired engine recovered that result after upgrade, including beyond the hot-index horizon and with an empty request file. The original IR03 unit reproduction passed unchanged, 12 copied review checks passed, and all 8 deliberate IR03 mutations were detected. Those are archival acceptance checks; the portable repository suite uses an explicit legacy-behavior fixture instead of shipping the entire old checkout.

Reproduction commands are in [README.md](README.md). The harness returns a nonzero exit status for a FAIL or setup error. The compatibility repair changes neither the journal nor event format and does not disable preflight. Full integration includes the production package with lab hooks removed.

R3 remains a finding about an unanswered write while the runtime is stopped; timeout is not treated as proof of non-execution. D11 event/sample authentication and the real Agent-facing control MCP remain unimplemented. `verified` retains its local evidence limitations and is not proof of physical effect or causation.

All runtime work used loopback or device-free simulation. Test services are stopped and their ports released. Hardware timing, wiring, physical effect and actual power-loss durability remain **BENCH PENDING**, requiring measurements on the intended device and storage. No binaries, deployment keys or personal deployment records are committed, and this PR is not a hardware release.
