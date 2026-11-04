# D11 / control MCP integrated validation

2026-10-30 UTC+8. This record covers the tree prepared for the D11/control MCP PR, after integrating `origin/main`. Earlier implementation and rereview evidence remains in [VALIDATION-D11.md](VALIDATION-D11.md) and [the handoff](HANDOFF-D11-MCP-20261030.md).

Submitted as [PR #102](https://github.com/FREEZONEX/ia2/pull/102) against `main`, open for review and not merged. Validated code is committed in `2824b6a25bc0c878a9668b177787bcb07cacaeda`; the following handoff/status update changes documentation only. GitHub reports a mergeable PR with all 61 changed files under `examples/tier0-control/`.

## Source and review scope

| Item | Revision |
|---|---|
| Main integrated | `f0fa410bc8c22263aba7de0bc5400d0273d61c13` (includes merged #100 and #101) |
| Local branch before integration | `b7a4480ebb8a762aec9db156233aabbe71f7cf87` |
| Integration merge | `9eaa24bea7e70f44aba8f655d29afc10e60032df` |
| Evidence directory | `evidence/d11-pr-20261030/` (gitignored; fresh evidence, no old run overwritten) |
| Initial executable inputs | `source-manifest.json`; source snapshot under `source/` |
| Final executable inputs | `source-final-manifest.json`; source snapshot under `source-final/` |

The merge was conflict-free. Relative to main, all PR changes are under `examples/tier0-control/`; Rust, Web, API documentation and repository skill code match main. Review checked that the merged tree retains both D11 authentication and upstream's corrected audit wording, macOS R3 socket/process-stop checks, and fake-device connection cleanup. No runtime fix is being reimplemented in this PR.

One regression was added after integration: `test_action_outcomes_and_tool_errors_have_distinct_boundaries`. It exercises actual tool dispatch and signed local files, preserving the documented four-way distinction: rejected envelopes are counted while remaining evidence is used; verifier exceptions return `unknown` with `isError:false` and may have zero rejected envelopes; outer lookup/budget errors return `isError:true`; ambiguous request persistence returns the same action ID with `unknown`, without automatic resubmission. No production behavior was changed for this addition.

The first complete Python run found one stale assertion in `test_r3_reconcile`: main changed the second-look diagnostic, but the test still required the old phrase `without an entry`. The assertion now requires both the historical older-runtime audit gap and possible 256-entry ring turnover. Existing assertions that absent evidence cannot prove non-execution, never means `landed:false`, and leaves the action `unknown` remain intact. The original failure is preserved under `unit/`; final validation uses a new snapshot and output directory. This is a test wording correction, not a change to runtime behavior or case acceptance criteria.

## Runtime and execution

This run builds the runtime from the integrated repository with:

```sh
cargo build --release -p server -p ia2-cli -p ia2-runtime -p lsp-launcher
```

The result is copied to `evidence/d11-pr-20261030/bin/ia2-runtime-integrated` before integration cases run, and its hash is saved in `runtime.json`. This is a new validation of the integrated build, not substitution of the previously unmerged branch's old runtime. The historical `c18b0a…` binary remains the pin for the earlier runs only.

```text
SHA-256 4379dc4fa39459cb2d92532566bcbe2f35c6ea14cbb3883fb53ff3690dba75c6
```

All cases use loopback services and fake Modbus devices. No grey0, real Tier0, physical hardware, service deployment or client MCP configuration is changed. The simulator uses its own ephemeral loopback server with access to user IDE state directories denied.

## Results

**OFFLINE PASS.** Machine-readable results are in [validation-d11-integrated-summary.json](validation-d11-integrated-summary.json). All results below were rerun on the integrated source; earlier counts have not been reused as current validation.

| Check | Result | Evidence |
|---|---|---|
| Repository adaptation, formatting, Clippy, server/release builds, Rust/Web tests | **8/8 PASS**; Rust 8,594 passed / 105 ignored; Web 423 tests / 39 files | `runs.json`, individual command logs |
| Complete Python suites | **366 tests / 30 suites PASS** | `unit-final/summary.json` |
| D11 mutations | **5/5 detected by assertion** | `d11-mutations/summary.json` |
| Reconciliation mutations | **64/64 detected by assertion** | `reconcile-mutations/summary.json` |
| Nullable-failsafe controls | **3/3 detected by assertion** | `source-final/evidence/failsafe-controls/summary.json` |
| Complete signed-path case suite | **50/50 PASS; 596 checks, zero failures** | `full-cases/`, `full-cases.log` |
| Per-action crash recovery | **3/3 PASS** | `crash-per-action.json` |
| Device-free simulator | **7/7 steps PASS**; project check/open passed; alarms `[]`, `running:false`, `forces:[]`; own server stopped | `sim/` |

The case suite includes all original P0/P1 cases, R3 timing variants, F6/F7/F8 forgery cases, and real stdio MCP / standalone-package cases. Tests use new evidence paths; runtime writes and audit/device records are checked, including no duplicate write across MCP restart and bridge recovery. A4 explicitly exercises 300 writes and audit-ring turnover. No new two-hour soak or hardware acceptance is claimed.

All 104 Python files and the contract vocabulary match the final frozen snapshot byte-for-byte. `final-code-manifest.json` has SHA-256 `0d2cc2fa7a29105915a7dee95d5ca42a947c86aafcff93d390d01b71cd694a33`; the complete snapshot manifest is `e6e7067d85a022220be08503e1c843271f7001195883c3da01d550709fc84d5d`. Documentation was updated after execution. `EVIDENCE-INDEX.sha256.json` indexes the local evidence; the generated keys, logs and binaries remain gitignored.

The integrated diff was reviewed against main: signing/recovery, read-side evidence selection, MCP result/error boundaries, sampler and packaging remain consistent. No new production-code finding was identified. This is a same-author integration review, not an independent audit. The final fetch still points to main `f0fa410`; PR changes remain confined to the example directory.

## Remaining limits

The local MCP is simulation-only and uses signed files; live Tier0 transport and deployment remain out of scope. Each indexed log is limited to 20,000 records (about 67 minutes of samples at 5 Hz); exceeding a budget is a tool error, and restarting against the same log does not reset its size. HMAC verifiers possess signing-capable secrets; same-UID files do not isolate processes. `verified` authenticates the stated evidence and passes the configured feedback checks, without proving physical effect or causation. On-target sampler load, hardware timing and power-loss durability remain bench work.

Operational instructions and key-revocation boundaries are in [LOCAL-CONTROL.md](LOCAL-CONTROL.md). The detailed status/error interpretation is in the [handoff](HANDOFF-D11-MCP-20261030.md#异常返回的判读).
