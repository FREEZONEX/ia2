# Validation — 2026-10-28 UTC+8

The repository import preserves every `bridge/*.py` file, `bridge_run.py` and `contract_vocab.json` byte-for-byte except removal of whitespace on one blank line in `bridge/main.py` from the independently accepted IR03 repair. Packaging changes move fixtures and design paths into the example, replace legacy-checkout imports with an explicit test-only legacy-behavior fixture, give the BOOL runner a `--bin` argument, and scope F1's read-only check to its pinned schema fixture, and make the harness exit nonzero on FAIL/setup errors.

| Check | Result |
|---|---|
| Isolated Python suites in this checkout | 240 tests passed |
| Full runtime/fake-device suite on the historical `1c42df70d2581babbf79f0ba96726f61cb8a68f5` runtime | 26 PASS, 1 known R3 FINDING; 395 assertions passed |
| Current-main runtime smoke on `e6a38aa355cde1747114866b52c2e26f0e3ca724` (A1/B3/C1/F1/PROD/P1F5/P1F6) | **BLOCKED / FAIL:** all 7 stop at preflight because `status.failsafe_status` is `null` |
| Crash recovery assertions per action | Passed |
| BOOL fake-device checks | 21 passed |
| Device-free project compilation and `cs sim run` | Passed; 7 scenario steps, stopped afterward, no forces |
| `./scripts/check-agent-adaptation.sh` | Passed |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --offline --workspace --all-targets -- -D warnings` | Passed |
| `cargo build --offline -p server` | Passed |
| `cargo test --offline --workspace` | Passed |
| `cargo build --offline --release -p server -p ia2-cli -p ia2-runtime -p lsp-launcher` | Passed |
| `pnpm install --frozen-lockfile --offline` | Passed |
| `pnpm --filter @cs/web build` and `pnpm --filter @cs/web test` | Passed |

The historical runtime SHA-256 is `cf64e6a28d375496fb39664588cde25287d40c70e9bf3f23f0e857acfb7f4b27`. Integration runs record their binary and Python-source hashes in each `verdict.json`. No binaries, test-generated keys, personal deployment records or raw evidence directories are committed.

During independent repair acceptance, the actual archived pre-IR03 engine/process generated a terminal followed by a trace failure and request quarantine. The repaired engine recovered that result after upgrade, including beyond the hot-index horizon and with an empty request file. The original IR03 unit reproduction passed unchanged, 12 copied review checks passed, and all 8 deliberate IR03 defect mutations were detected by assertion failures. These archival checks were local acceptance evidence; the portable suite provides its own legacy-behavior fixture instead of claiming to ship the complete old checkout.

R3 remains a finding about an unanswered write while the runtime is stopped. The observed outcome is reported without claiming timeout proves non-execution. D11 event/sample signatures and the real Agent-facing control MCP remain unimplemented. The verified state is limited to the stated local evidence model.

All runtime work used loopback or device-free simulation. Hardware timing, wiring, physical effect and actual power-loss durability remain **BENCH PENDING**; they require measurements on the intended device and storage. This PR does not constitute a hardware release.

The current runtime intentionally reports no failsafe attempt as `failsafe_status: null` (`docs/api.md`, failsafe-status contract). The imported schema accepts an omitted field or a populated object, but rejects null. No control POST occurs in these blocked cases. This is a pre-existing integration gap outside IR03, and is a merge blocker for use with current main. The PR is a draft for review; the IR03 repair itself is accepted against its pinned baseline.

After final whitespace cleanup and the harness exit-status change, A1 passed again on the pinned runtime (24 assertions, exit 0); current-main A1 reproduced the preflight refusal and correctly returned exit 1. The complete integration suite above predates these nonsemantic bridge/runner changes; final bridge ASTs match the accepted IR03 sources.
