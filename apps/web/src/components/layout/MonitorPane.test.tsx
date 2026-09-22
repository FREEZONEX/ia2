// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { MonitorPane, NumericEditor } from "./MonitorPane"
import { currentForceValue, monitorWriteValue } from "./monitor-values"

const mocks = vi.hoisted(() => ({
  runtime: vi.fn(), snapshot: vi.fn(),
  fetchRuntimeStatus: vi.fn(), fetchRuntimeHistory: vi.fn(), fetchRuntimeAlarms: vi.fn(),
  pauseRuntime: vi.fn(), resumeRuntime: vi.fn(), stepRuntime: vi.fn(),
  forceVariable: vi.fn(), unforceVariable: vi.fn(), writeVariable: vi.fn(), ackRuntimeAlarm: vi.fn(),
}))
vi.mock("@/state/runtime", () => ({ useRuntime: mocks.runtime }))
vi.mock("@/state/live-feed", () => ({ useLastSnapshot: mocks.snapshot }))
vi.mock("@/lib/api", () => mocks)

const level = { name: "level", type_name: "REAL", value: "2.5", bits: 1075838976 }
const runningStatus = () => ({ mode: { kind: "running" }, forces: [], device_health: [], watchdog_tripped: false, last_error: null })
beforeEach(() => {
  vi.resetAllMocks()
  mocks.runtime.mockReturnValue({ isRunning: true, currentPou: { path: "main" }, running: null, attached: null })
  mocks.snapshot.mockReturnValue({ timestamp_us: 1000000n, scan_count: 10n, vars: [level] })
  mocks.fetchRuntimeStatus.mockResolvedValue(runningStatus())
  mocks.fetchRuntimeAlarms.mockResolvedValue([])
  mocks.fetchRuntimeHistory.mockResolvedValue({ series: [] })
})
afterEach(cleanup)

function mountMonitor(collapsed = false) {
  return render(<MonitorPane collapsed={collapsed} onToggleCollapse={vi.fn()} />)
}

describe("confirmed Monitor controls", () => {
  it("still permits releasing an existing force on a stale input", async () => {
    mocks.snapshot.mockReturnValue({ timestamp_us: 1000000n, scan_count: 1n,
      vars: [{ ...level, input: { device: "io", channel: "level", stale: true } }] })
    mocks.fetchRuntimeStatus.mockResolvedValue({ ...runningStatus(), forces: [{ name: "level", value: 2.5 }] })
    mountMonitor()
    fireEvent.click(await screen.findByRole("button", { name: "Unforce level" }))
    await waitFor(() => expect(mocks.unforceVariable).toHaveBeenCalledWith("level"))
  })
  it("labels last-known inputs and gaps their traces without blocking unrelated writes", async () => {
    const vars = [level, { name: "run", type_name: "BOOL", value: "TRUE", bits: 1 }]
    mocks.snapshot.mockReturnValue({ timestamp_us: 1000000n, scan_count: 1n, vars })
    const view = mountMonitor()
    await waitFor(() => expect(mocks.fetchRuntimeStatus).toHaveBeenCalled())
    mocks.snapshot.mockReturnValue({ timestamp_us: 2000000n, scan_count: 2n,
      vars: [{ ...level, input: { device: "sensor_io", channel: "level", stale: true } }, vars[1]] })
    view.rerender(<MonitorPane collapsed={false} onToggleCollapse={vi.fn()} />)
    expect(screen.getByText("Stale · sensor_io/level")).toBeTruthy()
    expect(screen.getByText("2.5")).toBeTruthy()
    expect(screen.queryByRole("button", { name: "Force level" })).toBeNull()
    fireEvent.click(screen.getByRole("button", { name: "Toggle run" }))
    await waitFor(() => expect(mocks.writeVariable).toHaveBeenCalledWith("run", 0, "BOOL"))
    mocks.snapshot.mockReturnValue({ timestamp_us: 3000000n, scan_count: 3n, vars })
    view.rerender(<MonitorPane collapsed={false} onToggleCollapse={vi.fn()} />)
    expect(screen.queryByText("Stale · sensor_io/level")).toBeNull()
    const row = screen.getByRole("button", { name: "Force level" }).closest("tr")!
    expect(row.querySelectorAll("polyline").length).toBe(2) // separate fresh segments
  })
  it.each([
    { action: "Resume", initialMode: "paused", failure: "resume denied", api: mocks.resumeRuntime },
    { action: "Step", initialMode: "paused", failure: "step denied", api: mocks.stepRuntime },
  ])("keeps the confirmed mode when $action fails", async ({ action, initialMode, failure, api }) => {
    mocks.fetchRuntimeStatus.mockResolvedValue({ ...runningStatus(), mode: { kind: initialMode } })
    api.mockRejectedValue(new Error(failure))
    mountMonitor()
    const button = await screen.findByRole("button", { name: action })
    await waitFor(() => expect((button as HTMLButtonElement).disabled).toBe(false))
    fireEvent.click(button)
    expect(await screen.findByRole("alert")).toHaveProperty("textContent", expect.stringContaining(failure))
    expect(screen.getByText("Paused")).toBeTruthy()
    expect(screen.queryByRole("button", { name: "Pause" })).toBeNull()
  })

  it("keeps running mode and reports a rejected pause", async () => {
    mocks.pauseRuntime.mockRejectedValue(new Error("PLC refused pause"))
    mountMonitor()
    const pause = await screen.findByRole("button", { name: "Pause" })
    await waitFor(() => expect((pause as HTMLButtonElement).disabled).toBe(false))
    fireEvent.click(pause)
    expect(await screen.findByRole("alert")).toHaveProperty("textContent", expect.stringContaining("PLC refused pause"))
    expect(screen.queryByRole("button", { name: "Resume" })).toBeNull()
  })

  it("does not advertise a pending pause before the runtime confirms it", async () => {
    let confirm!: () => void
    mocks.pauseRuntime.mockReturnValue(new Promise<void>(resolve => { confirm = resolve }))
    mountMonitor()
    await waitFor(() => expect((screen.getByRole("button", { name: "Pause" }) as HTMLButtonElement).disabled).toBe(false))
    fireEvent.click(screen.getByRole("button", { name: "Pause" }))
    expect(screen.queryByRole("button", { name: "Resume" })).toBeNull()
    expect(screen.getByRole("status").textContent).toContain("waiting for confirmation")
    mocks.fetchRuntimeStatus.mockResolvedValue({ ...runningStatus(), mode: { kind: "paused" } })
    await act(async () => confirm())
    expect(await screen.findByRole("button", { name: "Resume" })).toBeTruthy()
  })

  it("preserves a REAL fraction when forcing and displays failure without a forced state", async () => {
    mocks.forceVariable.mockRejectedValue(new Error("force denied"))
    mountMonitor()
    fireEvent.click(await screen.findByRole("button", { name: "Force level" }))
    await waitFor(() => expect(mocks.forceVariable).toHaveBeenCalledWith("level", 2.5, "REAL"))
    expect(await screen.findByRole("alert")).toHaveProperty("textContent", expect.stringContaining("force denied"))
    expect(screen.queryByRole("button", { name: "Unforce level" })).toBeNull()
  })

  it("distinguishes an accepted pause from an unavailable status readback", async () => {
    mocks.pauseRuntime.mockResolvedValue(undefined)
    mountMonitor()
    const pause = await screen.findByRole("button", { name: "Pause" })
    await waitFor(() => expect((pause as HTMLButtonElement).disabled).toBe(false))
    mocks.fetchRuntimeStatus.mockRejectedValue(new Error("status link lost"))
    fireEvent.click(pause)
    expect(await screen.findByRole("alert")).toHaveProperty("textContent", expect.stringContaining("Pause accepted; status unconfirmed"))
    expect(screen.queryByRole("button", { name: "Resume" })).toBeNull()
    expect((screen.getByRole("button", { name: "Pause" }) as HTMLButtonElement).disabled).toBe(true)
  })

  it("keeps live status polling when collapsed and exposes an expand control", async () => {
    mountMonitor(true)
    expect(screen.getByRole("button", { name: "Expand Monitor" }).getAttribute("aria-expanded")).toBe("false")
    expect(screen.queryByRole("textbox", { name: "Search variables" })).toBeNull()
    await waitFor(() => expect(mocks.fetchRuntimeStatus).toHaveBeenCalled())
  })

  it("retains an alarm read failure in the collapsed title bar", async () => {
    mocks.fetchRuntimeAlarms.mockRejectedValue(new Error("alarm link lost"))
    mountMonitor(true)
    expect(await screen.findByRole("alert")).toHaveProperty("textContent", expect.stringContaining("alarm link lost"))
  })

  it("shows a compact stopped state without an unusable search box", () => {
    mocks.runtime.mockReturnValue({ isRunning: false, currentPou: null, running: null, attached: null })
    mocks.snapshot.mockReturnValue(null)
    mountMonitor()
    expect(screen.getByText("No program running")).toBeTruthy()
    expect(screen.queryByRole("textbox", { name: "Search variables" })).toBeNull()
  })

  it("keeps an alarm unacknowledged when acknowledgment fails", async () => {
    mocks.fetchRuntimeAlarms.mockResolvedValue([{ id: "high_level", variable: "level", severity: "warn", message: "High level", active: true, acked: false, raised_at_us: 0n, value_at_raise: 95, count: 1 }])
    mocks.ackRuntimeAlarm.mockRejectedValue(new Error("acknowledgment denied"))
    mountMonitor()
    fireEvent.click(await screen.findByRole("button", { name: "ack" }))
    expect(await screen.findByRole("alert")).toHaveProperty("textContent", expect.stringContaining("acknowledgment denied"))
    expect(screen.getByRole("button", { name: "ack" })).toBeTruthy()
    expect(screen.queryByText("ackd")).toBeNull()
  })

  it("filters variables without removing the current scan status", async () => {
    mountMonitor()
    fireEvent.change(screen.getByRole("textbox", { name: "Search variables" }), { target: { value: "missing" } })
    expect(screen.getByText("No matching variables")).toBeTruthy()
    expect(screen.getByText("scan #10")).toBeTruthy()
  })
})

describe("numeric draft commit", () => {
  it("Escape cancels even when it synchronously triggers blur", () => {
    const write = vi.fn().mockResolvedValue(true)
    render(<NumericEditor name="level" typeName="REAL" value="2.5" onWrite={write} />)
    const input = screen.getByRole("textbox")
    act(() => input.focus())
    fireEvent.change(input, { target: { value: "8.75" } })
    fireEvent.keyDown(input, { key: "Escape" })
    expect(write).not.toHaveBeenCalled()
    expect((input as HTMLInputElement).value).toBe("2.5")
  })

  it("Enter followed by blur sends exactly one write", async () => {
    const write = vi.fn().mockResolvedValue(true)
    render(<NumericEditor name="level" typeName="REAL" value="2.5" onWrite={write} />)
    const input = screen.getByRole("textbox")
    act(() => input.focus())
    fireEvent.change(input, { target: { value: "8.75" } })
    fireEvent.keyDown(input, { key: "Enter" })
    await waitFor(() => expect(write).toHaveBeenCalledExactlyOnceWith(8.75))
  })

  it("rejects partial numbers and 64-bit writes instead of guessing", () => {
    expect(() => monitorWriteValue("2.5oops", "REAL")).toThrow("finite")
    expect(() => monitorWriteValue("", "REAL")).toThrow("finite")
    expect(() => monitorWriteValue("2.5", "INT")).toThrow("whole")
    expect(() => monitorWriteValue("2.5", "LREAL")).toThrow("not supported")
    expect(currentForceValue({ ...level, type_name: "WORD", value: "16#1637" })).toBe(0x1637)
  })
})
