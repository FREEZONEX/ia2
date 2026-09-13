import type { ReactNode } from "react"
import { cn } from "@/lib/utils"

export function EmptyState({ icon, title, description, actions, className }: {
  icon?: ReactNode
  title: string
  description?: ReactNode
  actions?: ReactNode
  className?: string
}) {
  return (
    <div className={cn("flex min-h-0 flex-1 items-start gap-4 px-6 py-8", className)}>
      {icon && <span aria-hidden className="mt-0.5 shrink-0 text-muted-foreground [&_svg]:size-6">{icon}</span>}
      <div className="min-w-0 max-w-lg">
        <h2 className="text-sm font-medium text-foreground">{title}</h2>
        {description && <div className="mt-2 text-[13px] leading-relaxed text-muted-foreground">{description}</div>}
        {actions && <div className="mt-4 flex flex-wrap items-center gap-2">{actions}</div>}
      </div>
    </div>
  )
}
