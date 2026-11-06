# Synchronized EtherCAT DC qualification — 2026-11-06

The revised **2 us CPU idle latency limit + full kernel preemption** completed
a **1800.023335709 s disabled-only observation window**. All **903,902 disabled
PDO frames** had adjacent DC slots. The preceding **50 us + full** attempt
failed after **139.727163902 s**. Its earlier passing window is historical
evidence, not continuing acceptance of that profile.

Evidence grade: **TRANSCRIPT in this repository**. The
[machine-readable measurements](data/ethercat-dc-20261106.json) preserve the
recorded results and original artifact hashes. Full packet captures, scheduler
traces and sampled status are retained in the bench archive. Recomputing the
wire claims requires those artifacts; the existing `data/recompute.py` gate
does not validate this new capture. Hostnames, addresses and operator identities
are omitted here. Times below use UTC+8.

## Change and related work

The deployment uses `IA2_EC_MAX_IDLE_LATENCY_US=2`, with the existing full
preemption policy, bus/scan `SCHED_RR` priorities 10/9, and NAPI settings 0/0.
The engine and fixed-disabled PLC program are unchanged. This is an explicit
target profile, not a new generic runtime default. The native worker's PM QoS
request owns the constraint until teardown; the effective limit was 2 us at
both running-profile checks and returned to 2,000,000,000 us after exit.

This host reports C1/C2/C3 exit latencies of 1/18/350 us. A 50 us request still
allows C2. The revised request excludes C2 and C3; usage and residency counters
on all 16 online CPUs remained unchanged for those states over the window.
It can increase idle power, which was not quantified in this run. The existing
2 ms period, DC selection deadlines and first-fault zero-output latch remain
intact. See the [deployment procedure](../edge-deploy.md#opt-in-bus-clocked-scans).

- Related to [issue #65](https://github.com/FREEZONEX/ia2/issues/65), especially
  its [later idle/wakeup experiment](https://github.com/FREEZONEX/ia2/issues/65#issuecomment-6313145630).
  Those short bus-outage experiments identified a similar late-wakeup tail.
  They used a different workload and do not establish this DC qualification.
  This update does **not close #65** or qualify its full bus-loss/load scope.
- [PR #108](https://github.com/FREEZONEX/ia2/pull/108) supplies the steady DC
  continuity latch on which [PR #109](https://github.com/FREEZONEX/ia2/pull/109)
  is stacked.
- [Issue #66](https://github.com/FREEZONEX/ia2/issues/66), closed by
  [PR #67](https://github.com/FREEZONEX/ia2/pull/67), concerns missing device-down
  alarms and stale-input quality. That notification fix does not bound timer
  wakeup latency.

## Failure and revised qualification

| Measurement | 50 us + full, 2026-11-06 | 2 us + full, 2026-11-06 |
|---|---:|---:|
| Healthy observation window | 139.727163902 s; failed | 1800.023335709 s; passed |
| Wire disabled window | 140.441419 s | 1807.870764 s |
| Disabled PDO frames | 70,221 | 903,902 |
| DC slot step at first zero frame | 2, on fault | 1, on normal shutdown |
| Normal-window scan overruns | 0 before the fault; later fault handling overran | 0 |
| Captured TX/RX pairs | 74,885 | 904,166 |
| Capture kernel drops / unpaired frames | 0 / 0 | 0 / 0 |

At **11:20:38.514788**, sequence 70,222 expired its send window: the PLC had
committed 1397 us early, but wakeup was 1056 us late and selection exceeded the
window by 57 us. The trace showed C2 before the timer callback; wakeup to
dispatch was only 14 us. The main delay preceded runnable-task scheduling.
This locates the delay but does not identify a particular firmware mechanism.
The first zero frame skipped a DC slot; 4519 subsequent zero-output frames
covered 9.036986 s. No active output resumed before shutdown.

The revised run started at **11:48:07** and completed at **12:18:17**:

- All 903,901 adjacent comparisons of disabled frames had DC slot step 1.
  TX interval median/p99/max were 2000/2021/2346 us; packet timing is distinct
  from reference-clock continuity.
- All four axes remained disabled; control words were only 0 or 6, with no
  enable feedback or reset commands. Valve command and readback stayed zero.
- 174 initialization SDO uploads and 62 parameter-verification uploads, with
  zero downloads. All 62 values matched the retained parameter baseline.
- 1747 saved status samples were healthy and fresh, with no scan overruns.
  Initial/final process image, effective QoS, priorities, full preemption and
  NAPI readbacks matched the intended profile.
- Normal shutdown retained 119 all-zero PDO frames; the final zero-output
  exchange was acknowledged with WKC 12. Service ended inactive, PID 0,
  without a restart. The known axis3 `0x821B` fault was retained.
- All 28 copied evidence files, totaling 793,034,336 bytes, matched the remote
  archive by SHA-256. The trace ring retains only its final interval; full
  wire capture, idle counter snapshots and sampled status have separate
  coverage.

The engine SHA-256 is
`f6081398882a40c716df60ad0f2b3fe1e08234aed4b13f714bb139c8db98996c`,
built from `dea501cccded2d7bcb58acbcfb2ca1a280282afe` with Rust 1.95.0.
No axis motion, drive reset, parameter download or fault injection was done in
either November 6 field run. The older November 5 6 ms injection belongs to a
separate historical test. The original interactive console remains stopped.

## Supplemental deterministic tests

Seven test groups execute the production gate and DC observer in worker order:

- 10,000 complete, exactly-once handoffs with phase jitter;
- 45 phase/deadline/startup-or-pending-frame boundary combinations;
- discontinuity after output selection, before the next input ticket;
- zero, duplicate, backwards, skipped and wrapped DC samples;
- first-fault preservation and 64 subsequent exchanges without rearming;
- the host deadline inside the phase selection window;
- response/input-copy completion at the input-publication deadline.

The time-control entry point is test-only. These deterministic tests model
ordering and deadlines; physical send time and host wakeup tails require the
separate field evidence above.

| Validation scope | Result |
|---|---|
| Supplemental tests on reviewed base `5988fbd`, commit `dfac3a5` | 167 EtherCAT tests on macOS; 6/6 targeted mutations detected |
| Full quality gates on that reviewed test tree | 8648 Rust tests passed, 105 ignored; clippy, server/release builds, frontend build and 423 frontend tests passed |
| Same test patch on PR base `e266c1f`, commit `f2f8591` | 167 EtherCAT tests on macOS; unchanged test implementation imported into this PR |
| Actual Linux execution on reviewed and PR test trees | 170 EtherCAT tests each; current iocore 14 passed, 1 ignored; total 354 passed |
| New disabled-run admission checks | 5 passed; rejects unsafe/unknown feedback, parameter mismatch, deep-idle entries, topology changes and counter resets |

Linux test executables were cross-built on macOS with Rust 1.95.0 and then
executed on x86_64 Linux, kernel 6.8.0-139-generic, as an unprivileged user
without a physical bus interface. The ignored kernel QoS unit test was not
rerun in that batch; it changes host state. The scheduler test exercised the
permission-denial branch. Successful privileged scheduling and the 2 us lease
were checked separately in the disabled field run.

This is one finite disabled qualification on a specific host and workload.
Motion quality, long-duration reliability, heavy-load behavior and a hard
real-time guarantee remain outside its evidence scope.
