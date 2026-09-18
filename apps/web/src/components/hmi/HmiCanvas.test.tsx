// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { HmiCanvas, type CanvasMode } from "./HmiCanvas"
import { HmiHostProvider, type HmiHost } from "./host"
import { liveFeedStore } from "@/state/live-feed"
import { fitCanvasScale } from "./canvas-viewport"
import type { HmiDoc } from "@/types/generated/HmiDoc"
import type { HmiNode } from "@/types/generated/HmiNode"

vi.mock("@/state/hmi-live", () => ({ useHmiMutation: () => null }))
const doc: HmiDoc = {
  title: "Tank", version: 1, level: 2, grid: { w: 1000, h: 800, snap: 8 },
  root: { id: "root", type: "group", layout: "absolute", x: 0, y: 0, w: 1000, h: 800, gap: 0, bind: {}, action: {}, children: [
    { id: "set", type: "button", label: "Set level", x: 20, y: 20, w: 100, h: 32, bind: {}, action: { tap: { kind: "write", variable: "level", value: 5, confirm: true } } },
  ] },
}
function rootChildren(screen: HmiDoc): HmiNode[] {
  if (screen.root.type !== "group") throw new Error("expected a group root in the test screen")
  return screen.root.children
}
let host: HmiHost
beforeEach(() => {
  // Pin the clock. The write-freshness budget is measured with
  // `performance.now()`, so on a loaded machine a real-clock gap between this
  // setup and the confirm click could age the snapshot out and fail a test
  // that is about typing, not timing.
  vi.spyOn(performance, "now").mockReturnValue(1_000)
  liveFeedStore.setSnapshot(null)
  liveFeedStore.setConnected(true)
  liveFeedStore.setSnapshot({ timestamp_us: 1n, scan_count: 1n, vars: [{ name: "level", type_name: "REAL", value: "2.5", bits: 0 }] })
  vi.stubGlobal("ResizeObserver", class { constructor(private callback: () => void) {} observe() { this.callback() } disconnect() {} })
  vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockReturnValue(624)
  vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockReturnValue(324)
  host = { fetchDoc: vi.fn().mockResolvedValue(doc), moveNode: vi.fn().mockResolvedValue(undefined), write: vi.fn().mockResolvedValue(null), nav: vi.fn(),
    runtimeState: vi.fn().mockResolvedValue({ running: true, alarm: null }), history: vi.fn().mockResolvedValue({ series: [] }), alarms: vi.fn().mockResolvedValue([]), ackAlarm: vi.fn() }
})
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals() })
const canvas = (mode: CanvasMode) => <HmiHostProvider value={host}><HmiCanvas path="overview" mode={mode} selected={null} onSelect={vi.fn()} /></HmiHostProvider>

describe("Arrange-mode drag", () => {
  // An agent's `cs hmi op` lands on disk; this canvas has not reloaded yet
  // (the SSE mutation is still in flight — here it never arrives). Moving an
  // element must not write back the canvas's in-memory copy of the whole
  // screen: that copy predates the agent's node and would delete it.
  it("moves only the dragged node, keeping what another writer added meanwhile", async () => {
    let server: HmiDoc = structuredClone(doc)
    rootChildren(server).push({ id: "agent_lamp", type: "symbol", symbol: "lamp", props: {}, x: 300, y: 20, w: 40, h: 40, bind: {}, action: {} })
    const saveDoc = vi.fn(async (_path: string, next: HmiDoc) => { server = structuredClone(next) })
    ;(host as HmiHost & { saveDoc?: unknown }).saveDoc = saveDoc
    host.moveNode = vi.fn(async (_path: string, id: string, x: number, y: number) => {
      const node = rootChildren(server).find((n) => n.id === id)!
      node.x = x
      node.y = y
    })

    render(canvas("arrange"))
    const node = await screen.findByRole("button", { name: "Set level" })
    // Drag distance is divided by the fitted scale; let the fit land first.
    await waitFor(() => expect(screen.getByTestId("hmi-screen").style.transform).toBe("scale(0.375)"))
    const wrapper = node.closest("[data-hmi-id]")!
    const viewport = screen.getByTestId("hmi-viewport")
    fireEvent.pointerDown(wrapper, { clientX: 100, clientY: 100, pointerId: 1 })
    fireEvent.pointerMove(viewport, { clientX: 130, clientY: 100, pointerId: 1 })
    fireEvent.pointerUp(viewport, { clientX: 130, clientY: 100, pointerId: 1 })

    await waitFor(() => expect(rootChildren(server).find((n) => n.id === "set")!.x).not.toBe(20))
    expect(rootChildren(server).map((n) => n.id)).toEqual(["set", "agent_lamp"])
    expect(saveDoc).not.toHaveBeenCalled()
    expect(host.moveNode).toHaveBeenCalledWith("overview", "set", 104, 24) // 30px / 0.375 = 80, snapped to the 8-unit grid
  })

  it("reloads and reports when the move is refused", async () => {
    host.moveNode = vi.fn().mockRejectedValue(new Error("node vanished"))
    render(canvas("arrange"))
    const node = await screen.findByRole("button", { name: "Set level" })
    const viewport = screen.getByTestId("hmi-viewport")
    fireEvent.pointerDown(node.closest("[data-hmi-id]")!, { clientX: 100, clientY: 100, pointerId: 1 })
    fireEvent.pointerMove(viewport, { clientX: 130, clientY: 100, pointerId: 1 })
    fireEvent.pointerUp(viewport, { clientX: 130, clientY: 100, pointerId: 1 })
    expect(await screen.findByText(/layout was not saved: .*node vanished/i)).toBeTruthy()
    await waitFor(() => expect(host.fetchDoc).toHaveBeenCalledTimes(2))
  })
})

describe("HMI viewport and action modes", () => {
  it("fits both axes and keeps explicit actual-size controls", async () => {
    render(canvas("operate"))
    await screen.findByRole("button", { name: "100%" })
    await waitFor(() => expect(screen.getByTestId("hmi-screen").style.transform).toBe("scale(0.375)"))
    expect(screen.getByTestId("hmi-footprint").style.height).toBe("300px")
    fireEvent.click(screen.getByRole("button", { name: "100%" }))
    expect(screen.getByTestId("hmi-screen").style.transform).toBe("scale(1)")
    expect(screen.getByTestId("hmi-footprint").style.width).toBe("1000px")
    fireEvent.click(screen.getByRole("button", { name: "Fit" }))
    expect(screen.getByTestId("hmi-screen").style.transform).toBe("scale(0.375)")
  })

  it("cancels an outstanding write confirmation when switching to Arrange", async () => {
    const view = render(canvas("operate"))
    fireEvent.click(await screen.findByRole("button", { name: "Set level" }))
    expect(await screen.findByRole("dialog")).toBeTruthy()
    view.rerender(canvas("arrange"))
    expect(screen.queryByRole("dialog")).toBeNull()
    fireEvent.click(screen.getByRole("button", { name: "Set level" }))
    expect(host.write).not.toHaveBeenCalled()
    view.rerender(canvas("operate"))
    expect(screen.queryByRole("dialog")).toBeNull()
  })

  it("keeps a confirmed REAL write typed and displays a transport failure", async () => {
    vi.mocked(host.write).mockRejectedValue(new Error("write unavailable"))
    render(canvas("operate"))
    fireEvent.click(await screen.findByRole("button", { name: "Set level" }))
    fireEvent.click(await screen.findByRole("button", { name: "Confirm" }))
    await waitFor(() => expect(host.write).toHaveBeenCalledWith("level", 5, "REAL", undefined))
    expect(await screen.findByRole("alert")).toHaveProperty("textContent", expect.stringContaining("write unavailable"))
  })

  it("keeps the HMI alarm pending and visible when acknowledgment fails", async () => {
    const alarmDoc = structuredClone(doc)
    if (alarmDoc.root.type !== "group") throw new Error("Expected group fixture")
    alarmDoc.root.children.push({ id: "alarms", type: "alarmlist", x: 20, y: 80, w: 600, h: 200, max_rows: 0, bind: {}, action: {} })
    vi.mocked(host.fetchDoc).mockResolvedValue(alarmDoc)
    vi.mocked(host.alarms).mockResolvedValue([{ id: "high_level", variable: "level", severity: "warn", message: "High level", active: true, acked: false, raised_at_us: 0n, value_at_raise: 95, count: 1 }])
    vi.mocked(host.ackAlarm).mockRejectedValue(new Error("acknowledgment denied"))
    render(canvas("operate"))
    fireEvent.click(await screen.findByRole("button", { name: "ack" }))
    expect(await screen.findByRole("alert")).toHaveProperty("textContent", expect.stringContaining("acknowledgment denied"))
    expect(screen.getByRole("button", { name: "ack" })).toBeTruthy()
  })

  it("shows standing alarms first and labels a never-raised JSON timestamp honestly", async () => {
    const alarmDoc = structuredClone(doc)
    if (alarmDoc.root.type !== "group") throw new Error("Expected group fixture")
    alarmDoc.root.children.push({ id: "alarms", type: "alarmlist", x: 20, y: 80, w: 600, h: 200, max_rows: 0, bind: {}, action: {} })
    vi.mocked(host.fetchDoc).mockResolvedValue(alarmDoc)
    // Use the actual JSON representation: the network does not return bigint.
    vi.mocked(host.alarms).mockResolvedValue(JSON.parse(JSON.stringify([
      { id: "active", variable: "level", severity: "warn", message: "Still active", active: true, acked: true, raised_at_us: 1_700_000_000_000_000, value_at_raise: 95, count: 1 },
      { id: "returned", variable: "level", severity: "warn", message: "Returned, needs acknowledgment", active: false, acked: false, raised_at_us: 1_700_000_000_000_000, value_at_raise: 95, count: 1 },
      { id: "never", variable: "level", severity: "critical", message: "High-high level", active: false, acked: true, raised_at_us: 0, value_at_raise: 0, count: 0 },
    ])))
    render(canvas("operate"))
    const all = await screen.findByRole("button", { name: "Show all 3" })
    expect(screen.getByText("Still active")).toBeTruthy()
    expect(screen.getByText("Returned, needs acknowledgment")).toBeTruthy()
    expect(screen.queryByText("High-high level")).toBeNull()
    expect(screen.getAllByRole("button", { name: "ack" })).toHaveLength(1)
    fireEvent.click(all)
    expect(screen.getByText("High-high level")).toBeTruthy()
    expect(screen.getByText("Never raised")).toBeTruthy()
    fireEvent.click(screen.getByRole("button", { name: "Standing only" }))
    expect(screen.queryByText("High-high level")).toBeNull()
    expect(screen.getByText("Returned, needs acknowledgment")).toBeTruthy()
  })

  it("fits a short pane without a readability floor that hides controls", () => {
    expect(fitCanvasScale(600, 160, 1000, 800)).toBe(0.2)
    expect(fitCanvasScale(2000, 1600, 1000, 800)).toBe(1)
    expect(fitCanvasScale(0, 0, 1000, 800)).toBe(1)
  })
})
