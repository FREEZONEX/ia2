# 5 Hz history budget and complete-session rotation

2026-11-03 UTC+8. Work in progress: the 75-minute acceptance lanes are running; no long-run PASS is claimed yet.

The local MCP keeps the existing hard limit of 20,000 complete rows per request/event/sample log. New requests now refuse before append when a log is full. The request count is checked under the existing append lock. Queries remain complete at the boundary and explicitly fail beyond it. Restarting against the same files does not reset capacity.

Regression coverage uses the real 19,999 / 20,000 / 20,001 row boundaries, rejected rows, repeated errors after restart, and two independent MCP processes competing for the final request slot. Whole-session rotation follows [LOCAL-CONTROL.md](LOCAL-CONTROL.md); it preserves complete evidence and does not replay old actions or restart the runtime.

The local acceptance script is [lab/soak_history.py](lab/soak_history.py). The task's evidence lives inside the enclosing soft-PLC workspace at `tier0-history-5hz-20261103/`; raw logs and disposable signing keys are not part of this Git repository. Runtime identity is the same frozen binary used by the previous final soak, SHA-256 `7f4572bb7b87d50ee78165fa7c85b209fdea001c1a06105e96255caf8cb94d9f`.

This local example remains simulation-only. The real grey0 preflight found the unchanged older diagnostic runtime, which does not expose `runtime_id`; it cannot provide samples for this signed chain. Read-only field load observations, deployment compatibility, live Tier0 transport, authorized physical actions and power-loss durability require separate conclusions.
