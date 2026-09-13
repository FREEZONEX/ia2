// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react"
import { afterEach, expect, it, vi } from "vitest"

import { ProjectTree } from "./ProjectTree"

vi.mock("./HmiSection", () => ({ HmiSection: () => null }))
vi.mock("@/state/runtime", () => ({ useRuntime: () => ({
  view: "app", currentPou: { path: "controls/main" },
  project: {
    pous: [{ path: "controls/main", declarations: [{ name: "main", type: "program", language: "st" }] }],
    pou_folders: ["controls"], devices: [], device_folders: [], edges: [], edge_folders: [],
    tasks: { tasks: [], programs: [] }, iomap: { mappings: [] },
  },
}) }))
afterEach(cleanup)

it("collapses a default-open folder on the first click and can reopen it", () => {
  render(<ProjectTree />)
  const folder = screen.getByRole("button", { name: "controls" })
  expect(folder.getAttribute("aria-expanded")).toBe("true")
  expect(screen.getByRole("button", { name: /main.*PRG.*ST/ }).getAttribute("aria-current")).toBe("page")
  fireEvent.click(folder)
  expect(folder.getAttribute("aria-expanded")).toBe("false")
  expect(screen.queryByRole("button", { name: /main.*PRG.*ST/ })).toBeNull()
  fireEvent.click(folder)
  expect(folder.getAttribute("aria-expanded")).toBe("true")
  expect(screen.getByRole("button", { name: /main.*PRG.*ST/ })).not.toBeNull()
  expect(screen.getByRole("button", { name: "New POU" })).not.toBeNull()
})
