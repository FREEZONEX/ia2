import { describe, expect, it } from "vitest"

import type { HmiRuntimeState } from "./host"
import {
  ALARM_FAULT_RECENT_MS,
  alarmsBlindReason,
  COMMS_LOST_POLLS,
  derivePanelHealth,
  edgeRuntimeState,
  spanText,
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

  // The operator's question is "how long, and what happens next": the runtime's
  // own counts go on the banner and, for the countdown, on the header chip too.
  it("says how long alarms have been off and when a safe_state project latches the outputs", () => {
    const blind = { ...running, alarmsBlind: "the runtime's snapshot task has stopped" }
    const plain = derivePanelHealth(blind, 0)
    expect(plain.badge).toBe("ALARMS OFF")
    expect(plain.text).not.toContain("lost for")
    expect(plain.text).not.toContain("latches the outputs")

    const report = derivePanelHealth({ ...blind, alarmsLostForMs: 95_400 }, 0)
    expect(report.text).toContain("(lost for 1 min 35 s)")
    expect(report.badge).toBe("ALARMS OFF")
    expect(report.text).not.toContain("latches the outputs")

    const counting = derivePanelHealth(
      { ...blind, alarmsLostForMs: 12_900, outputsLatchInMs: 32_600 },
      0,
    )
    expect(counting.badge).toBe("ALARMS OFF · OUTPUT PHASE INHIBITED IN 32 s")
    expect(counting.text).toContain("(lost for 12 s)")
    expect(counting.text).toContain("inhibits the normal output phase in 32 s unless alarm evaluation returns")
    expect(counting.kind).toBe("alarms_blind")

    const due = derivePanelHealth({ ...blind, alarmsLostForMs: 45_100, outputsLatchInMs: 0 }, 0)
    expect(due.badge).toBe("ALARMS OFF · OUTPUT PHASE INHIBITED NOW")
    expect(due.text).toContain("inhibiting the normal output phase now")
  })

  it("rounds the countdown down: a status that is a poll old errs towards sooner", () => {
    expect(spanText(999)).toBe("0 s")
    expect(spanText(32_999)).toBe("32 s")
    expect(spanText(60_000)).toBe("1 min")
    expect(spanText(95_400)).toBe("1 min 35 s")
    expect(spanText(3_600_000)).toBe("1 h")
    expect(spanText(3_600_000 + 5 * 60_000 + 59_000)).toBe("1 h 5 min")
    expect(spanText(-5)).toBe("0 s")
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
    expect(state.alarm).toContain(
      "Normal output phase inhibited — alarm supervision lost for 30 s: the snapshot task has stopped",
    )
    expect(state.alarm).toContain("output state is unconfirmed")
    const health = derivePanelHealth(state, 0)
    expect(health.kind).toBe("fault")
    expect(health.text).not.toContain("Watchdog")
  })

  it("shows pending and rejected failsafes despite healthy transport", () => {
    const state = edgeRuntimeState({
      mode: { kind: "running" }, watchdog_tripped: true,
      device_health: [{ name: "drive", healthy: true }],
      failsafe_status: {
        pending: ["coupler"], acknowledged: [],
        unconfirmed: [{ device: "drive", error: "failsafe rejected" }],
      },
    })
    const health = derivePanelHealth(state, 0)
    expect(health.tone).toBe("alert")
    expect(health.text).toContain("Pending: coupler")
    expect(health.text).toContain("Unconfirmed: drive (failsafe rejected)")
    expect(health.text).not.toContain("zeroed")
  })

  it("keeps program faults first without hiding failsafe state", () => {
    const state = edgeRuntimeState({
      fault: "VM trap: DivideByZero", watchdog_tripped: true,
      failsafe_status: { pending: ["drive"], acknowledged: [], unconfirmed: [] },
    })
    expect(state.alarm).toMatch(/^VM trap: DivideByZero\./)
    expect(state.alarm).toContain("Pending: drive")
  })

  it("describes successful calls as adapter acknowledgements only", () => {
    const state = edgeRuntimeState({
      watchdog_tripped: true,
      failsafe_status: { pending: [], acknowledged: ["drive"], unconfirmed: [] },
    })
    expect(state.alarm).toContain("Adapter acknowledgements: drive")
    expect(state.alarm).toContain("do not confirm wire or physical outputs")
    expect(derivePanelHealth(state, 0).kind).toBe("fault")
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

  // `latch_in_ms: null` means nothing pending (report, not lost, already
  // latched), and an older runtime omits the whole object.
  it("carries the supervisor's counts from /status, and null when it says none", () => {
    const stopped = tap({ running: false })
    const s = edgeRuntimeState({
      snapshot_tap: stopped,
      supervision: { on_loss: "safe_state", grace_secs: 45, lost_for_ms: 12_000, latch_in_ms: 33_000 },
    })
    expect([s.alarmsLostForMs, s.outputsLatchInMs]).toEqual([12_000, 33_000])
    const report = edgeRuntimeState({
      snapshot_tap: stopped,
      supervision: { on_loss: "report", grace_secs: 30, lost_for_ms: 5_000, latch_in_ms: null },
    })
    expect([report.alarmsLostForMs, report.outputsLatchInMs]).toEqual([5_000, null])
    const older = edgeRuntimeState({ snapshot_tap: stopped })
    expect([older.alarmsLostForMs, older.outputsLatchInMs]).toEqual([null, null])
  })

  // The runtime's supervisor decides when the outputs are latched; the panel's
  // own reading of `snapshot_tap` must not be able to hide that countdown.
  it("shows the banner when the runtime says alarm evaluation is lost even if its own reading does not", () => {
    const calmTap = tap({})
    expect(edgeRuntimeState({ snapshot_tap: calmTap }).alarmsBlind).toBeNull()
    const s = edgeRuntimeState({
      snapshot_tap: calmTap,
      supervision: { on_loss: "safe_state", grace_secs: 30, lost_for_ms: 1_000, latch_in_ms: 29_000 },
    })
    expect(s.alarmsBlind).toContain("runtime reports that alarm evaluation is lost")
    const h = derivePanelHealth(s, 0)
    expect(h.kind).toBe("alarms_blind")
    expect(h.badge).toBe("ALARMS OFF · OUTPUT PHASE INHIBITED IN 29 s")
    // Not lost: `null` is not a banner.
    expect(
      edgeRuntimeState({
        snapshot_tap: calmTap,
        supervision: { on_loss: "safe_state", grace_secs: 30, lost_for_ms: null, latch_in_ms: null },
      }).alarmsBlind,
    ).toBeNull()
  })

  it("rides on the runtime state mapped from /status", () => {
    expect(
      edgeRuntimeState({ snapshot_tap: tap({ running: false }) }).alarmsBlind,
    ).toContain("has stopped")
    expect(edgeRuntimeState({}).alarmsBlind).toBeNull()
  })
})
