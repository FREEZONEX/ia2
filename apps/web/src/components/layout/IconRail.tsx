import { useEffect, useState } from "react"
import { Files, Moon, Search, Settings, Sun } from "@/components/ui/icons"

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import { setTheme, useDarkMode } from "@/lib/dark-mode"
import { cn } from "@/lib/utils"
import { QuickOpen } from "./QuickOpen"

/**
 * The 52px activity rail down the left edge — the outermost layer of the
 * design's IDE chrome. Sits left of the project sidebar.
 *
 * Every control here is REAL, never decorative: the design's rail is an
 * activity bar, and a native-feel app doesn't paint buttons that do
 * nothing (cf. the no-dead-chrome rule). So the rail carries exactly the
 * views IA2 actually has:
 *
 *   • Explorer — the project tree. The only persistent left view, so it's
 *     always the active tab (white/card chip + subtle shadow, per Figma).
 *   • Search   — opens the quick-open palette (also ⌘P / ⌘K).
 *   • Settings — bottom-docked gear; theme + motion, in a real menu.
 *
 * The git/source-control slot from the mock is intentionally omitted:
 * IA2 has no VCS backend, and a dead branch icon would be exactly the
 * kind of web-app tell this app is trying to avoid.
 */
export function IconRail() {
  const [quickOpen, setQuickOpen] = useState(false)

  // ⌘P / ⌘K — the shortcuts the Search button stands in for.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && (e.key === "p" || e.key === "k")) {
        e.preventDefault()
        setQuickOpen((v) => !v)
      }
    }
    window.addEventListener("keydown", onKey)
    return () => window.removeEventListener("keydown", onKey)
  }, [])

  return (
    <>
      <nav
        aria-label="Activity"
        className="flex w-[52px] shrink-0 flex-col items-center border-r border-border bg-secondary py-2"
      >
        <div className="flex flex-col items-center gap-1">
          <RailButton label="Explorer" active>
            <Files className="size-[18px]" />
          </RailButton>
          <RailButton
            label="Search  (⌘P)"
            onClick={() => setQuickOpen(true)}
          >
            <Search className="size-[18px]" />
          </RailButton>
        </div>
        <div className="mt-auto">
          <SettingsButton />
        </div>
      </nav>
      <QuickOpen open={quickOpen} onClose={() => setQuickOpen(false)} />
    </>
  )
}

function RailButton({
  label,
  active = false,
  onClick,
  children,
}: {
  label: string
  active?: boolean
  onClick?: () => void
  children: React.ReactNode
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={label}
      aria-label={label}
      aria-pressed={active}
      className={cn(
        "flex size-9 items-center justify-center rounded-md transition-colors",
        active
          ? // Active tab: raised card chip, like the Figma's Explorer.
            "bg-card text-foreground shadow-sm"
          : "text-muted-foreground hover:bg-accent/50 hover:text-foreground",
      )}
    >
      {children}
    </button>
  )
}

function SettingsButton() {
  const theme = useDarkMode()
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          title="Settings"
          aria-label="Settings"
          className="flex size-9 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent/50 hover:text-foreground"
        >
          <Settings className="size-[18px]" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent side="right" align="end" className="w-44">
        <DropdownMenuLabel className="text-[11px] uppercase tracking-wider text-muted-foreground">
          Appearance
        </DropdownMenuLabel>
        <DropdownMenuItem onSelect={() => setTheme("light")}>
          <Sun className="size-3.5" />
          Light
          {theme === "light" && <Check />}
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={() => setTheme("dark")}>
          <Moon className="size-3.5" />
          Dark
          {theme === "dark" && <Check />}
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

function Check() {
  return <span className="ml-auto text-highlight">✓</span>
}
