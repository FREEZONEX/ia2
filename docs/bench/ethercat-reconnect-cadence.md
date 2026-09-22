# EtherCAT outage / reconnect cadence

## Scope and evidence

Issue #65 reported about 0.45% scan-period inflation during a prolonged
partial EtherCAT outage on a non-RT host. The original scheduling cause
is **not established**: async timeout duration is not CPU time, and a
correlation with re-walks does not identify a scheduler or mutex bottleneck.

The code now avoids a narrower, proven waste: a full reset/configuration
walk on a bus whose responding-slave count cannot pass the existing
topology check. Recovery uses EtherCrab's BRD(Type) census first. A missing
or extra slave defers the walk with the unchanged 0/1/2/4/5 s backoff.
No response still reaches the PDU timeout and backoff. Initial connection
is unchanged; the expected count is learned after the first successful
walk even when the configuration does not list slave identities. Count
equality is not identity equality: the existing configured vendor/product
checks, OP transition, health state, and failsafe/watchdog latch remain.

Offline regression commands (no NIC, device, or privilege required):

```sh
cargo test -p iomap-ethercat real::tests::
cargo test -p iomap-ethercat validate::tests::
```

The census tests use the real EtherCrab PDU encoder/parser and synthetic
return frames. They assert that absent/partial/extra topology sends one
read-only census, complete count permits a full walk, and no response
times out without busy-polling. They do **not** measure physical bus
recovery, scan interference, or full walk timing. `cs sim run` uses the
in-memory adapter and cannot replace these wire-path tests or bench timing.

## Bench acceptance — pending

Run only on an authorized, safely isolated bench with a rollback owner;
do not introduce an outage to a production PLC. Record commit/binary hash,
project/task interval, slave topology, kernel, CPU governor, scheduler and
thread placement. Keep those settings identical for the before/after
comparison; do not tune affinity or real-time priority as part of this fix.

For both old and new binaries, record stable online, partial outage, total
outage, and recovery windows. First use the light original workload, then
a representative near-budget workload. Capture `/status` at 1 Hz and the
journal with `iomap_ethercat::real=debug` enabled for the test window.
Retain monotonic elapsed time, `scan_count`, `scan_overruns`,
`consecutive_scan_overruns`, watchdog state, and `device_health`.
Compare deltas over the same window, not cumulative counters alone:

- scan rate = delta scans / elapsed seconds;
- overruns per hour = delta overruns / elapsed seconds * 3600;
- recovery attempt count from `reinits`; successful censuses and full
  walks from `ethercat complete topology observed; starting full re-walk`;
- census wall time from `ethercat re-walk census` / `elapsed_us`, and time
  from restoring the complete topology to healthy live input updates.

While a partial bus reports fewer slaves, acceptance requires **zero full
re-walks** and continued bounded retries. The timing result must also show
whether scan loss and overrun rate approach the online baseline without
delaying recovery beyond the existing backoff. A count-matched wrong
identity must still be refused when that identity is configured; stopping
during outage must remain bounded and a latched watchdog must not re-arm.

If overruns remain, the optimization is not a scheduling fix. Capture a
short `perf sched` / equivalent trace correlating scan-thread runnable vs
running time with the adapter thread, smol TX/RX executor, kernel IRQs and
logging. Report that evidence before changing thread policy. The original
40+ hour hardware cadence claim remains **BENCH PENDING**, not an offline
test pass.
