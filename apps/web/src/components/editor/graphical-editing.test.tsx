// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { useState } from "react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

vi.mock("@/state/runtime", () => ({
  useRuntime: () => ({ isRunning: false, running: null, tasks: { tasks: [], programs: [] }, projectEpoch: 0 }),
}))
vi.mock("@/state/live-feed", () => ({ useLastSnapshot: () => null }))
vi.mock("@/lib/api", async (original) => ({
  ...(await original<typeof import("@/lib/api")>()),
  checkProgram: vi.fn().mockResolvedValue([]),
}))

import { checkProgram } from "@/lib/api"
import { FBDEditor } from "./FBDEditor"
import { LDEditor } from "./LDEditor"

const variables = [
  { name: "start", type: "BOOL", section: "input", init: null },
  { name: "backup", type: "BOOL", section: "input", init: null },
  { name: "done", type: "BOOL", section: "output", init: null },
  { name: "spare", type: "BOOL", section: "output", init: null },
]
const contact = { op: "contact", var: "start", negated: false }
const timerInputs = [
  { pin: "IN", value: { kind: "var", name: "start" } },
  { pin: "PT", value: { kind: "literal", value: "T#1s" } },
]
const ld = {
  name: "main", pou_type: "program", variables,
  rungs: [{ id: "stable-rung", label: "Keep this label", logic: contact, coils: [{ var: "done", kind: "standard" }] }],
}
const ldTimer = {
  ...ld, rungs: [{ ...ld.rungs[0], logic: { op: "fb_call", fb_type: "TON", instance: "delay", inputs: timerInputs, output_pin: "Q" } }],
}
const fbd = {
  name: "main", pou_type: "program", variables,
  blocks: [
    { id: "first", fb_type: "TON", instance: "delay", inputs: timerInputs, position: { x: 40, y: 40 } },
    { id: "second", fb_type: "TON", instance: "next", inputs: timerInputs, position: { x: 300, y: 40 } },
  ],
  outputs: [{ variable: "done", from_block: "first", from_pin: "Q" }],
}

type Editor = typeof LDEditor
function mounted(Editor: Editor, initial: object, readOnly = false) {
  const changes: string[] = []
  function Harness() {
    const [value, setValue] = useState(JSON.stringify(initial))
    return <Editor value={value} readOnly={readOnly} path="main" onChange={(next) => { changes.push(next); setValue(next) }} />
  }
  const view = render(<Harness />)
  return { ...view, changes, document: () => JSON.parse(changes.at(-1) ?? JSON.stringify(initial)) }
}
function glyph(container: HTMLElement, label: string) {
  const text = [...container.querySelectorAll("svg text")].find((el) => el.textContent === label)
  expect(text, `glyph ${label}`).toBeDefined()
  return text!
}
function selectBlock(container: HTMLElement, index = 0) {
  fireEvent.pointerDown(container.querySelectorAll('rect[style*="cursor"]')[index * 2], { clientX: 100, clientY: 100 })
  fireEvent.pointerUp(container.querySelector("svg.select-none[viewBox]")!, { clientX: 100, clientY: 100 })
}
function geometry(container: HTMLElement, scale = 1) {
  const svg = container.querySelector("svg.select-none[viewBox]")!
  const [, , width, height] = svg.getAttribute("viewBox")!.split(" ").map(Number)
  vi.spyOn(svg, "getBoundingClientRect").mockReturnValue({ x: 0, y: 0, left: 0, top: 0, right: width * scale, bottom: height * scale, width: width * scale, height: height * scale, toJSON() {} })
  return svg
}

beforeEach(() => {
  vi.mocked(checkProgram).mockResolvedValue([])
  vi.stubGlobal("PointerEvent", MouseEvent)
  Element.prototype.setPointerCapture = vi.fn()
  Element.prototype.hasPointerCapture = vi.fn().mockReturnValue(false)
  Element.prototype.releasePointerCapture = vi.fn()
  Element.prototype.scrollIntoView = vi.fn()
})
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals() })

describe("LD editing through the public editor", () => {
  it("preserves rung identity and fields through serial/parallel edits and reopening", () => {
    const view = mounted(LDEditor, ld)
    fireEvent.click(glyph(view.container, "start"))
    fireEvent.click(screen.getByTitle("Insert a contact in series to the right"))
    expect(view.document().rungs[0].logic).toEqual({ op: "and", args: [contact, { ...contact, var: "x" }] })
    fireEvent.click(glyph(view.container, "start"))
    fireEvent.click(screen.getByTitle("Insert a contact in parallel below"))
    const saved = view.document()
    expect(saved).toEqual({ ...ld, rungs: [{ ...ld.rungs[0], logic: { op: "and", args: [{ op: "or", args: [contact, { ...contact, var: "x" }] }, { ...contact, var: "x" }] } }] })
    view.unmount()
    const reopened = mounted(LDEditor, saved)
    expect([...reopened.container.querySelectorAll("svg text")].filter((el) => el.textContent === "x")).toHaveLength(2)
    expect(reopened.document()).toEqual(saved)
  })

  it("keeps a contact draft across parent rerenders, commits Enter once, and cancels selection", () => {
    const onChange = vi.fn()
    const value = JSON.stringify(ld)
    const view = render(<LDEditor value={value} onChange={onChange} />)
    fireEvent.click(glyph(view.container, "start"))
    const input = screen.getByDisplayValue("start")
    fireEvent.change(input, { target: { value: "backup" } })
    view.rerender(<LDEditor value={value} onChange={onChange} className="rerendered" />)
    expect(screen.getByDisplayValue("backup")).toBe(input)
    expect(onChange).not.toHaveBeenCalled()
    fireEvent.keyDown(input, { key: "Enter" })
    expect(onChange).toHaveBeenCalledTimes(1)
    const saved = JSON.parse(onChange.mock.calls[0][0])
    expect(saved.rungs[0].logic).toEqual({ ...contact, var: "backup" })
    view.rerender(<LDEditor value={JSON.stringify(saved)} onChange={onChange} />)
    expect(screen.queryByDisplayValue("backup")).toBeNull()
    fireEvent.click(glyph(view.container, "backup"))
    fireEvent.click(glyph(view.container, "backup"))
    expect(screen.queryByDisplayValue("backup")).toBeNull()
  })

  it("edits coil bindings on blur and supports repeated contact edits", () => {
    const view = mounted(LDEditor, ld)
    fireEvent.click(glyph(view.container, "done"))
    fireEvent.change(screen.getByDisplayValue("done"), { target: { value: "spare" } })
    fireEvent.blur(screen.getByDisplayValue("spare"))
    expect(view.document().rungs[0].coils).toEqual([{ var: "spare", kind: "standard" }])
    for (const negated of [true, false]) {
      fireEvent.click(glyph(view.container, "start"))
      fireEvent.click(screen.getByTitle("Toggle normally-closed"))
      expect(view.document().rungs[0].logic.negated).toBe(negated)
    }
    expect(view.changes).toHaveLength(3)
  })

  it("edits compare operands without changing the comparator or surrounding rung", () => {
    const compare = { op: "compare", left: { kind: "literal", value: "1" }, cmp: "lt", right: { kind: "literal", value: "5" } }
    const seed = { ...ld, rungs: [{ ...ld.rungs[0], logic: compare }] }
    const view = mounted(LDEditor, seed)
    fireEvent.click(glyph(view.container, "<"))
    fireEvent.change(screen.getByDisplayValue("5"), { target: { value: "10" } })
    fireEvent.blur(screen.getByDisplayValue("10"))
    expect(view.document()).toEqual({ ...seed, rungs: [{ ...seed.rungs[0], logic: { ...compare, right: { kind: "literal", value: "10" } } }] })
  })

  it("edits function-block identity and pin literals without losing other bindings", () => {
    const view = mounted(LDEditor, ldTimer)
    fireEvent.click(glyph(view.container, "delay"))
    const instance = screen.getByTitle("FB instance name")
    fireEvent.change(instance, { target: { value: "wait_timer" } })
    fireEvent.keyDown(instance, { key: "Enter" })
    fireEvent.click(glyph(view.container, "wait_timer"))
    fireEvent.change(screen.getByDisplayValue("T#1s"), { target: { value: "T#2s" } })
    fireEvent.blur(screen.getByDisplayValue("T#2s"))
    expect(view.document().rungs[0].logic).toEqual({ ...ldTimer.rungs[0].logic, instance: "wait_timer", inputs: [timerInputs[0], { pin: "PT", value: { kind: "literal", value: "T#2s" } }] })
    expect(view.changes).toHaveLength(2)
  })
})

describe("FBD editing through the public editor", () => {
  it("keeps a draft through parent rerenders and discards it on external document replacement", () => {
    const onChange = vi.fn()
    const value = JSON.stringify(fbd)
    const view = render(<FBDEditor value={value} onChange={onChange} />)
    geometry(view.container)
    selectBlock(view.container)
    const input = screen.getByTitle("FB instance name")
    fireEvent.change(input, { target: { value: "unfinished" } })
    view.rerender(<FBDEditor value={value} onChange={onChange} className="rerendered" />)
    expect(screen.getByDisplayValue("unfinished")).toBe(input)
    view.rerender(<FBDEditor value={JSON.stringify({ ...fbd, blocks: [fbd.blocks[1]] })} onChange={onChange} />)
    expect(screen.queryByTitle("FB instance name")).toBeNull()
    expect(onChange).not.toHaveBeenCalled()
  })

  it("commits instance and operand edits, then reopens the saved document", () => {
    const view = mounted(FBDEditor, fbd)
    geometry(view.container)
    selectBlock(view.container)
    fireEvent.change(screen.getByTitle("FB instance name"), { target: { value: "wait_timer" } })
    fireEvent.keyDown(screen.getByTitle("FB instance name"), { key: "Enter" })
    selectBlock(view.container)
    fireEvent.change(screen.getByDisplayValue("T#1s"), { target: { value: "T#2s" } })
    fireEvent.blur(screen.getByDisplayValue("T#2s"))
    const saved = view.document()
    expect(saved.blocks).toEqual([{ ...fbd.blocks[0], instance: "wait_timer", inputs: [timerInputs[0], { pin: "PT", value: { kind: "literal", value: "T#2s" } }] }, fbd.blocks[1]])
    expect(saved.outputs).toEqual(fbd.outputs)
    expect(view.changes).toHaveLength(2)
    view.unmount()
    const reopened = mounted(FBDEditor, saved)
    expect(glyph(reopened.container, "wait_timer")).toBeDefined()
    expect(reopened.document()).toEqual(saved)
  })

  it("moves a block in SVG coordinates across repeated pointer moves", () => {
    const view = mounted(FBDEditor, fbd)
    const svg = geometry(view.container, 0.5)
    fireEvent.pointerDown(view.container.querySelector('rect[style*="cursor"]')!, { clientX: 40, clientY: 40 })
    fireEvent.pointerMove(svg, { clientX: 60, clientY: 50 })
    fireEvent.pointerMove(svg, { clientX: 70, clientY: 60 })
    fireEvent.pointerUp(svg, { clientX: 70, clientY: 60 })
    expect(view.document().blocks).toEqual([{ ...fbd.blocks[0], position: { x: 100, y: 80 } }, fbd.blocks[1]])
    expect(view.document().outputs).toEqual(fbd.outputs)
  })

  it("connects ports and removes both input and output references when deleting the source", () => {
    const view = mounted(FBDEditor, fbd)
    const svg = geometry(view.container)
    fireEvent.pointerDown(svg.querySelector('circle[r="9"]')!, { clientX: 208, clientY: 77 })
    fireEvent.pointerMove(svg, { clientX: 292, clientY: 75 })
    fireEvent.pointerUp(svg, { clientX: 292, clientY: 75 })
    expect(view.document().blocks[1].inputs[0]).toEqual({ pin: "IN", value: { kind: "block", block_id: "first", pin: "Q" } })
    selectBlock(view.container)
    fireEvent.click(screen.getByTitle("Delete this block"))
    expect(view.document().blocks).toHaveLength(1)
    expect(view.document().blocks[0].id).toBe("second")
    expect(view.document().blocks[0].inputs[0].value).toEqual({ kind: "literal", value: "FALSE" })
    expect(view.document().outputs).toEqual([])
  })

  it("adds and removes output bindings without changing block identities or inputs", () => {
    const view = mounted(FBDEditor, fbd)
    fireEvent.click(screen.getByRole("button", { name: /^add$/ }))
    expect(view.document().outputs).toEqual([...fbd.outputs, { variable: "spare", from_block: "first", from_pin: "Q" }])
    fireEvent.click(screen.getByTitle("Unbind done"))
    expect(view.document()).toEqual({ ...fbd, outputs: [{ variable: "spare", from_block: "first", from_pin: "Q" }] })
  })
})

describe("editor boundaries", () => {
  it.each([["LD", LDEditor, ld], ["FBD", FBDEditor, fbd]] as const)("%s does not write in read-only mode", (_, Editor, seed) => {
    const view = mounted(Editor, seed, true)
    if (Editor === LDEditor) fireEvent.click(glyph(view.container, "start"))
    else { geometry(view.container); selectBlock(view.container) }
    expect(screen.queryByTitle("FB instance name")).toBeNull()
    expect(screen.queryByTitle("Toggle normally-closed")).toBeNull()
    expect(view.changes).toEqual([])
  })

  it("locates LD diagnostics at the same rung and can jump to its coil", async () => {
    vi.mocked(checkProgram).mockResolvedValue([{ severity: "error", message: "missing coil", code: "test", fbd_location: null, sfc_location: null, context: [], related: [], explanation: null, start_line: 1, start_column: 1, end_line: 1, end_column: 1, ld_location: { kind: "coil", rung_id: "stable-rung", coil_index: 0 } }])
    const view = mounted(LDEditor, ld)
    await waitFor(() => expect(screen.getByLabelText("rung has errors")).toBeDefined())
    expect(view.container.querySelector('[title="missing coil"]')).not.toBeNull()
    fireEvent.click(screen.getByText("missing coil"))
    expect(screen.getByDisplayValue("done")).toBeDefined()
  })

  it("locates FBD diagnostics on the original block", async () => {
    vi.mocked(checkProgram).mockResolvedValue([{ severity: "error", message: "bad pin", code: "test", ld_location: null, sfc_location: null, context: [], related: [], explanation: null, start_line: 1, start_column: 1, end_line: 1, end_column: 1, fbd_location: { kind: "block", block_id: "second" } }])
    const view = mounted(FBDEditor, fbd)
    await waitFor(() => expect(screen.getByText("bad pin")).toBeDefined())
    expect(view.container.querySelector('rect.stroke-destructive')?.getAttribute("x")).toBe("300")
  })
})
