import type { ReactNode } from "react"

/** One title and one action area for each workbench surface. */
export function PaneHeader({ title, description, meta, actions }: {
  title: ReactNode
  description?: ReactNode
  meta?: ReactNode
  actions?: ReactNode
}) {
  return (
    <header className="ia2-pane-header">
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1">
          <h1 className="min-w-0 truncate text-[14px] font-medium text-foreground">{title}</h1>
          {meta && <div className="flex min-w-0 flex-wrap items-center gap-2 text-xs text-muted-foreground">{meta}</div>}
        </div>
        {description && <div className="mt-1 text-xs leading-relaxed text-muted-foreground">{description}</div>}
      </div>
      {actions && <div className="flex shrink-0 flex-wrap items-center gap-2">{actions}</div>}
    </header>
  )
}
