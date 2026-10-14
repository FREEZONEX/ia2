/**
 * Run-health derivation for the operator surface — one pure function so
 * the alarmbar and the standalone shell header agree on what the
 * runtime's state means, and so the mapping is testable offline.
 *
 * Priority: comms lost > fault > alarms not evaluated > device link down >
 * paused > running.
 * An unreachable or paused runtime must never present as a green
 * "Running" — the canvas values are frozen at that point, so the calm
 * state would be a lie an operator acts on.
 */

import type { DeviceHealth } from "@/types/generated/DeviceHealth"
import type { RuntimeMode } from "@/types/generated/RuntimeMode"
import type { SnapshotTapHealth } from "@/types/generated/SnapshotTapHealth"

import type { HmiRuntimeState } from "./host"

/** Consecutive failed status polls (2 s cadence) before the panel calls
 *  the runtime unreachable. One miss is a network blip; two is a dead
 *  runtime — keeping the last state green forever is not an option. */
export const COMMS_LOST_POLLS = 2

export type PanelTone = "ok" | "idle" | "warn" | "alert"

/** A fault of the runtime's snapshot task this recent still counts as
 *  "failing now" (the panel polls every 2 s, so this spans a few polls). */
export const ALARM_FAULT_RECENT_MS = 10_000

export type PanelHealth = {
  kind:
    | "running"
    | "stopped"
    | "paused"
    | "degraded"
    | "fault"
    | "unreachable"
    | "alarms_blind"
  tone: PanelTone
  /** Short uppercase label for the shell header chip. */
  badge: string
  /** Full alarmbar line. */
  text: string
}

export function derivePanelHealth(
  state: HmiRuntimeState | null,
  failedPolls: number,
): PanelHealth {
  if (failedPolls >= COMMS_LOST_POLLS) {
    return {
      kind: "unreachable",
      tone: "alert",
      badge: "COMMS LOST",
      text: "COMMS LOST — runtime unreachable, values stale",
    }
  }
  if (state?.alarm) {
    return { kind: "fault", tone: "alert", badge: "FAULT", text: state.alarm }
  }
  // The runtime answers and its program runs, but the task that feeds the alarm
  // engine, the history and the live feed has stopped, is stuck or is failing.
  // The alarm list is quiet then because nothing is looking: shown as an
  // alert, ahead of a device link, since a missing alarm is the quieter lie.
  if (state?.alarmsBlind) {
    return {
      kind: "alarms_blind",
      tone: "alert",
      badge: "ALARMS OFF",
      text: `ALARMS NOT EVALUATED — ${state.alarmsBlind}. Treat the alarm list and live values as stale.`,
    }
  }
  const down = state?.unhealthyDevices ?? []
  if (down.length > 0) {
    return {
      kind: "degraded",
      tone: "warn",
      badge: "DEVICE DOWN",
      text: `${down.join(", ")} link down — inputs frozen`,
    }
  }
  if (state?.mode === "paused" || state?.mode === "step") {
    return {
      kind: "paused",
      tone: "warn",
      badge: "PAUSED",
      text: "Paused — scan loop halted",
    }
  }
  if (state?.running) {
    return {
      kind: "running",
      tone: "ok",
      badge: "",
      text: "Running — no active faults",
    }
  }
  return { kind: "stopped", tone: "idle", badge: "STOPPED", text: "Stopped" }
}

/** The slice of the edge runtime's /status the panel reads. The full
 *  shape is documented in docs/api.md (edge runtime table). */
export type EdgeStatus = {
  project?: string | null
  fault?: string | null
  watchdog_tripped?: boolean
  mode?: RuntimeMode | null
  device_health?: DeviceHealth[] | null
  /** Fastest task interval in ms; absent on an older runtime. */
  scan_period_ms?: number | null
  /** Standing alarm count from the runtime's alarm engine (0 / absent
   *  = none). Drives the shell's compact "N ALARMS" chip. */
  alarms_standing?: number | null
  /** The runtime's snapshot task (feeds alarms, history and the live feed).
   *  Absent on a runtime that predates it, and the newer fields on one that
   *  predates those: absent means unknown, never a problem. */
  snapshot_tap?: Partial<SnapshotTapHealth> | null
}

/** Why alarms are not being evaluated, from the runtime's `snapshot_tap`;
 *  null when they are, or when the runtime does not say. One read is
 *  enough: `last_fault_age_ms` makes "failing now" stateless, so the two
 *  panel polls need not remember each other. */
export function alarmsBlindReason(
  tap: Partial<SnapshotTapHealth> | null | undefined,
): string | null {
  if (!tap) return null
  if (tap.running === false) return "the runtime's snapshot task has stopped"
  if (tap.stalled_for_ms != null) {
    return `the runtime's snapshot task has been stuck for ${(tap.stalled_for_ms / 1000).toFixed(1)} s`
  }
  if (tap.last_fault_age_ms != null && tap.last_fault_age_ms < ALARM_FAULT_RECENT_MS) {
    const seconds = Math.max(1, Math.round(tap.last_fault_age_ms / 1000))
    return `the runtime's snapshot task faulted ${seconds} s ago${tap.last_fault ? ` (${tap.last_fault})` : ""}`
  }
  return null
}

/** Map the edge /status payload to the host seam's runtime state.
 *  `mode == null` (older runtime, no mode field) counts as running so
 *  a version skew degrades to the pre-mode behaviour, not to a false
 *  "Stopped". */
export function edgeRuntimeState(s: EdgeStatus): HmiRuntimeState {
  return {
    running: !s.watchdog_tripped && s.fault == null && (s.mode == null || s.mode.kind === "running"),
    alarm: s.watchdog_tripped ? "Watchdog tripped — outputs are locked" : s.fault ?? null,
    mode: s.mode?.kind,
    scanPeriodMs: s.scan_period_ms ?? null,
    unhealthyDevices: (s.device_health ?? [])
      .filter((d) => !d.healthy)
      .map((d) => d.name),
    alarmsBlind: alarmsBlindReason(s.snapshot_tap),
  }
}
