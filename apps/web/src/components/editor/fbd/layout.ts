import { blockOutputs } from "@/lib/fbd-edit"
import type { FbdBlock } from "@/types/generated/FbdBlock"
import type { FbdProgram } from "@/types/generated/FbdProgram"
export const BLOCK_W = 160
export const PIN_ROW_H = 18
export const HEADER_H = 26
const LAYER_GAP = 80
const ROW_GAP = 20
const CANVAS_PAD = 24

/** Length of the connector stub that pokes out from the block edge,
 *  in SVG units. Wires terminate at the outer end of the stub (the
 *  dot), not at the block edge — same as Codesys / TIA. */
export const STUB_LEN = 8

export interface BlockLayout {
  x: number
  y: number
  w: number
  h: number
}

export interface FbdLayout {
  blockById: Map<string, BlockLayout>
  width: number
  height: number
}

export function layoutBlocks(prog: FbdProgram): FbdLayout {
  // Phase 1: layered defaults (same as the read-only viewer used).
  const fallback = layeredDefaults(prog)

  // Phase 2: any block with a saved `position` overrides its fallback.
  // This lets newly-added blocks land at the explicit drop point
  // immediately, and survive reloads.
  const blockById = new Map<string, BlockLayout>()
  let maxX = 0
  let maxY = 0
  for (const block of prog.blocks) {
    const def = fallback.get(block.id)!
    const pos = block.position
    const layout: BlockLayout = {
      x: pos ? pos.x : def.x,
      y: pos ? pos.y : def.y,
      w: def.w,
      h: def.h,
    }
    blockById.set(block.id, layout)
    maxX = Math.max(maxX, layout.x + layout.w)
    maxY = Math.max(maxY, layout.y + layout.h)
  }

  // Leave a margin for output-binding stubs on the right.
  const width = Math.max(maxX + CANVAS_PAD + 160, 600)
  const height = Math.max(maxY + CANVAS_PAD, 240)

  return { blockById, width, height }
}

function layeredDefaults(prog: FbdProgram): Map<string, BlockLayout> {
  const n = prog.blocks.length
  const layer = new Array<number>(n).fill(0)
  const idToIdx = new Map<string, number>()
  prog.blocks.forEach((b, i) => idToIdx.set(b.id, i))

  for (let pass = 0; pass < n; pass++) {
    let changed = false
    for (let i = 0; i < n; i++) {
      let deepest = -1
      for (const input of prog.blocks[i].inputs) {
        if (input.value.kind === "block") {
          const src = idToIdx.get(input.value.block_id)
          if (src !== undefined && src !== i) {
            deepest = Math.max(deepest, layer[src])
          }
        }
      }
      const want = deepest + 1
      if (want > layer[i]) {
        layer[i] = want
        changed = true
      }
    }
    if (!changed) break
  }

  // Group by layer, in authoring order.
  const byLayer = new Map<number, number[]>()
  for (let i = 0; i < n; i++) {
    const l = layer[i]
    if (!byLayer.has(l)) byLayer.set(l, [])
    byLayer.get(l)!.push(i)
  }

  const heights = prog.blocks.map(
    (b) => HEADER_H + Math.max(b.inputs.length, 1) * PIN_ROW_H + 8,
  )

  const result = new Map<string, BlockLayout>()
  for (const [l, members] of byLayer.entries()) {
    let cursorY = CANVAS_PAD
    members.forEach((i) => {
      const h = heights[i]
      result.set(prog.blocks[i].id, {
        x: CANVAS_PAD + l * (BLOCK_W + LAYER_GAP),
        y: cursorY,
        w: BLOCK_W,
        h,
      })
      cursorY += h + ROW_GAP
    })
  }
  return result
}

// (DiagnosticsBanner moved to ./DiagnosticsBanner.tsx — shared with
//  LDEditor and SFCEditor. We keep `describeLocation` local because
//  the formatter is FBD-specific.)


/** Vertical centre of a block's output dot, matching EXACTLY how
 *  BlockGlyph lays the dots out (see "Output pins on the right side"):
 *  bool outputs spread evenly down the box body by their index in the
 *  output list, fallback `["Q"]` when a block exposes no BOOL output.
 *  The wire source endpoint must use this so each wire leaves its own
 *  dot — the previous `inputs.length / 2` heuristic on the input-row
 *  pitch drew every wire of a multi-output block (TON Q/ET, CTU Q/CV)
 *  from the box midpoint, visually detached from the dots. */
export function outputPinCy(
  blocks: FbdBlock[],
  layout: BlockLayout,
  blockId: string,
  pin: string,
): number {
  const b = blocks.find((bb) => bb.id === blockId)
  const outs = b ? blockOutputs(b) : []
  const arr = outs.length > 0 ? outs : ["Q"]
  const idx = Math.max(0, arr.indexOf(pin))
  return layout.y + HEADER_H + ((idx + 0.5) * (layout.h - HEADER_H)) / arr.length
}
