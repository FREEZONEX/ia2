import { describe, expect, it } from "vitest"

import type { FbdProgram } from "@/types/generated/FbdProgram"
import {
  addBlock,
  connectWire,
  disconnectWire,
  parseProgram,
  removeBlock,
  removeOutputBinding,
  serializeProgram,
  setBlockFbType,
  setBlockInput,
  setBlockInstance,
  setBlockPosition,
  setOutputBinding,
} from "./fbd-edit"

/** Minimal valid FBD program: one TON block, no wires, no outputs. */
function seed(): FbdProgram {
  return {
    name: "demo",
    pou_type: "program",
    variables: [
      { name: "btn", type: "BOOL", section: "input", init: null },
      { name: "done", type: "BOOL", section: "output", init: null },
    ],
    blocks: [
      {
        id: "b0",
        fb_type: "TON",
        instance: "myT1",
        inputs: [
          { pin: "IN", value: { kind: "var", name: "btn" } },
          { pin: "PT", value: { kind: "literal", value: "T#3s" } },
        ],
        position: null,
      },
    ],
    outputs: [],
  }
}

describe("addBlock", () => {
  it("adds a block with unique id and instance", () => {
    const { prog, blockId, instance } = addBlock(seed(), "TON")
    expect(blockId).toBe("b1")
    expect(instance).toBe("myT2") // myT1 in seed
    expect(prog.blocks.length).toBe(2)
    expect(prog.blocks[1].fb_type).toBe("TON")
  })

  // IEC names are case-insensitive and the transpiler declares each
  // instance next to the variables, so both kinds of name are taken.
  it("avoids variables and other spellings of taken names", () => {
    const start = seed()
    start.blocks[0].instance = "MYT1"
    start.variables.push({ name: "MyT2", type: "BOOL", section: "internal", init: null })
    expect(addBlock(start, "TON").instance).toBe("myT3")
  })

  it("populates inputs with defaults from the FB metadata", () => {
    const empty: FbdProgram = {
      name: "p",
      pou_type: "program",
      variables: [],
      blocks: [],
      outputs: [],
    }
    const { prog } = addBlock(empty, "TON")
    const block = prog.blocks[0]
    expect(block.inputs.map((i) => i.pin)).toEqual(["IN", "PT"])
    expect(block.inputs[0].value).toEqual({ kind: "literal", value: "FALSE" })
    expect(block.inputs[1].value).toEqual({ kind: "literal", value: "T#1s" })
  })

  it("accepts an optional render position", () => {
    const { prog, blockId } = addBlock(seed(), "TON", { x: 100, y: 50 })
    const b = prog.blocks.find((b) => b.id === blockId)!
    expect(b.position).toEqual({ x: 100, y: 50 })
  })
})

describe("removeBlock", () => {
  it("drops the block and rewrites any inbound wires to defaults", () => {
    // b0 (TON) → b1 (CTU on CU)
    const start: FbdProgram = {
      ...seed(),
      blocks: [
        ...seed().blocks,
        {
          id: "b1",
          fb_type: "CTU",
          instance: "myCnt",
          inputs: [
            {
              pin: "CU",
              value: { kind: "block", block_id: "b0", pin: "Q" },
            },
            { pin: "R", value: { kind: "literal", value: "FALSE" } },
            { pin: "PV", value: { kind: "literal", value: "5" } },
          ],
          position: null,
        },
      ],
      outputs: [
        { variable: "done", from_block: "b0", from_pin: "Q" },
      ],
    }
    const next = removeBlock(start, "b0")
    expect(next.blocks.length).toBe(1)
    // The wire that pointed at b0 should now be a literal
    const ctu = next.blocks[0]
    const cu = ctu.inputs.find((i) => i.pin === "CU")!
    expect(cu.value.kind).toBe("literal")
    // Output binding referencing b0 must be removed
    expect(next.outputs).toEqual([])
  })

  it("returns the program unchanged when the block doesn't exist", () => {
    const start = seed()
    const next = removeBlock(start, "ghost")
    expect(next).toEqual(start)
  })
})

describe("setBlockPosition", () => {
  it("stores and clears position", () => {
    let prog = setBlockPosition(seed(), "b0", { x: 10, y: 20 })
    expect(prog.blocks[0].position).toEqual({ x: 10, y: 20 })
    prog = setBlockPosition(prog, "b0", null)
    expect(prog.blocks[0].position).toBeNull()
  })
})

describe("setBlockInstance", () => {
  it("renames", () => {
    const prog = setBlockInstance(seed(), "b0", "armTimer")
    expect(prog.blocks[0].instance).toBe("armTimer")
  })

  it("rejects duplicates", () => {
    const start: FbdProgram = {
      ...seed(),
      blocks: [
        ...seed().blocks,
        {
          id: "b1",
          fb_type: "TOF",
          instance: "myT2",
          inputs: [],
          position: null,
        },
      ],
    }
    const next = setBlockInstance(start, "b0", "myT2")
    expect(next.blocks[0].instance).toBe("myT1") // unchanged
  })

  it("rejects empty / whitespace", () => {
    const next = setBlockInstance(seed(), "b0", "  ")
    expect(next.blocks[0].instance).toBe("myT1")
  })

  it("rejects another spelling of a taken instance or variable", () => {
    const start = addBlock(seed(), "TON").prog // b1 = myT2
    expect(setBlockInstance(start, "b0", "MYT2").blocks[0].instance).toBe("myT1")
    expect(setBlockInstance(start, "b0", " Btn ").blocks[0].instance).toBe("myT1")
  })

  it("allows re-casing a block's own instance", () => {
    expect(setBlockInstance(seed(), "b0", "MyT1").blocks[0].instance).toBe("MyT1")
  })
})

describe("setBlockFbType", () => {
  it("TON → TOF preserves IN / PT operands", () => {
    const next = setBlockFbType(seed(), "b0", "TOF")
    expect(next.blocks[0].fb_type).toBe("TOF")
    expect(next.blocks[0].inputs).toEqual([
      { pin: "IN", value: { kind: "var", name: "btn" } },
      { pin: "PT", value: { kind: "literal", value: "T#3s" } },
    ])
  })

  it("TON → CTU resets inputs", () => {
    const next = setBlockFbType(seed(), "b0", "CTU")
    expect(next.blocks[0].inputs.map((i) => i.pin)).toEqual(["CU", "R", "PV"])
    // None of the original IN/PT bindings survive (CTU has neither)
    expect(
      next.blocks[0].inputs.every((i) => i.value.kind === "literal"),
    ).toBe(true)
  })
})

describe("setBlockInput", () => {
  it("replaces only the matching pin", () => {
    const next = setBlockInput(seed(), "b0", "PT", {
      kind: "literal",
      value: "T#10s",
    })
    const pt = next.blocks[0].inputs.find((i) => i.pin === "PT")
    expect(pt?.value).toEqual({ kind: "literal", value: "T#10s" })
    const inp = next.blocks[0].inputs.find((i) => i.pin === "IN")
    expect(inp?.value).toEqual({ kind: "var", name: "btn" })
  })
})

describe("connectWire / disconnectWire", () => {
  function two(): FbdProgram {
    return {
      ...seed(),
      blocks: [
        ...seed().blocks,
        {
          id: "b1",
          fb_type: "CTU",
          instance: "myCnt",
          inputs: [
            { pin: "CU", value: { kind: "literal", value: "FALSE" } },
            { pin: "R", value: { kind: "literal", value: "FALSE" } },
            { pin: "PV", value: { kind: "literal", value: "5" } },
          ],
          position: null,
        },
      ],
    }
  }

  it("wires b0.Q → b1.CU", () => {
    const next = connectWire(two(), "b1", "CU", "b0", "Q")
    const cu = next.blocks[1].inputs.find((i) => i.pin === "CU")!
    expect(cu.value).toEqual({ kind: "block", block_id: "b0", pin: "Q" })
  })

  it("rejects self-loops", () => {
    const start = two()
    const next = connectWire(start, "b0", "IN", "b0", "Q")
    expect(next).toEqual(start)
  })

  it("disconnects back to a literal", () => {
    let prog = connectWire(two(), "b1", "CU", "b0", "Q")
    prog = disconnectWire(prog, "b1", "CU")
    const cu = prog.blocks[1].inputs.find((i) => i.pin === "CU")!
    expect(cu.value.kind).toBe("literal")
  })
})

describe("output bindings", () => {
  it("set adds a fresh binding", () => {
    const prog = setOutputBinding(seed(), "done", "b0", "Q")
    expect(prog.outputs).toEqual([
      { variable: "done", from_block: "b0", from_pin: "Q" },
    ])
  })

  it("set replaces an existing binding for the same variable", () => {
    let prog = setOutputBinding(seed(), "done", "b0", "Q")
    prog = setOutputBinding(prog, "done", "b0", "ET") // different pin
    expect(prog.outputs).toEqual([
      { variable: "done", from_block: "b0", from_pin: "ET" },
    ])
  })

  it("remove drops the binding", () => {
    let prog = setOutputBinding(seed(), "done", "b0", "Q")
    prog = removeOutputBinding(prog, "done")
    expect(prog.outputs).toEqual([])
  })
})

describe("JSON round-trip", () => {
  it("parse(serialize(prog)) preserves shape", () => {
    const p1 = seed()
    const p2 = parseProgram(serializeProgram(p1))
    expect(p2).toEqual(p1)
  })
})
