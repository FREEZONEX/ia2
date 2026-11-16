# IA2 axis tuning milestone and roadmap

## V1 offline replay and comparison

IA2 evaluates the drive's closed-loop response under a recorded PLC
trajectory. Vendor auto-tuning can supply the initial drive settings.
Replay makes it possible to compare those settings against the application's
startup, steady motion, reversal, and endpoint requirements.

`POST /api/axis-replay` accepts normalized cyclic feedback, target
positions, and archived run evidence. It checks capture continuity and
drive state before computing metrics. Comparisons require a matched
trajectory, start position, and parameter readback. `cs api` exposes
the same result as the HTTP interface. See the [API contract](api.md#offline-axis-replay).

### Acceptance

| Case | Expected result |
|---|---|
| Single move with a common encoder window | Recompute the population RMS of measured 100 ms speed over the full requested span. |
| Repeated reverse startups | Keep startup peaks and steady RMS separate. Compare only runs with the same preceding conditions and target trajectory. |
| A/B/A reversal trials | Recompute each leg, then compare each direction's group median with both references. Report following bias and endpoint overshoot independently. |
| Missing DC slot with valid WKC | Return `dc_discontinuity`, including the first affected timestamps and missing slot count. The run has no metrics and is excluded from comparisons. |
| Consumed request before enable | When the PLC did not start and the capture shows zero enable commands and feedback, return `pre_enable_abort` with the archived reason. |
| Changed profile or additional parameter | Return an exclusion reason for the comparison. |

The workspace bench archive carries the source captures and the replay
acceptance driver. The driver checks the sealed indexes, decodes PCAP
returns, and submits the measured samples to IA2 through `cs api`. IA2
calculates the metrics independently. The driver compares the results
with the archived per-run and per-leg values.

## Roadmap

| Milestone | Deliverable | Exit evidence |
|---|---|---|
| V1 · Offline replay | JSON contract, local API, per-run analysis, paired and A/B/A comparisons | The archived startup, steady and reversal metrics reproduce. DC gaps and no-start records stay outside parameter scoring. |
| V2 · Read-only capture | A bounded cycle-rate capture on the edge with cycle ID, monotonic time, target and feedback, working counter, input quality, drive state, and end reason. Export the V1 format. | Replay the same motion from the export and a passive wire capture. Inject a missing slot and prove that the capture marks it. Measure scan and bus timing with capture enabled and disabled before adopting the collector. |
| V3 · Assisted comparison | A project-level test recipe with an operator-selected trajectory and limits. An adapter supplies vendor parameter mapping, readback and disabled-state restore. The PLC owns motion and stop supervision. | Simulate the sequence, failed readback, missing feedback and stop paths. On the bench, verify the same recipe under representative load with independent output feedback, then repeat on a second drive with its own scaling. |
| V4 · Parameter recommendations | Rank eligible settings using the application's separate steady, startup and endpoint limits. Record the tested range, independent repeats and conditions for each recommendation. | Repeat candidate trials across representative starts, directions and load states. Demonstrate that protective events and insufficient evidence prevent a recommendation. |

V1 describes observed motor-side response. An estimate of physical
inertia requires an informative excitation and verified torque scaling.
Output-end positioning acceptance requires an independent measurement
at the output.
