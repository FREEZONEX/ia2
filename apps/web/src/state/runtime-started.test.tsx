// @vitest-environment jsdom
//
// A run started somewhere else — an agent's `cs run`, another window —
// reaches this window only as a payload-less `started` event. The provider
// set `isRunning` and left `running` empty, so nothing could say which
// program was live.
import { act, cleanup, render, waitFor } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

vi.mock("@/lib/api", async (original) => ({
  ...(await original<typeof import("@/lib/api")>()),
  fetchProject: vi.fn(), fetchProjects: vi.fn(), fetchRuntimeStatus: vi.fn(), checkProgram: vi.fn(),
}))
vi.mock("@/lib/lsp-client", () => ({
  pouDocumentUri: (path: string) => `file:///${path}.st`,
  LspClient: class { dispose() {} setSource() {} },
}))
import * as api from "@/lib/api"
import { RuntimeProvider, useRuntime } from "@/state/runtime"
import type { RuntimeStatus } from "@/types/generated/RuntimeStatus"

const streams: Array<{ onmessage: ((m: { data: string }) => void) | null }> = []
class FakeEventSource {
  onmessage: ((m: { data: string }) => void) | null = null
  onopen: (() => void) | null = null
  onerror: (() => void) | null = null
  constructor() { streams.push(this) }
  close() {}
}

const idle: RuntimeStatus = {
  running: false, project: "Current", program_instances: [], devices: [], device_health: [],
  watchdog_tripped: false, scan_period_ms: null, scan_overruns: null, consecutive_scan_overruns: null,
  scan_count: 0n, last_snapshot_us: 0n, last_error: null, running_info: null, mode: null, forces: [],
}

let runtime: ReturnType<typeof useRuntime>
function Probe() { runtime = useRuntime(); return null }

beforeEach(() => {
  streams.length = 0
  vi.clearAllMocks()
  vi.stubGlobal("EventSource", FakeEventSource)
  vi.mocked(api.fetchProject).mockResolvedValue(null)
  vi.mocked(api.fetchProjects).mockResolvedValue([])
  vi.mocked(api.fetchRuntimeStatus).mockResolvedValue(idle)
})
afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

const deliver = (event: unknown) =>
  act(async () => {
    for (const s of streams) s.onmessage?.({ data: JSON.stringify(event) })
    await Promise.resolve()
  })

describe("a run started elsewhere", () => {
  it("is described once the started event arrives", async () => {
    render(<RuntimeProvider><Probe /></RuntimeProvider>)
    await waitFor(() => expect(api.fetchRuntimeStatus).toHaveBeenCalled())
    expect(runtime.running).toBeNull()

    vi.mocked(api.fetchRuntimeStatus).mockResolvedValue({
      ...idle,
      running: true,
      running_info: { kind: "scheduled", programs: ["main", "mixer"] },
    })
    await deliver({ type: "started" })
    await waitFor(() => expect(runtime.running).toEqual({ kind: "scheduled", programs: ["main", "mixer"] }))
    expect(runtime.isRunning).toBe(true)
  })

  it("does not resurrect a run that stopped before the status arrived", async () => {
    render(<RuntimeProvider><Probe /></RuntimeProvider>)
    await waitFor(() => expect(api.fetchRuntimeStatus).toHaveBeenCalled())
    let answer!: (s: RuntimeStatus) => void
    vi.mocked(api.fetchRuntimeStatus).mockReturnValue(new Promise((resolve) => { answer = resolve }))
    await deliver({ type: "started" })
    await deliver({ type: "stopped" })
    await act(async () => { answer({ ...idle, running: false }) })
    expect(runtime.running).toBeNull()
    expect(runtime.isRunning).toBe(false)
  })
})
