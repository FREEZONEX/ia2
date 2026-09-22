import { describe, expect, it } from "vitest"

import type { HistoryPoint } from "@/types/generated/HistoryPoint"

import {
  historyToSamples,
  MAX_TIMED_HISTORY,
  mergeHistory,
  pushTimedHistory,
  seedTimedBuffer,
  sampleSegments,
  windowSlice,
  type TimedSample,
} from "./var-history"

function fill(
  buf: TimedSample[],
  t0: number,
  n: number,
  dt: number,
  windowS: number,
): TimedSample[] {
  for (let i = 0; i < n; i++) pushTimedHistory(buf, t0 + i * dt, i, windowS)
  return buf
}

describe("pushTimedHistory", () => {
  it("retains by age, not by count", () => {
    // 10 Hz for 60 s into a 30 s window: depth follows the window
    // (~301 samples), not a fixed cap — the old 256-sample trim held
    // ~26 s regardless of the contracted window_s.
    const buf = fill([], 1000, 600, 0.1, 30)
    expect(buf[0].t).toBeGreaterThanOrEqual(buf[buf.length - 1].t - 30)
    expect(buf.length).toBeGreaterThan(256)
    expect(buf.length).toBeLessThanOrEqual(302)
  })

  it("holds the full window at a slow snapshot rate", () => {
    // 1 Hz into a 300 s window: all 300 s stay.
    const buf = fill([], 0, 400, 1, 300)
    expect(buf.length).toBe(301)
    expect(buf[0].t).toBe(99)
  })

  it("enforces the hard cap for wild windows", () => {
    const buf = fill([], 0, MAX_TIMED_HISTORY + 500, 0.1, 86400)
    expect(buf.length).toBe(MAX_TIMED_HISTORY)
  })

  it("trims a hidden-tab gap down to what the window covers", () => {
    const buf = fill([], 0, 100, 1, 60)
    pushTimedHistory(buf, 1000, 42, 60)
    expect(buf.length).toBe(1)
    expect(buf[0]).toEqual({ t: 1000, v: 42 })
  })
})

describe("windowSlice", () => {
  it("slices a narrower per-node window off a shared buffer", () => {
    const buf = fill([], 0, 300, 1, 300)
    const view = windowSlice(buf, 60)
    expect(view[view.length - 1]).toEqual(buf[buf.length - 1])
    expect(view[0].t).toBe(buf[buf.length - 1].t - 60)
  })

  it("returns the buffer untouched when the window covers it", () => {
    const buf = fill([], 0, 50, 1, 300)
    expect(windowSlice(buf, 300)).toBe(buf)
    expect(windowSlice([], 300)).toEqual([])
  })
})

function pt(t_us: number, min: number, max: number, v: number): HistoryPoint {
  return { t_us: BigInt(t_us), min, max, v }
}

describe("historyToSamples", () => {
  it("keeps stale buckets as timed gaps and resumes only at a fresh sample", () => {
    const out = historyToSamples([pt(1_000_000, 3, 9, 7), { ...pt(2_000_000, 3, 9, 7), stale: true }, pt(3_000_000, 4, 8, 6)])
    expect(out[1]).toEqual({ t: 2, v: null, lo: undefined, hi: undefined })
    expect(sampleSegments(out.map(p => p.v))).toEqual([
      [{ index: 0, value: 7 }], [{ index: 2, value: 6 }],
    ])
  })
  it("puts micros onto the seconds axis and carries the band", () => {
    const out = historyToSamples([pt(2_000_000, 3, 9, 7)])
    expect(out).toEqual([{ t: 2, v: 7, lo: 3, hi: 9 }])
  })
})

describe("mergeHistory", () => {
  it("keeps history before the live boundary and drops the overlap", () => {
    const history: TimedSample[] = [
      { t: 0, v: 0 },
      { t: 1, v: 1 },
      { t: 2, v: 2 }, // overlaps live[0] — dropped
      { t: 3, v: 3 }, // newer than live[0] — dropped
    ]
    const live: TimedSample[] = [
      { t: 2, v: 20 },
      { t: 2.5, v: 25 },
    ]
    const merged = mergeHistory(history, live)
    expect(merged.map((p) => p.t)).toEqual([0, 1, 2, 2.5])
    expect(merged.map((p) => p.v)).toEqual([0, 1, 20, 25])
  })

  it("returns a copy of whichever side is empty", () => {
    const live: TimedSample[] = [{ t: 5, v: 5 }]
    expect(mergeHistory([], live)).toEqual(live)
    expect(mergeHistory([], live)).not.toBe(live)
    const history: TimedSample[] = [{ t: 1, v: 1 }]
    expect(mergeHistory(history, [])).toEqual(history)
  })
})

describe("seedTimedBuffer", () => {
  it("prepends history and trims to the display window", () => {
    const history = historyToSamples(
      Array.from({ length: 600 }, (_, i) => pt(i * 1_000_000, i, i, i)),
    )
    const live: TimedSample[] = [{ t: 600, v: 999 }]
    const seeded = seedTimedBuffer(live, history, 60)
    // Newest is the live sample at t=600; window keeps t >= 540.
    expect(seeded[seeded.length - 1]).toEqual({ t: 600, v: 999 })
    expect(seeded[0].t).toBeGreaterThanOrEqual(540)
    expect(seeded[0].lo).toBeDefined()
  })
})
