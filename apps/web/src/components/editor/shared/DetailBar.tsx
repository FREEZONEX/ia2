/** Shared inspector labels and actions for graphical editors. */

import { Button } from "@/components/ui/button"

export function DetailLabel({ children }: { children: React.ReactNode }) {
  return (
    <span className="font-mono text-[13px] text-muted-foreground">
      {children}
    </span>
  )
}

export function Separator() {
  return <span className="mx-1 h-4 w-px bg-border" />
}

export function ActionBtn({
  onClick,
  title,
  disabled = false,
  destructive = false,
  children,
}: {
  onClick: () => void
  title?: string
  disabled?: boolean
  destructive?: boolean
  children: React.ReactNode
}) {
  return (
    <Button
      type="button"
      size="sm"
      variant="ghost"
      onClick={onClick}
      disabled={disabled}
      title={title}
      className={destructive ? "text-destructive hover:bg-destructive/10 hover:text-destructive" : undefined}
    >
      {children}
    </Button>
  )
}
