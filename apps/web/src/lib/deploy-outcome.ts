import type { DeployReport } from "@/types/generated/DeployReport"

/** A deployment result only claims the operating state it observed. */
export type DeployOutcome =
  | { kind: "live"; version: string; detail: string }
  | { kind: "not_running"; version: string; detail: string }
  | { kind: "unconfirmed"; version: string; detail: string }
  | { kind: "failed" }

export function deployOutcome(report: DeployReport): DeployOutcome {
  const health = report.health
  if (report.version && health != null) {
    if (report.ok && health.state === "running") {
      return { kind: "live", version: report.version, detail: health.detail }
    }
    if (health.state === "faulted" || health.state === "not_running") {
      return { kind: "not_running", version: report.version, detail: health.detail }
    }
    return { kind: "unconfirmed", version: report.version, detail: health.detail }
  }
  if (report.ok && report.version) {
    return { kind: "unconfirmed", version: report.version, detail: "Server did not report program state" }
  }
  return { kind: "failed" }
}
