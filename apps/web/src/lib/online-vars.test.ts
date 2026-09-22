import { describe, expect, it } from "vitest"

import type { RunningInfo } from "@/state/runtime"
import type { Tasks } from "@/types/generated/Tasks"
import type { VarSnapshot } from "@/types/generated/VarSnapshot"
import {
  onlineBool,
  onlineNumber,
  onlineScope,
  onlineVars,
  runtimeStringLiteral,
} from "./online-vars"

const tasks = (...pairs: Array<[string, string]>): Tasks => ({
  tasks: [{ name: "t", interval_ms: 10, priority: 1 }],
  programs: pairs.map(([instance, program]) => ({ instance, program, task: "t" })),
})
const main = { name: "main", pou_type: "program" as const }
const scope = (running: RunningInfo, t: Tasks = tasks(), path = "main", program = main) =>
  onlineScope({ running, tasks: t, path, program })

describe("onlineScope", () => {
  it("is empty when nothing runs, or the POU is not a program", () => {
    expect(scope(null)).toBeNull()
    expect(
      onlineScope({
        running: { kind: "isolated", program: "main", filePath: "main" },
        tasks: tasks(),
        path: "main",
        program: { name: "main", pou_type: "function_block" },
      }),
    ).toBeNull()
  })

  it("matches an isolated run of this program from this file", () => {
    expect(scope({ kind: "isolated", program: "Main", filePath: "main" })).toEqual({ instance: null })
    // run(program) with no file records the program name as the path
    expect(scope({ kind: "isolated", program: "main", filePath: "main" }, tasks(), "folder/main")).toEqual({
      instance: null,
    })
  })

  it("does not match an isolated run of another program or another file", () => {
    expect(scope({ kind: "isolated", program: "other", filePath: "other" })).toBeNull()
    expect(scope({ kind: "isolated", program: "main", filePath: "copy/main" })).toBeNull()
  })

  it("names the single scheduled instance", () => {
    const t = tasks(["main_inst", "MAIN"], ["o", "other"])
    expect(scope({ kind: "scheduled", programs: ["MAIN", "other"] }, t)).toEqual({ instance: "main_inst" })
    expect(scope({ kind: "remote", edge: "grey0" }, t)).toEqual({ instance: "main_inst" })
  })

  it("is empty when the program is not scheduled, or scheduled twice", () => {
    expect(scope({ kind: "scheduled", programs: ["other"] }, tasks(["o", "other"]))).toBeNull()
    expect(
      scope({ kind: "scheduled", programs: ["main"] }, tasks(["a", "main"], ["b", "main"])),
    ).toBeNull()
  })
})

describe("onlineVars", () => {
  const snap: VarSnapshot = {
    timestamp_us: 0n,
    scan_count: 0n,
    vars: [
      { name: "main_inst.estop", type_name: "BOOL", value: "TRUE", bits: 1 },
      { name: "other_inst.estop", type_name: "BOOL", value: "FALSE", bits: 0 },
      { name: "Level", type_name: "REAL", value: "3.5", bits: 0 },
      { name: "state", type_name: "STRING", value: "'idle'", bits: 0 },
    ],
  }

  it("prefers this instance's qualified name, then the bare one", () => {
    const vars = onlineVars(snap, { instance: "main_inst" })!
    expect(onlineBool(vars, "estop")).toBe(true)
    expect(onlineNumber(vars, "level")).toBe(3.5)
    expect(vars("state")?.value).toBe("'idle'")
  })

  it("returns unknown for stale field input without falling back to another value", () => {
    const stale = { ...snap, vars: snap.vars.map(v => v.name === "main_inst.estop"
      ? { ...v, input: { device: "io", channel: "stop", stale: true } } : v) }
    stale.vars.push({ name: "estop", type_name: "BOOL", value: "FALSE", bits: 0 })
    const vars = onlineVars(stale, { instance: "main_inst" })!
    expect(vars("estop")).toBeUndefined()
    expect(onlineBool(vars, "estop")).toBeNull()
    expect(onlineNumber(vars, "level")).toBe(3.5)
    expect(onlineBool(onlineVars(snap, { instance: "main_inst" })!, "estop")).toBe(true)
  })

  it("reads nothing without a snapshot or a scope", () => {
    expect(onlineVars(null, { instance: null })).toBeNull()
    expect(onlineVars(snap, null)).toBeNull()
  })

  it("answers unknown for a missing or mistyped value", () => {
    const vars = onlineVars(snap, { instance: null })!
    expect(onlineBool(vars, "estop")).toBeNull() // only qualified copies exist
    expect(onlineBool(vars, "level")).toBeNull() // not a BOOL
    expect(onlineNumber(vars, "state")).toBeNull()
  })
})

describe("runtimeStringLiteral", () => {
  // Mirrors ironplc's `encode_string_literal` (low byte per character) and
  // ironplc's `VariableRenderer` narrow-string rendering.
  it.each([
    ["idle", "'idle'"],
    ["等待", "'I$85'"], // U+7B49 → 0x49, U+5F85 → 0x85
    ["café", "'caf$E9'"],
    ["a$b", "'a$$b'"],
    ["it's", "'it$'s'"],
    ["a\tb", "'a$Tb'"],
    ["", "'$01'"],
  ])("%j is shown as %s", (text, shown) => {
    expect(runtimeStringLiteral(text)).toBe(shown)
  })
})
