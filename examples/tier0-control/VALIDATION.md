# Validation — 2026-10-28 UTC+8

IR03 result publication recovery and current-runtime nullable-failsafe compatibility are validated. The prior `failsafe_status: null` startup blocker is resolved. A null report means there is no failsafe-attempt report; it does not override any other safety evidence. Pending, acknowledged or unconfirmed entries still inhibit writes, and malformed reports still fail closed.

| Check | Latest result |
|---|---|
| Isolated Python suites in this checkout | 247 tests passed, including 14 IR03 and 7 nullable-failsafe regressions |
| Full runtime/fake-device suite on current-main runtime `e6a38aa355cde1747114866b52c2e26f0e3ca724` | 26 PASS, 1 known R3 FINDING; all 395 assertions passed; no setup errors |
| Crash recovery assertions per action | Passed |
| BOOL fake-device checks on current runtime | 21 passed |
| Nullable-failsafe mutation controls | All 3 detected by assertion failures, no test errors |
| Device-free project compilation and `cs sim run` | 7 scenario steps passed during initial import; program stopped, no forces |
| Repository agent-adaptation check and formatting check | Passed again after the compatibility change |
| Clippy, server build, full Rust workspace tests, release builds, Web build and Web tests | Passed during initial import; the compatibility follow-up changes Python/docs only, and the compiled sources and dependency locks are unchanged |

The current runtime SHA-256 is `8247e4525d89a273d0cff2d7fe525c7fe91c321d3c708b75afbcb11318c76483`. Every current-runtime integration verdict matches the final bridge/lab source-tree hash `d1b56f18dc1198bd59ce953ccf8a6828b9fdae8749b5bbdff5d2ff88c4e80d93`. The machine-readable suite results and file hashes are in [validation-summary.json](validation-summary.json). Raw logs and generated projects remain under the gitignored `evidence/` directory.

The nullable-failsafe tests first reproduced 44 assertion failures on the previous code, then passed after repair. They cover startup, pre-write checks, post-write checks, recovery and replay, mixed null/omitted/empty reports, nonempty reports on either endpoint, all three pre-write snapshots, malformed reports and missing required fields. The three mutation controls reintroduce null rejection, ignore another endpoint's nonempty report, or accept malformed falsy values; each is detected.

The earlier pinned runtime `1c42df70d2581babbf79f0ba96726f61cb8a68f5` also passed the 27-case integration set (26 PASS plus the same R3 FINDING), all 395 assertions, crash checks and 21 BOOL checks before the nullable-failsafe follow-up. Its binary SHA-256 is `cf64e6a28d375496fb39664588cde25287d40c70e9bf3f23f0e857acfb7f4b27`. Historical results, including the pre-fix current-main startup failures, are retained separately in the JSON summary rather than presented as current failures.

During independent IR03 acceptance, the actual archived pre-IR03 engine/process generated a terminal followed by a trace failure and request quarantine. The repaired engine recovered that result after upgrade, including beyond the hot-index horizon and with an empty request file. The original IR03 unit reproduction passed unchanged, 12 copied review checks passed, and all 8 deliberate IR03 mutations were detected. Those are archival acceptance checks; the portable repository suite uses an explicit legacy-behavior fixture instead of shipping the entire old checkout.

Reproduction commands are in [README.md](README.md). The harness returns a nonzero exit status for a FAIL or setup error. The compatibility repair changes neither the journal nor event format and does not disable preflight. Full integration includes the production package with lab hooks removed.

R3 remains a finding about an unanswered write while the runtime is stopped; timeout is not treated as proof of non-execution. D11 event/sample authentication and the real Agent-facing control MCP remain unimplemented. `verified` retains its local evidence limitations and is not proof of physical effect or causation.

All runtime work used loopback or device-free simulation. Test services are stopped and their ports released. Hardware timing, wiring, physical effect and actual power-loss durability remain **BENCH PENDING**, requiring measurements on the intended device and storage. No binaries, deployment keys or personal deployment records are committed, and this PR is not a hardware release.
