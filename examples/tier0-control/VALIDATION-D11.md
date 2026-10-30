# D11 and local control MCP validation

This is the original implementation validation. Subsequent review fixes and fresh results are recorded in [the D11/MCP handoff](HANDOFF-D11-MCP-20261030.md); the counts and failures below remain historical evidence.

2026-10-30 UTC+8. Branch `feat/tier0-signed-control-mcp`, based on `a5e4a851044f9fa6462e6eff71576d9de82a00b2` (PR #101). This is a separate local increment; PR #100/#101 were not merged or changed by this work. No grey0, live Tier0, hardware, client MCP configuration or deployment was modified.

The implementation covers mandatory HMAC envelopes for bridge events and independent samples, authenticated trace recovery, a dedicated read-only sampling process, and a real two-tool MCP stdio server using signed local files. It preserves the existing status meanings and the bridge's rule that result-publication retries never repeat a runtime write. Setup and operational limits are in [LOCAL-CONTROL.md](LOCAL-CONTROL.md).

## Evidence and verification

Evidence root: `evidence/d11-local-20261030/` (gitignored). Disposable keys in this evidence are local test keys, never deployment keys. The source snapshot and its SHA-256 manifest are retained there. Integration uses the previously validated runtime with the detached-write audit fix:

```text
binary: ia2-runtime-detached-r2
sha256: c18b0aaab60425ef848e8e278b3ac60c1ec7a50dfb708ed25184a0a987018893
```

| Check | Result | Evidence |
|---|---|---|
| Python regressions | 362 tests, 30 suites, PASS | `unit-final/summary.json` |
| D11 authentication mutations | All 5 detected by assertion | `mutations-r3/summary.json` |
| Nullable-failsafe controls | All 3 detected by assertion | `evidence/failsafe-controls/` |
| Reconciliation mutations | All 64 detected by assertion | `reconcile-mutations/summary.json` |
| Crash-boundary audit | Three actions checked separately, PASS | `snapshot/evidence/crash-per-action.json` |
| Signed bridge/standalone package smoke | A1 and PROD PASS | `smoke-r2/` |
| Forged evidence and real MCP | F6, F7, F8, MCP, MCPPROD PASS | `d11-mcp-r1/` |
| Final MCP error boundary and standalone package | MCP and MCPPROD PASS, including restart and one runtime write | `mcp-final/` |
| Complete signed-path fault suite | 49/50 PASS; P1F5 setup race fixed and retested below | `full-r2/` |
| P1F5 final setup | Three consecutive PASS runs; all 50 case types now covered by passing runs | `p1f5-final/` |
| Repeated R3 timing cases | All ten runs PASS | `r3-repeat/` |
| Full repository quality gate | All eight commands exit 0; Rust 8,589 passed / 105 ignored; Web 423 passed in 39 files | `gate/` |
| Device-free simulator | Project check/open, seven scenario steps and stopped final status, PASS | `sim/` |

The added tests cover real HMAC interoperability, unsigned/forged records, domain and role separation, revocation, historical keys and validity windows, malformed strict JSON, hint mismatch, signed journal anchors, missing keys, real stdio initialization/tool calls, MCP restart and authenticated action lookup, file/index limits, ambiguous request persistence, standalone sampler behavior, malformed signed producer data and nonfinite request rejection publication. The existing arithmetic vectors test the pure post-authentication arithmetic separately. Test-only signing adapters are excluded from the standalone package.

The five deliberate D11 mutations skip the MAC, use event keys for samples, ignore hint mismatch, ignore revocation, or validate against current time rather than the signed timestamp. Each must fail an assertion rather than merely crash the suite.

The machine-readable [validation summary](validation-d11-summary.json) records the final totals and source-manifest hash. The complete suite's earlier P1F5 failure is not relabeled as a pass: its corrected setup passed three fresh runs. The final MCP boundary change passed the full 362-test Python suite and two fresh integration cases. The ten additional R3 runs passed unchanged timing assertions. No new two-hour signed soak or hardware acceptance is claimed.

`snapshot-final/` differs from the full-suite snapshot only in the P1F5 harness wait and documentation. `snapshot-mcp-final/` adds the MCP malformed-data error boundary and its stdio regression; the final unit and MCP/package reruns cover that change. All bridge, sampler, verifier and vocabulary bytes remain identical to the full-suite run. `final-code-manifest.json` pins the final executable source. The simulator ran against its own ephemeral loopback server with access to the user's persisted IDE directories denied; its server was stopped afterward and the scenario's final runtime state was not running.

## Findings fixed during implementation

- The first transition run exposed legacy assertions reading unsigned trace objects. They now authenticate envelopes before inspecting events. Publication/recovery fault hooks still instrument the real bridge writer, not the test fixture wrapper.
- Exponent input such as `1e999` was correctly rejected by the existing value gate, but its nonfinite parsed value could make the new rejection event unsignable. Rejection events now encode invalid requested/applied floating values as null. The new regression confirms a signed `rejected/bad_value`, zero runtime writes and no pending publication. Original request text remains in the journal.
- Nonfinite feedback is now represented as null in strict signed JSON; the raw runtime bit pattern remains in the sample. P1F5 checks those bits, the null marker and the unchanged rule that invalid feedback never verifies an action, while later good feedback can.
- The second complete run caught a P1F5 setup race: waiting for null could reuse the previous +infinity samples before -infinity had propagated. The setup now waits for three samples carrying both null and the exact expected raw bits. No verifier behavior or timing budget was relaxed; the original failure is retained in `full-r2/`.
- Final review found that authenticated but malformed request/evidence fields could raise an exception while MCP selected evidence, before reaching the verifier's own error boundary. MCP now reports a generic `local_data_error` without retrying and keeps its stdio connection alive. A real subprocess test covers missing deadlines, a nonstring target key, an invalid runtime ID and an invalid sample time, and confirms a subsequent ping still works. A valid signature does not guarantee that a producer's data is well formed.
- The initial full run also recorded delayed runtime startup, a missed reconcile wait window, and one A4 request rejected for arriving over 20 seconds after creation. These failures remain in `full-r1/`; expectations and timing limits were not relaxed. The final-source run uses a scoped idle-sleep assertion and fresh processes/evidence. A delay's cause is not inferred from the eventual correct audit result alone.

## Measured local cost

Python 3.9.6, three trials of 5,000 iterations each, using a signed MCP integration case. These are process-CPU microbenchmarks, not end-to-end latency or hardware measurements (`microbenchmark.json`).

| Operation | Median |
|---|---:|
| Sign one bridge event | 6.57 µs |
| Verify one bridge event | 14.45 µs |
| Sign one sample | 17.41 µs |
| Verify one sample | 37.23 µs |
| Authenticate and derive one event plus four samples | 178.12 µs |

One sample sign plus one verification at 5 Hz is approximately **0.0273% of one CPU**. This excludes `/status` work, HTTP, IPC, disk writes and application query frequency. The example event grew from 1087 to 1455 bytes; the example sample from 1890 to 2438 bytes. These are examples, not fixed protocol sizes.

## Remaining scope

- **LOCAL ONLY:** signed file transport and a simulation-only MCP. Real Tier0 HTTP/MQTT integration and service deployment are not supplied by this increment.
- **BOUNDED HISTORY:** MCP indexes at most 20,000 records per log; at 5 Hz that is about 67 minutes of samples. A limit produces an explicit tool error. Long-running use needs session management or a persistent indexed adapter.
- **BENCH PENDING:** measure the dedicated sampler's extra `/status` load on grey0/#65; verify actual Tier0 permissions and transport; validate device timing, physical feedback and power-loss durability before field use.
- HMAC authenticates a speaker, not its factual correctness. The verifier has symmetric secrets and can forge records if compromised. Separate files under one OS account are not process isolation. `verified` still does not prove physical effect or that this command caused the observed value.
