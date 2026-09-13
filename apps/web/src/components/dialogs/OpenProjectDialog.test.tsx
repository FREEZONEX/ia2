// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

import { OpenProjectDialog } from "./OpenProjectDialog"
import { NewProjectDialog } from "./NewProjectDialog"

const { openProject, createProject, browseFs } = vi.hoisted(() => ({
  openProject: vi.fn(),
  createProject: vi.fn(),
  browseFs: vi.fn(),
}))

vi.mock("@/state/runtime", () => ({ useRuntime: () => ({ openProject, createProject }) }))
vi.mock("@/lib/api", () => ({ browseFs }))

beforeEach(() => {
  vi.clearAllMocks()
  browseFs.mockResolvedValue({ path: "C:\\Projects", parent: "C:\\", is_project: false, entries: [] })
})
afterEach(cleanup)

async function openDialog() {
  render(<OpenProjectDialog trigger={<button>Browse projects</button>} />)
  fireEvent.click(screen.getByRole("button", { name: "Browse projects" }))
  const dialog = await screen.findByRole("dialog")
  await waitFor(() => expect(browseFs).toHaveBeenCalledOnce())
  return within(dialog)
}

describe("project opening feedback", () => {
  it("keeps the failed path editable and closes only after a successful retry", async () => {
    openProject.mockResolvedValueOnce(false).mockResolvedValueOnce(true)
    const dialog = await openDialog()
    const path = dialog.getByLabelText("Project folder path") as HTMLInputElement
    fireEvent.change(path, { target: { value: "C:\\项目 测试\\controller" } })
    fireEvent.click(dialog.getByRole("button", { name: "Open project" }))
    await screen.findByRole("alert")
    expect(path.value).toBe("C:\\项目 测试\\controller")
    expect(path.disabled).toBe(false)
    expect(screen.queryByRole("dialog")).not.toBeNull()
    fireEvent.click(dialog.getByRole("button", { name: "Open project" }))
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull())
    expect(openProject).toHaveBeenNthCalledWith(2, "C:\\项目 测试\\controller")
  })

  it("does not issue duplicate open requests while the first is pending", async () => {
    let finish!: (ok: boolean) => void
    openProject.mockReturnValue(new Promise<boolean>((resolve) => { finish = resolve }))
    const dialog = await openDialog()
    const path = dialog.getByLabelText("Project folder path") as HTMLInputElement
    fireEvent.change(path, { target: { value: "C:\\Projects\\test" } })
    fireEvent.keyDown(path, { key: "Enter" })
    fireEvent.keyDown(path, { key: "Enter" })
    expect(openProject).toHaveBeenCalledOnce()
    expect(path.disabled).toBe(true)
    await act(async () => finish(false))
    expect(path.disabled).toBe(false)
  })
})


describe("project dialogs controlled by the project menu", () => {
  it("opens and dismisses the folder picker without an in-dialog trigger", async () => {
    const onOpenChange = vi.fn()
    const { rerender } = render(<OpenProjectDialog open onOpenChange={onOpenChange} />)
    await screen.findByRole("dialog")
    await waitFor(() => expect(browseFs).toHaveBeenCalledOnce())
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }))
    expect(onOpenChange).toHaveBeenCalledExactlyOnceWith(false)
    rerender(<OpenProjectDialog open={false} onOpenChange={onOpenChange} />)
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull())
  })

  it("reports successful project creation to the controlling menu", async () => {
    createProject.mockResolvedValue(true)
    const onOpenChange = vi.fn()
    const { rerender } = render(<NewProjectDialog open onOpenChange={onOpenChange} />)
    await screen.findByRole("dialog")
    fireEvent.change(screen.getByLabelText("Project name"), { target: { value: "control" } })
    fireEvent.click(screen.getByRole("button", { name: "Create" }))
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledExactlyOnceWith(false))
    expect(createProject).toHaveBeenCalledExactlyOnceWith("control")
    rerender(<NewProjectDialog open={false} onOpenChange={onOpenChange} />)
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull())
  })
})
