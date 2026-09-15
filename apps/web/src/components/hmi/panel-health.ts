/**
 * Run-health derivation for the operator surface — one pure function so
 * the alarmbar and the standalone shell header agree on what the
 * runtime's state means, and so the mapping is testable offline.
 *
 * Priority: comms lost > fault > device link down > paused > running.
 * An unreachable or paused runtime must never present as a green
 * "Running" — the canvas values are frozen at that point, so the calm
 * state would be a lie an operator acts on.
 */

import type { DeviceHealth } from "@/types/generated/DeviceHealth"
import type { RuntimeMode } from "@/types/generated/RuntimeMode"

import type { HmiRuntimeState } from "./host"

/** Consecutive failed status polls (2 s cadence) before the panel calls
 *  the runtime unreachable. One miss is a network blip; two is a dead
 *  runtime — keeping the last state green forever is not an option. */
export const COMMS_LOST_POLLS = 2

export type PanelTone = "ok" | "idle" | "warn" | "alert"

export type PanelHealth = {
  kind: "running" | "stopped" | "paused" | "degraded" | "fault" | "unreachable"
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
  /** Standing alarm count from the runtime's alarm engine (0 / absent
   *  = none). Drives the shell's compact "N ALARMS" chip. */
  alarms_standing?: number | null
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
    unhealthyDevices: (s.device_health ?? [])
      .filter((d) => !d.healthy)
      .map((d) => d.name),
  }
}
