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
    latch_reason: null,
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

  // The latch is one flag; the reason says who set it. Blaming the scan
  // watchdog for a latch the supervision policy made sends people to the
  // wrong fault.
  it("says why an edge is locked when the runtime says so", () => {
    const reach = edgeReach(
      probe({
        watchdog_tripped: true,
        latch_reason: "alarm supervision lost for 30 s: the snapshot task has stopped",
      }),
    )
    expect(reach.kind).toBe("locked")
    expect(reach.detail).toContain("alarm supervision lost for 30 s")
    expect(reach.detail).not.toContain("Scan watchdog")
    expect(edgeReach(probe({ watchdog_tripped: true })).detail).toContain("Scan watchdog")
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
        snapshot_tap: {
          running: false,
          faults: 1,
          last_fault: "the alarms stage panicked on scan 4",
          stalled_for_ms: null,
          last_fault_age_ms: null,
        },
      }),
    )
    expect(reach.kind).toBe("degraded")
    expect(reach.detail).toContain("history is not recording")
    expect(reach.detail).toContain("the alarms stage panicked on scan 4")
  })

  // A blocked stage neither panics nor lags: running stays true, faults 0.
  it("calls an edge whose snapshot task is stuck degraded", () => {
    const reach = edgeReach(
      probe({
        snapshot_tap: {
          running: true,
          faults: 0,
          last_fault: null,
          stalled_for_ms: 4200,
          last_fault_age_ms: null,
        },
      }),
    )
    expect(reach.kind).toBe("degraded")
    expect(reach.detail).toContain("stuck on one snapshot for 4.2 s")
  })

  it("does not call an edge degraded for a tap that is running, or unknown", () => {
    expect(
      edgeReach(probe({
        snapshot_tap: {
          running: true,
          faults: 3,
          last_fault: "x",
          stalled_for_ms: null,
          last_fault_age_ms: 90000,
        },
      })).kind,
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
