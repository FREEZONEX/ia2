# Validation

## Stage 1a close-out: WP6, P1 and the WP9 soak — 2026-10-29 UTC+8

PR #99 was merged as `a2dec2a` (with the maintainer's harness fix `f44755a`). This round finishes its open acceptance list on top of that merge. Everything ran on loopback against the runtime built from `main` (Rust sources identical to `e6a38aa`, SHA-256 `52160ab8ba63cd16852c66b5647f0a16844bdc45f410c48da2c589c2cc2e74de`), with fake Modbus TCP devices. No hardware was contacted. The exit-criteria verdicts are in [RESULT-1A.md](RESULT-1A.md).

| Check | Result |
|---|---|
| Isolated Python suites | 288 tests passed in 28 suites, including 22 vectors for the reconciliation program |
| Reconciliation program, 16 deliberately broken variants | All 16 detected by the vectors (`evidence/reconcile-mutations.log`) |
| Full loopback suite on the final tree: 28 P0, `PROD`, P1F5, P1F6 and 13 P1 cases, with the R3 family run three times | 54 of 54 PASS, 727 assertions, 0 failed, no setup errors; code tree `255e1166af05f67fb3e3317409361fa974b8f1feff093740cf4d7158c76cbea2` (`evidence/final-20261029/`). Crash recovery per action 3/3, BOOL fake-device checks 21, nullable-failsafe controls 3/3 |
| R3 family and R1, R2 on Python 3.14.8 (the review's hang was on 3.14.7) | 7/7 PASS; no hang |
| 2-hour soak, `main` runtime (`evidence/soak-main-20261029`) | 121 minutes, 4,496 request lines, 40 faults, 7 runtime instances, no unexpected exit. Reconciliation with the final program: CLEAN, 0 unknown writes, 2,166 of 2,179 intents consistent; the other 13 each match an injected fault. The program frozen with the soak reported 1 device-side discrepancy; see below |
| 60-minute soak, fixed runtime `b7e455d4…` (`evidence/soak-detached-20261029`) | 60 minutes, 2,252 request lines, 17 faults. 1,125 of 1,128 intents consistent and 3 never sent (R1); 0 executed without an audit entry, including both R3T1 stops. CLEAN in the run and with the final program; verdict PASS |

The P1 cases changed several EXPECT values after running; [TEST-CASES.md](contract/TEST-CASES.md) §3.2 lists them. B5: the start-up preflight already refuses a registry that names a missing variable (exit 4). C5: while the runtime is paused, its scan count stands still, so G8 rejects first. C10 (e): the bridge rereads `northbound.toml` before every request. R5: a burst alone cannot reach the bridge-wide limit, because requests read more than 2 s after `requested_at_ms` expire under G4. F4: the registry check missed an instance-prefixed name; it now takes the running runtime's variable names. C5's direct writes add another "answered 200, not in effect" case: IA2 answers 200 while paused (the VM takes the value; the device does not) and while the target is forced (the VM keeps the forced value).

The soak sent a random mix every 1.0–2.2 s: valid writes to four targets, governance-denied, out-of-range, unknown-key, forged, malformed, expired, simulation-mismatched, mistyped, duplicate and burst requests. Every two to four minutes it injected one fault, and once it paused the `/audit` archive for 12 minutes. Afterwards the reconciliation program had to classify every journal intent against the archive and the devices' own logs. On the `main` runtime the 13 differences were: 6 writes applied after the client disconnected with no `/audit` entry (6 of the 7 R3T1 stops), 1 request the runtime never read (the 7th stop came before the read), 5 intents the bridge never sent (R1), and 1 write whose entry was lost when the runtime was killed 0.5 s later. There were no unknown writes and no unexplained device writes; all 3,999 authenticated requests were answered; the 362 forged and malformed lines stayed in the journal; the journal chain verified. The 12-minute archive pause covered about 216 entries, fewer than the ring holds, so the soak did not actually turn the ring over; A4 and the vectors cover turnover.

**A criterion changed after the run.** The reconciliation that ran with the soak reported `DISCREPANCY` for one write, and the difference was on the device side, not between the journal and `/audit`. Device 2 had closed its link 0.18 s before a `d2_sp` write. IA2 had not noticed: it answered 200 with `undelivered_device: null` and audited `ok`. A newer write replaced the value 7.6 s later, and when the link came back 15.2 s later IA2 delivered only the newer value. The device answered no request at all in that whole stretch, although the runtime polls it every 100 ms. The program now accepts exactly that: no answered request from the write until the value was replaced, or until the log ends. One answered request in between still counts as a discrepancy. A two-way vector and two inverse mutations cover the rule. In both soaks this one write is the only one it applies to. Both reports are kept (`reconcile.json`, `reconcile-final-program.json`). The read side left that write `unknown` and never reported `verified`. This is another case of the window C4 measures: IA2 answers 200 for a write that cannot reach the device.

The second soak and the final suite ran alongside the main soak, on their own ports (`IA2_LAB_PORT_BASE` 14500 and 15500). The soaks' runtime logs show exactly the two injected watchdog trips and no others. Each soak ran from a frozen copy of this directory (hashes recorded outside the repository). The bridge, `lab/soak.py` and `lab/hooks_soak.py` in those copies equal the final tree. Later changes are in the reconciliation program and its tests, the P1 cases, `run_cases.py`, `IA2_LAB_PORT_BASE`, `check_registry.py` and the docs. Both soaks were reconciled again with the final program. A4 was also rechecked with the final program, with a stop marker for the two zero writes the harness's `/stop` adds after the case (`evidence/final-20261029/A4-r1/`). The counts were unchanged.

## R3 timing and audit reconciliation — 2026-10-29 UTC+8

This round covered the missing R3 timings and corrected how the bridge reports reconciliation. Previously the bridge treated a missing `/audit` entry as "not landed"; the timing tests showed that inference is false. The bridge now reports only what `/audit` shows. Afterwards the trace startup read was made consistent, and the IA2 runtime gap itself was fixed on a separate branch (see [Runtime fix](#runtime-fix-separate-branch)). On the final code the integration result is **31 PASS** (the 27 earlier cases plus 4 new R3 timing cases; 471 assertions, none failed), and the R3 family passed all three passes (15/15). **[Stage 1a](contract/TEST-CASES.md#4-阶段-1a-的退出条件) has not met its exit criteria**: WP6 three-way reconciliation and the WP9 soak are still open. This round's changes are uncommitted in the PR worktree on top of `2d68f5b`.

| Check | Latest result |
|---|---|
| Isolated Python suites on the final code | 266 tests passed, including 9 new R3 reconcile regressions and 4 new trace-reload regressions (the 14 IR03, 7 nullable-failsafe and 6 D6 regressions are unchanged) |
| New regressions against the unfixed code | R3 reconcile 9/9 detected on `2d68f5b` (2 assertion failures, 7 errors from the missing `audit_entry` field); trace reload 4/4 detected (the process test's bridge exits with a traceback) |
| Full runtime/fake-device suite, 31 cases, runtime `52160ab8…` (build `047e7c17ffda`) | 31 PASS; 471 assertions, 0 failed; no setup errors; every verdict has code-tree hash `a0311eb5e8adf1e9877276e7a645f8d0303aca662350a26ea87eccabb56d2e46`, the current tree |
| R3 family (R3, R3T1, R3T1K, R3T1X, R3T2) × 3 passes, same runtime | 15/15 PASS, 261 assertions; ground truth matched EXPECT in every run; the IA2 audit gap reproduced in all 6 R3T1/R3T1X runs |
| R3 family × 3 passes, fixed runtime `b7e455d4…`, same code tree | 9 PASS and 6 FINDING (R3T1 and R3T1X: executed **and** audited), 258 assertions, 0 failed — the expected signal to update `R3_EXPECT` once the runtime fix merges |
| Crash recovery per action | 3 actions passed |
| BOOL fake-device checks | 21 passed (`evidence/bool-device-6/`) |
| Nullable-failsafe mutation controls | All 3 detected by assertion failures |
| Repository gate on the PR worktree (adaptation, format, Clippy, server build, Rust tests, release build, Web build and tests) | All 8 steps passed earlier this round; 8589 Rust tests passed (105 ignored), 423 Web tests passed (39 files). Only Python and docs changed afterwards, so it was not repeated. The release step relinked `target/release/ia2-runtime` (SHA-256 `54df4183…`, kept as `evidence/gate-r3fix-20261029/ia2-runtime.gate-build`); the tested binary `52160ab8…` was restored to that path afterwards |
| Device-free `cs sim run` | Not repeated: no Rust, project or scenario file changed in this PR |

### R3 timings

The bridge's POST times out after 3 s. Each case pauses the runtime at a different point in handling that write:

- **R3**: stopped before it reads the request. The socket's receive queue still holds the request, which is never executed.
- **R3T1**: the runtime has read the request and queued the write, then is stopped during a single 1 s injected scan stall. After the bridge disconnects and the runtime continues, the write **is applied**: the VM holds the value and the fake device receives it. **No `/audit` entry is recorded.**
- **R3T1K** (control): identical, but the proxy keeps the runtime-side connection open. The write is applied and audited.
- **R3T1X**: the same window, but the bridge is SIGKILLed instead. The write is applied without an audit entry. The restarted bridge concludes `unknown / bridge_restarted` without resending.
- **R3T2**: the runtime applies and answers, but the answer is held at the proxy while the runtime is stopped. The entry exists, and the first reconcile look finds it.

Each case checks ground truth from three sources: the fake device's write log, the VM value and `/audit`. A case is a FINDING if IA2 executes or audits differently from the recorded EXPECT.

The cause is in the runtime code, and R3T1K confirms it. `rt_write` records the audit entry only after the queued write is acknowledged. hyper's default `h1_half_close = false` drops the handler when the client disconnects mid-request. The same pattern exists in `rt_force` and `rt_unforce`, which were not run. Fixing this belongs in a separate IA2 runtime change.

### Reconcile semantics now

A `reconcile_note` and the journal `reconcile` record carry `audit_entry`: `found`, `absent`, or `unavailable` (the runtime restarted and `/audit` was cleared). `landed` is `true` only for `found` and `null` otherwise; the bridge never writes `false`. The first look's `absent` schedules the second look, as before. An older journal record with `landed: false` still schedules it. The read side reads an older `landed: false` as `absent`; malformed fields claim nothing. The action status never changes, and nothing is resent. Crash recovery without an entry still concludes `unknown`, and its detail says absence does not show non-execution.

The nine new regressions (`tests/test_r3_reconcile.py`) cover:

- two looks that find no entry
- a found entry
- a restarted runtime
- a first look written by an older bridge
- crash recovery without an entry
- read-side normalization of legacy and malformed notes

All nine fail against the unfixed bridge. In the pre-fix exploratory run (kept outside the repository), the bridge's final note said `landed: false` for writes that had executed, in all three passes of both R3T1 and R3T1X. One earlier integration attempt this round was stopped on purpose at C4 because a lab file was edited mid-run. Its partial evidence is kept in `evidence/r3fix-20261029-e2e-aborted/` and is not counted. Final evidence: `evidence/r3fix-20261029-final2/` (with `summary.json`), `evidence/r3-reconcile-before/`, `evidence/trace-reload-before/`, `evidence/unit/` and `evidence/bool-device-6/`. The earlier all-pass run `evidence/r3fix-20261029-final/` (code tree `d5ed5fba…`) predates the trace fix and the R3T1X change and is superseded.

R3T1X now asserts the path that `/audit` supports at the restart: with no entry, recovery concludes `unknown / bridge_restarted` and the final note is `absent`; with an entry (a runtime that audits a write whose client went away), recovery concludes `accepted` from it (D8), no reconcile note follows and the read side shows `verified`. Either way there is exactly one POST, and `R3_EXPECT` decides whether the run is a FINDING. Before this, the case only covered the first path and reported FAIL on the fixed runtime.

### Trace startup read

A complete trace line the bridge could not have written used to be handled two ways. A line that was not JSON was skipped silently. A line with non-ASCII bytes, or JSON that was not an object, made startup end with an uncaught exception, which is none of the §8.4 exit codes. Now every such line is skipped: non-ASCII bytes, JSON that is not an object, a non-string event ID, and nesting too deep to parse. A skipped line neither suppresses an event nor supplies an anchor, and startup reports its line number on stderr. An anchor needs a non-negative integer `journal_seq` and a string `journal_hash`. Torn tails are handled as before. `tests/test_trace_reload.py` covers this, including the real bridge process.

### Runtime fix (separate branch)

The audit gap is fixed outside this PR, in worktree `~/Projects/soft-PLC/ia2-control-write-detached-20261029` on branch `fix/control-write-detached`. The branch starts from `origin/main` `e6a38aa` and is **not committed**.

Reading the code turned up a worse sibling of the gap. `monitor::write_with_pulse` awaited `start_pulse` in the caller's future. If the caller was dropped while the pulse write was queued, the write still landed but no reset was ever scheduled, so the value stayed latched. Preventing exactly that is the purpose of `pulse_ms`. The IDE server and the edge runtime share this path.

The fix:

- A pulse write and its reset now run in one task of their own.
- The runtime's `/write`, `/force` and `/unforce` run the command and its audit entry in their own task (`run_detached`). A handler panic still reaches the panic layer, and a runtime shutdown answers 503.

Two new tests drop the caller while its command is queued behind an injected stall. Before the fix, the value stayed at 1, and `/audit` stayed empty while x was 7. After the fix both tests pass, and both passed 15 repeated runs.

The repository gate passed with all 8589 previous Rust tests plus the 2 new ones (8591, 0 failed, 105 ignored, run with `--no-fail-fast`). In the gate script's first run, `cargo test` stopped on a `sim_e2e` "server did not come up within 5 s"; the full rerun did not reproduce it. On this runtime the R3 family gives the results in the table above. Report: `~/Projects/soft-PLC/pr99-r3-timing-20261029/RESULT-RUNTIME-FIX.md`.

## Review follow-up before the PR #99 merge — 2026-10-29 UTC+8

Review of `2d68f5b6976d35448084a6c922dcd51a83e4ca3c` reproduced a harness hang on macOS 27.0.1 with Python 3.14.7: R3 stopped the runtime, then the asyncio child watcher blocked the event-loop thread in `waitpid`. The bridge had already recorded `unknown / runtime_unreachable`; the harness could not resume the runtime. That run required terminating its test child and is retained as a failed run, not a pass.

The harness now uses `subprocess.Popen` for the runtime and waits for exit in a worker thread. The same R3 scenario completes in 8.7 seconds on both Python 3.14.7 and 3.9.6, with all its assertions passing. Its original **FINDING remains**: no matching audit entry was observed after resume. The generated note now states that observation without claiming the runtime definitely dropped the write. Stage 1a remains incomplete; its criteria and follow-up list are unchanged.

Normal writes, duplicate suppression, bridge crash recovery before and after sending, and runtime restart checks passed using the real loopback runtime and fake Modbus devices. The existing Python suites passed before the harness-only fix; bridge code was unchanged afterward. The device-free project compiled and its scenario passed, finishing stopped with no forces or alarms. Bundled Tomli source and license bytes were also compared with the PyPI 2.2.1 source archive.

The reused runtime reports build `ab00029af76b`; its Rust sources, dependencies and configuration match `main` at `e6a38aa`. Binary SHA-256: `79bcbe8894aef6221922386c4720b614103a96c0a9174a7cf7c10ec234a2c0d1`. Final bridge/lab source-tree SHA-256: `215bd171447135a6c26e80023b1b33fec20464003dc0e0257a3eec5b06ff375f`. Evidence directories are `evidence/pr99-review-20261029` (including the failed pre-fix run), `evidence/pr99-review-fixed-20261029`, `evidence/pr99-final-20261029`, `evidence/pr99-python39-20261029` and `evidence/pr99-sim-20261029`. No Rust/Web changes required another full repository build or test run. No hardware was contacted.

## Unchanged from the previous round

IR03 publication recovery, the nullable-failsafe gate and the D6 TOML guard are unchanged by this round; their earlier evidence (and the pre-fix reproductions: 44 nullable-failsafe assertion failures, 18 D6 assertion failures, 8 IR03 mutations) remains in [validation-summary.json](validation-summary.json) under the `prior_*` sections. The runtime SHA-256 is still `52160ab8ba63cd16852c66b5647f0a16844bdc45f410c48da2c589c2cc2e74de` (build commit `047e7c17ffda`, Rust sources identical to `main` `e6a38aa`); no Rust or Web source changed. The device-free `cs sim run` was not repeated this round.

D11 event/sample authentication and the real Agent-facing control MCP remain unimplemented. `verified` keeps its local evidence limitations and is not proof of physical effect or causation. All runtime work used loopback only. Hardware timing, wiring, physical effect and power-loss durability remain **BENCH PENDING**. No binaries, deployment keys or personal deployment records are committed. Raw logs and generated projects remain under the gitignored `evidence/` directory.
