import { sampleSegments } from "@/lib/var-history"

type Props = {
  values: (number | null)[]
  /** Force a 0/1 Y scale for BOOL — renders as a stair-step. */
  binary?: boolean
  width?: number
  height?: number
  /** Override stroke colour; defaults to currentColor. */
  color?: string
  /** Subtle area fill under the line. */
  filled?: boolean
}

export function Sparkline({
  values,
  binary = false,
  width = 96,
  height = 20,
  color,
  filled = false,
}: Props) {
  if (values.length < 2) {
    return (
      <svg viewBox={`0 0 ${width} ${height}`} preserveAspectRatio="none" className="block h-full w-full" aria-hidden="true">
        <line
          x1={0}
          y1={height / 2}
          x2={width}
          y2={height / 2}
          stroke="currentColor"
          strokeOpacity={0.2}
          strokeDasharray="2 2"
        />
      </svg>
    )
  }

  let min: number
  let max: number
  if (binary) {
    min = 0
    max = 1
  } else {
    min = Infinity
    max = -Infinity
    for (const v of values) {
      if (v === null || !Number.isFinite(v)) continue
      if (v < min) min = v
      if (v > max) max = v
    }
    if (max === min) {
      max = min + 1
    }
    if (!Number.isFinite(min)) { min = 0; max = 1 }
  }
  const range = max - min
  const padY = 1.5

  const toY = (v: number) =>
    height - padY - ((v - min) / range) * (height - 2 * padY)

  const n = values.length
  const stepX = width / Math.max(1, n - 1)

  // For BOOL render a literal stair-step so transitions are vertical;
  // for analog values use a smooth polyline.
  const segments = sampleSegments(values).map((run) => {
    const points: string[] = []
    run.forEach(({ index, value }, i) => {
      const x = (index * stepX).toFixed(1)
      if (binary && i > 0) points.push(`${x},${toY(run[i - 1].value).toFixed(1)}`)
      points.push(`${x},${toY(value).toFixed(1)}`)
    })
    return { points: points.join(" "), start: run[0].index * stepX, end: run[run.length - 1].index * stepX }
  })

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      // Stretch to fill — height is set by parent — without the auto-axis
      // preservation. `vectorEffect=non-scaling-stroke` on the polyline
      // pins the stroke to screen-pixel units regardless of how the SVG
      // is scaled, so lines don't look chunkier in wider rows.
      preserveAspectRatio="none"
      className="block h-full w-full"
      style={{ color: color ?? "currentColor" }}
    >
      {segments.map((segment, i) => <g key={i}>
      {filled && !binary && (
        <path
          d={`M${segment.start},${height} L ${segment.points} L ${segment.end},${height} Z`}
          fill="currentColor"
          fillOpacity={0.1}
          vectorEffect="non-scaling-stroke"
        />
      )}
      <polyline
        points={segment.points}
        fill="none"
        stroke="currentColor"
        strokeWidth={1}
        strokeLinejoin={binary ? "miter" : "round"}
        strokeLinecap="round"
        vectorEffect="non-scaling-stroke"
      />
      </g>)}
    </svg>
  )
}
