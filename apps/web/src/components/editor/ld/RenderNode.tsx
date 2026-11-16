import { powerClass } from "@/components/editor/shared/power"
import {
  comparatorSymbol,
  evaluateNode,
  operandText,
  type LiveReadings,
  type NodePath,
} from "@/lib/ld-edit"
import type { LdNode } from "@/types/generated/LdNode"
import type { LdProgram } from "@/types/generated/LdProgram"
import type { Selection } from "./selection"
import { CELL_W, CELL_H, measure } from "./layout"
import { ContactGlyph, ConstGlyph, CompareGlyph, FbCallGlyph } from "./Glyphs"

interface NodeRenderProps {
  node: LdNode
  path: NodePath
  x: number
  y: number
  cols: number
  rows: number
  rungIdx: number
  selection: Selection
  readOnly: boolean
  liveValues: LiveReadings | null
  onSelect: (s: Selection) => void
  onCommit: (transform: (prog: LdProgram) => LdProgram) => void
}

export function RenderNode(props: NodeRenderProps) {
  const { node, path, x, y, cols, rows, rungIdx, selection, liveValues, onSelect } = props
  const midY = y + ((rows - 1) * CELL_H) / 2 + CELL_H / 2
  const selected =
    selection?.kind === "node" &&
    selection.rungIdx === rungIdx &&
    pathsEqual(selection.path, path)
  // Power state of this whole sub-tree. Drives glyph colouring; child
  // recursions compute their own. Null means no live reading for it —
  // nothing running here, or a value the runtime does not publish — and
  // renders static.
  const powered = liveValues
    ? evaluateNode(node, liveValues)
    : null

  const click = () => onSelect(selected ? null : { kind: "node", rungIdx, path })

  switch (node.op) {
    case "contact":
      return (
        <ContactGlyph
          x={x}
          y={midY}
          width={CELL_W}
          name={node.var}
          negated={node.negated}
          selected={selected}
          powered={powered}
          onClick={click}
        />
      )
    case "const":
      return (
        <ConstGlyph
          x={x}
          y={midY}
          width={CELL_W}
          value={node.value}
          selected={selected}
          powered={powered}
          onClick={click}
        />
      )
    case "compare":
      return (
        <CompareGlyph
          x={x}
          y={midY}
          width={cols * CELL_W}
          left={operandText(node.left)}
          cmp={comparatorSymbol(node.cmp)}
          right={operandText(node.right)}
          selected={selected}
          powered={powered}
          onClick={click}
        />
      )
    case "fb_call":
      return (
        <FbCallGlyph
          x={x}
          y={midY}
          width={cols * CELL_W}
          instance={node.instance}
          fbType={node.fb_type}
          outputPin={node.output_pin}
          selected={selected}
          powered={powered}
          onClick={click}
        />
      )
    case "not": {
      if (node.arg.op === "contact") {
        return (
          <ContactGlyph
            x={x}
            y={midY}
            width={CELL_W}
            name={node.arg.var}
            negated={!node.arg.negated}
            selected={selected}
            powered={powered}
            onClick={click}
          />
        )
      }
      const m = measure(node.arg)
      return (
        <g>
          <RenderNode
            {...props}
            node={node.arg}
            path={[...path, 0]}
            cols={m.cols}
            rows={m.rows}
          />
          <rect
            x={x + 2}
            y={y + 2}
            width={m.cols * CELL_W - 4}
            height={m.rows * CELL_H - 4}
            fill="none"
            className="stroke-muted-foreground"
            strokeWidth={1}
            strokeDasharray="3 3"
            vectorEffect="non-scaling-stroke"
          />
          <text
            x={x + 4}
            y={y + 12}
            className="fill-muted-foreground"
            fontSize="9"
            fontFamily='"IBM Plex Mono", ui-monospace, monospace'
          >
            NOT
          </text>
        </g>
      )
    }
    case "and": {
      if (node.args.length === 0) {
        return (
          <ConstGlyph
            x={x}
            y={midY}
            width={CELL_W}
            value={true}
            selected={selected}
            powered={powered}
            onClick={click}
          />
        )
      }
      let cursor = x
      const elems: React.ReactNode[] = []
      // For an AND series, the wire BETWEEN args[k-1] and args[k] is
      // powered only if every arg up to and including k-1 conducts.
      // Children render their own glyphs with their own power state;
      // adjacent contact glyphs already cover the wire pixels via
      // their internal stroke. So we don't need to inject extra wires
      // here — the contact's own "lead-in" and "lead-out" sub-lines
      // already pick up that arg's power state.
      node.args.forEach((child, i) => {
        const m = measure(child)
        elems.push(
          <RenderNode
            key={i}
            {...props}
            node={child}
            path={[...path, i]}
            x={cursor}
            cols={m.cols}
            rows={rows}
          />,
        )
        cursor += m.cols * CELL_W
      })
      return <g>{elems}</g>
    }
    case "or": {
      if (node.args.length === 0) {
        return (
          <ConstGlyph
            x={x}
            y={midY}
            width={CELL_W}
            value={false}
            selected={selected}
            powered={powered}
            onClick={click}
          />
        )
      }
      let rowCursor = 0
      const elems: React.ReactNode[] = []
      const inX = x
      const outX = x + cols * CELL_W
      node.args.forEach((child, i) => {
        const m = measure(child)
        const laneY = y + rowCursor * CELL_H
        const laneMidY = laneY + ((m.rows - 1) * CELL_H) / 2 + CELL_H / 2
        // The per-branch padding wire is powered iff that branch
        // itself conducts — it's the branch's "tail" extending out
        // to align with longer siblings.
        const branchPowered = liveValues
          ? evaluateNode(child, liveValues)
          : null
        elems.push(
          <RenderNode
            key={i}
            {...props}
            node={child}
            path={[...path, i]}
            x={inX}
            y={laneY}
            cols={cols}
            rows={m.rows}
          />,
        )
        if (m.cols < cols) {
          elems.push(
            <line
              key={`pad-${i}`}
              x1={inX + m.cols * CELL_W}
              y1={laneMidY}
              x2={outX}
              y2={laneMidY}
              className={powerClass(branchPowered)}
              strokeWidth={1}
              vectorEffect="non-scaling-stroke"
            />,
          )
        }
        rowCursor += m.rows
      })
      const firstY = y + CELL_H / 2
      const lastChildRows = measure(node.args[node.args.length - 1]).rows
      const lastY =
        y + (rowCursor - lastChildRows) * CELL_H + lastChildRows * CELL_H - CELL_H / 2
      // Merge bars: powered if ANY branch is conducting, since the
      // OR's output is the disjunction.
      elems.push(
        <line
          key="merge-in"
          x1={inX}
          y1={firstY}
          x2={inX}
          y2={lastY}
          className={powerClass(powered)}
          strokeWidth={1}
          vectorEffect="non-scaling-stroke"
        />,
        <line
          key="merge-out"
          x1={outX}
          y1={firstY}
          x2={outX}
          y2={lastY}
          className={powerClass(powered)}
          strokeWidth={1}
          vectorEffect="non-scaling-stroke"
        />,
      )
      return <g>{elems}</g>
    }
  }
}

function pathsEqual(a: NodePath, b: NodePath) {
  if (a.length !== b.length) return false
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false
  return true
}
