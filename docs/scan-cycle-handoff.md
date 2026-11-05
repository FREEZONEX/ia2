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

Real-time scheduling does not bound the time to leave a deep CPU idle state.
On Linux, `IA2_EC_MAX_IDLE_LATENCY_US=0..1000000` optionally holds a CPU latency
PM QoS request through `/dev/cpu_dma_latency` for the synchronized bus worker's
lifetime. For example, `50` permits idle states with a reported exit latency
up to 50 microseconds. Choose and qualify the value on the target hardware.
The effective aggregate limit is read back before bus initialization; invalid
values, unavailable devices, denied requests or an insufficient readback fail
initialization. Omitting it preserves the OS policy. Unsynchronized and
non-Linux workers do not request it.

The constraint is system-wide while its file is open, can increase idle power,
and is released automatically on worker teardown or process exit. It does not
change CPU affinity, priorities, drive parameters or any cycle deadline, and
does not guarantee wakeup latency against other kernel or firmware delays.
See the Linux [CPU idle PM QoS interface](https://docs.kernel.org/admin-guide/pm/cpuidle.html).

Idle wake latency and runnable-task dispatch latency are different. An idle
constraint does not make kernel code preemptible. On a `PREEMPT_DYNAMIC` host,
verify the selected mode as well as the bus/scan priorities: `voluntary` can
leave a ready realtime thread waiting behind ordinary kernel work. A qualified
deployment may require `full` preemption before it starts the cyclic worker;
that host policy is separate from this runtime's process-owned PM QoS lease.
Preserve wakeup, context-switch and packet timing evidence to distinguish a
late timer, delayed dispatch, lock wait and actual network latency.

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

Pause or step also trips the synchronized run. The common runtime sets the existing
`watchdog_tripped` latch and keeps serving diagnostics; it does not exit/restart on
this fault. The first cause appears in `latch_reason`; adapter safe-value attempts
use the shared `failsafe_status` reporting and panic isolation. An adapter's
acknowledgement does not establish physical output state. Transport recovery or
re-walk cannot clear the gate. Explicit program
restart creates a new gate; it is not an automatic retry of a failed trajectory.
Stopping still invokes the existing failsafe/shutdown sequence. An already handed
off/in-flight bus frame cannot be retracted by a later software stop.

## Observability

`cs get runtime/status`, server `GET /api/runtime/status`, edge `GET /status` and
the existing edge status proxy carry `device_health[].cycle` for clock owners:

| Field | Meaning |
|---|---|
| `period_us` | Requested bus period |
| `issued` | Tickets published after successful exchanges |
| `committed` | Complete frames accepted before their deadlines |
| `consumed` | Frames selected for exchange, not proof of drive application |
| `fault` | First latched reason; null before fault |
| `events` | Last 32 issue/begin/commit/consume/dc_send_expired/fault events |
| `events_dropped` | Earlier events evicted from the bounded ring |

Events have a sequence, `host_us`, and optional `deadline_us`, all relative to one
monotonic host-clock origin. They are not UTC timestamps or EtherCAT DC timestamps.
Status snapshots refresh approximately every 100 ms; read counters as snapshots,
not an atomic live transaction with other status fields. No per-cycle file I/O is
performed. A cycle fault makes the device unhealthy until restart. Independent
scheduler `scan_overruns` is not used to decide synchronized deadlines: the gate
and `watchdog_tripped` report that path.

When the DC worker resumes a whole period late, the runtime journal records
the prior timer entry margin, the time from prior exchange completion to timer
entry, and the current pre-exchange and exchange durations.
An expired ticket publication additionally records the time from exchange
completion to publication.

The first expired DC output-selection window also records its sequence,
previous reference-clock sample, excess over the send limit, wakeup lateness,
time from loop entry to the locked selection check, and prior timer-entry
margin/late time. This covers sub-period misses that do not reach the
whole-period warning threshold. Selection delay includes any lock wait or
preemption between loop entry and the check; a scheduler trace is still needed
to distinguish those causes. The report is emitted after selecting zero outputs
and releasing the gate lock, once per latch. It does not change any deadline.

## Offline proof and hardware boundary

`examples/cycle_handoff` uses only `_sim`, two scalar output fields and a 50 ms
period suitable for ordinary CI hosts. Its independent OS bus thread uses the same
gate as the real adapter. `continuity.toml` proves progress without a watchdog;
`late_frame.toml` injects one 150 ms scan stall and requires the latch to remain set.
The CLI e2e also reads the HTTP event stream and checks consecutive consumed
sequences. Unit tests exercise 10,000 deterministic 2 ms complete frames, old
mailbox repeat/skip behavior, invalid submissions, failsafe persistence, fixed
inputs, and the real adapter's byte packing without opening a NIC.

These tests prove state transitions and data ownership, not host jitter at a 2 ms
hardware deadline. Hardware acceptance still needs actual process scheduling,
wire traces/WKC, DC phase, compute budget and stop behavior under load. A deadline
failure intentionally stops rather than silently holding or skipping a target.
SDO uploads remain read-only but still need mailbox/CPU budget during a synchronized
run; offline proof does not establish that budget on any particular edge host.
