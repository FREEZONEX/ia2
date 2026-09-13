import { describe, expect, it } from "vitest"

import type { AlarmState } from "@/types/generated/AlarmState"

import {
  alarmStanding,
  fmtAlarmClock,
  fmtAlarmValue,
  severityTone,
  standingCount,
} from "./alarms"

function alarm(over: Partial<AlarmState>): AlarmState {
  return {
    id: "a",
    severity: "warn",
    message: "m",
    variable: "v",
    active: false,
    acked: true,
    raised_at_us: 0n,
    value_at_raise: 0,
    count: 0,
    ...over,
  }
}

describe("alarmStanding", () => {
  it("is standing while active", () => {
    expect(alarmStanding(alarm({ active: true, acked: true }))).toBe(true)
  })
  it("is standing while unacked, even after returning", () => {
    expect(alarmStanding(alarm({ active: false, acked: false }))).toBe(true)
  })
  it("is calm once returned and acked", () => {
    expect(alarmStanding(alarm({ active: false, acked: true }))).toBe(false)
  })
})

describe("standingCount", () => {
  it("counts only the standing ones", () => {
    expect(
      standingCount([
        alarm({ active: true }),
        alarm({ acked: false }),
        alarm({ active: false, acked: true }),
      ]),
    ).toBe(2)
  })
})

describe("severityTone", () => {
  it("stays muted for any severity when not standing", () => {
    for (const s of ["info", "warn", "high", "critical"] as const) {
      expect(severityTone(s, false)).toBe("muted")
    }
  })
  it("escalates standing warn/high to ochre and critical to red", () => {
    expect(severityTone("warn", true)).toBe("warn")
    expect(severityTone("high", true)).toBe("warn")
    expect(severityTone("critical", true)).toBe("alert")
  })
  it("keeps standing info muted — status, not a call to action", () => {
    expect(severityTone("info", true)).toBe("muted")
  })
})

describe("fmtAlarmClock", () => {
  it("shows an em-dash for never-raised", () => {
    expect(fmtAlarmClock(0n)).toBe("—")
    expect(fmtAlarmClock(0)).toBe("—")
  })
  it("does not turn missing or invalid timestamps into a clock", () => {
    for (const invalid of [null, undefined, NaN, Infinity, -Infinity, -1, -1n, Number.MAX_VALUE]) {
      expect(fmtAlarmClock(invalid)).toBe("—")
    }
  })
  it("renders a HH:MM:SS-shaped time for a real stamp", () => {
    expect(fmtAlarmClock(1_700_000_000_000_000n)).toMatch(/\d{1,2}:\d{2}:\d{2}/)
    expect(fmtAlarmClock(1_700_000_000_000_000)).toBe(fmtAlarmClock(1_700_000_000_000_000n))
  })
})

describe("fmtAlarmValue", () => {
  it("keeps integers verbatim and trims floats", () => {
    expect(fmtAlarmValue(42)).toBe("42")
    expect(fmtAlarmValue(3.14159)).toBe("3.142")
    expect(fmtAlarmValue(1.5)).toBe("1.5")
    expect(fmtAlarmValue(NaN)).toBe("—")
  })
})
