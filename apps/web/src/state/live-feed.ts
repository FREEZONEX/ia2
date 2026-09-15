/**
 * High-frequency runtime feed — the last VarSnapshot and the SSE link
 * state — kept OUTSIDE the RuntimeProvider context on purpose.
 *
 * Snapshots arrive at up to tens of Hz while a program runs. Anything
 * carried in the runtime context re-renders all of its consumers on
 * every change, and only four components actually read the snapshot
 * (Monitor + the three graphical editors' live overlays). A plain
 * singleton with `useSyncExternalStore` gives those four a cheap,
 * precise subscription and leaves every dialog, tree and toolbar
 * untouched by the firehose — same pattern as `agentActivityStore`.
 *
 * RuntimeProvider owns the SSE plumbing and WRITES here; components
 * READ via the two hooks. Nothing else should import the store
 * directly for writes.
 */

import { useSyncExternalStore } from "react"

import type { VarSnapshot } from "@/types/generated/VarSnapshot"

type Listener = () => void

/** Floor of the operator-write freshness budget — UI policy, not a PLC
 *  or safety timeout. Fast projects never leave this floor: snapshots fan
 *  out at a fixed 10 Hz regardless of task period, so their observed
 *  advance gap is ~100 ms. */
export const ACTION_SNAPSHOT_BASE_BUDGET_MS = 2000

/** Hard ceiling on the widened budget. The window follows the runtime's
 *  own cadence, which means a DEGRADING scan would otherwise keep buying
 *  itself more tolerance — the failure this check exists to catch. Cap it
 *  so the self-reference stays bounded. */
export const ACTION_SNAPSHOT_MAX_BUDGET_MS = 30_000

/** How many observed scan gaps the budget covers. One would refuse on
 *  ordinary jitter; two tolerates a single late scan and no more. */
export const ACTION_BUDGET_SCAN_MULTIPLE = 2

/** Observed gaps kept for the median. Five lets the lower median outvote
 *  a dropped frame without smearing a real cadence change. */
const CADENCE_SAMPLES = 5

class LiveFeedStore {
  private snapshot: VarSnapshot | null = null
  private connected = false
  private progressedAt: number | null = null
  private scanGaps: number[] = []
  private reportedScanPeriodMs: number | null = null
  private generation = 0
  private snapshotListeners = new Set<Listener>()
  private connectedListeners = new Set<Listener>()

  // Two listener sets so a connectivity flip doesn't wake snapshot
  // subscribers and vice versa — they change at wildly different rates.

  getSnapshot = (): VarSnapshot | null => this.snapshot
  getConnected = (): boolean => this.connected
  getGeneration = (): number => this.generation

  /** The freshness window in force right now. A fixed window asks the
   *  wrong question: `scan_count` advances once per PLC scan, `interval_ms`
   *  has no upper bound, and on a 5 s-cycle project a 4 s-old value IS the
   *  current picture — there is nothing newer to have. So the floor widens
   *  to cover this runtime's own observed cadence, bounded by the cap.
   *
   *  Surface this wherever a widened window could surprise an operator: a
   *  tolerance that grows silently is a silent downgrade. */
  getActionBudgetMs = (): number => {
    const cadence = this.reportedScanPeriodMs ?? this.observedCadenceMs()
    if (cadence == null) return ACTION_SNAPSHOT_BASE_BUDGET_MS
    return Math.min(
      ACTION_SNAPSHOT_MAX_BUDGET_MS,
      Math.max(ACTION_SNAPSHOT_BASE_BUDGET_MS, cadence * ACTION_BUDGET_SCAN_MULTIPLE),
    )
  }

  /** Observed gaps are the FALLBACK, used only until the runtime states its
   *  own period. They are what a stalled stream inflates and what a
   *  degrading scan quietly widens; the reported number is neither, because
   *  it is configuration rather than measurement. */
  private observedCadenceMs(): number | null {
    if (this.scanGaps.length === 0) return null
    const sorted = [...this.scanGaps].sort((a, b) => a - b)
    // LOWER median: with two samples the smaller one wins, so a single
    // dropped frame or SSE hiccup cannot widen the window by itself.
    return sorted[Math.floor((sorted.length - 1) / 2)]
  }

  /** The runtime's own fastest task interval, from `/status`. Authoritative
   *  where it is available; older runtimes send nothing and keep the
   *  observed-cadence fallback. Pass `null` when unknown — never 0, which
   *  would pin the budget to the floor and reintroduce the bug. */
  setScanPeriodMs = (ms: number | null | undefined): void => {
    this.reportedScanPeriodMs =
      typeof ms === "number" && Number.isFinite(ms) && ms > 0 ? ms : null
  }

  /** Read at dispatch time, not from a React render's cached snapshot.
   *  A socket opening is insufficient; an advancing scan must arrive. */
  getFreshSnapshot = (): VarSnapshot | null => {
    const age = this.progressedAt == null ? Infinity : performance.now() - this.progressedAt
    return this.connected && age >= 0 && age < this.getActionBudgetMs()
      ? this.snapshot
      : null
  }

  subscribeSnapshot = (l: Listener): (() => void) => {
    this.snapshotListeners.add(l)
    return () => {
      this.snapshotListeners.delete(l)
    }
  }

  subscribeConnected = (l: Listener): (() => void) => {
    this.connectedListeners.add(l)
    return () => {
      this.connectedListeners.delete(l)
    }
  }

  /** A new connection, a cleared feed or a restarted run is a different
   *  cadence. Never carry the old one across a generation bump — that
   *  would let one runtime's slowness widen the window for the next. */
  private resetCadence(): void {
    this.progressedAt = null
    this.scanGaps = []
    // A different runtime has a different period. Carrying the old one over
    // would widen the window for a target that never claimed it.
    this.reportedScanPeriodMs = null
  }

  /** One interval between observed scan advances. Units faster than the
   *  snapshot cadence share a frame, so this is the gap the freshness
   *  check actually measures, not the declared task period. */
  private recordScanGap(gap: number): void {
    if (!Number.isFinite(gap) || gap <= 0) return
    this.scanGaps.push(gap)
    if (this.scanGaps.length > CADENCE_SAMPLES) this.scanGaps.shift()
  }

  setSnapshot(snap: VarSnapshot | null): void {
    const previous = this.snapshot
    if (!snap) {
      this.resetCadence()
      this.generation++
    } else {
      const scan = Number(snap.scan_count)
      const stamp = Number(snap.timestamp_us)
      const valid = Number.isFinite(scan) && Number.isFinite(stamp) && scan > 0 && stamp > 0
      const restarted = previous != null &&
        (scan < Number(previous.scan_count) || stamp < Number(previous.timestamp_us))
      if (restarted) {
        this.generation++
        this.resetCadence()
      }
      const advanced = previous == null || restarted ||
        (scan > Number(previous.scan_count) && stamp > Number(previous.timestamp_us))
      if (!valid || !this.connected) this.progressedAt = null
      else if (advanced) {
        const at = performance.now()
        // Only measurable against a previous advance in the SAME run; an
        // invalid frame drops one sample rather than the whole cadence.
        if (this.progressedAt != null) this.recordScanGap(at - this.progressedAt)
        this.progressedAt = at
      }
    }
    this.snapshot = snap
    this.snapshotListeners.forEach((l) => l())
  }

  setConnected(up: boolean): void {
    if (this.connected === up) return
    this.connected = up
    this.resetCadence()
    this.generation++
    this.connectedListeners.forEach((l) => l())
  }
}

export const liveFeedStore = new LiveFeedStore()

/** Last VarSnapshot from the running program (server or attached edge),
 *  `null` when nothing has run yet or the project was closed. */
export function useLastSnapshot(): VarSnapshot | null {
  return useSyncExternalStore(
    liveFeedStore.subscribeSnapshot,
    liveFeedStore.getSnapshot,
  )
}

/** Is the SSE event stream to the server currently up? */
export function useConnected(): boolean {
  return useSyncExternalStore(
    liveFeedStore.subscribeConnected,
    liveFeedStore.getConnected,
  )
}
