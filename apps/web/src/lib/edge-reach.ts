import type { EdgeProbe } from "@/types/generated/EdgeProbe"
import { outputLatchMessage } from "./output-latch"

/** What the edge pane's badge says about a probe, most severe first. */
export type EdgeReach =
  | { kind: "unreachable"; detail: string }
  | { kind: "faulted"; detail: string }
  | { kind: "locked"; detail: string }
  | { kind: "degraded"; detail: string }
  | { kind: "running"; detail: string }

/**
 * Reachable is not the same as working. A runtime answers `/health` and may
 * keep scanning while its program has died (`fault`), while its watchdog
 * inhibits its normal output phase (`watchdog_tripped`), or while a fieldbus is down
 * (`unhealthy_devices`). Each of those used to show the same green
 * `running` as a healthy edge, except a down bus. The root cause wins:
 * a faulted program explains a latch or a silent bus, not the reverse.
 */
export function edgeReach(probe: EdgeProbe): EdgeReach {
  if (!probe.reachable) return { kind: "unreachable", detail: probe.error ?? "" }
  const latch = outputLatchMessage(probe)
  if (probe.fault) {
    return {
      kind: "faulted",
      detail: `The runtime answers, but its program stopped: ${probe.fault}${latch ? `. ${latch}` : ""}`,
    }
  }
  if (latch) {
    return {
      kind: probe.watchdog_tripped ? "locked" : "degraded",
      detail: latch,
    }
  }
  const down = probe.unhealthy_devices
  if (down.length > 0) {
    return {
      kind: "degraded",
      detail: `Runtime is up, but ${down.length === 1 ? "this device is" : "these devices are"} down (inputs frozen, outputs dropped): ${down.join(", ")}`,
    }
  }
  // The program runs and every bus is up, but the task that feeds history and
  // process alarms from the scan loop has ended: nothing is recorded, no alarm
  // is evaluated, and the scan count is frozen. Only `running: false` is judged
  // here; whether `faults` is still climbing needs two probes, and `cs probe`
  // prints it.
  const tap = probe.snapshot_tap
  if (tap && tap.stalled_for_ms != null) {
    return {
      kind: "degraded",
      detail: `The program is running, but the runtime's snapshot task has been stuck on one snapshot for ${(tap.stalled_for_ms / 1000).toFixed(1)} s (a blocked write, usually): history is not recording, process alarms are not being evaluated and the scan count is frozen`,
    }
  }
  if (tap && !tap.running) {
    return {
      kind: "degraded",
      detail:
        "The program is running, but the runtime's snapshot task has ended: history is not recording, process alarms are not being evaluated and the scan count is frozen. Restart the runtime to recover" +
        (tap.last_fault ? `. Last: ${tap.last_fault}` : ""),
    }
  }
  return { kind: "running", detail: "Edge runtime is responding" }
}
