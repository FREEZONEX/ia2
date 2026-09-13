import * as React from "react"

import { Label } from "@/components/ui/label"
import { cn } from "@/lib/utils"

/**
 * Label-over-child form field. The canonical version of a wrapper that
 * had drifted into three near-identical copies (DevicePane, EdgePane,
 * TasksPane). `className` lands on the outer wrapper so callers can size
 * the field (e.g. `w-44`) inside a flex/grid row.
 */
export function Field({
  label,
  className,
  children,
}: {
  label: string
  className?: string
  children: React.ReactNode
}) {
  const generated = React.useId()
  const child = React.isValidElement<{ id?: string; "aria-labelledby"?: string }>(children)
    ? children : null
  const id = child?.props.id ?? generated
  return (
    <div className={cn("min-w-0 space-y-1.5", className)}>
      <Label id={`${id}-label`} htmlFor={id} className="text-xs font-medium text-muted-foreground">
        {label}
      </Label>
      {child ? React.cloneElement(child, { id, "aria-labelledby": child.props["aria-labelledby"] ?? `${id}-label` }) : children}
    </div>
  )
}
