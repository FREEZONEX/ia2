# Bus-clocked PLC scans

`scan_sync = true` on one EtherCAT device replaces independent PLC timer wakeups
with completed bus exchanges. It is opt-in; omitted/false preserves the existing
timer and output mirror behavior. This is an output handoff contract, not an OS
real-time guarantee or a claim that a drive physically applied every command.

```toml
# devices/bus.toml (other identity/channel fields omitted)
scan_sync = true
cycle_us = 2000
dc_sync = "sync0"
```

Every scheduled PLC task must have `interval_ms = 2` for this example. There must
be exactly one clock source, no in-cycle gear on that device, and at least one
effective SYNC0 slave. Mixed task periods are rejected at startup before device
connection. Invalid device options are also reported by project/device validation.
The real synchronized adapter currently requires total discovered PDI <= 1024
bytes, so the expected LRW working counter fits one PDI chunk. Extra devices can
still use their own transport; atomicity applies to the clock-owning EtherCAT
image, not across unrelated fieldbuses. Keep safety logic in the PLC and drives.

```mermaid
sequenceDiagram
    participant Bus as Existing EtherCAT worker
    participant Gate as Bounded handoff
    participant PLC as PLC scan
    Bus->>Gate: Successful OP exchange; latch inputs, issue sequence N + deadline
    Gate->>PLC: Wake scan; fixed input image
    PLC->>PLC: Execute all same-period tasks; stage output fields
    PLC->>Gate: Commit one complete image for N
    Bus->>Gate: Consume N at next bus cycle
    Gate->>Bus: Complete image, or latched zero outputs
```

There is one outstanding ticket and one pending frame. There is no growing queue,
catch-up loop, interpolation, or producer timer in synchronized operation. The
worker selects an exact complete frame before the next exchange. An unarmed gate
sends zero outputs. After arming, a missing frame, late/duplicate/stale commit,
whole-period bus lateness, a missed DC selection window, non-adjacent DC slots,
expired/overlapping issue, bad WKC, exchange failure or
non-OP state latches the gate. Output selection and failsafe are serialized under
one lock; a previously committed frame cannot undo a latched failsafe. Commit,
consume and publication sample their deadline-check time after taking that lock,
so time spent waiting for it counts toward expiry. The real
adapter uses the existing master's DC next-cycle wait; no second master is created.

On Linux, the synchronized worker waits for that deadline using an absolute
`CLOCK_MONOTONIC` sleep with reduced timer slack on that worker. The kernel
dispatches the cycle timer directly. The same worker polls the socket pump
with a local executor while the socket uses the existing reactor for I/O
readiness. A native timer failure trips the gate,
and the worker continues safe-zero exchange using the asynchronous timer.
The worker owns and drains its socket task before a transport replacement or
completed shutdown, including when initialization returns early or unwinds.
An operator can explicitly set `IA2_EC_SYNC_RT_PRIORITY` to 1–20 to run
this one thread under Linux `SCHED_RR` after socket registration, including
startup exchanges. Omit it
for the normal OS scheduling policy. The service needs permission to set
the requested scheduling policy. A rejected request fails initialization.

The producer also needs to run before the ticket deadline. Raising only the
EtherCAT worker does not prevent an ordinary-policy PLC thread from waiting in
the OS run queue past that deadline. `IA2_SCAN_SYNC_RT_PRIORITY=1..20` explicitly
selects Linux `SCHED_RR` for the dedicated synchronized PLC scan thread. It is
applied after I/O, reconnect and diagnostic workers have been created, before
the first `begin_cycle` can arm outputs. Both thread policies are read back.
An invalid or denied PLC request records a runtime fault and executes normal
failsafe/shutdown without starting scans. Timer-driven projects ignore this
option. Omitting it preserves the existing policy.

Choose the bus worker priority above the scan producer (for example bus 10,
scan 9), and verify the actual per-thread policies on the target. These explicit
deployment options do not guarantee a deadline or change the missing-frame
latch; load, blocking, interrupt handling and device latency still require
field acceptance. Neither option modifies drive parameters.

During synchronized startup the worker follows the reference clock while
sending zero outputs. It waits for four adjacent DC slots with all devices
in OP and the expected working counter, then retains the next deadline
and last DC slot when handing the bus clock to the PLC. Before publishing
the first input ticket, it checks that the first cyclic exchange used the
immediately following DC slot and started less than one period after the
retained deadline. A whole-period delay or a non-adjacent slot latches
zero outputs even if the PLC has not started waiting for its first ticket;
later healthy exchanges cannot clear this fault.

The worker retains that check for every steady exchange. Reference-clock time
must increase into exactly the next `dc_ns / period_ns` slot. Zero DC time,
repeated slots, backwards time and skipped slots latch the gate, including when
host lateness is less than a period. The first DC fault includes both raw DC
nanosecond samples; its journal entry also records the issued sequence and host
lateness. DC samples are from the returned FRMW, not host capture timestamps.
The half-period SYNC0 shift sets the intended transmit phase inside each slot;
it does not change this reference-clock slot numbering.

Before output selection, the gate checks a conservative host estimate of the end
of the next DC slot, after acquiring its mutex. For a sample in slot N, this
limit is `sample_host_start + 2 * period - dc_phase`. Expiry emits a
`dc_send_expired` event with the limit in `deadline_us` and selects zero outputs,
even before the first ticket. The host sample precedes the reference-clock read,
so this limit can fail early; it is not a guarantee of the physical send time.
Preemption after selection may still delay a frame. A discontinuity discovered
from its response prevents the next ticket and selects zero for the next
exchange; software cannot retract the frame already sent. The check does not
apply to unsynchronized DC or free-run adapters and never queues catch-up frames.

The input image is fixed for the entire scan. Output channel writes affect a
private staging image until the whole scan commits. Channel errors fail the
synchronized scan closed. Direct adapter writes outside an active scan are refused;
HTTP variable writes still enter the VM and are processed by subsequent scans.

Ordinary runtime commands use a bounded FIFO. A full queue rejects new debug
commands with HTTP 429. Each scan round applies at most 16 commands and, for
bus-clocked runs, stops admitting work once it has spent one eighth of a
period on commands. It admits one command for progress even at short periods.
Stop and output inhibition have separate paths. A pulse duration starts at
VM application. Each round has a separate budget of up to 16 pulse resets,
earliest expiry first, including while paused. A reset is eligible when the
round's command phase starts at or after expiry. An accepted write or force
to the same resolved VM variable can supersede it. The scan samples monitor
and RETAIN values. Separate bounded workers format monitor snapshots and
write RETAIN checkpoints, so a slow formatter or storage device
cannot make the scan wait for a completed operation. A full monitor mailbox
skips that sample. The RETAIN writer keeps the newest pending checkpoint and
is drained after device teardown.

In a bus-clocked run, pause, step and missed handoffs latch normal outputs off.
The first cause appears in `latch_reason`. Each adapter keeps sole ownership of its transport
while its independent safe-value attempt runs. The scan can resume in a
latched observation mode. Safe-value and teardown workers are created from
the ordinary I/O runtime and do not inherit the scan's optional real-time
priority. Inputs from an adapter are marked stale while its
attempt owns it. Modbus discards queued ordinary writes after the fence. A
previously admitted write may still start or finish after the fence. The same
owner then starts the safe-value sweep. Timer-driven EtherCAT selects an
all-zero image at the worker's admission point after the fence, including
gear outputs.
Adapter acknowledgement has the scope documented in [edge deployment](edge-deploy.md#shutdown-and-failsafe-evidence).
The latch survives transport recovery and re-walk. Each adapter proceeds to
final cleanup independently. Explicit restart creates a new gate after all
ownership from the previous run has drained.

## Observability

`cs get runtime/status`, server `GET /api/runtime/status`, edge `GET /status` and
the existing edge status proxy carry `device_health[].cycle` for clock owners:

| Field | Meaning |
|---|---|
| `period_us` | Requested bus period |
| `issued` | Tickets published after successful exchanges |
| `committed` | Complete frames accepted before their deadlines |
| `consumed` | Frames selected for exchange, not proof of drive application |
| `handoff_timing` | Optional timing extrema for all `committed` frames since this gate was created |
| `fault` | First latched reason; null before fault |
| `events` | Last 32 issue/begin/commit/consume/dc_send_expired/fault events |
| `events_dropped` | Earlier events evicted from the bounded ring |
| `host_mono_origin_ns`, `host_mono_uncertainty_ns` | Optional Linux CLOCK_MONOTONIC origin and calibration bracket width in ns |

Events have a sequence, `host_us`, and optional `deadline_us`, relative to one
monotonic host-clock origin. On Linux, add `host_mono_origin_ns` to `host_us × 1000`
to align with scan trace and scheduler data. The calibration width and
microsecond quantization bound the timestamp mapping. EtherCAT DC uses a
separate clock. The gate distinguishes a scan that never began, one that
began without a commit, and a bus that missed its selection deadline. A
first fault records the active ticket deadline when available. Status
refreshes approximately every 100 ms and may retain its previous cycle
sample during lock contention.

`handoff_timing` is absent until the first accepted commit and on older
runtimes. All four values use host monotonic time in microseconds.

| Field | Meaning |
|---|---|
| `max_issue_to_begin_us` | Longest elapsed time from ticket issue to scan begin |
| `max_begin_to_commit_us` | Longest elapsed time from scan begin to accepted commit |
| `min_begin_margin_us` | Smallest time remaining at scan begin before that ticket's deadline |
| `min_commit_margin_us` | Smallest time remaining at commit before that ticket's deadline |

The gate updates these extrema on every accepted commit, independently of
event retention. `committed` is the sample count. Use the event ring to
inspect recent sequence details and the fault to identify failed handoffs.

The gate and `watchdog_tripped` report synchronized deadline failures.
`scan_overruns` counts timer-driven deadlines. After a synchronized fault,
the observation timer starts from a fresh anchor. Safe-value processing and
the abandoned bus deadline stay separate from observation overrun accounting.

When the DC worker resumes a whole period late, the runtime journal records
the prior timer entry margin, the time from prior exchange completion to timer
entry, and the current pre-exchange and exchange durations.
An expired ticket publication additionally records the time from exchange
completion to publication.

## Offline proof and hardware boundary

`examples/cycle_handoff` uses only `_sim`, two scalar output fields and a 50 ms
period suitable for ordinary CI hosts. Its independent OS bus thread uses the same
gate as the real adapter. `continuity.toml` proves progress without a watchdog;
`late_frame.toml` injects one 150 ms scan stall and requires the latch to remain set.
The CLI e2e also reads the HTTP event stream and checks consecutive consumed
sequences. Unit tests exercise 10,000 deterministic 2 ms complete frames, old
mailbox repeat/skip behavior, invalid submissions, failsafe persistence, fixed
inputs, and the real adapter's byte packing without opening a NIC.

These tests exercise state transitions and data ownership. Hardware acceptance
at a 2 ms deadline needs process scheduling, wire traces/WKC, DC phase, compute
budget and stop behavior under representative load. Use `handoff_timing` for
extrema from gate creation through the `committed` count in that status sample.
Status polling samples the event ring. Use a complete timestamp trace with
verified coverage for extrema within a specific load interval of an existing run.
Check gate faults and wire evidence separately, including the transition to
safe output and fresh device feedback. SDO uploads remain read-only and still
need mailbox and CPU budget during a synchronized run.
