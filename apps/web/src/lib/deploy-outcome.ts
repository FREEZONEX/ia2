import type { DeployReport } from "@/types/generated/DeployReport"

/** A deployment result only claims the operating state it observed. */
export type DeployOutcome =
  | { kind: "live"; version: string; detail: string }
  | { kind: "not_running"; version: string; detail: string }
  | { kind: "unmonitored"; version: string; detail: string }
  | { kind: "unprotected"; version: string; detail: string }
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
    // The program may be scanning, but the runtime's history and alarm task is
    // gone: nothing is recorded and no alarm can be raised.
    // The program may run, but the alarm-supervision policy the project declares
    // is not the one the edge's runtime enforces (often an older runtime that
    // ignores the setting).
    if (health.state === "unprotected") {
      return { kind: "unprotected", version: report.version, detail: health.detail }
    }
    if (health.state === "unmonitored") {
      return { kind: "unmonitored", version: report.version, detail: health.detail }
    }
    return { kind: "unconfirmed", version: report.version, detail: health.detail }
  }
  if (report.ok && report.version) {
    return { kind: "unconfirmed", version: report.version, detail: "Server did not report program state" }
  }
  return { kind: "failed" }
}
