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

  // The program may scan, but the runtime's history and alarm task is gone:
  // nothing is recorded and no alarm can be raised. Not live, and not "not
  // running" either.
  it("shows a runtime that cannot raise alarms as unmonitored, not live", () => {
    const outcome = deployOutcome(
      report({
        ok: false,
        health: {
          state: "unmonitored",
          detail: "the runtime's snapshot task has stopped: no alarm is evaluated",
          unhealthy_devices: [],
        },
      }),
    )
    expect(outcome).toEqual({
      kind: "unmonitored",
      version: "2026-09-24T02-00-00Z.abc",
      detail: "the runtime's snapshot task has stopped: no alarm is evaluated",
    })
  })

  // The program runs, but the protection the project declared is not in force
  // on this edge (an older runtime ignores `[supervision]`). Not live.
  it("shows a declared policy the runtime does not enforce as unprotected, not live", () => {
    const outcome = deployOutcome(
      report({
        ok: false,
        health: {
          state: "unprotected",
          detail: "the project declares safe_state, but the runtime predates the setting",
          unhealthy_devices: [],
        },
      }),
    )
    expect(outcome).toEqual({
      kind: "unprotected",
      version: "2026-09-24T02-00-00Z.abc",
      detail: "the project declares safe_state, but the runtime predates the setting",
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

  // A failed restart came back ok:false and was still shown as "Version  live".
  it("shows a deploy that failed before the restart as failed", () => {
    expect(deployOutcome(report({ ok: false, version: "" })).kind).toBe("failed")
  })

  it("shows an unchecked deployment as unconfirmed", () => {
    const outcome = deployOutcome(
      report({
        health: { state: "not_checked", detail: "nothing was restarted", unhealthy_devices: [] },
      }),
    )
    expect(outcome).toMatchObject({
      kind: "unconfirmed",
      detail: "nothing was restarted",
    })
  })
  it("does not mistake a communication timeout for a stopped program", () => {
    expect(deployOutcome(report({ ok: false, health: {
      state: "unknown", detail: "status read timed out", unhealthy_devices: [],
    } }))).toMatchObject({ kind: "unconfirmed", detail: "status read timed out" })
  })

  it("does not infer running from a legacy successful report", () => {
    expect(deployOutcome(report()).kind).toBe("unconfirmed")
  })

})
