import { describe, expect, it } from "vitest"

import type { DeployReport } from "@/types/generated/DeployReport"
import { deployOutcome } from "./deploy-outcome"

function report(patch: Partial<DeployReport> = {}): DeployReport {
  return {
    ok: true,
    version: "2026-09-24T02-00-00Z.abc",
    log: "",
    warning: null,
    health: null,
    rollback: null,
    ...patch,
  }
}

describe("deployOutcome", () => {
  it("shows a running program as live, with what the check saw", () => {
    const outcome = deployOutcome(
      report({
        health: {
          state: "running",
          detail: "the program is running (57 scans)",
          unhealthy_devices: [],
        },
      }),
    )
    expect(outcome).toEqual({
      kind: "live",
      version: "2026-09-24T02-00-00Z.abc",
      detail: "the program is running (57 scans)",
    })
  })

  // The restart succeeded and the program trapped at once: not live.
  it("shows an installed version whose program faulted as not running", () => {
    const outcome = deployOutcome(
      report({
        ok: false,
        health: {
          state: "faulted",
          detail: "the program stopped: VM trap in main_inst: DivideByZero",
          unhealthy_devices: [],
        },
      }),
    )
    expect(outcome.kind).toBe("not_running")
    if (outcome.kind === "not_running") {
      expect(outcome.detail).toContain("DivideByZero")
    }
  })

  it("carries the automatic rollback of a version whose program did not run", () => {
    const outcome = deployOutcome(
      report({
        ok: false,
        health: { state: "faulted", detail: "the program stopped: X", unhealthy_devices: [] },
        rollback: {
          to: "2026-09-24T01-00-00Z.good",
          detail: "rolled back to 2026-09-24T01-00-00Z.good: the program is running (40 scans)",
          health: null,
        },
      }),
    )
    expect(outcome.kind).toBe("not_running")
    if (outcome.kind === "not_running") {
      expect(outcome.rollback?.to).toBe("2026-09-24T01-00-00Z.good")
    }
  })

  // A failed restart came back ok:false and was still shown as "Version  live".
  it("shows a deploy that failed before the restart as failed", () => {
    expect(deployOutcome(report({ ok: false, version: "" })).kind).toBe("failed")
  })

  it("keeps a deploy with nothing restarted live, saying nothing was checked", () => {
    const outcome = deployOutcome(
      report({
        health: { state: "not_checked", detail: "nothing was restarted", unhealthy_devices: [] },
      }),
    )
    expect(outcome).toMatchObject({
      kind: "live",
      detail: "Program state not checked: nothing was restarted",
    })
  })
})
