/**
 * Pure alarm-presentation helpers, shared by the IDE Monitor's alarm
 * section and the HMI `alarmlist` node so the two surfaces classify and
 * format alarms identically (and so the rules are testable offline).
 *
 * ISA-101 discipline lives here: a row is neutral/muted until it is
 * standing, and severity colour only ever escalates to ochre (warn/high
 * context) or red (critical) — never the acid green reserved for agent
 * chrome.
 */

import type { AlarmSeverity } from "@/types/generated/AlarmSeverity"
import type { AlarmState } from "@/types/generated/AlarmState"

/** Tone tokens the alarm rows map to CSS variables:
 *  muted → --muted-foreground, warn → --warn (ochre),
 *  alert → --destructive (red). */
export type AlarmTone = "muted" | "warn" | "alert"

/** "Standing" = still needs an operator's eyes: the condition is active,
 *  or it raised and hasn't been acknowledged. The same predicate the
 *  backend sorts standing-first by. */
export function alarmStanding(a: AlarmState): boolean {
  return a.active || !a.acked
}

/** Number of standing alarms in a list (drives the header/edge chip). */
export function standingCount(alarms: AlarmState[]): number {
  let n = 0
  for (const a of alarms) if (alarmStanding(a)) n++
  return n
}

/** Colour tone for a row. Calm (muted) unless the alarm is standing;
 *  then ochre for warn/high, red for critical. `info` is status, not a
 *  call to action, so it never escalates past muted. */
export function severityTone(sev: AlarmSeverity, standing: boolean): AlarmTone {
  if (!standing) return "muted"
  switch (sev) {
    case "critical":
      return "alert"
    case "high":
    case "warn":
      return "warn"
    case "info":
      return "muted"
  }
}

/** HTTP JSON carries microseconds as numbers; generated types use bigint.
 * Missing, zero and invalid stamps must never look like a raised alarm. */
export function fmtAlarmClock(us: bigint | number | null | undefined): string {
  if (typeof us !== "bigint" && typeof us !== "number") return "—"
  const micros = Number(us)
  if (!Number.isFinite(micros) || micros <= 0) return "—"
  const d = new Date(micros / 1000)
  if (!Number.isFinite(d.getTime())) return "—"
  return d.toLocaleTimeString([], { hour12: false })
}

/** Compact value formatting for `value_at_raise`: integers verbatim,
 *  otherwise trimmed to 3 decimals. Non-finite → em-dash. */
export function fmtAlarmValue(v: number): string {
  if (!Number.isFinite(v)) return "—"
  if (Number.isInteger(v)) return String(v)
  return Number(v.toFixed(3)).toString()
}
