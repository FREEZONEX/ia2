/** Screens live behind their own endpoint and refresh on HMI mutations. */
import { useCallback, useEffect, useRef, useState } from "react"
import { ChevronDown, ChevronRight, MonitorDot, Plus, Trash2 } from "@/components/ui/icons"

import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { createHmi, deleteHmi, fetchHmis } from "@/lib/api"
import { cn } from "@/lib/utils"
import { useHmiMutation } from "@/state/hmi-live"
import { useRuntime } from "@/state/runtime"
import type { HmiListEntry } from "@/types/generated/HmiListEntry"

export function HmiSection() {
  const { view, currentHmi, selectHmi, projectEpoch } = useRuntime()
  const [open, setOpen] = useState(true)
  const [screens, setScreens] = useState<HmiListEntry[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [deleting, setDeleting] = useState<string | null>(null)
  const [dialogOpen, setDialogOpen] = useState(false)
  const request = useRef(0)
  const deletePending = useRef(false)
  const mutation = useHmiMutation()

  const refresh = useCallback(async () => {
    const id = ++request.current
    setLoading(true)
    setError(null)
    try {
      const next = await fetchHmis()
      if (id === request.current) setScreens(next)
    } catch (e) {
      if (id === request.current) setError(`Could not load HMI screens: ${String(e)}`)
    } finally {
      if (id === request.current) setLoading(false)
    }
  }, [])

  useEffect(() => {
    setScreens([])
    setDialogOpen(false)
  }, [projectEpoch])
  useEffect(() => {
    void refresh()
    return () => { request.current += 1 }
  }, [projectEpoch, mutation, refresh])

  const remove = async (path: string) => {
    if (deletePending.current || !confirm(`Delete screen "${path}"?`)) return
    deletePending.current = true
    setDeleting(path)
    setError(null)
    try {
      await deleteHmi(path)
      await refresh()
    } catch (e) {
      setError(`Could not delete screen "${path}": ${String(e)}`)
    } finally {
      deletePending.current = false
      setDeleting(null)
    }
  }

  return (
    <div>
      <div className="flex min-h-8 items-center gap-1 pl-1 pr-1.5">
        <button
          type="button"
          onClick={() => setOpen(!open)}
          aria-expanded={open}
          className="flex h-8 min-w-0 flex-1 items-center gap-1.5 text-left text-xs font-medium text-foreground focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring"
        >
          {open ? <ChevronDown className="size-3 shrink-0" /> : <ChevronRight className="size-3 shrink-0" />}
          HMI
          <span className="text-xs font-normal text-muted-foreground">{screens.length}</span>
        </button>
        <button
          type="button"
          title="New HMI screen"
          aria-label="New HMI screen"
          onClick={() => setDialogOpen(true)}
          className="flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring"
        >
          <Plus className="size-3.5" />
        </button>
      </div>
      {open && screens.map((screen) => (
        <div
          key={screen.path}
          className={cn("group flex h-8 w-full min-w-0 items-center gap-1 pl-6 pr-1.5 text-[13px] hover:bg-accent/50", view === "hmi" && currentHmi === screen.path && "bg-accent text-foreground")}
        >
          <button
            type="button"
            onClick={() => void selectHmi(screen.path)}
            aria-current={view === "hmi" && currentHmi === screen.path ? "page" : undefined}
            className="flex h-8 min-w-0 flex-1 items-center gap-1.5 text-left focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring"
          >
            <MonitorDot className="size-3.5 shrink-0 text-muted-foreground" />
            <span className="min-w-0 flex-1 truncate" title={screen.title ? `${screen.path} — ${screen.title}` : screen.path}>{screen.path}</span>
            <span className="shrink-0 text-xs text-muted-foreground" title={`HMI level ${screen.level}`}>L{screen.level}</span>
          </button>
          <button
            type="button"
            title={`Delete screen "${screen.path}"`}
            aria-label={`Delete screen "${screen.path}"`}
            disabled={deleting !== null}
            onClick={() => void remove(screen.path)}
            className="flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-destructive/5 hover:text-destructive focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring disabled:opacity-50"
          >
            <Trash2 className="size-3.5" />
          </button>
        </div>
      ))}
      {open && loading && <p role="status" className="py-2 pl-6 pr-2 text-xs text-muted-foreground">Loading screens…</p>}
      {open && deleting && <p role="status" className="py-2 pl-6 pr-2 text-xs text-muted-foreground">Deleting screen…</p>}
      {open && error && (
        <div className="space-y-1 py-2 pl-6 pr-2">
          <p role="alert" className="break-words text-xs text-destructive">{error}</p>
          <Button variant="ghost" size="sm" onClick={() => void refresh()} disabled={loading}>Refresh screens</Button>
        </div>
      )}
      {open && !loading && !error && screens.length === 0 && <p className="py-2 pl-6 pr-2 text-xs leading-relaxed text-muted-foreground">No screens yet. Add an HMI screen to get started.</p>}
      <NewHmiDialog
        open={dialogOpen}
        onOpenChange={setDialogOpen}
        onCreated={(path) => { void refresh(); void selectHmi(path) }}
      />
    </div>
  )
}

function NewHmiDialog({ open, onOpenChange, onCreated }: {
  open: boolean
  onOpenChange: (v: boolean) => void
  onCreated: (path: string) => void
}) {
  const [name, setName] = useState("")
  const [title, setTitle] = useState("")
  const [error, setError] = useState<string | null>(null)
  const [creating, setCreating] = useState(false)
  const pending = useRef(false)

  useEffect(() => {
    if (open) { setName(""); setTitle(""); setError(null) }
  }, [open])

  const create = async () => {
    const slug = name.trim()
    if (!slug || pending.current) return
    pending.current = true
    setCreating(true)
    setError(null)
    try {
      await createHmi(slug, title.trim() || undefined)
      onOpenChange(false)
      onCreated(slug)
    } catch (e) {
      setError(String(e))
    } finally {
      pending.current = false
      setCreating(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!creating) onOpenChange(next) }}>
      <DialogContent className="sm:max-w-[420px]" showCloseButton={!creating}>
        <DialogHeader>
          <DialogTitle>New HMI screen</DialogTitle>
          <DialogDescription>Add an operator screen to this project.</DialogDescription>
        </DialogHeader>
        <form id="new-hmi-screen" onSubmit={(event) => { event.preventDefault(); void create() }} className="space-y-4">
          <div className="space-y-1.5">
            <Label htmlFor="hmi-name">Screen name</Label>
            <Input id="hmi-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="overview" disabled={creating} autoFocus aria-describedby="hmi-name-help" />
            <p id="hmi-name-help" className="text-xs text-muted-foreground">Used as the screen’s resource name.</p>
          </div>
          <div className="space-y-1.5">
            <Label htmlFor="hmi-title">Operator title <span className="font-normal text-muted-foreground">(optional)</span></Label>
            <Input id="hmi-title" value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Plant overview" disabled={creating} />
          </div>
          {error && <p role="alert" className="break-words text-[13px] text-destructive">{error}</p>}
        </form>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={creating}>Cancel</Button>
          <Button type="submit" form="new-hmi-screen" disabled={!name.trim() || creating}>{creating ? "Creating…" : "Create screen"}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
