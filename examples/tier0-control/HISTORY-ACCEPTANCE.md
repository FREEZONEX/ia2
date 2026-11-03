# 5 Hz history budget and complete-session rotation

2026-11-03 UTC+8. **OFFLINE PASS:** two separate 75-minute real-time acceptance lanes completed, followed by independent saved-evidence verification and read-only queries against the sealed sessions.

The local MCP keeps the existing hard limit of 20,000 complete rows per request/event/sample log. New requests now refuse before append when a log is full. The request count is checked under the existing append lock. Queries remain complete at the boundary and explicitly fail beyond it. Restarting against the same files does not reset capacity.

Regression coverage uses the real 19,999 / 20,000 / 20,001 row boundaries, rejected rows, repeated errors after restart, and two independent MCP processes competing for the final request slot. Whole-session rotation follows [LOCAL-CONTROL.md](LOCAL-CONTROL.md); it preserves complete evidence and does not replay old actions or restart the runtime.

| Lane | Active duration | Samples | Actions | Outcome |
|---|---:|---:|---:|---|
| Deliberately unrotated | 4,500.227 s | 22,095 | 130 | At sample 20,001, queries and new submissions explicitly fail; MCP restart preserves the error and the request file remains unchanged |
| Complete rotation after 35 minutes | 4,500.099 s | 10,318 + 11,781 | 69 + 79 | Both sessions verify all actions; no pending/replayed action, no runtime restart, sealed first session unchanged |

Both lanes use the production bridge, actual stdio MCP and the standalone sampler's default 200 ms interval. Observed cadence was 4.908–4.910 Hz (median interval 204 ms), so “5 Hz” describes the nominal configuration. The unrotated lane crossed the hard limit at 4,073.947 s. Its 130 actions had verified before overflow; subsequent query errors are not reported as successful queries. The rotation lane rejected the old action ID in the new session. The last old sample and first new sample were 168 ms apart.

An independent stdlib-only checker authenticated 278 requests, 278 events and 44,194 samples; cross-checked 426 in-run verified observations (including repeat queries); verified journal chains and native scan-clock duration; and reconciled 278 unique HTTP writes/audits with the complete fake-device changed-value sequence. Archive-only MCP queries preserved all sealed files: bounded sessions remain queryable, and the oversized session still explicitly refuses. No historical bridge was restarted or request replayed.

Validation: 384 Python tests / 31 suites, including 17 MCP tests and four new regressions; three negative controls catch the old implementation. F6/F7/F8/MCP/MCPPROD passed 5/5 with 33 assertions. The full repository gate passed: adaptation, formatting, clippy, Rust 8,594 passed / 105 ignored, release builds, Web build and 423 tests. Both lane/driver exits were zero; runtime/samplers exited normally, bridges received the planned SIGTERM, and test ports were released.

The local acceptance script is [lab/soak_history.py](lab/soak_history.py). The task's evidence lives inside the enclosing soft-PLC workspace at `tier0-history-5hz-20261103/`; raw logs and disposable signing keys are not part of this Git repository. Runtime identity is the same frozen binary used by the previous final soak, SHA-256 `7f4572bb7b87d50ee78165fa7c85b209fdea001c1a06105e96255caf8cb94d9f`.

This local example remains simulation-only. The real grey0 preflight found the unchanged older diagnostic runtime, which does not expose `runtime_id`; it cannot provide samples for this signed chain. Read-only field load observations, deployment compatibility, live Tier0 transport, authorized physical actions and power-loss durability require separate conclusions.

This accepts the bounded explicit-rotation contract, not an automatic production rollover service, a persistent index, multi-day resource stability or maximum throughput. Evidence can grow after a submission's budget check; operators must drain and rotate early. Same-author execution plus an independent calculation implementation is not independent-person code-review approval.
