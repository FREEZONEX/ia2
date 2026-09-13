// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react"
import { afterEach, describe, expect, it, vi } from "vitest"
import { useDiscardChanges } from "./use-discard-changes"

let confirmDiscard: () => Promise<boolean>
function Harness({ dirty }: { dirty: boolean }) {
  const guard = useDiscardChanges(dirty)
  confirmDiscard = guard.confirmDiscard
  return guard.discardDialog
}
afterEach(() => { cleanup(); vi.restoreAllMocks() })

describe("unsaved changes confirmation", () => {
  it("allows clean navigation without a dialog or unload listener", async () => {
    const listen = vi.spyOn(window, "addEventListener")
    render(<Harness dirty={false} />)
    expect(await confirmDiscard()).toBe(true)
    expect(screen.queryByRole("dialog")).toBeNull()
    expect(listen.mock.calls.some(([name]) => name === "beforeunload")).toBe(false)
  })

  it.each(["Keep editing", "Close"])("treats %s as cancel and allows another attempt", async (button) => {
    render(<Harness dirty />)
    let pending!: Promise<boolean>
    act(() => { pending = confirmDiscard() })
    expect(screen.getByRole("dialog")).toBeTruthy()
    fireEvent.click(screen.getByRole("button", { name: button }))
    expect(await pending).toBe(false)
    act(() => { pending = confirmDiscard() })
    fireEvent.click(screen.getByRole("button", { name: "Discard changes" }))
    expect(await pending).toBe(true)
  })

  it("does not overwrite an outstanding confirmation with a second navigation", async () => {
    render(<Harness dirty />)
    let first!: Promise<boolean>
    act(() => { first = confirmDiscard() })
    expect(await confirmDiscard()).toBe(false)
    fireEvent.click(screen.getByRole("button", { name: "Keep editing" }))
    expect(await first).toBe(false)
  })

  it("warns on unload only while dirty and removes the listener after saving", () => {
    const { rerender, unmount } = render(<Harness dirty={false} />)
    const unload = () => new Event("beforeunload", { cancelable: true })
    let event = unload()
    window.dispatchEvent(event)
    expect(event.defaultPrevented).toBe(false)
    rerender(<Harness dirty />)
    event = unload()
    window.dispatchEvent(event)
    expect(event.defaultPrevented).toBe(true)
    rerender(<Harness dirty={false} />)
    event = unload()
    window.dispatchEvent(event)
    expect(event.defaultPrevented).toBe(false)
    rerender(<Harness dirty />)
    unmount()
    event = unload()
    window.dispatchEvent(event)
    expect(event.defaultPrevented).toBe(false)
  })

  it("rejects a pending navigation on unmount instead of leaving its promise unresolved", async () => {
    const { unmount } = render(<Harness dirty />)
    let pending!: Promise<boolean>
    act(() => { pending = confirmDiscard() })
    unmount()
    expect(await pending).toBe(false)
  })

  it("does not cancel the pending decision just because saving made the buffer clean", async () => {
    const { rerender } = render(<Harness dirty />)
    let settled = false
    let pending!: Promise<boolean>
    act(() => { pending = confirmDiscard().then((value) => { settled = true; return value }) })
    rerender(<Harness dirty={false} />)
    await act(async () => {})
    expect(settled).toBe(false)
    expect(await confirmDiscard()).toBe(false)
    fireEvent.click(screen.getByRole("button", { name: "Keep editing" }))
    expect(await pending).toBe(false)
  })
})
