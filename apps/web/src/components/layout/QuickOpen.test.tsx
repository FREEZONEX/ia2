// @vitest-environment jsdom
import { useState } from "react"
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

import { QuickOpen } from "./QuickOpen"

const { fetchHmis, selectHmi, selectPou, runtime } = vi.hoisted(() => ({
  fetchHmis: vi.fn(), selectHmi: vi.fn(), selectPou: vi.fn(),
  runtime: { projectEpoch: 1, project: { name: "tank", pous: [{ path: "main", declarations: [{ type: "program" }] }], devices: [], edges: [] } },
}))
vi.mock("@/lib/api", () => ({ fetchHmis }))
vi.mock("@/state/hmi-live", () => ({ useHmiMutation: () => 0 }))
vi.mock("@/state/runtime", () => ({ useRuntime: () => ({ ...runtime, selectHmi, selectPou, selectDevice: vi.fn(), selectEdge: vi.fn() }) }))

function Harness() {
  const [open, setOpen] = useState(false)
  return <><button onClick={() => setOpen(true)}>Open picker</button><button>Outside action</button><QuickOpen open={open} onClose={() => setOpen(false)} /></>
}

async function openPicker() {
  render(<Harness />)
  const trigger = screen.getByRole("button", { name: "Open picker" })
  trigger.focus()
  fireEvent.click(trigger)
  const input = await screen.findByRole("combobox")
  await waitFor(() => expect(document.activeElement).toBe(input))
  return { input, trigger }
}

beforeEach(() => {
  vi.clearAllMocks()
  fetchHmis.mockResolvedValue([{ path: "overview", title: "Tank overview", level: 1 }, { path: "detail", title: "Tank details", level: 2 }])
})
afterEach(cleanup)

describe("Quick open", () => {
  it("searches HMI titles and opens the keyboard-selected real screen", async () => {
    const { input, trigger } = await openPicker()
    await screen.findByRole("option", { name: /Tank overview/ })
    fireEvent.change(input, { target: { value: "Tank" } })
    expect(screen.getAllByRole("option")).toHaveLength(2)
    fireEvent.keyDown(input, { key: "ArrowDown" })
    const detail = screen.getByRole("option", { name: /Tank details/ })
    expect(input.getAttribute("aria-activedescendant")).toBe(detail.id)
    expect(detail.getAttribute("aria-selected")).toBe("true")
    fireEvent.keyDown(input, { key: "Enter" })
    expect(selectHmi).toHaveBeenCalledExactlyOnceWith("detail")
    expect(selectPou).not.toHaveBeenCalled()
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull())
    await waitFor(() => expect(document.activeElement).toBe(trigger))
  })

  it("keeps focus inside the shared dialog and restores it on Escape", async () => {
    const { input, trigger } = await openPicker()
    await screen.findByRole("option", { name: /Tank overview/ })
    const close = screen.getByRole("button", { name: "Close" })
    close.focus()
    fireEvent.keyDown(close, { key: "Tab" })
    expect(document.activeElement).toBe(input)
    fireEvent.keyDown(input, { key: "Tab", shiftKey: true })
    expect(document.activeElement).toBe(close)
    fireEvent.keyDown(close, { key: "Escape" })
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull())
    await waitFor(() => expect(document.activeElement).toBe(trigger))
  })

  it("reports HMI loading failures without hiding available POUs and retries", async () => {
    fetchHmis.mockRejectedValueOnce(new Error("offline"))
    const { input } = await openPicker()
    expect(await screen.findByText("HMI screens could not be loaded.")).not.toBeNull()
    expect(screen.getByRole("option", { name: /main/ })).not.toBeNull()
    fireEvent.click(screen.getByRole("button", { name: "Retry" }))
    await screen.findByRole("option", { name: /Tank overview/ })
    expect(screen.queryByText("HMI screens could not be loaded.")).toBeNull()
    fireEvent.change(input, { target: { value: "no-such-screen" } })
    expect(input.hasAttribute("aria-activedescendant")).toBe(false)
    fireEvent.keyDown(input, { key: "Enter" })
    expect(selectHmi).not.toHaveBeenCalled()
    expect(screen.queryByRole("dialog")).not.toBeNull()
  })

  it("does not open a resource while an IME composition is being committed", async () => {
    const { input } = await openPicker()
    await act(async () => {})
    fireEvent.keyDown(input, { key: "Enter", isComposing: true })
    expect(selectPou).not.toHaveBeenCalled()
    expect(screen.queryByRole("dialog")).not.toBeNull()
  })
})
