// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { documentVersion, fetchTasks, fetchIomap } from "@/lib/api"
import type { Tasks } from "@/types/generated/Tasks"
import type { IoMap } from "@/types/generated/IoMap"
import { TasksPane } from "./TasksPane"
import { IoMapPane } from "./IoMapPane"

vi.mock("@/lib/api", async (original) => ({
  ...await original<typeof import("@/lib/api")>(),
  fetchProjectPous: vi.fn().mockResolvedValue({ pous: [] }),
  fetchPouVariables: vi.fn().mockResolvedValue([]),
}))
const runtime = vi.hoisted(() => ({
  project: { name: "plant", pous: [], devices: [] },
  tasks: { tasks: [], programs: [] } as Tasks,
  iomap: { mappings: [] } as IoMap,
  saveTasks: vi.fn(), saveIomap: vi.fn(), migrateTasks: vi.fn(),
  isRunning: false, run: vi.fn(), stop: vi.fn(),
}))
vi.mock("@/state/runtime", () => ({ useRuntime: () => runtime }))

beforeEach(() => { vi.clearAllMocks(); runtime.saveTasks.mockResolvedValue(undefined); runtime.saveIomap.mockResolvedValue(undefined) })
afterEach(() => { cleanup(); vi.unstubAllGlobals(); vi.restoreAllMocks() })
const respond = (body: unknown, version: string) => new Response(JSON.stringify(body), { headers: { ETag: version } })
const schedule = (name: string): Tasks => ({ tasks: [{ name, interval_ms: 100, priority: 1 }], programs: [] })

describe("full-document editors", () => {
  it("keeps the task edit and its base after an external refresh and failed save, until explicit reload", async () => {
    const fetch = vi.fn()
      .mockResolvedValueOnce(respond(schedule("original"), '"v1"'))
      .mockResolvedValueOnce(respond(schedule("external"), '"v2"'))
      .mockResolvedValueOnce(respond(schedule("external"), '"v2"'))
    vi.stubGlobal("fetch", fetch)
    runtime.tasks = await fetchTasks()
    const { rerender } = render(<TasksPane />)
    fireEvent.change(screen.getByDisplayValue("original"), { target: { value: "mine" } })
    runtime.tasks = await fetchTasks()
    rerender(<TasksPane />)
    expect(screen.getByDisplayValue("mine")).toBeTruthy()
    runtime.saveTasks.mockRejectedValueOnce(new Error("412"))
    fireEvent.click(screen.getByRole("button", { name: /save changes/i }))
    await waitFor(() => expect(runtime.saveTasks).toHaveBeenCalledOnce())
    const submitted = runtime.saveTasks.mock.calls[0][0] as Tasks
    expect(documentVersion(submitted)).toBe('"v1"')
    expect(submitted.tasks[0].name).toBe("mine")
    expect(screen.getByDisplayValue("mine")).toBeTruthy()
    vi.spyOn(window, "confirm").mockReturnValue(true)
    fireEvent.click(screen.getByRole("button", { name: /changed on disk.*reload/i }))
    await screen.findByDisplayValue("external")
  })

  it("preserves the iomap read version while constructing a new mapping draft", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValueOnce(respond({ mappings: [] }, '"map-v1"')))
    runtime.iomap = await fetchIomap()
    render(<IoMapPane />)
    fireEvent.click(screen.getAllByRole("button", { name: /add mapping/i })[0])
    fireEvent.click(screen.getByRole("button", { name: /save changes/i }))
    await waitFor(() => expect(runtime.saveIomap).toHaveBeenCalledOnce())
    const submitted = runtime.saveIomap.mock.calls[0][0] as IoMap
    expect(documentVersion(submitted)).toBe('"map-v1"')
    expect(submitted.mappings).toHaveLength(1)
  })
})
