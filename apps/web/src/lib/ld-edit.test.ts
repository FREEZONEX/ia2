import { describe, expect, it } from "vitest"

import type { LdNode } from "@/types/generated/LdNode"
import type { LdProgram } from "@/types/generated/LdProgram"
import {
  addCoil,
  addInParallel,
  addInSeries,
  addRung,
  addVariable,
  deleteCoil,
  deleteNode,
  deleteRung,
  getNode,
  moveRung,
  newContact,
  parseProgram,
  removeVariable,
  serializeProgram,
  setCoilKind,
  setContactVar,
  toggleNegated,
  updateVariable,
} from "./ld-edit"

/** Minimal program with one rung whose logic is a single contact
 *  driving one coil. Used as the starting point for most tests. */
function seed(): LdProgram {
  return {
    name: "p",
    pou_type: "program",
    variables: [
      { name: "a", type: "BOOL", section: "input", init: null },
      { name: "out", type: "BOOL", section: "output", init: null },
    ],
    rungs: [
      {
        id: "r0",
        label: null,
        logic: { op: "contact", var: "a", negated: false },
        coils: [{ var: "out", kind: "standard" }],
      },
    ],
  }
}

describe("getNode", () => {
  it("returns root for empty path", () => {
    const p = seed()
    expect(getNode(p.rungs[0].logic, [])).toEqual({
      op: "contact",
      var: "a",
      negated: false,
    })
  })

  it("descends into AND args", () => {
    const root: LdNode = {
      op: "and",
      args: [
        { op: "contact", var: "a", negated: false },
        { op: "contact", var: "b", negated: true },
      ],
    }
    expect(getNode(root, [1])).toMatchObject({ var: "b", negated: true })
  })

  it("throws on out-of-range step", () => {
    const root: LdNode = { op: "and", args: [{ op: "contact", var: "a", negated: false }] }
    expect(() => getNode(root, [5])).toThrow(/out of range/)
  })

  it("descends into NOT's only child", () => {
    const root: LdNode = {
      op: "not",
      arg: { op: "contact", var: "a", negated: false },
    }
    expect(getNode(root, [0])).toMatchObject({ var: "a" })
  })

  it("rejects non-zero step on NOT", () => {
    const root: LdNode = {
      op: "not",
      arg: { op: "contact", var: "a", negated: false },
    }
    expect(() => getNode(root, [1])).toThrow(/only child 0/)
  })
})

describe("addInSeries", () => {
  it("wraps a leaf root in AND", () => {
    const out = addInSeries(seed(), 0, [], "after", newContact("b"))
    expect(out.rungs[0].logic).toEqual({
      op: "and",
      args: [
        { op: "contact", var: "a", negated: false },
        { op: "contact", var: "b", negated: false },
      ],
    })
  })

  it("appends to an existing AND parent (no extra wrapping)", () => {
    let p = seed()
    p = addInSeries(p, 0, [], "after", newContact("b"))
    // Now root is AND(a, b). Add `c` after `b` (path = [1]).
    p = addInSeries(p, 0, [1], "after", newContact("c"))
    const logic = p.rungs[0].logic
    expect(logic.op).toBe("and")
    if (logic.op !== "and") return
    expect(logic.args.map((n) => (n.op === "contact" ? n.var : "?"))).toEqual([
      "a",
      "b",
      "c",
    ])
  })

  it("inserts on the left when side='before'", () => {
    let p = seed()
    p = addInSeries(p, 0, [], "after", newContact("b"))
    p = addInSeries(p, 0, [0], "before", newContact("z"))
    const logic = p.rungs[0].logic
    if (logic.op !== "and") throw new Error("expected and")
    expect(logic.args.map((n) => (n.op === "contact" ? n.var : "?"))).toEqual([
      "z",
      "a",
      "b",
    ])
  })
})

describe("addInParallel", () => {
  it("wraps a leaf root in OR", () => {
    const out = addInParallel(seed(), 0, [], "after", newContact("b"))
    expect(out.rungs[0].logic).toEqual({
      op: "or",
      args: [
        { op: "contact", var: "a", negated: false },
        { op: "contact", var: "b", negated: false },
      ],
    })
  })

  it("appends to existing OR without re-wrapping", () => {
    let p = seed()
    p = addInParallel(p, 0, [], "after", newContact("b"))
    p = addInParallel(p, 0, [], "after", newContact("c"))
    const logic = p.rungs[0].logic
    if (logic.op !== "or") throw new Error("expected or")
    expect(logic.args).toHaveLength(3)
  })

  it("turns a nested branch into AND(OR(...))", () => {
    let p = seed()
    p = addInSeries(p, 0, [], "after", newContact("b")) // AND(a, b)
    p = addInParallel(p, 0, [0], "after", newContact("a2")) // AND(OR(a, a2), b)
    const logic = p.rungs[0].logic
    if (logic.op !== "and") throw new Error("expected and root")
    const first = logic.args[0]
    expect(first).toMatchObject({ op: "or" })
  })
})

describe("deleteNode", () => {
  it("collapses singleton-arg parent after deletion", () => {
    let p = seed()
    p = addInSeries(p, 0, [], "after", newContact("b"))
    // AND(a, b). Delete `b` (path [1]). Should collapse back to just `a`.
    p = deleteNode(p, 0, [1])
    expect(p.rungs[0].logic).toEqual({
      op: "contact",
      var: "a",
      negated: false,
    })
  })

  it("replaces the whole logic with const true when root is deleted", () => {
    const out = deleteNode(seed(), 0, [])
    expect(out.rungs[0].logic).toEqual({ op: "const", value: true })
  })

  it("collapses NOT to const true when its child is deleted", () => {
    const p: LdProgram = {
      ...seed(),
      rungs: [
        {
          id: "r",
          label: null,
          logic: { op: "not", arg: { op: "contact", var: "a", negated: false } },
          coils: [{ var: "out", kind: "standard" }],
        },
      ],
    }
    const out = deleteNode(p, 0, [0])
    expect(out.rungs[0].logic).toEqual({ op: "const", value: true })
  })
})

describe("toggleNegated", () => {
  it("flips the negated flag on a contact", () => {
    const out = toggleNegated(seed(), 0, [])
    expect((out.rungs[0].logic as Extract<LdNode, { op: "contact" }>).negated).toBe(true)
    const back = toggleNegated(out, 0, [])
    expect((back.rungs[0].logic as Extract<LdNode, { op: "contact" }>).negated).toBe(false)
  })

  it("is a no-op on non-contact nodes", () => {
    let p = seed()
    p = addInSeries(p, 0, [], "after", newContact("b"))
    const out = toggleNegated(p, 0, [])
    expect(out).toEqual(p)
  })
})

describe("setContactVar", () => {
  it("renames the variable a contact references", () => {
    const out = setContactVar(seed(), 0, [], "renamed")
    expect((out.rungs[0].logic as Extract<LdNode, { op: "contact" }>).var).toBe(
      "renamed",
    )
  })
})

describe("rung-level ops", () => {
  it("addRung appends with auto-generated id", () => {
    const out = addRung(seed())
    expect(out.rungs).toHaveLength(2)
    expect(out.rungs[1].id).toBe("r1")
  })

  it("addRung at index 0 prepends", () => {
    const out = addRung(seed(), 0)
    expect(out.rungs[0].id).toBe("r1")
    expect(out.rungs[1].id).toBe("r0")
  })

  it("deleteRung removes the targeted rung", () => {
    let p = addRung(seed())
    p = deleteRung(p, 0)
    expect(p.rungs).toHaveLength(1)
    expect(p.rungs[0].id).toBe("r1")
  })

  it("moveRung swaps positions", () => {
    const p = addRung(seed()) // r0, r1
    const out = moveRung(p, 0, 1) // r1, r0
    expect(out.rungs.map((r) => r.id)).toEqual(["r1", "r0"])
  })
})

describe("coil ops", () => {
  it("addCoil appends a standard coil", () => {
    const out = addCoil(seed(), 0, "second_out")
    expect(out.rungs[0].coils).toHaveLength(2)
    expect(out.rungs[0].coils[1]).toEqual({ var: "second_out", kind: "standard" })
  })

  it("deleteCoil removes by index", () => {
    let p = addCoil(seed(), 0, "second_out")
    p = deleteCoil(p, 0, 0)
    expect(p.rungs[0].coils).toEqual([{ var: "second_out", kind: "standard" }])
  })

  it("setCoilKind changes the latch type", () => {
    const out = setCoilKind(seed(), 0, 0, "set")
    expect(out.rungs[0].coils[0].kind).toBe("set")
  })
})

describe("variable ops", () => {
  it("addVariable refuses duplicates", () => {
    const out = addVariable(seed(), {
      name: "a",
      type: "BOOL",
      section: "internal",
      init: null,
    })
    expect(out.variables).toHaveLength(2) // unchanged
  })

  it("removeVariable drops by name", () => {
    const out = removeVariable(seed(), "a")
    expect(out.variables).toHaveLength(1)
    expect(out.variables[0].name).toBe("out")
  })

  it("updateVariable patches in place", () => {
    const out = updateVariable(seed(), "a", { init: "FALSE" })
    expect(out.variables.find((v) => v.name === "a")?.init).toBe("FALSE")
  })
})

describe("round-trip", () => {
  it("parseProgram(serializeProgram(p)) === p", () => {
    const p = seed()
    const back = parseProgram(serializeProgram(p))
    expect(back).toEqual(p)
  })
})

// =================================================================
//   evaluateNode — online-mode evaluator
// =================================================================
import { evaluateNode, readingsFrom } from "./ld-edit"

describe("evaluateNode", () => {
  it("contact conducts when var is true and not negated", () => {
    expect(
      evaluateNode({ op: "contact", var: "a", negated: false }, readingsFrom({ a: true })),
    ).toBe(true)
    expect(
      evaluateNode({ op: "contact", var: "a", negated: false }, readingsFrom({ a: false })),
    ).toBe(false)
  })

  it("negated contact inverts", () => {
    expect(
      evaluateNode({ op: "contact", var: "a", negated: true }, readingsFrom({ a: true })),
    ).toBe(false)
    expect(
      evaluateNode({ op: "contact", var: "a", negated: true }, readingsFrom({ a: false })),
    ).toBe(true)
  })

  it("a variable with no live value is unknown, not FALSE", () => {
    // Reading it as FALSE drew an NC contact on an unpublished variable as
    // closed — e.g. a pressed e-stop whose name arrived instance-qualified.
    expect(
      evaluateNode({ op: "contact", var: "missing", negated: false }, readingsFrom({})),
    ).toBe(null)
    expect(
      evaluateNode({ op: "contact", var: "missing", negated: true }, readingsFrom({})),
    ).toBe(null)
  })

  it("settles what the known values settle, and nothing more", () => {
    const a = { op: "contact", var: "a", negated: false } as const
    const u = { op: "contact", var: "unknown", negated: false } as const
    const and = (...args: LdNode[]): LdNode => ({ op: "and", args })
    const or = (...args: LdNode[]): LdNode => ({ op: "or", args })
    const off = readingsFrom({ a: false })
    const on = readingsFrom({ a: true })
    expect(evaluateNode(and(a, u), off)).toBe(false)
    expect(evaluateNode(and(a, u), on)).toBe(null)
    expect(evaluateNode(or(a, u), on)).toBe(true)
    expect(evaluateNode(or(a, u), off)).toBe(null)
    expect(evaluateNode({ op: "not", arg: u }, on)).toBe(null)
    expect(
      evaluateNode(
        { op: "compare", left: { kind: "var", name: "temp" }, cmp: "gt", right: { kind: "literal", value: "50" } },
        readingsFrom({}, {}),
      ),
    ).toBe(null)
    expect(
      evaluateNode(
        { op: "compare", left: { kind: "var", name: "temp" }, cmp: "gt", right: { kind: "literal", value: "50" } },
        readingsFrom({}, { temp: 60 }),
      ),
    ).toBe(true)
  })

  it("AND requires all children", () => {
    const tree: LdNode = {
      op: "and",
      args: [
        { op: "contact", var: "a", negated: false },
        { op: "contact", var: "b", negated: false },
      ],
    }
    expect(evaluateNode(tree, readingsFrom({ a: true, b: true }))).toBe(true)
    expect(evaluateNode(tree, readingsFrom({ a: true, b: false }))).toBe(false)
    expect(evaluateNode(tree, readingsFrom({ a: false, b: true }))).toBe(false)
  })

  it("OR fires when any child fires", () => {
    const tree: LdNode = {
      op: "or",
      args: [
        { op: "contact", var: "a", negated: false },
        { op: "contact", var: "b", negated: false },
      ],
    }
    expect(evaluateNode(tree, readingsFrom({ a: false, b: false }))).toBe(false)
    expect(evaluateNode(tree, readingsFrom({ a: true, b: false }))).toBe(true)
    expect(evaluateNode(tree, readingsFrom({ a: false, b: true }))).toBe(true)
  })

  it("evaluates the seal-in pattern correctly", () => {
    // network: start OR (motor_run AND NOT stop)
    const tree: LdNode = {
      op: "or",
      args: [
        { op: "contact", var: "start", negated: false },
        {
          op: "and",
          args: [
            { op: "contact", var: "motor_run", negated: false },
            { op: "contact", var: "stop", negated: true },
          ],
        },
      ],
    }
    const at = (start: boolean, motor_run: boolean, stop: boolean) =>
      evaluateNode(tree, readingsFrom({ start, motor_run, stop }))
    // not yet started
    expect(at(false, false, false)).toBe(false)
    // press start
    expect(at(true, false, false)).toBe(true)
    // start released, motor running, stop not pressed -> sealed in
    expect(at(false, true, false)).toBe(true)
    // stop pressed releases
    expect(at(false, true, true)).toBe(false)
  })

  it("empty AND/OR collapse to identity", () => {
    expect(evaluateNode({ op: "and", args: [] }, readingsFrom({}))).toBe(true)
    expect(evaluateNode({ op: "or", args: [] }, readingsFrom({}))).toBe(false)
  })

  it("treats omitted `negated` field as false (serde-default round-trip)", () => {
    // Real .ld.json files often omit `negated: false`, relying on
    // serde's #[default]. On the TS side this comes through as
    // undefined, and a naïve `var !== undefined` would always be
    // true. Regression test for that exact bug.
    const node = { op: "contact", var: "x" } as unknown as LdNode
    expect(evaluateNode(node, readingsFrom({ x: false }))).toBe(false)
    expect(evaluateNode(node, readingsFrom({ x: true }))).toBe(true)
    // explicit-false should match the omitted case
    expect(
      evaluateNode(
        { op: "contact", var: "x", negated: false } as LdNode, readingsFrom({ x: false })),
    ).toBe(false)
  })

  it("fb_call reads instance.outputPin as a dotted variable name", () => {
    // The evaluator can't simulate a TON itself; it reads whatever
    // value the runtime exposes under `inst.Q` (or `.QU`, `.Q1`...).
    // Missing — the normal case, the runtime publishes the instance only —
    // is unknown; present is used.
    const node: LdNode = {
      op: "fb_call",
      instance: "myT",
      fb_type: "TON",
      inputs: [],
      output_pin: "Q",
    }
    expect(evaluateNode(node, readingsFrom({}))).toBe(null)
    expect(evaluateNode(node, readingsFrom({ "myT.Q": false }))).toBe(false)
    expect(evaluateNode(node, readingsFrom({ "myT.Q": true }))).toBe(true)
    // Other instances or other pins must not be picked up
    expect(evaluateNode(node, readingsFrom({ "other.Q": true }))).toBe(null)
    expect(evaluateNode(node, readingsFrom({ "myT.ET": true }))).toBe(null)
  })

  it("fb_call participates in AND/OR like any other boolean leaf", () => {
    // The whole point of FbCall in the expression position is that it
    // composes with contact/compare in the same tree shape. Sanity
    // check end-to-end.
    const tree: LdNode = {
      op: "and",
      args: [
        { op: "contact", var: "btn", negated: false },
        {
          op: "fb_call",
          instance: "myT",
          fb_type: "TON",
          inputs: [],
          output_pin: "Q",
        },
      ],
    }
    // btn down but timer not done → false
    expect(evaluateNode(tree, readingsFrom({ btn: true, "myT.Q": false }))).toBe(false)
    // btn down + timer done → true (this is the "delayed start" pattern)
    expect(evaluateNode(tree, readingsFrom({ btn: true, "myT.Q": true }))).toBe(true)
    // btn up overrides timer
    expect(evaluateNode(tree, readingsFrom({ btn: false, "myT.Q": true }))).toBe(false)
    // …even when the timer's output is not published at all
    expect(evaluateNode(tree, readingsFrom({ btn: false }))).toBe(false)
    expect(evaluateNode(tree, readingsFrom({ btn: true }))).toBe(null)
  })
})

import { newFbCall, setFbInputValue, setFbType, updateFbCall } from "./ld-edit"

describe("FbCall editing", () => {
  function seedWithFb(instanceName = "myT1"): LdProgram {
    return {
      name: "p",
      pou_type: "program",
      variables: [
        { name: "btn", type: "BOOL", section: "input", init: null },
        { name: "out", type: "BOOL", section: "output", init: null },
      ],
      rungs: [
        {
          id: "r0",
          label: null,
          logic: {
            op: "fb_call",
            instance: instanceName,
            fb_type: "TON",
            inputs: [
              { pin: "IN", value: { kind: "var", name: "btn" } },
              { pin: "PT", value: { kind: "literal", value: "T#3s" } },
            ],
            output_pin: "Q",
          },
          coils: [{ var: "out", kind: "standard" }],
        },
      ],
    }
  }

  it("newFbCall picks a unique instance name", () => {
    const seed = seedWithFb()
    const { node, instance } = newFbCall(seed, "TON")
    // `myT1` already-in-use → must be myT2
    expect(instance).toBe("myT2")
    expect(node.op).toBe("fb_call")
    if (node.op === "fb_call") {
      expect(node.fb_type).toBe("TON")
      expect(node.output_pin).toBe("Q")
      // Inputs are populated with sensible defaults for each pin
      expect(node.inputs.map((i) => i.pin)).toEqual(["IN", "PT"])
    }
  })

  // IEC names are case-insensitive and the transpiler declares each
  // instance next to the variables, so both kinds of name are taken.
  it("newFbCall avoids variables and other spellings of taken names", () => {
    const seed = seedWithFb("MYT1")
    seed.variables.push({ name: "MyT2", type: "BOOL", section: "internal", init: null })
    expect(newFbCall(seed, "TON").instance).toBe("myT3")
  })

  it("newFbCall starts at instance suffix 1 when none in use", () => {
    const empty: LdProgram = {
      name: "p",
      pou_type: "program",
      variables: [],
      rungs: [],
    }
    expect(newFbCall(empty, "CTU").instance).toBe("myCnt1")
    expect(newFbCall(empty, "R_TRIG").instance).toBe("myEdge1")
  })

  it("setFbType TON→TOF preserves operands for shared pin names", () => {
    // TON and TOF share IN and PT, so the user shouldn't lose what
    // they wired when swapping FB type.
    const next = setFbType(seedWithFb(), 0, [], "TOF")
    const node = next.rungs[0].logic
    expect(node.op).toBe("fb_call")
    if (node.op === "fb_call") {
      expect(node.fb_type).toBe("TOF")
      expect(node.inputs).toEqual([
        { pin: "IN", value: { kind: "var", name: "btn" } },
        { pin: "PT", value: { kind: "literal", value: "T#3s" } },
      ])
      expect(node.output_pin).toBe("Q") // still valid for TOF
    }
  })

  it("setFbType TON→CTU resets inputs to new pin set", () => {
    const next = setFbType(seedWithFb(), 0, [], "CTU")
    const node = next.rungs[0].logic
    expect(node.op).toBe("fb_call")
    if (node.op === "fb_call") {
      expect(node.fb_type).toBe("CTU")
      // CTU pins: CU, R, PV (TON's IN/PT do not survive)
      expect(node.inputs.map((i) => i.pin)).toEqual(["CU", "R", "PV"])
    }
  })

  it("setFbType CTUD→TON falls back from QD to a valid output_pin", () => {
    const start: LdProgram = {
      name: "p",
      pou_type: "program",
      variables: [],
      rungs: [
        {
          id: "r",
          label: null,
          logic: {
            op: "fb_call",
            instance: "c",
            fb_type: "CTUD",
            inputs: [],
            output_pin: "QD", // CTUD has both QU and QD; TON has only Q
          },
          coils: [{ var: "x", kind: "standard" }],
        },
      ],
    }
    const next = setFbType(start, 0, [], "TON")
    const node = next.rungs[0].logic
    if (node.op === "fb_call") {
      expect(node.output_pin).toBe("Q")
    }
  })

  it("setFbInputValue replaces only the matching pin", () => {
    const next = setFbInputValue(
      seedWithFb(),
      0,
      [],
      "PT",
      { kind: "literal", value: "T#10s" },
    )
    const node = next.rungs[0].logic
    if (node.op === "fb_call") {
      const pt = node.inputs.find((i) => i.pin === "PT")
      expect(pt?.value).toEqual({ kind: "literal", value: "T#10s" })
      // IN binding untouched
      const inp = node.inputs.find((i) => i.pin === "IN")
      expect(inp?.value).toEqual({ kind: "var", name: "btn" })
    }
  })

  it("updateFbCall lets us change instance and output_pin", () => {
    const renamed = updateFbCall(seedWithFb(), 0, [], { instance: "delay" })
    if (renamed.rungs[0].logic.op === "fb_call") {
      expect(renamed.rungs[0].logic.instance).toBe("delay")
    }
  })
})
