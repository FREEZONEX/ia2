import { beforeEach, afterEach, describe, expect, it, vi } from "vitest"
import { ACTION_SNAPSHOT_MAX_AGE_MS, liveFeedStore } from "./live-feed"

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
    now += ACTION_SNAPSHOT_MAX_AGE_MS
    expect(liveFeedStore.getFreshSnapshot()).toBeNull()
    const next = tick(2)
    liveFeedStore.setSnapshot(next)
    expect(liveFeedStore.getFreshSnapshot()).toBe(next)
  })
  it("requires both counters to progress and does not renew duplicate snapshots", () => {
    liveFeedStore.setSnapshot(tick(1, 1))
    now += ACTION_SNAPSHOT_MAX_AGE_MS
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
