import { describe, expect, it } from "vitest"

import type { HmiRuntimeState } from "./host"
import {
  ALARM_FAULT_RECENT_MS,
  alarmsBlindReason,
  COMMS_LOST_POLLS,
  derivePanelHealth,
  edgeRuntimeState,
} from "./panel-health"

const running: HmiRuntimeState = {
  running: true,
  alarm: null,
  mode: "running",
  unhealthyDevices: [],
}

describe("derivePanelHealth", () => {
  it("reports a healthy run as green", () => {
    const h = derivePanelHealth(running, 0)
    expect(h.kind).toBe("running")
    expect(h.tone).toBe("ok")
  })

  it("never shows paused as running", () => {
    const h = derivePanelHealth({ ...running, running: false, mode: "paused" }, 0)
    expect(h.kind).toBe("paused")
    expect(h.tone).toBe("warn")
    // The IDE reports running=true while paused; mode still wins.
    expect(derivePanelHealth({ ...running, mode: "paused" }, 0).kind).toBe(
      "paused",
    )
    expect(derivePanelHealth({ ...running, mode: "step" }, 0).kind).toBe(
      "paused",
    )
  })

  it("surfaces a fault over everything but comms loss", () => {
    const h = derivePanelHealth(
      { running: false, alarm: "trap: divide by zero", mode: "paused" },
      0,
    )
    expect(h.kind).toBe("fault")
    expect(h.tone).toBe("alert")
    expect(h.text).toBe("trap: divide by zero")
  })

  // The program runs and the buses are up, but nothing evaluates alarms: a
  // quiet alarm list must not read as "all clear".
  it("never shows a runtime that is not evaluating alarms as a calm run", () => {
    const h = derivePanelHealth(
      { ...running, alarmsBlind: "the runtime's snapshot task has stopped" },
      0,
    )
    expect(h.kind).toBe("alarms_blind")
    expect(h.tone).toBe("alert")
    expect(h.badge).toBe("ALARMS OFF")
    expect(h.text).toContain("ALARMS NOT EVALUATED")
    expect(h.text).toContain("snapshot task has stopped")
  })

  it("ranks alarm blindness below comms loss and a fault, above a device and a pause", () => {
    const blind = { alarmsBlind: "stuck" }
    expect(derivePanelHealth({ ...running, ...blind }, COMMS_LOST_POLLS).kind).toBe("unreachable")
    expect(
      derivePanelHealth({ ...running, ...blind, alarm: "trap: divide by zero" }, 0).kind,
    ).toBe("fault")
    expect(
      derivePanelHealth({ ...running, ...blind, unhealthyDevices: ["plc1"] }, 0).kind,
    ).toBe("alarms_blind")
    expect(derivePanelHealth({ ...running, ...blind, mode: "paused" }, 0).kind).toBe(
      "alarms_blind",
    )
  })

  it("surfaces unhealthy devices with their names", () => {
    const h = derivePanelHealth(
      { ...running, unhealthyDevices: ["plc1", "drive2"] },
      0,
    )
    expect(h.kind).toBe("degraded")
    expect(h.tone).toBe("warn")
    expect(h.text).toContain("plc1, drive2")
    expect(h.text).toContain("inputs frozen")
  })

  it("keeps the last state through a single failed poll", () => {
    expect(derivePanelHealth(running, COMMS_LOST_POLLS - 1).kind).toBe(
      "running",
    )
  })

  it("flips to COMMS LOST after repeated failed polls, green or not", () => {
    const h = derivePanelHealth(running, COMMS_LOST_POLLS)
    expect(h.kind).toBe("unreachable")
    expect(h.tone).toBe("alert")
    expect(derivePanelHealth(null, COMMS_LOST_POLLS).kind).toBe("unreachable")
  })

  it("reports stopped before any state arrives", () => {
    const h = derivePanelHealth(null, 0)
    expect(h.kind).toBe("stopped")
    expect(h.tone).toBe("idle")
  })
})

describe("edgeRuntimeState", () => {
  it("derives running from mode, not just fault", () => {
    const s = edgeRuntimeState({
      project: "p",
      fault: null,
      mode: { kind: "paused" },
      device_health: [],
    })
    expect(s.running).toBe(false)
    expect(s.mode).toBe("paused")
  })

  it("collects only unhealthy device names", () => {
    const s = edgeRuntimeState({
      fault: null,
      mode: { kind: "running" },
      device_health: [
        { name: "plc1", healthy: true },
        { name: "drive2", healthy: false },
      ],
    })
    expect(s.running).toBe(true)
    expect(s.unhealthyDevices).toEqual(["drive2"])
  })

  it("treats a missing mode field (older runtime) as running", () => {
    const s = edgeRuntimeState({ fault: null })
    expect(s.running).toBe(true)
    expect(edgeRuntimeState({ fault: "trap" }).running).toBe(false)
  })
})

describe("edge watchdog status", () => {
  it("does not show an advancing but output-locked runtime as running", () => {
    const status = { fault: null, mode: { kind: "running" as const }, watchdog_tripped: true }
    const state = edgeRuntimeState(status)
    expect(state.running).toBe(false)
    const health = derivePanelHealth(state, 0)
    expect(health.kind).toBe("fault")
    expect(health.text).toContain("Watchdog")
    expect(derivePanelHealth(state, COMMS_LOST_POLLS).kind).toBe("unreachable")
  })
})

describe("edge latch reason", () => {
  it("says why the outputs are latched when the runtime says so, not 'watchdog'", () => {
    const state = edgeRuntimeState({
      fault: null,
      mode: { kind: "running" as const },
      watchdog_tripped: true,
      latch_reason: "alarm supervision lost for 30 s: the snapshot task has stopped",
    })
    expect(state.running).toBe(false)
    expect(state.alarm).toBe(
      "Outputs latched off — alarm supervision lost for 30 s: the snapshot task has stopped",
    )
    const health = derivePanelHealth(state, 0)
    expect(health.kind).toBe("fault")
    expect(health.text).not.toContain("Watchdog")
  })
})

describe("edge scan period", () => {
  it("carries the runtime's own cadence through, and null when absent", () => {
    expect(edgeRuntimeState({ fault: null, scan_period_ms: 5000 }).scanPeriodMs).toBe(5000)
    expect(edgeRuntimeState({ fault: null }).scanPeriodMs).toBeNull()
  })
})

describe("alarmsBlindReason", () => {
  const tap = (patch: object) => ({
    running: true,
    faults: 0,
    last_fault: null,
    stalled_for_ms: null,
    last_fault_age_ms: null,
    ...patch,
  })

  it("names a stopped task, a stuck one and a recent fault", () => {
    expect(alarmsBlindReason(tap({ running: false }))).toContain("has stopped")
    expect(alarmsBlindReason(tap({ stalled_for_ms: 4200 }))).toContain("stuck for 4.2 s")
    const recent = alarmsBlindReason(
      tap({ faults: 2, last_fault: "the alarms stage panicked on scan 9", last_fault_age_ms: 1500 }),
    )
    expect(recent).toContain("faulted 2 s ago")
    expect(recent).toContain("alarms stage panicked")
  })

  // One old incident is history, not a banner; a missing or older runtime is
  // unknown, never a problem.
  it("says nothing about a healthy task, an old fault or an unknown runtime", () => {
    expect(alarmsBlindReason(tap({}))).toBeNull()
    expect(
      alarmsBlindReason(tap({ faults: 1, last_fault: "x", last_fault_age_ms: ALARM_FAULT_RECENT_MS })),
    ).toBeNull()
    expect(alarmsBlindReason(null)).toBeNull()
    expect(alarmsBlindReason(undefined)).toBeNull()
    expect(alarmsBlindReason({ running: true })).toBeNull()
  })

  it("rides on the runtime state mapped from /status", () => {
    expect(
      edgeRuntimeState({ snapshot_tap: tap({ running: false }) }).alarmsBlind,
    ).toContain("has stopped")
    expect(edgeRuntimeState({}).alarmsBlind).toBeNull()
  })
})
