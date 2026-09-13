// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

const runtime = vi.hoisted(() => ({
  currentPou: {
    path: "main", source: "PROGRAM Main END_PROGRAM",
    declarations: [{ name: "Main", type: "program", language: "st" }],
  },
  source: "PROGRAM Main VAR value : BOOL; END_VAR END_PROGRAM",
  isDirty: true,
  isRunning: false,
  diagnostics: [],
  tasks: { tasks: [], programs: [] } as {
    tasks: { name: string; interval_ms: number; priority: number }[]
    programs: { instance: string; program: string; task: string }[]
  },
  setSource: vi.fn(), saveCurrentPou: vi.fn(), run: vi.fn(), stop: vi.fn(), saveTasks: vi.fn(), clearError: vi.fn(),
}))
vi.mock("@/state/runtime", () => ({ useRuntime: () => runtime, usePouSpawnTick: () => 0 }))
vi.mock("@/components/editor/STEditor", () => ({ STEditor: () => <div>ST editor</div> }))
vi.mock("@/components/editor/LDEditor", () => ({ LDEditor: () => null }))
vi.mock("@/components/editor/FBDEditor", () => ({ FBDEditor: () => null }))
vi.mock("@/components/editor/SFCEditor", () => ({ SFCEditor: () => null }))
vi.mock("./DatasheetView", () => ({ DatasheetView: () => null }))
vi.mock("./VariablesPanel", () => ({ VariablesPanel: () => null }))

import { ProgramPane } from "./ProgramPane"

beforeEach(() => {
  vi.clearAllMocks()
  runtime.isRunning = false
  runtime.isDirty = true
  runtime.currentPou.path = "main"
  runtime.currentPou.declarations = [{ name: "Main", type: "program", language: "st" }]
  runtime.tasks = { tasks: [], programs: [] }
  for (const action of [runtime.saveCurrentPou, runtime.run, runtime.stop, runtime.saveTasks]) action.mockResolvedValue(undefined)
})
afterEach(cleanup)

describe("program actions", () => {
  it("keeps save progress visible and blocks overlapping actions until the request finishes", async () => {
    let finish!: () => void
    runtime.saveCurrentPou.mockReturnValue(new Promise<void>((resolve) => { finish = resolve }))
    render(<ProgramPane />)
    fireEvent.click(screen.getByRole("button", { name: "Save" }))
    expect(screen.getByRole("button", { name: "Saving…" }).getAttribute("aria-busy")).toBe("true")
    for (const name of ["Saving…", "Revert", "Run Main", "Add to task"]) {
      expect((screen.getByRole("button", { name }) as HTMLButtonElement).disabled).toBe(true)
    }
    fireEvent.click(screen.getByRole("button", { name: "Saving…" }))
    fireEvent.keyDown(window, { key: "s", ctrlKey: true })
    expect(runtime.saveCurrentPou).toHaveBeenCalledOnce()
    await act(async () => finish())
    expect((screen.getByRole("button", { name: "Save" }) as HTMLButtonElement).disabled).toBe(false)
  })

  it("shows rejected saves without reverting or claiming saved state", async () => {
    runtime.saveCurrentPou.mockRejectedValue(new Error("Disk is read only"))
    render(<ProgramPane />)
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Save" })))
    expect(screen.getByRole("alert").textContent).toContain("Disk is read only")
    expect(screen.getByText("Modified")).toBeTruthy()
    expect(runtime.setSource).not.toHaveBeenCalled()
  })

  it("uses the same save operation for Ctrl/Cmd+S", async () => {
    render(<ProgramPane />)
    await act(async () => fireEvent.keyDown(window, { key: "s", ctrlKey: true }))
    await act(async () => fireEvent.keyDown(window, { key: "S", metaKey: true }))
    expect(runtime.saveCurrentPou).toHaveBeenCalledTimes(2)
  })

  it("runs the file's PROGRAM in isolation and retains explicit revert behavior", async () => {
    render(<ProgramPane />)
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Run Main" })))
    expect(runtime.run).toHaveBeenCalledWith("Main", "main")
    fireEvent.click(screen.getByRole("button", { name: "Revert" }))
    expect(runtime.setSource).toHaveBeenCalledWith(runtime.currentPou.source)
  })

  it("keeps Stop pending until confirmed and shows a rejected stop", async () => {
    let reject!: (error: Error) => void
    runtime.isRunning = true
    runtime.stop.mockReturnValue(new Promise<void>((_, fail) => { reject = fail }))
    render(<ProgramPane />)
    fireEvent.click(screen.getByRole("button", { name: "Stop" }))
    expect(screen.getByRole("button", { name: "Stopping…" }).getAttribute("aria-busy")).toBe("true")
    await act(async () => reject(new Error("Stop unconfirmed")))
    expect(screen.getByRole("alert").textContent).toContain("Stop unconfirmed")
    expect(screen.getByRole("button", { name: "Stop" })).toBeTruthy()
  })

  it("adds to the project task schedule with a unique instance without starting it", async () => {
    runtime.tasks = {
      tasks: [{ name: "fast", interval_ms: 20, priority: 1 }],
      programs: [
        { instance: "Main_inst", program: "Other", task: "fast" },
        { instance: "Main_inst_1", program: "Other", task: "fast" },
      ],
    }
    render(<ProgramPane />)
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Add to task" })))
    expect(runtime.saveTasks).toHaveBeenCalledWith({
      tasks: runtime.tasks.tasks,
      programs: [...runtime.tasks.programs, { instance: "Main_inst_2", program: "Main", task: "fast" }],
    })
    expect(runtime.run).not.toHaveBeenCalled()
  })

  it("keeps library source read only and disables Run without a PROGRAM", () => {
    runtime.currentPou.path = "lib/example/block"
    runtime.currentPou.declarations = [{ name: "Block", type: "function_block", language: "st" }]
    render(<ProgramPane />)
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull()
    expect(screen.queryByRole("button", { name: "Revert" })).toBeNull()
    expect((screen.getByRole("button", { name: "Run" }) as HTMLButtonElement).disabled).toBe(true)
  })
})
