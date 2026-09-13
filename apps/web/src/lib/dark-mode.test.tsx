// @vitest-environment jsdom
import { act, cleanup, render, screen } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

const registeredStorageListeners: EventListenerOrEventListenerObject[] = []
beforeEach(() => {
  vi.resetModules()
  localStorage.clear()
  document.documentElement.className = ""
  document.documentElement.removeAttribute("style")
  document.head.querySelectorAll('meta[name="theme-color"],meta[name="color-scheme"]').forEach((meta) => meta.remove())
  const add = window.addEventListener.bind(window)
  vi.spyOn(window, "addEventListener").mockImplementation((type, callback, options) => {
    if (type === "storage") registeredStorageListeners.push(callback)
    add(type, callback, options)
  })
})
afterEach(() => {
  cleanup()
  registeredStorageListeners.splice(0).forEach((callback) => window.removeEventListener("storage", callback))
  vi.restoreAllMocks()
})

function expectTheme(theme: "light" | "dark") {
  expect(document.documentElement.classList.contains("dark")).toBe(theme === "dark")
  expect(document.documentElement.style.colorScheme).toBe(theme)
  expect(document.head.querySelector('meta[name="theme-color"]')?.getAttribute("content")).toBe(theme === "dark" ? "#151a19" : "#ffffff")
  expect(document.head.querySelector('meta[name="color-scheme"]')?.getAttribute("content")).toBe(theme)
}
function storage(key: string | null, newValue: string | null, storageArea = localStorage) {
  window.dispatchEvent(new StorageEvent("storage", { key, newValue, storageArea, url: window.location.href }))
}

describe("workbench theme", () => {
  it("starts light with matching browser chrome and controls", async () => {
    await import("./dark-mode")
    expectTheme("light")
  })

  it("restores dark before the first render and migrates the legacy preference", async () => {
    localStorage.setItem("controlsoftware.theme", "dark")
    await import("./dark-mode")
    expectTheme("dark")
    expect(localStorage.getItem("ia2.theme")).toBe("dark")
    expect(localStorage.getItem("controlsoftware.theme")).toBeNull()
  })

  it("keeps a readable legacy preference when writing its migration is blocked", async () => {
    localStorage.setItem("controlsoftware.theme", "dark")
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new Error("denied") })
    await import("./dark-mode")
    expectTheme("dark")
  })

  it("works without storage and updates existing metadata without duplicates", async () => {
    document.head.insertAdjacentHTML("beforeend", '<meta name="theme-color" content="#000000">')
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => { throw new Error("denied") })
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new Error("denied") })
    const { setTheme } = await import("./dark-mode")
    expectTheme("light")
    setTheme("dark")
    expectTheme("dark")
    setTheme("light")
    expectTheme("light")
    expect(document.head.querySelectorAll('meta[name="theme-color"]')).toHaveLength(1)
  })

  it("updates mounted consumers from another window without writing back", async () => {
    const { setTheme, useDarkMode } = await import("./dark-mode")
    function Consumer() { return <output>{useDarkMode()}</output> }
    render(<Consumer />)
    const write = vi.spyOn(Storage.prototype, "setItem")
    await act(async () => storage("ia2.theme", "dark"))
    expectTheme("dark")
    expect(screen.getByRole("status").textContent).toBe("dark")
    expect(write).not.toHaveBeenCalled()
    await act(async () => setTheme("light"))
    expectTheme("light")
    expect(screen.getByRole("status").textContent).toBe("light")
    expect(write).toHaveBeenCalledWith("ia2.theme", "light")
  })

  it("ignores unrelated, session-storage and malformed changes, and handles removal/clear", async () => {
    const { setTheme } = await import("./dark-mode")
    setTheme("dark")
    storage("unrelated", "light")
    storage("ia2.theme", "light", sessionStorage)
    storage("ia2.theme", "invalid")
    expectTheme("dark")
    storage("ia2.theme", null)
    expectTheme("light")
    setTheme("dark")
    storage(null, null)
    expectTheme("light")
  })
})
