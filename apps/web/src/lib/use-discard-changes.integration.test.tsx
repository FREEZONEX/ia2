// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import type { Pou } from "@/types/generated/Pou"
import type { ProjectTree } from "@/types/generated/ProjectTree"

vi.mock("@/lib/api", async (original) => ({
  ...await original<typeof import("@/lib/api")>(),
  fetchProject: vi.fn(), fetchPou: vi.fn(), fetchProjects: vi.fn(), fetchRuntimeStatus: vi.fn(),
  checkProgram: vi.fn(), createProject: vi.fn(), openProject: vi.fn(), closeProject: vi.fn(), createPou: vi.fn(),
}))
vi.mock("@/lib/lsp-client", () => ({
  pouDocumentUri: (path: string) => `file:///${path}.st`,
  LspClient: class { dispose() {} setSource() {} },
}))
import * as api from "@/lib/api"
import { RuntimeProvider, useRuntime } from "@/state/runtime"

const pou: Pou = {
  path: "main", source: "PROGRAM Main END_PROGRAM",
  declarations: [{ name: "Main", type: "program", language: "st" }],
}
const tree: ProjectTree = {
  name: "Current", path: "/current", pous: [pou], pou_folders: [],
  devices: [], device_folders: [], edges: [], edge_folders: [],
  iomap: { mappings: [] }, tasks: { tasks: [], programs: [] },
}
let runtime: ReturnType<typeof useRuntime>
function Probe() { runtime = useRuntime(); return <output>{runtime.source}</output> }

beforeEach(() => {
  vi.clearAllMocks()
  vi.stubGlobal("EventSource", class { close() {} })
  window.history.replaceState(null, "", "/?project=Current")
  vi.mocked(api.fetchProject).mockResolvedValue(tree)
  vi.mocked(api.fetchPou).mockResolvedValue(pou)
  vi.mocked(api.fetchProjects).mockResolvedValue([])
  vi.mocked(api.fetchRuntimeStatus).mockResolvedValue({
    running: false, project: "Current", program_instances: [], devices: [], device_health: [],
    watchdog_tripped: false, scan_period_ms: null, scan_overruns: null, consecutive_scan_overruns: null, scan_count: 0n, last_snapshot_us: 0n, last_error: null,
    running_info: null, mode: null, forces: [],
  })
  vi.mocked(api.checkProgram).mockResolvedValue([])
})
afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
  window.history.replaceState(null, "", "/")
})

async function mounted(dirty = true) {
  const view = render(<RuntimeProvider><Probe /></RuntimeProvider>)
  await waitFor(() => expect(runtime.currentPou?.path).toBe("main"))
  if (dirty) act(() => runtime.setSource("unsaved buffer"))
  vi.clearAllMocks()
  return view
}

describe("runtime discard guard integration", () => {
  const actions = [
    ["switch POU", () => runtime.selectPou("other"), () => api.fetchPou],
    ["create project", () => runtime.createProject("New"), () => api.createProject],
    ["open project", () => runtime.openProject("/other"), () => api.openProject],
    ["close project", () => runtime.closeProject(), () => api.closeProject],
    ["create POU", () => runtime.createPou("new", "program"), () => api.createPou],
  ] as const

  it.each(actions)("cancelling %s preserves URL, buffer and selection without an API call", async (_, action, call) => {
    await mounted()
    let pending!: Promise<unknown>
    act(() => { pending = action() })
    expect(call()).not.toHaveBeenCalled()
    fireEvent.click(screen.getByRole("button", { name: "Keep editing" }))
    await act(async () => { await pending })
    expect(call()).not.toHaveBeenCalled()
    expect(api.fetchProject).not.toHaveBeenCalled()
    expect(runtime.currentPou?.path).toBe("main")
    expect(runtime.source).toBe("unsaved buffer")
    expect(runtime.project?.name).toBe("Current")
    expect(window.location.search).toBe("?project=Current")
  })

  it("loads the requested POU only after explicit discard", async () => {
    await mounted()
    vi.mocked(api.fetchPou).mockResolvedValue({ ...pou, path: "other", source: "other source" })
    let pending!: Promise<void>
    act(() => { pending = runtime.selectPou("other") })
    expect(api.fetchPou).not.toHaveBeenCalled()
    fireEvent.click(screen.getByRole("button", { name: "Discard changes" }))
    await act(async () => { await pending })
    expect(api.fetchPou).toHaveBeenCalledExactlyOnceWith("other")
    expect(runtime.source).toBe("other source")
    expect(runtime.currentPou?.path).toBe("other")
  })

  it("unmount cancels an outstanding project action before its API request", async () => {
    const { unmount } = await mounted()
    let pending!: Promise<boolean>
    act(() => { pending = runtime.openProject("/other") })
    unmount()
    expect(await pending).toBe(false)
    expect(api.openProject).not.toHaveBeenCalled()
  })

  it("does not apply a previous project's delayed POU response after switching projects", async () => {
    await mounted(false)
    let finish!: (value: Pou) => void
    vi.mocked(api.fetchPou).mockReturnValueOnce(new Promise((resolve) => { finish = resolve }))
    let selecting!: Promise<void>
    act(() => { selecting = runtime.selectPou("slow") })
    await act(async () => {})
    vi.mocked(api.openProject).mockResolvedValue({ name: "Other", path: "/other" })
    vi.mocked(api.fetchProject).mockResolvedValue({ ...tree, name: "Other", path: "/other", pous: [] })
    await act(async () => { await runtime.openProject("/other") })
    await act(async () => { finish({ ...pou, path: "slow", source: "old project source" }); await selecting })
    expect(runtime.project?.name).toBe("Other")
    expect(runtime.currentPou).toBeNull()
    expect(runtime.source).toBe("")
    expect(window.location.search).toBe("?project=Other")
  })
})
