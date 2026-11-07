import type { FbdBlock } from "@/types/generated/FbdBlock"

/** Live state of a block output pin; `null` when it is not on the wire. */
export type LivePins = { pin: (block: FbdBlock, pin: string) => boolean | null }

/** Snapshot of an in-flight wire-drag. `fromBlockId` / `fromPin` are
 *  the source endpoint locked at drag-start; `currentX` / `currentY`
 *  follow the cursor in SVG-space and drive the preview line. */
export interface WireDrag {
  fromBlockId: string
  fromPin: string
  /** Source-end SVG coordinate (right edge of the source pin's dot). */
  fromX: number
  fromY: number
  /** Cursor SVG coordinate, updated on every pointermove. */
  currentX: number
  currentY: number
}

/** Target endpoint of a wire-drag drop. */
export interface WireDropTarget {
  blockId: string
  pin: string
}
