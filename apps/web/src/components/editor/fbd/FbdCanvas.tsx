import { useRef } from "react"
import type { DiagnosticIndex } from "@/components/editor/shared/diagnostics"
import { powerClass } from "@/components/editor/shared/power"
import { blockOutputs } from "@/lib/fbd-edit"
import type { FbdProgram } from "@/types/generated/FbdProgram"
import { type LivePins, type WireDrag, type WireDropTarget } from "./types"
import { PIN_ROW_H, HEADER_H, STUB_LEN, type FbdLayout, outputPinCy } from "./layout"
import { BlockGlyph } from "./BlockGlyph"

export function FbdCanvas({
  prog,
  layout,
  liveValues,
  diagIndex,
  selectedBlockId,
  readOnly,
  wireDrag,
  onSelectBlock,
  onMoveBlock,
  onStartWireDrag,
  onUpdateWireDrag,
  onEndWireDrag,
}: {
  prog: FbdProgram
  layout: FbdLayout
  liveValues: LivePins | null
  diagIndex: DiagnosticIndex
  selectedBlockId: string | null
  readOnly: boolean
  wireDrag: WireDrag | null
  onSelectBlock: (id: string | null) => void
  onMoveBlock: (id: string, pos: { x: number; y: number }) => void
  onStartWireDrag: (drag: WireDrag) => void
  onUpdateWireDrag: (pos: { x: number; y: number }) => void
  onEndWireDrag: (target: WireDropTarget | null) => void
}) {
  // Block drag state — same as before.
  const dragRef = useRef<{
    id: string
    origX: number
    origY: number
    startClientX: number
    startClientY: number
    scale: number
  } | null>(null)
  const svgRef = useRef<SVGSVGElement | null>(null)

  /** Convert a client-space (px) point to SVG-space (layout units),
   *  using the viewBox-to-rendered-size scale. */
  const clientToSvg = (clientX: number, clientY: number): { x: number; y: number } => {
    const svg = svgRef.current
    if (!svg) return { x: clientX, y: clientY }
    const rect = svg.getBoundingClientRect()
    const scaleX = layout.width / rect.width
    const scaleY = layout.height / rect.height
    return {
      x: (clientX - rect.left) * scaleX,
      y: (clientY - rect.top) * scaleY,
    }
  }

  const onCanvasPointerMove = (e: React.PointerEvent) => {
    // Block drag takes precedence (it's the more common gesture).
    const drag = dragRef.current
    if (drag) {
      const dx = (e.clientX - drag.startClientX) / drag.scale
      const dy = (e.clientY - drag.startClientY) / drag.scale
      onMoveBlock(drag.id, {
        x: Math.round(drag.origX + dx),
        y: Math.round(drag.origY + dy),
      })
      return
    }
    if (wireDrag) {
      const p = clientToSvg(e.clientX, e.clientY)
      onUpdateWireDrag(p)
    }
  }

  /** Attempt to find an input pin dot near the cursor on pointerup.
   *  Returns the {blockId, pin} pair if hit within HIT_RADIUS px, or
   *  null otherwise. Iterates all input pins of all blocks; n is small
   *  enough this is fine. */
  const findDropTarget = (svgX: number, svgY: number): WireDropTarget | null => {
    const HIT_RADIUS = 14 // forgiving by design — small dots need a big target
    for (const block of prog.blocks) {
      // Don't self-loop
      if (wireDrag && block.id === wireDrag.fromBlockId) continue
      const lay = layout.blockById.get(block.id)
      // Skip an unpositioned block, don't abort the whole search — a
      // missing layout for one block must not make later blocks
      // un-droppable as wire targets.
      if (!lay) continue
      for (let i = 0; i < Math.max(block.inputs.length, 1); i++) {
        const input = block.inputs[i]
        if (!input) continue
        // Hit-test against the stub dot (outside the left edge), where
        // the user expects the connection point to be.
        const x = lay.x - STUB_LEN
        const y = lay.y + HEADER_H + i * PIN_ROW_H + PIN_ROW_H / 2
        const d = Math.hypot(svgX - x, svgY - y)
        if (d <= HIT_RADIUS) {
          return { blockId: block.id, pin: input.pin }
        }
      }
    }
    return null
  }

  const onCanvasPointerUp = (e: React.PointerEvent) => {
    // End block drag.
    dragRef.current = null
    // Resolve wire drag.
    if (wireDrag) {
      const p = clientToSvg(e.clientX, e.clientY)
      const target = findDropTarget(p.x, p.y)
      onEndWireDrag(target)
    }
  }

  const endDrag = () => {
    dragRef.current = null
    if (wireDrag) onEndWireDrag(null)
  }

  return (
    <svg
      ref={svgRef}
      viewBox={`0 0 ${layout.width} ${layout.height}`}
      width={layout.width}
      height={layout.height}
      className="block max-w-full select-none"
      onPointerMove={onCanvasPointerMove}
      onPointerUp={onCanvasPointerUp}
      onPointerCancel={endDrag}
      onPointerLeave={endDrag}
      onMouseDown={(e) => {
        // Clicking blank canvas deselects.
        if (e.target === e.currentTarget) onSelectBlock(null)
      }}
    >
      {/* Wires first so block borders draw on top. */}
      {prog.blocks.flatMap((b, i) =>
        b.inputs
          .filter((inp) => inp.value.kind === "block")
          .map((inp, j) => {
            const src = inp.value
            if (src.kind !== "block") return null
            const from = layout.blockById.get(src.block_id)
            const to = layout.blockById.get(b.id)
            if (!from || !to) return null
            const targetPinIdx = b.inputs.findIndex((x) => x.pin === inp.pin)
            // Wires terminate at the STUB dots, not at the block edge.
            // Source Y uses the SAME placement BlockGlyph gives the
            // output dots (see outputPinCy) — the input-row pitch does
            // not apply to the right side, so a multi-output source must
            // leave each wire from its own dot.
            const x1 = from.x + from.w + STUB_LEN
            const y1 = outputPinCy(prog.blocks, from, src.block_id, src.pin)
            const x2 = to.x - STUB_LEN
            const y2 = to.y + HEADER_H + targetPinIdx * PIN_ROW_H + PIN_ROW_H / 2
            const dx = Math.max(20, (x2 - x1) / 2)
            const d = `M ${x1} ${y1} C ${x1 + dx} ${y1}, ${x2 - dx} ${y2}, ${x2} ${y2}`
            const srcBlock = prog.blocks.find((bb) => bb.id === src.block_id)
            const powered =
              liveValues && srcBlock ? liveValues.pin(srcBlock, src.pin) : null
            return (
              <path
                key={`wire-${i}-${j}`}
                d={d}
                fill="none"
                strokeWidth={1.5}
                vectorEffect="non-scaling-stroke"
                className={powerClass(powered)}
              />
            )
          }),
      )}

      {prog.blocks.map((b) => (
        <BlockGlyph
          key={b.id}
          block={b}
          layout={layout.blockById.get(b.id)!}
          liveValues={liveValues}
          hasError={diagIndex.byElement.has(b.id)}
          selected={selectedBlockId === b.id}
          readOnly={readOnly}
          onPointerDown={(e) => {
            e.stopPropagation()
            onSelectBlock(b.id)
            if (readOnly) return
            const svg = svgRef.current
            const rect = svg?.getBoundingClientRect()
            const scale = svg && rect ? rect.width / layout.width : 1
            const lay = layout.blockById.get(b.id)!
            dragRef.current = {
              id: b.id,
              origX: lay.x,
              origY: lay.y,
              startClientX: e.clientX,
              startClientY: e.clientY,
              scale,
            }
            ;(e.currentTarget as Element).setPointerCapture(e.pointerId)
          }}
          onOutputPinPointerDown={(pin, e) => {
            e.stopPropagation()
            if (readOnly) return
            const lay = layout.blockById.get(b.id)!
            const outs = blockOutputs(b)
            const pinIdx = outs.indexOf(pin)
            const total = Math.max(outs.length, 1)
            const yOffset =
              HEADER_H + ((pinIdx + 0.5) * (lay.h - HEADER_H)) / total
            // Source anchor for the preview line = the stub dot, not
            // the block edge. Matches the rendered output-pin
            // position so the rubber-band line attaches visually.
            onStartWireDrag({
              fromBlockId: b.id,
              fromPin: pin,
              fromX: lay.x + lay.w + STUB_LEN,
              fromY: lay.y + yOffset,
              currentX: lay.x + lay.w + STUB_LEN,
              currentY: lay.y + yOffset,
            })
          }}
        />
      ))}

      {/* Wire-drag preview line */}
      {wireDrag && (
        <g pointerEvents="none">
          <path
            d={(() => {
              const dx = Math.max(20, (wireDrag.currentX - wireDrag.fromX) / 2)
              return `M ${wireDrag.fromX} ${wireDrag.fromY} C ${wireDrag.fromX + dx} ${wireDrag.fromY}, ${wireDrag.currentX - dx} ${wireDrag.currentY}, ${wireDrag.currentX} ${wireDrag.currentY}`
            })()}
            fill="none"
            className="stroke-highlight"
            strokeWidth={1.5}
            strokeDasharray="4 3"
            vectorEffect="non-scaling-stroke"
          />
          <circle
            cx={wireDrag.currentX}
            cy={wireDrag.currentY}
            r={3}
            className="fill-highlight"
          />
        </g>
      )}

      {prog.outputs.map((o, i) => {
        const from = layout.blockById.get(o.from_block)
        if (!from) return null
        const x1 = from.x + from.w + STUB_LEN
        const y1 = outputPinCy(prog.blocks, from, o.from_block, o.from_pin)
        const x2 = x1 + 40
        const srcBlock = prog.blocks.find((b) => b.id === o.from_block)
        const powered =
          liveValues && srcBlock ? liveValues.pin(srcBlock, o.from_pin) : null
        return (
          <g key={`out-${i}`} pointerEvents="none">
            <line
              x1={x1}
              y1={y1}
              x2={x2}
              y2={y1}
              strokeWidth={1.5}
              vectorEffect="non-scaling-stroke"
              className={powerClass(powered)}
            />
            <text
              x={x2 + 4}
              y={y1 + 3}
              className="fill-foreground"
              fontSize="11"
              fontFamily='"IBM Plex Mono", ui-monospace, monospace'
            >
              → {o.variable}
            </text>
          </g>
        )
      })}
    </svg>
  )
}
