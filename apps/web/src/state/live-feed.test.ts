import { beforeEach, afterEach, describe, expect, it, vi } from "vitest"
import {
  ACTION_SNAPSHOT_BASE_BUDGET_MS,
  ACTION_SNAPSHOT_MAX_BUDGET_MS,
  liveFeedStore,
} from "./live-feed"

let now: number
const tick = (scan: number | bigint, timestamp: number | bigint = scan) => ({
  scan_count: scan, timestamp_us: timestamp, vars: [],
}) as Parameters<typeof liveFeedStore.setSnapshot>[0]

beforeEach(() => {
  now = 100
  vi.spyOn(performance, "now").mockImplementation(() => now)
  liveFeedStore.setConnected(false)
  liveFeedStore.setSnapshot(null)
  liveFeedStore.setConnected(true)
})
afterEach(() => vi.restoreAllMocks())

describe("operator-write snapshot freshness", () => {
  it("does not treat an open stream without a snapshot as ready", () => {
    expect(liveFeedStore.getFreshSnapshot()).toBeNull()
  })
  it.each([1, 1n])("accepts JSON and typed advancing counters (%s)", scan => {
    const snap = tick(scan)
    liveFeedStore.setSnapshot(snap)
    expect(liveFeedStore.getFreshSnapshot()).toBe(snap)
    now += ACTION_SNAPSHOT_BASE_BUDGET_MS
    expect(liveFeedStore.getFreshSnapshot()).toBeNull()
    const next = tick(2)
    liveFeedStore.setSnapshot(next)
    expect(liveFeedStore.getFreshSnapshot()).toBe(next)
  })
  it("requires both counters to progress and does not renew duplicate snapshots", () => {
    liveFeedStore.setSnapshot(tick(1, 1))
    now += ACTION_SNAPSHOT_BASE_BUDGET_MS
    liveFeedStore.setSnapshot(tick(1, 1))
    expect(liveFeedStore.getFreshSnapshot()).toBeNull()
    liveFeedStore.setSnapshot(tick(2, 1))
    expect(liveFeedStore.getFreshSnapshot()).toBeNull()
    liveFeedStore.setSnapshot(tick(3, 2))
    expect(liveFeedStore.getFreshSnapshot()).not.toBeNull()
  })
  it("invalidates confirmations on disconnect and waits for post-reconnect data", () => {
    liveFeedStore.setSnapshot(tick(1))
    const generation = liveFeedStore.getGeneration()
    liveFeedStore.setConnected(false)
    liveFeedStore.setSnapshot(tick(2))
    liveFeedStore.setConnected(true)
    expect(liveFeedStore.getGeneration()).not.toBe(generation)
    expect(liveFeedStore.getFreshSnapshot()).toBeNull()
    liveFeedStore.setSnapshot(tick(3))
    expect(liveFeedStore.getFreshSnapshot()).not.toBeNull()
  })
  it("invalidates an old confirmation when a run restarts", () => {
    liveFeedStore.setSnapshot(tick(50))
    const generation = liveFeedStore.getGeneration()
    liveFeedStore.setSnapshot(tick(1))
    expect(liveFeedStore.getGeneration()).not.toBe(generation)
    expect(liveFeedStore.getFreshSnapshot()).not.toBeNull()
  })
  it("invalidates data and confirmations when the project closes", () => {
    liveFeedStore.setSnapshot(tick(1))
    const generation = liveFeedStore.getGeneration()
    liveFeedStore.setSnapshot(null)
    expect(liveFeedStore.getGeneration()).not.toBe(generation)
    expect(liveFeedStore.getFreshSnapshot()).toBeNull()
  })
  it.each([0, NaN, Infinity])("does not trust invalid counter %s", scan => {
    liveFeedStore.setSnapshot(tick(scan))
    expect(liveFeedStore.getFreshSnapshot()).toBeNull()
  })
})

describe("scan-cadence budget", () => {
  // `scan_count` advances once per PLC scan and `interval_ms` has no upper
  // bound, so a fixed window asks the wrong question: on a 5 s-cycle project
  // a 4 s-old value IS the current picture. The window follows the runtime's
  // own observed cadence instead — bounded, and never below the floor.
  let scan: number
  beforeEach(() => { scan = 1 })
  const advanceScans = (count: number, gapMs: number) => {
    for (let i = 0; i < count; i++) {
      now += gapMs
      liveFeedStore.setSnapshot(tick(++scan, scan))
    }
  }

  it("stays at the floor when the runtime scans faster than the floor", () => {
    liveFeedStore.setSnapshot(tick(1, 1))
    advanceScans(5, 100)
    expect(liveFeedStore.getActionBudgetMs()).toBe(ACTION_SNAPSHOT_BASE_BUDGET_MS)
  })

  it("covers a slow project's own cadence instead of refusing between scans", () => {
    liveFeedStore.setSnapshot(tick(1, 1))
    advanceScans(3, 5000)
    expect(liveFeedStore.getActionBudgetMs()).toBe(10_000)
    now += 4000
    expect(liveFeedStore.getFreshSnapshot()).not.toBeNull()
    now += 6100
    expect(liveFeedStore.getFreshSnapshot()).toBeNull()
  })

  it("does not let a single dropped frame widen the window", () => {
    liveFeedStore.setSnapshot(tick(1, 1))
    advanceScans(1, 100)
    advanceScans(1, 9000)
    // Lower median of [100, 9000] is 100 — one hiccup buys no tolerance.
    expect(liveFeedStore.getActionBudgetMs()).toBe(ACTION_SNAPSHOT_BASE_BUDGET_MS)
  })

  it("caps the window so a degrading scan cannot keep buying tolerance", () => {
    liveFeedStore.setSnapshot(tick(1, 1))
    advanceScans(3, 60_000)
    expect(liveFeedStore.getActionBudgetMs()).toBe(ACTION_SNAPSHOT_MAX_BUDGET_MS)
  })

  it("still expires a frozen scan on a slow project", () => {
    liveFeedStore.setSnapshot(tick(1, 1))
    advanceScans(3, 5000)
    // Paused: timestamps keep advancing at the snapshot cadence, scans do not.
    now += 10_100
    liveFeedStore.setSnapshot(tick(scan, scan + 50))
    expect(liveFeedStore.getFreshSnapshot()).toBeNull()
  })

  it.each(["disconnect", "cleared", "restart"])("forgets the cadence on %s", kind => {
    liveFeedStore.setSnapshot(tick(1, 1))
    advanceScans(3, 5000)
    expect(liveFeedStore.getActionBudgetMs()).toBe(10_000)
    if (kind === "disconnect") {
      liveFeedStore.setConnected(false)
      liveFeedStore.setConnected(true)
    }
    if (kind === "cleared") liveFeedStore.setSnapshot(null)
    if (kind === "restart") liveFeedStore.setSnapshot(tick(1, 1))
    // One runtime's slowness must not widen the window for the next one.
    expect(liveFeedStore.getActionBudgetMs()).toBe(ACTION_SNAPSHOT_BASE_BUDGET_MS)
  })
})
