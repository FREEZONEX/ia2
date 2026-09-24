import type { DeployReport } from "@/types/generated/DeployReport"
import type { DeployRollback } from "@/types/generated/DeployRollback"

/** How the edge pane presents a finished deploy. */
export type DeployOutcome =
  /** The new version runs; `detail` is what the health check saw. */
  | { kind: "live"; version: string; detail: string | null }
  /** Its program does not run. Still current unless `rollback.to` names
   *  the version the edge went back to (an edge with `auto_rollback`). */
  | { kind: "not_running"; version: string; detail: string; rollback: DeployRollback | null }
  /** Failed before anything was restarted; the log has the story. */
  | { kind: "failed" }

/**
 * A restart that systemd accepted is not a running program, and a report
 * with `ok: false` is not a live version — the pane used to show every
 * report as "Version … live". `health` says whether the deployed program
 * actually runs; `not_checked` (nothing restarted) still counts as live,
 * with the reason shown.
 */
export function deployOutcome(report: DeployReport): DeployOutcome {
  const health = report.health
  if (report.ok) {
    const detail =
      health == null
        ? null
        : health.state === "not_checked"
          ? `Program state not checked: ${health.detail}`
          : health.detail
    return { kind: "live", version: report.version, detail }
  }
  if (health != null && report.version) {
    return {
      kind: "not_running",
      version: report.version,
      detail: health.detail,
      rollback: report.rollback,
    }
  }
  return { kind: "failed" }
}
