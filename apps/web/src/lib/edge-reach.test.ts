import { describe, expect, it } from "vitest"

import type { EdgeProbe } from "@/types/generated/EdgeProbe"
import { edgeReach } from "./edge-reach"

function probe(patch: Partial<EdgeProbe> = {}): EdgeProbe {
  return {
    reachable: true,
    scan_count: 12n,
    uptime_secs: 30n,
    runtime_version: null,
    fieldbus_healthy: true,
    unhealthy_devices: [],
    watchdog_tripped: false,
    fault: null,
    snapshot_tap: null,
    error: null,
    ...patch,
  }
}

describe("edgeReach", () => {
  it("calls a healthy edge running", () => {
    expect(edgeReach(probe()).kind).toBe("running")
  })

  // A runtime whose program died still answers /health; it showed green.
  it("calls an edge whose program died faulted, with the reason", () => {
    const reach = edgeReach(probe({ fault: "VM trap in main_inst: DivideByZero" }))
    expect(reach.kind).toBe("faulted")
    expect(reach.detail).toContain("VM trap in main_inst: DivideByZero")
  })

  // A latched watchdog showed green too: reachable, buses up, driving nothing.
  it("calls a latched edge locked", () => {
    expect(edgeReach(probe({ watchdog_tripped: true })).kind).toBe("locked")
  })

  it("names the down devices of a degraded edge", () => {
    const reach = edgeReach(probe({ fieldbus_healthy: false, unhealthy_devices: ["coupler"] }))
    expect(reach.kind).toBe("degraded")
    expect(reach.detail).toContain("coupler")
  })

  // The program runs and the buses are up while history and alarms are not
  // being fed; it showed green.
  it("calls an edge whose snapshot task ended degraded, with the last fault", () => {
    const reach = edgeReach(
      probe({
        snapshot_tap: { running: false, faults: 1, last_fault: "the alarms stage panicked on scan 4" },
      }),
    )
    expect(reach.kind).toBe("degraded")
    expect(reach.detail).toContain("history is not recording")
    expect(reach.detail).toContain("the alarms stage panicked on scan 4")
  })

  it("does not call an edge degraded for a tap that is running, or unknown", () => {
    expect(
      edgeReach(probe({ snapshot_tap: { running: true, faults: 3, last_fault: "x" } })).kind,
    ).toBe("running")
    expect(edgeReach(probe({ snapshot_tap: null })).kind).toBe("running")
  })

  it("puts the root cause first", () => {
    const reach = edgeReach(
      probe({ fault: "VM trap in main_inst: X", watchdog_tripped: true, unhealthy_devices: ["c"] }),
    )
    expect(reach.kind).toBe("faulted")
  })

  it("reports an unreachable edge with its error", () => {
    const reach = edgeReach(probe({ reachable: false, error: "ssh: connect refused" }))
    expect(reach).toEqual({ kind: "unreachable", detail: "ssh: connect refused" })
  })
})
