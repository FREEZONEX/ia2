// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { HmiCanvas, type CanvasMode } from "./HmiCanvas"
import { HmiHostProvider, type HmiHost } from "./host"
import { liveFeedStore } from "@/state/live-feed"
import type { HmiDoc } from "@/types/generated/HmiDoc"

vi.mock("@/state/hmi-live", () => ({ useHmiMutation: () => null }))

const base: HmiDoc = {
  title: "Action test", version: 1, level: 2, grid: { w: 600, h: 300, snap: 8 },
  root: { id: "root", type: "group", layout: "absolute", x: 0, y: 0, w: 600, h: 300, gap: 0, bind: {}, action: {}, children: [
    { id: "set", type: "button", label: "Set level", x: 20, y: 20, w: 120, h: 32, bind: { enable: "permit" }, action: { tap: { kind: "write", variable: "level", value: 5, confirm: true } } },
    { id: "quick", type: "button", label: "Quick set", x: 20, y: 60, w: 120, h: 32, bind: { enable: "permit" }, action: { tap: { kind: "write", variable: "level", value: 7, confirm: false } } },
    { id: "nav", type: "button", label: "Next screen", x: 20, y: 100, w: 120, h: 32, bind: {}, action: { tap: { kind: "nav", target: "next" } } },
  ] },
}
let host: HmiHost
let clock: number
const update = (scan = 1, permit: boolean | null = true, typeName = "REAL", level = "2.5") => {
  liveFeedStore.setSnapshot({ timestamp_us: BigInt(scan * 2000), scan_count: BigInt(scan), vars: [
    { name: "level", type_name: typeName, value: level, bits: 0 },
    ...(permit === null ? [] : [{ name: "permit", type_name: "BOOL", value: permit ? "TRUE" : "FALSE", bits: permit ? 1 : 0 }]),
  ] })
}
const canvas = (mode: CanvasMode = "operate", path = "overview") => (
  <HmiHostProvider value={host}><HmiCanvas path={path} mode={mode} selected={null} onSelect={vi.fn()} /></HmiHostProvider>
)
const flush = async () => { await act(async () => {}) }
const openConfirm = async () => {
  fireEvent.click(await screen.findByRole("button", { name: "Set level" }))
  await screen.findByRole("dialog")
}

beforeEach(() => {
  clock = 100
  vi.spyOn(performance, "now").mockImplementation(() => clock)
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} })
  liveFeedStore.setConnected(false)
  liveFeedStore.setSnapshot(null)
  liveFeedStore.setConnected(true)
  update()
  host = {
    fetchDoc: vi.fn().mockResolvedValue(structuredClone(base)),
    write: vi.fn().mockResolvedValue(null), nav: vi.fn(),
    runtimeState: vi.fn().mockResolvedValue({ running: true, alarm: null }),
    history: vi.fn().mockResolvedValue({ series: [] }),
    alarms: vi.fn().mockResolvedValue([]), ackAlarm: vi.fn(),
  }
})
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals() })

describe("HMI write revalidation", () => {
  it("refuses cached data while disconnected, including no-confirm actions", async () => {
    render(canvas())
    await screen.findByRole("button", { name: "Quick set" })
    act(() => liveFeedStore.setConnected(false))
    fireEvent.click(screen.getByRole("button", { name: "Quick set" }))
    await flush()
    expect(host.write).not.toHaveBeenCalled()
    expect(screen.getByRole("alert").textContent).toMatch(/connection|live data/i)
  })

  it.each([false, null])("rechecks the interlock at Confirm (%s)", async permit => {
    render(canvas())
    await openConfirm()
    act(() => update(2, permit))
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }))
    await flush()
    expect(host.write).not.toHaveBeenCalled()
    expect(screen.getByRole("alert").textContent).toMatch(/enabled|interlock/i)
  })

  it("does not refresh stale data when identical scans keep arriving", async () => {
    render(canvas())
    await openConfirm()
    clock += 2100
    act(() => update(1))
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }))
    await flush()
    expect(host.write).not.toHaveBeenCalled()
  })

  it("requires a new snapshot after reconnection and a new confirmation", async () => {
    render(canvas())
    await openConfirm()
    act(() => { liveFeedStore.setConnected(false); liveFeedStore.setConnected(true) })
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }))
    await flush()
    expect(host.write).not.toHaveBeenCalled()
    fireEvent.click(screen.getByRole("button", { name: "Quick set" }))
    await flush()
    expect(host.write).not.toHaveBeenCalled()
    act(() => update(2))
    fireEvent.click(screen.getByRole("button", { name: "Quick set" }))
    await waitFor(() => expect(host.write).toHaveBeenCalledTimes(1))
  })

  it("rejects an old confirmation even if fresh scans arrive after reconnect", async () => {
    render(canvas())
    await openConfirm()
    act(() => { liveFeedStore.setConnected(false); liveFeedStore.setConnected(true); update(2) })
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }))
    await flush()
    expect(host.write).not.toHaveBeenCalled()
  })

  it.each([
    { running: true, alarm: "Watchdog tripped — outputs are locked" },
    { running: true, alarm: null, mode: "paused" as const },
    { running: false, alarm: null },
  ])("rejects a fresh unhealthy runtime status: %j", async state => {
    vi.mocked(host.runtimeState).mockResolvedValue(state)
    render(canvas())
    await openConfirm()
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }))
    await flush()
    expect(host.runtimeState).toHaveBeenCalled()
    expect(host.write).not.toHaveBeenCalled()
  })

  it("rejects a failed status read without retrying the write", async () => {
    vi.mocked(host.runtimeState).mockRejectedValue(new Error("status unavailable"))
    render(canvas())
    await openConfirm()
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }))
    await flush()
    expect(host.write).not.toHaveBeenCalled()
    expect(screen.getByRole("alert").textContent).toContain("status unavailable")
  })

  it.each(["interlock", "arrange", "unmount", "disconnect", "stale", "path"])("rechecks after the status await: %s", async change => {
    let release!: (s: { running: boolean; alarm: null }) => void
    vi.mocked(host.runtimeState).mockImplementation(() => new Promise(resolve => { release = resolve }))
    const view = render(canvas())
    await openConfirm()
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }))
    await flush()
    expect(host.write).not.toHaveBeenCalled()
    expect(release).toBeTypeOf("function")
    act(() => {
      if (change === "interlock") update(2, false)
      if (change === "arrange") view.rerender(canvas("arrange"))
      if (change === "path") view.rerender(canvas("operate", "another"))
      if (change === "unmount") view.unmount()
      if (change === "disconnect") liveFeedStore.setConnected(false)
      if (change === "stale") clock += 2100
    })
    await act(async () => release({ running: true, alarm: null }))
    expect(host.write).not.toHaveBeenCalled()
  })

  it("refuses a changed variable type instead of encoding the old confirmation", async () => {
    render(canvas())
    await openConfirm()
    act(() => update(2, true, "DINT"))
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }))
    await flush()
    expect(host.write).not.toHaveBeenCalled()
  })

  it("keeps the confirmed value when the current value changes", async () => {
    render(canvas())
    await openConfirm()
    act(() => update(2, true, "REAL", "99"))
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }))
    await waitFor(() => expect(host.write).toHaveBeenCalledWith("level", 5, "REAL", undefined))
    expect(host.write).toHaveBeenCalledTimes(1)
  })

  it("times out a status read and ignores its late successful response", async () => {
    let release!: (s: { running: boolean; alarm: null }) => void
    vi.mocked(host.runtimeState).mockImplementation(() => new Promise(resolve => { release = resolve }))
    render(canvas())
    await openConfirm()
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
    try {
      fireEvent.click(screen.getByRole("button", { name: "Confirm" }))
      await act(async () => { await vi.advanceTimersByTimeAsync(2001) })
      expect(screen.getByRole("alert").textContent).toContain("timed out")
      await act(async () => release({ running: true, alarm: null }))
      expect(host.write).not.toHaveBeenCalled()
    } finally { vi.useRealTimers() }
  })

  it("does not queue or duplicate rapid no-confirm writes", async () => {
    let release!: (s: { running: boolean; alarm: null }) => void
    vi.mocked(host.runtimeState).mockImplementation(() => new Promise(resolve => { release = resolve }))
    render(canvas())
    const button = await screen.findByRole("button", { name: "Quick set" })
    fireEvent.click(button)
    fireEvent.click(button)
    expect(host.runtimeState).toHaveBeenCalledTimes(1)
    await act(async () => release({ running: true, alarm: null }))
    expect(host.write).toHaveBeenCalledTimes(1)
  })

  it("keeps pulse reset in the runtime request", async () => {
    const pulseDoc = structuredClone(base)
    if (pulseDoc.root.type !== "group") throw new Error("Expected group")
    pulseDoc.root.children[0].action.tap = { kind: "pulse", variable: "permit", ms: 100, confirm: true }
    vi.mocked(host.fetchDoc).mockResolvedValue(pulseDoc)
    render(canvas())
    await openConfirm()
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }))
    await waitFor(() => expect(host.write).toHaveBeenCalledWith("permit", 1, "BOOL", 100))
    expect(host.write).toHaveBeenCalledTimes(1)
  })

  // A 5 s-cycle project: nothing newer than the last scan exists, so the
  // window follows that cadence rather than refusing 3 s out of every 5.
  const learnSlowCadence = () => {
    for (const scan of [2, 3, 4, 5]) {
      clock += 5000
      act(() => update(scan))
    }
  }

  it("allows a write between scans on a slow-cycle project", async () => {
    render(canvas())
    await screen.findByRole("button", { name: "Quick set" })
    learnSlowCadence()
    clock += 4000
    fireEvent.click(screen.getByRole("button", { name: "Quick set" }))
    await waitFor(() => expect(host.write).toHaveBeenCalledTimes(1))
  })

  it("names the widened budget when a slow project does go stale", async () => {
    render(canvas())
    await screen.findByRole("button", { name: "Quick set" })
    learnSlowCadence()
    clock += 10_100
    fireEvent.click(screen.getByRole("button", { name: "Quick set" }))
    await flush()
    expect(host.write).not.toHaveBeenCalled()
    expect(screen.getByRole("alert").textContent).toContain("within 10s")
  })

  it("does not take a control away because an unrelated device is down", async () => {
    // Device health belongs to ONE device and the panel cannot tell which
    // one carries this variable — the edge panel has no iomap at all. The
    // runtime scopes that; blanket-refusing here cost every control, Stop
    // included, over some unrelated island.
    vi.mocked(host.runtimeState).mockResolvedValue({
      running: true, alarm: null, unhealthyDevices: ["some_other_island"],
    })
    render(canvas())
    await screen.findByRole("button", { name: "Quick set" })
    fireEvent.click(screen.getByRole("button", { name: "Quick set" }))
    await waitFor(() => expect(host.write).toHaveBeenCalledTimes(1))
  })

  it("reports an applied-but-undelivered write instead of a bare success", async () => {
    vi.mocked(host.write).mockResolvedValue('device "bus_a" link is down')
    render(canvas())
    await screen.findByRole("button", { name: "Quick set" })
    fireEvent.click(screen.getByRole("button", { name: "Quick set" }))
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("bus_a"))
    // Reported, never retried — the value is already in the program.
    expect(host.write).toHaveBeenCalledTimes(1)
  })

  it("honours a runtime-reported slow cadence without learning any gaps", async () => {
    render(canvas())
    await screen.findByRole("button", { name: "Quick set" })
    // What a /status poll would have set. No observed gaps exist yet — that
    // is the point: the reported period needs no warm-up.
    act(() => liveFeedStore.setScanPeriodMs(5000))
    clock += 4000
    fireEvent.click(screen.getByRole("button", { name: "Quick set" }))
    await waitFor(() => expect(host.write).toHaveBeenCalledTimes(1))
  })

  // Attached to an edge, the canvas renders the EDGE's stream while the IDE's
  // `write` still posts to the project server's own runtime. Commanding a
  // runtime the operator is not looking at is worse than refusing.
  describe("a host that cannot deliver a write", () => {
    const BLOCKED = "Attached to a remote edge — read-only"

    it("refuses with the host's reason, not a live-state one", async () => {
      host.writesBlocked = BLOCKED
      render(canvas())
      fireEvent.click(await screen.findByRole("button", { name: "Quick set" }))
      await flush()
      expect(host.write).not.toHaveBeenCalled()
      // Never even asks the wrong runtime how it is doing.
      expect(host.runtimeState).not.toHaveBeenCalled()
      expect(screen.getByRole("alert").textContent).toContain(BLOCKED)
    })

    it("disables the write controls but not navigation", async () => {
      host.writesBlocked = BLOCKED
      render(canvas())
      expect(await screen.findByRole("button", { name: "Quick set" }))
        .toHaveProperty("disabled", true)
      expect(screen.getByRole("button", { name: "Set level" }))
        .toHaveProperty("disabled", true)
      // `nav` is not a write; it stays usable, as it does offline.
      const nav = screen.getByRole("button", { name: "Next screen" })
      expect(nav).toHaveProperty("disabled", false)
      fireEvent.click(nav)
      expect(host.nav).toHaveBeenCalledWith("next")
    })

    it("refuses a confirmation that was opened before the block", async () => {
      render(canvas())
      await openConfirm()
      // Mutated in place: a NEW host object would change the host identity,
      // which reloads the document and takes the card away on its own. This
      // is the narrower path — same host, block arrives mid-confirmation.
      host.writesBlocked = BLOCKED
      fireEvent.click(screen.getByRole("button", { name: "Confirm" }))
      await flush()
      expect(host.write).not.toHaveBeenCalled()
      expect(host.runtimeState).not.toHaveBeenCalled()
      expect(screen.getByRole("alert").textContent).toContain(BLOCKED)
    })
  })

  it("keeps navigation usable without a live runtime", async () => {
    render(canvas())
    await screen.findByRole("button", { name: "Next screen" })
    act(() => liveFeedStore.setConnected(false))
    fireEvent.click(screen.getByRole("button", { name: "Next screen" }))
    expect(host.nav).toHaveBeenCalledWith("next")
    expect(host.runtimeState).not.toHaveBeenCalled()
    expect(host.write).not.toHaveBeenCalled()
  })
})
