import type { EdgeProbe } from "@/types/generated/EdgeProbe"

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
 * holds every output off (`watchdog_tripped`), or while a fieldbus is down
 * (`unhealthy_devices`). Each of those used to show the same green
 * `running` as a healthy edge, except a down bus. The root cause wins:
 * a faulted program explains a latch or a silent bus, not the reverse.
 */
export function edgeReach(probe: EdgeProbe): EdgeReach {
  if (!probe.reachable) return { kind: "unreachable", detail: probe.error ?? "" }
  if (probe.fault) {
    return {
      kind: "faulted",
      detail: `The runtime answers, but its program stopped: ${probe.fault}`,
    }
  }
  if (probe.watchdog_tripped) {
    return {
      kind: "locked",
      detail:
        "Scan watchdog tripped: outputs are zeroed and held off until the program is restarted",
    }
  }
  const down = probe.unhealthy_devices
  if (down.length > 0) {
    return {
      kind: "degraded",
      detail: `Runtime is up, but ${down.length === 1 ? "this device is" : "these devices are"} down (inputs frozen, outputs dropped): ${down.join(", ")}`,
    }
  }
  return { kind: "running", detail: "Edge runtime is responding" }
}
