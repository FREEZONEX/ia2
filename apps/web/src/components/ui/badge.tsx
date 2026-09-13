import * as React from "react"

import { cn } from "@/lib/utils"

/** Status label using caption typography; callers supply state colours. */
export function StatusBadge({
  className,
  children,
  ...props
}: React.ComponentProps<"span">) {
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1 rounded-md px-1.5 py-0.5 font-mono text-xs normal-case tracking-normal",
        className,
      )}
      {...props}
    >
      {children}
    </span>
  )
}
