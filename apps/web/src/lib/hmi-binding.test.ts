import { describe, expect, it } from "vitest"

import {
  applyMap,
  colorBinding,
  cssColor,
  displayBinding,
  evalExpr,
  formatBinding,
  lookupVar,
  resolveBinding,
  resolveOn,
  toNumber,
} from "./hmi-binding"
import type { VarSnapshot } from "@/types/generated/VarSnapshot"

const snap: VarSnapshot = {
  scan_count: 1n as unknown as bigint,
  time_us: 0n as unknown as bigint,
  vars: [
    { name: "level_pct", type_name: "REAL", value: "42.5" },
    { name: "pump_run", type_name: "BOOL", value: "TRUE" },
    { name: "feeder.speed_rpm", type_name: "INT", value: "1500" },
    { name: "msg", type_name: "STRING", value: "'hello'" },
    { name: "status_w", type_name: "WORD", value: "16#1637" },
    { name: "phase_txt", type_name: "STRING", value: "CARBONATE" },
    // Same bare tail in two program instances — ambiguous on purpose.
    { name: "line_a.temp", type_name: "REAL", value: "20" },
    { name: "line_b.temp", type_name: "REAL", value: "80" },
  ],
} as unknown as VarSnapshot

describe("lookupVar", () => {
  it("rejects stale input values without substituting a healthy same-tail variable", () => {
    const stale: VarSnapshot = { ...snap, vars: snap.vars.map(v =>
      ["line_a.temp", "pump_run"].includes(v.name)
        ? { ...v, input: { device: "io_a", channel: "input", stale: true } } : v) }
    expect(lookupVar(stale, "temp")).toBeNull()
    expect(lookupVar(stale, "line_a.temp")).toBeNull()
    expect(lookupVar(stale, "line_b.temp")?.raw).toBe("80")
    expect(displayBinding(stale, "line_a.temp")).toBeNull()
    expect(resolveOn(stale, "pump_run")).toBe(false)
    expect(lookupVar(snap, "line_a.temp")?.raw).toBe("20") // recovered snapshot
  })
  it("matches exact names and instance tails", () => {
    expect(lookupVar(snap, "level_pct")?.raw).toBe("42.5")
    expect(lookupVar(snap, "speed_rpm")?.raw).toBe("1500")
    expect(lookupVar(snap, "feeder.speed_rpm")?.raw).toBe("1500")
    expect(lookupVar(snap, "ghost")).toBeNull()
    expect(lookupVar(null, "level_pct")).toBeNull()
  })
  it("refuses ambiguous tail matches instead of guessing", () => {
    expect(lookupVar(snap, "temp")).toBeNull()
    expect(lookupVar(snap, "line_a.temp")?.raw).toBe("20")
    expect(lookupVar(snap, "line_b.temp")?.raw).toBe("80")
  })
})

describe("toNumber", () => {
  it("collapses booleans and numerics", () => {
    expect(toNumber("TRUE")).toBe(1)
    expect(toNumber("false")).toBe(0)
    expect(toNumber(" 3.5 ")).toBe(3.5)
    expect(Number.isNaN(toNumber("banana"))).toBe(true)
  })
})

describe("evalExpr", () => {
  it("does arithmetic with precedence", () => {
    expect(evalExpr("x / 100", 250)).toBe(2.5)
    expect(evalExpr("1 + x * 2", 3)).toBe(7)
    expect(evalExpr("(1 + x) * 2", 3)).toBe(8)
    expect(evalExpr("-x + 1", 4)).toBe(-3)
  })
  it("does comparisons, logic and ternary", () => {
    expect(evalExpr("x > 50", 60)).toBe(1)
    expect(evalExpr("x > 50", 40)).toBe(0)
    expect(evalExpr("x >= 10 && x <= 20", 15)).toBe(1)
    expect(evalExpr("x < 0 || x > 100", 50)).toBe(0)
    expect(evalExpr("x > 50 ? 1 : 0", 99)).toBe(1)
    expect(evalExpr("!x", 0)).toBe(1)
  })
  it("rejects everything outside the language", () => {
    expect(evalExpr("y + 1", 0)).toBeNull()
    expect(evalExpr("x(", 0)).toBeNull()
    expect(evalExpr("alert", 0)).toBeNull()
    expect(evalExpr("x; x", 0)).toBeNull()
    expect(evalExpr("", 0)).toBeNull()
  })
})

describe("resolveBinding + formatBinding", () => {
  it("resolves bare-string bindings", () => {
    expect(resolveBinding(snap, "level_pct")).toBe(42.5)
    expect(resolveBinding(snap, "pump_run")).toBe(1)
  })
  it("applies expr and format from spec bindings", () => {
    const b = { variable: "level_pct", expr: "x / 100", format: "%.3f", map: null }
    expect(resolveBinding(snap, b)).toBe(0.425)
    expect(formatBinding(b, 0.425)).toBe("0.425")
  })
  it("formats %d and defaults sanely", () => {
    expect(formatBinding("v", 3)).toBe("3")
    expect(formatBinding("v", 3.14159)).toBe("3.14")
    expect(
      formatBinding({ variable: "v", expr: null, format: "%d", map: null }, 3.7),
    ).toBe("4")
  })
  it("collapses non-finite results to null / em-dash, never NaN text", () => {
    // Divide-by-zero expr, expr over a STRING, plain STRING/WORD vars.
    expect(
      resolveBinding(snap, { variable: "level_pct", expr: "x / 0", format: null, map: null }),
    ).toBeNull()
    expect(
      resolveBinding(snap, { variable: "msg", expr: "x + 1", format: null, map: null }),
    ).toBeNull()
    expect(resolveBinding(snap, "msg")).toBeNull()
    expect(formatBinding("v", Infinity)).toBe("—")
    expect(formatBinding({ variable: "v", expr: null, format: "%.1f", map: null }, NaN)).toBe("—")
  })
})

describe("resolveOn", () => {
  it("reads truthy values as on", () => {
    expect(resolveOn(snap, "pump_run")).toBe(true)
    expect(resolveOn(snap, "level_pct")).toBe(true)
    expect(
      resolveOn(snap, { variable: "level_pct", expr: "x > 50", format: null, map: null }),
    ).toBe(false)
  })
  it("stays off for zero, unresolved and non-numeric values", () => {
    expect(
      resolveOn(snap, { variable: "level_pct", expr: "x * 0", format: null, map: null }),
    ).toBe(false)
    expect(resolveOn(snap, "ghost")).toBe(false)
    expect(resolveOn(snap, "msg")).toBe(false)
    expect(resolveOn(null, "pump_run")).toBe(false)
  })
})

describe("applyMap", () => {
  const entries = [
    { eq: 0, min: null, max: null, out: "STOPPED" },
    { eq: null, min: 80, max: null, out: "alarm" },
    { eq: null, min: 50, max: 80, out: "warn" },
    { eq: null, min: null, max: null, out: "ok" },
  ]
  it("tries entries in order: eq, ranges, catch-all", () => {
    expect(applyMap(entries, 0)).toBe("STOPPED")
    expect(applyMap(entries, 95)).toBe("alarm")
    expect(applyMap(entries, 80)).toBe("alarm")
    expect(applyMap(entries, 79.9)).toBe("warn")
    expect(applyMap(entries, 10)).toBe("ok")
  })
  it("returns null when nothing matches and there is no catch-all", () => {
    expect(applyMap([{ eq: 1, min: null, max: null, out: "x" }], 2)).toBeNull()
  })
})

describe("displayBinding + colorBinding", () => {
  it("map output wins over format", () => {
    const b = {
      variable: "pump_run",
      expr: null,
      format: "%d",
      map: [
        { eq: 1, min: null, max: null, out: "RUNNING" },
        { eq: null, min: null, max: null, out: "STOPPED" },
      ],
    }
    expect(displayBinding(snap, b)).toBe("RUNNING")
  })
  it("%s shows the raw snapshot text (STRING variables)", () => {
    const b = { variable: "phase_txt", expr: null, format: "%s", map: null }
    expect(displayBinding(snap, b)).toBe("CARBONATE")
    // Bare STRINGs display directly too — no %s incantation required.
    expect(displayBinding(snap, "phase_txt")).toBe("CARBONATE")
    expect(displayBinding(snap, "msg")).toBe("hello")
  })
  it("surfaces hex literals and BOOL words instead of NaN", () => {
    expect(displayBinding(snap, "status_w")).toBe("16#1637")
    expect(displayBinding(snap, "pump_run")).toBe("TRUE")
    // An explicit numeric format keeps BOOLs numeric.
    expect(
      displayBinding(snap, { variable: "pump_run", expr: null, format: "%d", map: null }),
    ).toBe("1")
  })
  it("stays null for unresolvable or blown-up values", () => {
    expect(displayBinding(snap, "ghost")).toBeNull()
    expect(displayBinding(null, "level_pct")).toBeNull()
    // An expr declares numeric intent — its failures never fall back to text.
    expect(
      displayBinding(snap, { variable: "msg", expr: "x + 1", format: null, map: null }),
    ).toBeNull()
    expect(
      displayBinding(snap, { variable: "level_pct", expr: "x / 0", format: null, map: null }),
    ).toBeNull()
  })
  it("colors come only from maps", () => {
    expect(colorBinding(snap, "level_pct")).toBeNull()
    const b = {
      variable: "level_pct",
      expr: null,
      format: null,
      map: [{ eq: null, min: 40, max: null, out: "warn" }],
    }
    expect(colorBinding(snap, b)).toBe("warn")
  })
})

describe("cssColor", () => {
  it("maps tokens and passes literals through", () => {
    expect(cssColor("ok")).toBe("var(--highlight)")
    expect(cssColor("alarm")).toBe("var(--destructive)")
    expect(cssColor("#b2ed1d")).toBe("#b2ed1d")
    expect(cssColor("oklch(0.6 0.1 200)")).toBe("oklch(0.6 0.1 200)")
  })
})
