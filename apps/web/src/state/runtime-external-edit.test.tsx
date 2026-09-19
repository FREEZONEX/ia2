// @vitest-environment jsdom
//
// The open program changes on disk (an agent's `cs set pous/…`, a second
// IDE window) while this window holds unsaved edits to it. The per-POU
// subscription deliberately skips its silent reload when the buffer is
// dirty — it used to hand over to a "Reload" toast that was later removed,
// so nothing recorded the change at all, and the next Save, Revert-then-
// edit, or Run (which saves first) wrote the stale buffer over the other
// writer's version without a word. Run then compiled and started it.
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import type { Pou } from "@/types/generated/Pou"
import type { ProjectTree } from "@/types/generated/ProjectTree"

vi.mock("@/lib/api", async (original) => ({
  ...await original<typeof import("@/lib/api")>(),
  fetchProject: vi.fn(), fetchPou: vi.fn(), fetchProjects: vi.fn(), fetchRuntimeStatus: vi.fn(),
  checkProgram: vi.fn(), savePou: vi.fn(), runProgram: vi.fn(),
}))
vi.mock("@/lib/lsp-client", () => ({
  pouDocumentUri: (path: string) => `file:///${path}.st`,
  LspClient: class { dispose() {} setSource() {} },
}))
import * as api from "@/lib/api"
import { invalidationBus, Topic } from "@/state/invalidation"
import { RuntimeProvider, useRuntime } from "@/state/runtime"

const ORIGINAL = "PROGRAM Main END_PROGRAM"
const THEIRS = "PROGRAM Main (* agent: interlock *) END_PROGRAM"
const MINE = "PROGRAM Main (* human edit *) END_PROGRAM"

const pou = (source: string): Pou => ({
  path: "main", source,
  declarations: [{ name: "Main", type: "program", language: "st" }],
})
const tree: ProjectTree = {
  name: "Current", path: "/current", pous: [pou(ORIGINAL)], pou_folders: [],
  devices: [], device_folders: [], edges: [], edge_folders: [],
  iomap: { mappings: [] }, tasks: { tasks: [], programs: [] },
}
let runtime: ReturnType<typeof useRuntime>
function Probe() { runtime = useRuntime(); return <output>{runtime.source}</output> }

/** What is on disk right now, as the server would return it. */
let disk = ORIGINAL

beforeEach(() => {
  vi.clearAllMocks()
  disk = ORIGINAL
  vi.stubGlobal("EventSource", class { close() {} })
  window.history.replaceState(null, "", "/?project=Current")
  vi.mocked(api.fetchProject).mockResolvedValue(tree)
  vi.mocked(api.fetchPou).mockImplementation(async () => pou(disk))
  vi.mocked(api.savePou).mockImplementation(async (_path, source) => { disk = source; return { ok: true } })
  vi.mocked(api.runProgram).mockResolvedValue({ ok: true })
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

async function mountedWithEdit() {
  render(<RuntimeProvider><Probe /></RuntimeProvider>)
  await waitFor(() => expect(runtime.currentPou?.path).toBe("main"))
  act(() => runtime.setSource(MINE))
  vi.mocked(api.savePou).mockClear()
  vi.mocked(api.runProgram).mockClear()
}

/** Another writer changes the file; the server's `pou_updated` mutation
 *  reaches this window as an invalidation of the POU's topic. */
async function externalWrite(source: string) {
  disk = source
  await act(async () => {
    invalidationBus.emit(Topic.pou("main"))
    await Promise.resolve()
  })
}

describe("an external change to the open program while it has unsaved edits", () => {
  it("is announced in the editor instead of being dropped", async () => {
    await mountedWithEdit()
    await externalWrite(THEIRS)
    await waitFor(() => expect(runtime.externalChange?.source).toBe(THEIRS))
    expect(runtime.source).toBe(MINE)
  })

  it("does not let Save overwrite it without a decision", async () => {
    await mountedWithEdit()
    await externalWrite(THEIRS)
    await waitFor(() => expect(runtime.externalChange).not.toBeNull())

    let saving!: Promise<void>
    act(() => { saving = runtime.saveCurrentPou() })
    expect(await screen.findByRole("dialog", { name: /changed on disk/i })).toBeTruthy()
    expect(api.savePou).not.toHaveBeenCalled()

    fireEvent.click(screen.getByRole("button", { name: /cancel/i }))
    await act(async () => { await saving })
    expect(api.savePou).not.toHaveBeenCalled()
    expect(disk).toBe(THEIRS)
    expect(runtime.source).toBe(MINE)
  })

  it("overwrites only when the user chooses to, and then stops warning", async () => {
    await mountedWithEdit()
    await externalWrite(THEIRS)
    await waitFor(() => expect(runtime.externalChange).not.toBeNull())

    let saving!: Promise<void>
    act(() => { saving = runtime.saveCurrentPou() })
    fireEvent.click(await screen.findByRole("button", { name: /overwrite/i }))
    await act(async () => { await saving })
    expect(api.savePou).toHaveBeenCalledWith("main", MINE)
    expect(disk).toBe(MINE)
    await waitFor(() => expect(runtime.externalChange).toBeNull())
  })

  it("loads the other version when the user chooses it, discarding the edit", async () => {
    await mountedWithEdit()
    await externalWrite(THEIRS)
    await waitFor(() => expect(runtime.externalChange).not.toBeNull())

    let saving!: Promise<void>
    act(() => { saving = runtime.saveCurrentPou() })
    fireEvent.click(await screen.findByRole("button", { name: /load disk version/i }))
    await act(async () => { await saving })
    expect(api.savePou).not.toHaveBeenCalled()
    expect(runtime.source).toBe(THEIRS)
    expect(runtime.currentPou?.source).toBe(THEIRS)
    expect(runtime.externalChange).toBeNull()
  })

  it("does not let Run save over it and start the stale program", async () => {
    await mountedWithEdit()
    await externalWrite(THEIRS)
    await waitFor(() => expect(runtime.externalChange).not.toBeNull())

    let running!: Promise<void>
    act(() => { running = runtime.run("Main", "main") })
    fireEvent.click(await screen.findByRole("button", { name: /cancel/i }))
    await act(async () => { await running })
    expect(api.savePou).not.toHaveBeenCalled()
    expect(api.runProgram).not.toHaveBeenCalled()
    expect(disk).toBe(THEIRS)
  })

  it("adopts the disk version when the edit is reverted, rather than showing the stale base as clean", async () => {
    await mountedWithEdit()
    await externalWrite(THEIRS)
    await waitFor(() => expect(runtime.externalChange).not.toBeNull())

    // Revert = setSource(currentPou.source): back to what was loaded, which
    // is no longer what is on disk.
    act(() => runtime.setSource(runtime.currentPou!.source))
    await waitFor(() => expect(runtime.source).toBe(THEIRS))
    expect(runtime.currentPou?.source).toBe(THEIRS)
    expect(runtime.externalChange).toBeNull()
  })
})

describe("changes that are not a conflict", () => {
  it("still reloads a clean buffer silently", async () => {
    render(<RuntimeProvider><Probe /></RuntimeProvider>)
    await waitFor(() => expect(runtime.currentPou?.path).toBe("main"))
    await externalWrite(THEIRS)
    await waitFor(() => expect(runtime.source).toBe(THEIRS))
    expect(runtime.externalChange).toBeNull()
  })

  it("does not mistake this window's own save for someone else's", async () => {
    await mountedWithEdit()
    await act(async () => { await runtime.saveCurrentPou() })
    expect(disk).toBe(MINE)

    // Keep typing, then the server's echo of our own save arrives.
    act(() => runtime.setSource(MINE + " (* more *)"))
    await externalWrite(MINE)
    await act(async () => { await Promise.resolve() })
    expect(runtime.externalChange).toBeNull()

    vi.mocked(api.savePou).mockClear()
    await act(async () => { await runtime.saveCurrentPou() })
    expect(screen.queryByRole("dialog")).toBeNull()
    expect(api.savePou).toHaveBeenCalledWith("main", MINE + " (* more *)")
  })

  it("does not mistake an echo that arrives before our save returns", async () => {
    await mountedWithEdit()
    // The server emits the mutation before it answers the PUT, so the echo
    // can be processed while `currentPou.source` still holds the old base.
    let release!: () => void
    vi.mocked(api.savePou).mockImplementation(async (_path, source) => {
      disk = source
      invalidationBus.emit(Topic.pou("main"))
      await new Promise<void>((resolve) => { release = resolve })
      return { ok: true }
    })
    let saving!: Promise<void>
    act(() => { saving = runtime.saveCurrentPou() })
    await act(async () => { await Promise.resolve(); await Promise.resolve() })
    release()
    await act(async () => { await saving })
    await act(async () => { await Promise.resolve() })
    expect(runtime.externalChange).toBeNull()
    expect(screen.queryByRole("dialog")).toBeNull()
  })
})

describe("a newer external write while a save response is pending", () => {
  it.each(["save", "run"] as const)("preserves the conflict after %s and asks on the next save", async (action) => {
    await mountedWithEdit()
    let release!: () => void
    vi.mocked(api.savePou).mockImplementationOnce(async (_path, source) => {
      disk = source
      await new Promise<void>((resolve) => { release = resolve })
      return { ok: true }
    })
    let pending!: Promise<void>
    act(() => { pending = action === "save" ? runtime.saveCurrentPou() : runtime.run("Main", "main") })
    await waitFor(() => expect(api.savePou).toHaveBeenCalledOnce())
    act(() => runtime.setSource(MINE + " (* next edit *)"))
    await externalWrite(THEIRS)
    expect(runtime.externalChange?.source).toBe(THEIRS)
    await act(async () => { release(); await pending })
    expect(disk).toBe(THEIRS)
    expect(runtime.externalChange?.source).toBe(THEIRS)
    expect(api.runProgram).not.toHaveBeenCalled()

    vi.mocked(api.savePou).mockClear()
    let saving!: Promise<void>
    act(() => { saving = runtime.saveCurrentPou() })
    fireEvent.click(await screen.findByRole("button", { name: /cancel/i }))
    await act(async () => { await saving })
    expect(api.savePou).not.toHaveBeenCalled()
    expect(disk).toBe(THEIRS)
  })

  it("loads the newer disk version when the saved buffer has no further edits", async () => {
    await mountedWithEdit()
    let release!: () => void
    vi.mocked(api.savePou).mockImplementationOnce(async (_path, source) => {
      disk = source
      await new Promise<void>((resolve) => { release = resolve })
      return { ok: true }
    })
    let saving!: Promise<void>
    act(() => { saving = runtime.saveCurrentPou() })
    await waitFor(() => expect(api.savePou).toHaveBeenCalledOnce())
    await externalWrite(THEIRS)
    await act(async () => { release(); await saving })
    await waitFor(() => expect(runtime.source).toBe(THEIRS))
    expect(runtime.currentPou?.source).toBe(THEIRS)
    expect(runtime.isDirty).toBe(false)
  })
})
