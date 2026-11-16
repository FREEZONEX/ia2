import type { LdProgram } from "@/types/generated/LdProgram"
import type { LdNode } from "@/types/generated/LdNode"
export const CELL_W = 90
export const CELL_H = 44
export const RAIL_PAD = 16
export const COIL_W = 100
export const TOP_PAD = 12

interface LayoutBox {
  cols: number
  rows: number
}

export function measure(node: LdNode): LayoutBox {
  switch (node.op) {
    case "contact":
    case "const":
      return { cols: 1, rows: 1 }
    case "compare":
      // Compare blocks render as a rectangle wide enough to fit
      // "left CMP right" (e.g. `temperature < 50.0`). 2 cells fits
      // most typical labels comfortably; longer labels just clip.
      return { cols: 2, rows: 1 }
    case "fb_call":
      // FB call blocks (TON, CTU, R_TRIG, ...) render the same width
      // as a Compare block — the rectangle shows `instance.outputPin`
      // with a small `[TYPE]` tag above. Individual pin bindings
      // live in the NodeDetail editor, not in the rung itself, so we
      // don't grow vertically for input/output pin lists. This keeps
      // mixed contact + FB rungs visually compact and consistent.
      return { cols: 2, rows: 1 }
    case "not":
      return measure(node.arg)
    case "and": {
      if (node.args.length === 0) return { cols: 1, rows: 1 }
      let cols = 0
      let rows = 1
      for (const a of node.args) {
        const m = measure(a)
        cols += m.cols
        rows = Math.max(rows, m.rows)
      }
      return { cols, rows }
    }
    case "or": {
      if (node.args.length === 0) return { cols: 1, rows: 1 }
      let cols = 1
      let rows = 0
      for (const a of node.args) {
        const m = measure(a)
        cols = Math.max(cols, m.cols)
        rows += m.rows
      }
      return { cols, rows }
    }
  }
}

/** Contact network and parallel coil column share one SVG coordinate system. */
export function layoutRung(rung: LdProgram["rungs"][number]) {
  const layoutBox = measure(rung.logic)
  const cols = layoutBox.cols
  const networkRows = layoutBox.rows
  // Total rows = max(network height, coil count). A 3-coil rung needs
  // 3 rows of vertical space for the coil stack even if the network
  // is a single contact, and vice versa. This is the canonical IEC
  // 61131-3 LD layout — multiple coils on one rung are PARALLEL loads
  // driven by the same network output, drawn as vertical branches at
  // the right edge of the rung.
  const coilCount = rung.coils.length
  const totalRows = Math.max(networkRows, coilCount, 1)
  // Network is centered vertically within the rung's total height; the
  // coil column sits to the right with one row per coil, also centered.
  const networkY = TOP_PAD + ((totalRows - networkRows) * CELL_H) / 2
  const coilStackY =
    TOP_PAD + ((totalRows - Math.max(coilCount, 1)) * CELL_H) / 2
  // Single output column (coils stacked vertically) sits right after
  // the network. With multiple coils we still only need ONE column
  // width — they don't extend horizontally.
  const width = RAIL_PAD * 2 + cols * CELL_W + COIL_W + 24
  const height = totalRows * CELL_H + TOP_PAD * 2

  // The "network exit" y is the middle of the network's vertical band.
  const networkOutY =
    networkY + (networkRows * CELL_H) / 2
  // x where the coil column begins (right edge of the contact network).
  const coilColX = RAIL_PAD + cols * CELL_W
  // x where each coil glyph's left paren sits; centered in COIL_W.
  const coilGlyphX = coilColX + (COIL_W - 36) / 2
  // Y of each individual coil (one row per coil, centered in its row).
  const coilYs = rung.coils.map(
    (_, ci) => coilStackY + ci * CELL_H + CELL_H / 2,
  )
  return {
    cols, networkRows, networkY, width, height,
    coilCount, coilColX, coilGlyphX, coilYs, networkOutY,
  }
}
