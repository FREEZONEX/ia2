import { useEffect, useId, useMemo, useRef, useState } from "react"
import { Cpu, FileCode2, MonitorDot, Radio, Search } from "@/components/ui/icons"

import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog"
import { fetchHmis } from "@/lib/api"
import { cn } from "@/lib/utils"
import { useHmiMutation } from "@/state/hmi-live"
import { useRuntime } from "@/state/runtime"
import type { HmiListEntry } from "@/types/generated/HmiListEntry"

type Entry = {
  kind: "pou" | "device" | "edge" | "hmi"
  id: string
  label: string
  hint: string
  open: () => void
}

/** Project resources share one keyboard picker. HMI screens are fetched
 * from their own endpoint, so a failed lookup never looks like an empty list. */
export function QuickOpen({ open, onClose }: { open: boolean; onClose: () => void }) {
  const { project, selectPou, selectDevice, selectEdge, selectHmi, projectEpoch } = useRuntime()
  const [query, setQuery] = useState("")
  const [active, setActive] = useState(0)
  const [screens, setScreens] = useState<HmiListEntry[]>([])
  const [loadingScreens, setLoadingScreens] = useState(false)
  const [screenError, setScreenError] = useState(false)
  const [retry, setRetry] = useState(0)
  const mutation = useHmiMutation()
  const inputRef = useRef<HTMLInputElement>(null)
  const previousFocus = useRef<HTMLElement | null>(null)
  const listRef = useRef<HTMLUListElement>(null)
  const listId = useId()

  useEffect(() => {
    if (!open) return
    setQuery("")
    setActive(0)
  }, [open])

  useEffect(() => {
    setScreens([])
    if (!open || !project) { setLoadingScreens(false); setScreenError(false); return }
    let cancelled = false
    setLoadingScreens(true)
    setScreenError(false)
    fetchHmis()
      .then((next) => { if (!cancelled) setScreens(next) })
      .catch(() => { if (!cancelled) setScreenError(true) })
      .finally(() => { if (!cancelled) setLoadingScreens(false) })
    return () => { cancelled = true }
  }, [open, project?.name, projectEpoch, mutation, retry])

  const entries = useMemo<Entry[]>(() => {
    if (!project) return []
    return [
      ...project.pous.map((p): Entry => ({
        kind: "pou", id: p.path, label: p.path,
        hint: p.declarations.map((d) => d.type.replaceAll("_", " ")).join(", ") || "Empty POU",
        open: () => void selectPou(p.path),
      })),
      ...project.devices.map((d): Entry => ({
        kind: "device", id: d.name, label: d.name, hint: d.protocol,
        open: () => void selectDevice(d.name),
      })),
      ...project.edges.map((e): Entry => ({
        kind: "edge", id: e.name, label: e.name, hint: e.host,
        open: () => void selectEdge(e.name),
      })),
      ...screens.map((s): Entry => ({
        kind: "hmi", id: s.path, label: s.path, hint: s.title || "HMI screen",
        open: () => void selectHmi(s.path),
      })),
    ]
  }, [project, screens, selectPou, selectDevice, selectEdge, selectHmi])

  const matches = useMemo(() => {
    const q = query.trim().toLowerCase()
    return q ? entries.filter((e) => `${e.label} ${e.hint} ${e.kind}`.toLowerCase().includes(q)) : entries
  }, [entries, query])
  const results = matches.slice(0, 50)
  const activeIndex = Math.min(active, Math.max(0, results.length - 1))

  useEffect(() => {
    listRef.current?.querySelector('[aria-selected="true"]')?.scrollIntoView?.({ block: "nearest" })
  }, [activeIndex, query])

  const choose = (entry: Entry | undefined) => {
    if (!entry) return
    entry.open()
    onClose()
  }

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!next) onClose() }}>
      <DialogContent
        className="gap-0 overflow-hidden p-0 sm:max-w-xl"
        onOpenAutoFocus={(event) => {
          event.preventDefault()
          previousFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null
          inputRef.current?.focus()
        }}
        onCloseAutoFocus={(event) => {
          event.preventDefault()
          if (previousFocus.current?.isConnected) previousFocus.current.focus()
        }}
      >
        <div className="pl-4 pr-10 pb-2 pt-4">
          <DialogTitle className="text-[14px] font-medium">Quick open</DialogTitle>
          <DialogDescription className="mt-1 text-xs">Find a program, device, edge or HMI screen in this project.</DialogDescription>
        </div>
        <div className="flex items-center gap-2 border-b border-border px-4">
          <Search className="size-4 shrink-0 text-muted-foreground" aria-hidden />
          <input
            ref={inputRef}
            role="combobox"
            aria-label="Search project resources"
            aria-autocomplete="list"
            aria-expanded={open}
            aria-controls={listId}
            aria-activedescendant={results.length ? `${listId}-${activeIndex}` : undefined}
            autoComplete="off"
            value={query}
            onChange={(e) => { setQuery(e.target.value); setActive(0) }}
            onKeyDown={(e) => {
              if (e.nativeEvent.isComposing) return
              if (e.key === "ArrowDown") {
                e.preventDefault()
                setActive(Math.min(activeIndex + 1, Math.max(0, results.length - 1)))
              } else if (e.key === "ArrowUp") {
                e.preventDefault()
                setActive(Math.max(activeIndex - 1, 0))
              } else if (e.key === "Enter") {
                e.preventDefault()
                choose(results[activeIndex])
              }
            }}
            placeholder="Search by name, type or screen title"
            className="cs-selectable h-11 min-w-0 flex-1 bg-transparent text-[13px] text-foreground outline-none placeholder:text-muted-foreground"
          />
        </div>
        <ul ref={listRef} id={listId} role="listbox" aria-label="Project resources" aria-busy={loadingScreens} className="max-h-[46vh] min-h-24 overflow-auto py-1">
          {results.map((entry, i) => (
            <li
              key={`${entry.kind}:${entry.id}`}
              id={`${listId}-${i}`}
              role="option"
              aria-selected={i === activeIndex}
              onMouseMove={() => setActive(i)}
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => choose(entry)}
              className={cn("flex min-h-10 cursor-pointer items-center gap-3 px-4 py-2", i === activeIndex ? "bg-accent text-foreground" : "hover:bg-muted/50")}
            >
              <EntryIcon kind={entry.kind} />
              <span className="min-w-0 flex-1 truncate text-[13px]" title={entry.label}>{entry.label}</span>
              <span className="max-w-[45%] truncate text-xs text-muted-foreground" title={entry.hint}>{entry.kind === "hmi" ? "HMI · " : ""}{entry.hint}</span>
            </li>
          ))}
        </ul>
        {results.length === 0 && !loadingScreens && <p role="status" className="px-4 pb-4 text-[13px] text-muted-foreground">{query.trim() ? "No matching resources. Try a different name." : screenError ? "No other resources in this project yet." : "No resources in this project yet."}</p>}
        {loadingScreens && <p role="status" className="px-4 pb-3 text-xs text-muted-foreground">Loading HMI screens…</p>}
        {screenError && <div className="flex items-center justify-between gap-3 px-4 pb-3 text-xs text-destructive"><span role="status">HMI screens could not be loaded.</span><button type="button" onClick={() => setRetry((n) => n + 1)} className="min-h-7 rounded px-2 font-medium hover:bg-destructive/5">Retry</button></div>}
        <div className="flex flex-wrap items-center justify-between gap-2 border-t border-border px-4 py-2 text-xs text-muted-foreground">
          <span>{matches.length > 50 ? `Showing 50 of ${matches.length} results` : `${matches.length} ${matches.length === 1 ? "result" : "results"}`}</span>
          <span>↑ ↓ Navigate · Enter Open · Esc Close</span>
        </div>
      </DialogContent>
    </Dialog>
  )
}

function EntryIcon({ kind }: { kind: Entry["kind"] }) {
  const cls = "size-4 shrink-0 text-muted-foreground"
  if (kind === "device") return <Cpu className={cls} aria-hidden />
  if (kind === "edge") return <Radio className={cls} aria-hidden />
  if (kind === "hmi") return <MonitorDot className={cls} aria-hidden />
  return <FileCode2 className={cls} aria-hidden />
}
