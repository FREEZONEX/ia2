// @vitest-environment jsdom
//
// The three graphical editors, rendered for real: what their online mode
// shows, and that a source which stops parsing does not take the IDE down.
import { cleanup, render } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

import type { RunningInfo } from "@/state/runtime"
import type { Tasks } from "@/types/generated/Tasks"
import type { VarSnapshot } from "@/types/generated/VarSnapshot"

const live = vi.hoisted(() => ({
  isRunning: false,
  running: null as RunningInfo,
  tasks: { tasks: [], programs: [] } as Tasks,
  snapshot: null as VarSnapshot | null,
}))
vi.mock("@/state/runtime", () => ({
  useRuntime: () => ({
    isRunning: live.isRunning,
    running: live.running,
    tasks: live.tasks,
    projectEpoch: 0,
  }),
}))
vi.mock("@/state/live-feed", () => ({ useLastSnapshot: () => live.snapshot }))
vi.mock("@/lib/api", async (original) => ({
  ...(await original<typeof import("@/lib/api")>()),
  checkProgram: vi.fn().mockResolvedValue([]),
}))

import { FBDEditor } from "./FBDEditor"
import { LDEditor } from "./LDEditor"
import { SFCEditor } from "./SFCEditor"
import { runtimeStringLiteral } from "@/lib/online-vars"

beforeEach(() => {
  live.isRunning = false
  live.running = null
  live.tasks = { tasks: [], programs: [] }
  live.snapshot = null
})
afterEach(cleanup)

const snapshot = (vars: Array<[string, string, string]>): VarSnapshot => ({
  timestamp_us: 1n,
  scan_count: 1n,
  vars: vars.map(([name, type_name, value]) => ({ name, type_name, value, bits: 0 })),
})

const bool = (name: string, section = "internal") => ({ name, type: "BOOL", section, init: null })

/** One rung: `estop` NC contact → `run` coil. */
const ldEstop = (name = "main") =>
  JSON.stringify({
    name,
    pou_type: "program",
    variables: [bool("estop", "input"), bool("run", "output")],
    rungs: [
      {
        id: "r0",
        logic: { op: "contact", var: "estop", negated: true },
        coils: [{ var: "run", kind: "standard" }],
      },
    ],
  })

/** One rung: TON output → `lamp` coil. */
const ldTimer = JSON.stringify({
  name: "main",
  pou_type: "program",
  variables: [bool("start", "input"), bool("lamp", "output")],
  rungs: [
    {
      id: "r0",
      logic: {
        op: "fb_call",
        instance: "dwell",
        fb_type: "TON",
        inputs: [
          { pin: "IN", value: { kind: "var", name: "start" } },
          { pin: "PT", value: { kind: "literal", value: "T#1s" } },
        ],
        output_pin: "Q",
      },
      coils: [{ var: "lamp", kind: "standard" }],
    },
  ],
})

const lit = (container: HTMLElement) => container.querySelectorAll(".stroke-highlight").length
const dead = (container: HTMLElement) =>
  container.querySelectorAll('[class*="stroke-muted-foreground/40"]').length

const runningScheduled = (programs: Array<[string, string]>) => {
  live.isRunning = true
  live.running = { kind: "scheduled", programs: programs.map(([, p]) => p) }
  live.tasks = {
    tasks: [{ name: "t", interval_ms: 10, priority: 1 }],
    programs: programs.map(([instance, program]) => ({ instance, program, task: "t" })),
  }
}

describe("LD online mode", () => {
  it("shows nothing live for a program that is not the one running", () => {
    live.isRunning = true
    live.running = { kind: "isolated", program: "other", filePath: "other" }
    // `other` happens to declare `estop` too — its value is not ours.
    live.snapshot = snapshot([["estop", "BOOL", "FALSE"], ["run", "BOOL", "TRUE"]])
    const { container } = render(<LDEditor value={ldEstop()} onChange={() => {}} path="main" />)
    expect(lit(container)).toBe(0)
    expect(dead(container)).toBe(0)
  })

  it("reads this instance's value when another program shares the name", () => {
    runningScheduled([["main_inst", "main"], ["other_inst", "other"]])
    // e-stop pressed in main; the shared names arrive qualified.
    live.snapshot = snapshot([
      ["main_inst.estop", "BOOL", "TRUE"],
      ["other_inst.estop", "BOOL", "FALSE"],
      ["run", "BOOL", "FALSE"],
    ])
    const { container } = render(<LDEditor value={ldEstop()} onChange={() => {}} path="main" />)
    // The NC contact is open: nothing conducts. The bare lookup used to
    // miss `estop`, read FALSE, and draw the rung live.
    expect(lit(container)).toBe(0)
    expect(dead(container)).toBeGreaterThan(0)
  })

  it("does not draw a timer-driven rung as dead when the timer's output is not published", () => {
    live.isRunning = true
    live.running = { kind: "isolated", program: "main", filePath: "main" }
    // The runtime publishes the instance, never `dwell.Q`.
    live.snapshot = snapshot([["start", "BOOL", "TRUE"], ["lamp", "BOOL", "TRUE"], ["dwell", "TON", "0"]])
    const { container } = render(<LDEditor value={ldTimer} onChange={() => {}} path="main" />)
    expect(dead(container)).toBe(0)
  })

  it("still colours a program it can read", () => {
    live.isRunning = true
    live.running = { kind: "isolated", program: "main", filePath: "main" }
    live.snapshot = snapshot([["estop", "BOOL", "FALSE"], ["run", "BOOL", "TRUE"]])
    const { container } = render(<LDEditor value={ldEstop()} onChange={() => {}} path="main" />)
    expect(lit(container)).toBeGreaterThan(0)
  })
})

describe("FBD online mode", () => {
  const fbd = JSON.stringify({
    name: "main",
    pou_type: "program",
    variables: [bool("start", "input"), bool("done", "output")],
    blocks: [
      {
        id: "b1",
        fb_type: "TON",
        instance: "dwell",
        inputs: [
          { pin: "IN", value: { kind: "var", name: "start" } },
          { pin: "PT", value: { kind: "literal", value: "T#1s" } },
        ],
        position: { x: 40, y: 40 },
      },
    ],
    outputs: [{ variable: "done", from_block: "b1", from_pin: "Q" }],
  })

  it("lights an output pin through the variable it is bound to", () => {
    live.isRunning = true
    live.running = { kind: "isolated", program: "main", filePath: "main" }
    live.snapshot = snapshot([["start", "BOOL", "TRUE"], ["done", "BOOL", "TRUE"], ["dwell", "TON", "0"]])
    const { container } = render(<FBDEditor value={fbd} onChange={() => {}} path="main" />)
    // `dwell.Q` is never published; `done` carries the same value. Every
    // pin and wire used to read FALSE — the whole diagram looked dead.
    expect(lit(container) + container.querySelectorAll(".fill-highlight").length).toBeGreaterThan(0)
    expect(dead(container)).toBe(0)
  })
})

describe("SFC online mode", () => {
  const sfc = (active: string, initial = "空闲") =>
    JSON.stringify({
      name: "batch",
      pou_type: "program",
      variables: [],
      initial_step: initial,
      steps: [
        { name: initial, actions: [] },
        { name: active, actions: [] },
      ],
      transitions: [{ from: initial, to: active, condition: "TRUE" }],
    })

  it("finds the active step by the value the runtime stores for its name", () => {
    runningScheduled([["batch_inst", "batch"], ["other_inst", "other"]])
    // A chart with non-Latin-1 step names runs its state as WSTRING; the
    // snapshot types and values are the ones a live server reports.
    live.snapshot = snapshot([
      ["batch_inst.__sfc_step", "WSTRING", "\"$52A0$6599\""],
      ["other_inst.__sfc_step", "STRING", "'idle'"],
    ])
    const { container } = render(<SFCEditor value={sfc("加料")} onChange={() => {}} path="batch" />)
    // The header badge; transition labels also read "→ 加料".
    const badge = container.querySelector('[class*="bg-highlight/15"]')
    expect(badge?.textContent).toBe("→ 加料")
  })

  it("matches a Latin-1 chart's STRING state, and never across types", () => {
    runningScheduled([["batch_inst", "batch"]])
    // Every step name is Latin-1, so this chart keeps STRING.
    live.snapshot = snapshot([["batch_inst.__sfc_step", "STRING", "'caf$E9'"]])
    const { container, unmount } = render(
      <SFCEditor value={sfc("café", "idle")} onChange={() => {}} path="batch" />,
    )
    expect(container.querySelector('[class*="bg-highlight/15"]')?.textContent).toBe("→ café")
    unmount()

    // The narrow form of 加料 under a WSTRING type is not a match.
    live.snapshot = snapshot([["batch_inst.__sfc_step", "WSTRING", runtimeStringLiteral("加料")]])
    const again = render(<SFCEditor value={sfc("加料")} onChange={() => {}} path="batch" />)
    expect(again.container.querySelector('[class*="bg-highlight/15"]')).toBeNull()
  })
})

describe("a source that stops parsing", () => {
  // Each editor returned its parse-error view early, before further hooks.
  // A source flipping between parseable and not changed the hook count,
  // which React treats as a fatal error — and nothing in the app catches
  // it, so the whole IDE unmounted.
  it.each([
    ["LD", LDEditor, ldEstop()],
    [
      "FBD",
      FBDEditor,
      JSON.stringify({ name: "main", pou_type: "program", variables: [], blocks: [], outputs: [] }),
    ],
    [
      "SFC",
      SFCEditor,
      JSON.stringify({
        name: "s",
        pou_type: "program",
        variables: [],
        initial_step: "a",
        steps: [{ name: "a", actions: [] }],
        transitions: [],
      }),
    ],
  ] as const)("%s shows the error and recovers", (_, Editor, good) => {
    const view = render(<Editor value={good} onChange={() => {}} path="main" />)
    view.rerender(<Editor value="{ not json" onChange={() => {}} path="main" />)
    expect(document.body.textContent).toMatch(/parse|json|invalid/i)
    view.rerender(<Editor value={good} onChange={() => {}} path="main" />)
    expect(document.body.textContent).not.toMatch(/Unexpected token/)
  })
})
