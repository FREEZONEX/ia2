/**
 * Pure tree-transformation library for FBD POU editing.
 *
 * Same role as `ld-edit.ts` plays for LDEditor: every editing gesture
 * the user performs on FBDEditor's canvas turns into one or more calls
 * here, and the result is a fresh `FbdProgram` value. No DOM, no
 * React state.
 *
 * Why pure / immutable:
 *  - JSON on disk is the source of truth. We serialise/parse every
 *    keystroke. Sharing mutable objects with React would lose the
 *    round-trip property.
 *  - Each function is unit-testable on its own. Undo / redo is a
 *    stack of snapshots — built in if we want it.
 */

import { fbByType, fbInputs, fbOutputs, suggestInstanceName } from "./ld-fbs"
import type { FbdBlock } from "@/types/generated/FbdBlock"
import type { FbdInputBinding } from "@/types/generated/FbdInputBinding"
import type { FbdInputSource } from "@/types/generated/FbdInputSource"
import type { FbdOutputBinding } from "@/types/generated/FbdOutputBinding"
import type { FbdPosition } from "@/types/generated/FbdPosition"
import type { FbdProgram } from "@/types/generated/FbdProgram"

// =================================================================
//   JSON round-trip + shared variable CRUD
// =================================================================

// `serializeProgram` and the three variable mutators are byte-identical
// across the graphical editors — they live in `program-vars` now and
// re-export here so this module's public API is unchanged.
export {
  addVariable,
  removeVariable,
  serializeProgram,
  updateVariable,
} from "./program-vars"
import { parseProgramJson } from "./program-vars"

export function parseProgram(source: string): FbdProgram {
  return parseProgramJson<FbdProgram>(source)
}

// =================================================================
//   Block operations
// =================================================================

/**
 * Add a new block of the given FB type to the program at the supplied
 * position. Auto-generates a stable block id (`b0`, `b1`, …) and an
 * instance name from the FB type's preferred prefix. Inputs are
 * pre-populated with sensible default literals so the new block
 * transpiles cleanly even before the user wires it.
 */
export function addBlock(
  prog: FbdProgram,
  fbType: string,
  position?: FbdPosition,
): { prog: FbdProgram; blockId: string; instance: string } {
  const blockId = nextBlockId(prog)
  const instance = suggestInstanceName(fbType, declaredNames(prog))
  const def = fbByType(fbType)
  const inputs: FbdInputBinding[] = def
    ? def.pins
        .filter((p) => p.direction === "input")
        .map((p) => ({
          pin: p.pin,
          value: defaultLiteralFor(p.type),
        }))
    : []
  const block: FbdBlock = {
    id: blockId,
    fb_type: fbType,
    instance,
    inputs,
    position: position ?? null,
  }
  return {
    prog: { ...prog, blocks: [...prog.blocks, block] },
    blockId,
    instance,
  }
}

/**
 * Remove a block by id. Any wire that pointed AT the removed block
 * (as a source via `Block { block_id }`) is rewritten to a default
 * literal — the alternative would be silently breaking those bindings
 * and emitting a confusing diagnostic from ironplc.
 *
 * Output bindings sourced from the removed block are likewise dropped.
 */
export function removeBlock(prog: FbdProgram, blockId: string): FbdProgram {
  const blocks = prog.blocks
    .filter((b) => b.id !== blockId)
    .map((b) => ({
      ...b,
      inputs: b.inputs.map((inp) =>
        inp.value.kind === "block" && inp.value.block_id === blockId
          ? { ...inp, value: defaultLiteralForPin(inp.pin) }
          : inp,
      ),
    }))
  const outputs = prog.outputs.filter((o) => o.from_block !== blockId)
  return { ...prog, blocks, outputs }
}

/** Save / clear a block's render position. `null` removes the field. */
export function setBlockPosition(
  prog: FbdProgram,
  blockId: string,
  position: FbdPosition | null,
): FbdProgram {
  return updateBlock(prog, blockId, (b) => ({ ...b, position }))
}

/** Rename a block's FB instance variable. Rejects a name another block's
 *  instance or a variable already has, compared case-insensitively as
 *  IEC 61131-3 does — the transpiler would declare it twice (callers
 *  should validate before calling — defensive no-op rather than
 *  throwing).  */
export function setBlockInstance(
  prog: FbdProgram,
  blockId: string,
  instance: string,
): FbdProgram {
  const name = instance.trim()
  const others = declaredNames({
    ...prog,
    blocks: prog.blocks.filter((b) => b.id !== blockId),
  })
  const taken = others.some((n) => n.toLowerCase() === name.toLowerCase())
  if (taken || !name) return prog
  return updateBlock(prog, blockId, (b) => ({ ...b, instance: name }))
}

/** Every name the POU declares: block FB instances and variables. */
function declaredNames(prog: FbdProgram): string[] {
  return [...prog.blocks.map((b) => b.instance), ...prog.variables.map((v) => v.name)]
}

/**
 * Switch a block to a different FB type. Pins shared by name with the
 * new type retain their existing operands (lossless TON↔TOF swap);
 * pins not in the new type are dropped; new pins get default literals.
 * The output_pin selection is left unchanged — the editor's detail
 * bar maps it to whatever the new type allows.
 */
export function setBlockFbType(
  prog: FbdProgram,
  blockId: string,
  newType: string,
): FbdProgram {
  return updateBlock(prog, blockId, (b) => {
    const newPins = fbInputs(newType)
    const preserved = new Map(b.inputs.map((i) => [i.pin, i.value]))
    const inputs: FbdInputBinding[] = newPins.map((p) => ({
      pin: p.pin,
      value: preserved.get(p.pin) ?? defaultLiteralFor(p.type),
    }))
    return { ...b, fb_type: newType, inputs }
  })
}

/** Edit one input pin's binding. No-op if `pin` isn't in this block's
 *  input list. */
export function setBlockInput(
  prog: FbdProgram,
  blockId: string,
  pin: string,
  value: FbdInputSource,
): FbdProgram {
  return updateBlock(prog, blockId, (b) => ({
    ...b,
    inputs: b.inputs.map((i) => (i.pin === pin ? { ...i, value } : i)),
  }))
}

function updateBlock(
  prog: FbdProgram,
  blockId: string,
  transform: (b: FbdBlock) => FbdBlock,
): FbdProgram {
  if (!prog.blocks.some((b) => b.id === blockId)) return prog
  return {
    ...prog,
    blocks: prog.blocks.map((b) => (b.id === blockId ? transform(b) : b)),
  }
}

function nextBlockId(prog: FbdProgram): string {
  const used = new Set(prog.blocks.map((b) => b.id))
  let n = prog.blocks.length
  // eslint-disable-next-line no-constant-condition
  while (true) {
    const candidate = `b${n}`
    if (!used.has(candidate)) return candidate
    n += 1
  }
}

function defaultLiteralFor(iecType: string): FbdInputSource {
  const t = iecType.toUpperCase()
  if (t === "TIME") return { kind: "literal", value: "T#1s" }
  if (t === "BOOL") return { kind: "literal", value: "FALSE" }
  return { kind: "literal", value: "0" }
}

/** Pick a default literal for a pin whose IEC type isn't directly
 *  available (e.g. we're rewriting a wire after deleting its source
 *  block). Best-effort by pin name convention. */
function defaultLiteralForPin(pin: string): FbdInputSource {
  const p = pin.toUpperCase()
  if (p === "PT" || p.endsWith("_TIME")) {
    return { kind: "literal", value: "T#0ms" }
  }
  if (p === "PV" || p === "CV") {
    return { kind: "literal", value: "0" }
  }
  return { kind: "literal", value: "FALSE" }
}

// =================================================================
//   Wires
// =================================================================

/**
 * Connect a wire from one block's output pin to another block's input
 * pin (overwrites whatever was previously on the target pin).
 *
 * Self-loops are rejected — FBD doesn't permit feedback in our MVP
 * (would need CFC semantics + explicit feedback markers). Returns
 * the program unchanged if the connection would form a self-loop or
 * if either endpoint is unknown.
 */
export function connectWire(
  prog: FbdProgram,
  targetBlockId: string,
  targetPin: string,
  sourceBlockId: string,
  sourcePin: string,
): FbdProgram {
  if (targetBlockId === sourceBlockId) return prog
  if (!prog.blocks.some((b) => b.id === targetBlockId)) return prog
  if (!prog.blocks.some((b) => b.id === sourceBlockId)) return prog
  return setBlockInput(prog, targetBlockId, targetPin, {
    kind: "block",
    block_id: sourceBlockId,
    pin: sourcePin,
  })
}

/** Remove the wire feeding `targetPin` on `targetBlockId`, reverting
 *  the pin to a default literal so the program still transpiles. */
export function disconnectWire(
  prog: FbdProgram,
  targetBlockId: string,
  targetPin: string,
): FbdProgram {
  return setBlockInput(prog, targetBlockId, targetPin, defaultLiteralForPin(targetPin))
}

// =================================================================
//   Output bindings
// =================================================================

/** Bind a POU `VAR_OUTPUT` variable to a block output pin. Replaces
 *  any existing binding for the same variable (FBD allows at most one
 *  driver per output — the transpiler enforces this too). */
export function setOutputBinding(
  prog: FbdProgram,
  variable: string,
  fromBlock: string,
  fromPin: string,
): FbdProgram {
  const filtered = prog.outputs.filter((o) => o.variable !== variable)
  const binding: FbdOutputBinding = {
    variable,
    from_block: fromBlock,
    from_pin: fromPin,
  }
  return { ...prog, outputs: [...filtered, binding] }
}

export function removeOutputBinding(
  prog: FbdProgram,
  variable: string,
): FbdProgram {
  return { ...prog, outputs: prog.outputs.filter((o) => o.variable !== variable) }
}

// Variable CRUD (addVariable / removeVariable / updateVariable) is
// shared — see the re-export from `program-vars` above.

// =================================================================
//   Helpers / lookups
// =================================================================

/** ALL outputs of a given block (any type), derived from the FB type's
 *  pin definitions. FBD wires carry whatever the pin's type is — the
 *  transpiler and `connectWire` don't restrict to BOOL — so the editor
 *  renders and lets you wire every output: a PID's REAL `out`, a
 *  timer's `ET`, not only the BOOL ones. Falls back to `["Q"]` for an
 *  unknown FB type so a block can still be placed and wired. */
export function blockOutputs(block: FbdBlock): string[] {
  const outs = fbOutputs(block.fb_type).map((p) => p.pin)
  return outs.length > 0 ? outs : ["Q"]
}
