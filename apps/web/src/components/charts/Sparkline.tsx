type Props = {
  values: number[]
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
    min = values[0]
    max = values[0]
    for (const v of values) {
      if (v < min) min = v
      if (v > max) max = v
    }
    if (max === min) {
      max = min + 1
    }
  }
  const range = max - min
  const padY = 1.5

  const toY = (v: number) =>
    height - padY - ((v - min) / range) * (height - 2 * padY)

  const n = values.length
  const stepX = width / Math.max(1, n - 1)

  // For BOOL render a literal stair-step so transitions are vertical;
  // for analog values use a smooth polyline.
  const points: string[] = []
  if (binary) {
    for (let i = 0; i < n; i++) {
      const x = i * stepX
      const y = toY(values[i])
      if (i === 0) {
        points.push(`${x},${y}`)
      } else {
        const prevY = toY(values[i - 1])
        points.push(`${x},${prevY}`)
        points.push(`${x},${y}`)
      }
    }
  } else {
    for (let i = 0; i < n; i++) {
      points.push(`${(i * stepX).toFixed(1)},${toY(values[i]).toFixed(1)}`)
    }
  }
  const polylinePoints = points.join(" ")
  const areaPath =
    filled && !binary
      ? `M0,${height} L ${polylinePoints} L ${width},${height} Z`
      : null

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
      {areaPath && (
        <path
          d={areaPath}
          fill="currentColor"
          fillOpacity={0.1}
          vectorEffect="non-scaling-stroke"
        />
      )}
      <polyline
        points={polylinePoints}
        fill="none"
        stroke="currentColor"
        strokeWidth={1}
        strokeLinejoin={binary ? "miter" : "round"}
        strokeLinecap="round"
        vectorEffect="non-scaling-stroke"
      />
    </svg>
  )
}
