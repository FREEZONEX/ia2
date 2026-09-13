import { ArrowUp, Folder, FolderCheck, Loader2 } from "@/components/ui/icons"
import { useEffect, useRef, useState } from "react"

import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { ErrorBox } from "@/components/ui/error-box"
import { browseFs } from "@/lib/api"
import { useRuntime } from "@/state/runtime"
import type { FsListing } from "@/types/generated/FsListing"

type Props = {
  trigger?: React.ReactNode
  open?: boolean
  onOpenChange?: (open: boolean) => void
}

/**
 * Folder picker for opening a project. A browser can't surface a native OS
 * path dialog, so we navigate the filesystem through the local server's
 * `/api/fs/browse` endpoint: list sub-folders, descend into them, go up to
 * the parent, and open any folder that is an IA2 project (contains
 * `project.toml`). A manual path field remains as a fallback for typing /
 * pasting an absolute path.
 */
export function OpenProjectDialog({ trigger, open: controlledOpen, onOpenChange }: Props) {
  const { openProject } = useRuntime()
  const [internalOpen, setInternalOpen] = useState(false)
  const open = controlledOpen ?? internalOpen
  const setOpen = (next: boolean) => {
    if (controlledOpen === undefined) setInternalOpen(next)
    onOpenChange?.(next)
  }
  const [listing, setListing] = useState<FsListing | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [manual, setManual] = useState("")
  const [submitting, setSubmitting] = useState(false)
  const openingRef = useRef(false)
  const navigationRef = useRef(0)

  // Load the default projects dir when the dialog opens; reset on close.
  useEffect(() => {
    if (!open) {
      navigationRef.current += 1
      setListing(null)
      setManual("")
      setError(null)
      return
    }
    void navigate(undefined)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  const navigate = async (path: string | undefined) => {
    const request = ++navigationRef.current
    setLoading(true)
    setError(null)
    try {
      const next = await browseFs(path)
      if (navigationRef.current === request) setListing(next)
    } catch (e) {
      if (navigationRef.current === request) setError(e instanceof Error ? e.message : String(e))
    } finally {
      if (navigationRef.current === request) setLoading(false)
    }
  }

  const doOpen = async (path: string) => {
    const p = path.trim()
    if (!p || openingRef.current) return
    openingRef.current = true
    setSubmitting(true)
    setError(null)
    try {
      const ok = await openProject(p)
      if (ok) setOpen(false)
      else setError("Could not open this project. Check the path and try again.")
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      openingRef.current = false
      setSubmitting(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!openingRef.current) setOpen(next) }}>
      {trigger && <DialogTrigger asChild>{trigger}</DialogTrigger>}
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>Open project</DialogTitle>
          <DialogDescription>
            Browse to a project folder (one containing{" "}
            <code className="font-mono">project.toml</code>) and open it.
          </DialogDescription>
        </DialogHeader>

        {error && <div role="alert"><ErrorBox>{error}</ErrorBox></div>}

        {/* Current path + up button */}
        <div className="flex items-center gap-2">
          <Button
            variant="outline"
            size="sm"
            className="h-8 shrink-0 px-2"
            disabled={!listing?.parent || loading || submitting}
            onClick={() => navigate(listing?.parent ?? undefined)}
            title="Up one folder"
            aria-label="Up one folder"
          >
            <ArrowUp className="size-3.5" />
          </Button>
          <div className="min-w-0 flex-1 truncate rounded-md border border-border bg-muted/30 px-2 py-1.5 font-mono text-xs text-muted-foreground">
            <span title={listing?.path}>{listing?.path ?? "Loading folders…"}</span>
          </div>
          {listing?.is_project && (
            <Button
              size="sm"
              className="h-8 shrink-0"
              disabled={submitting}
              onClick={() => doOpen(listing.path)}
            >
              {submitting ? "Opening…" : "Open this folder"}
            </Button>
          )}
        </div>

        {/* Folder list */}
        <div className="h-64 overflow-auto rounded-md border border-border">
          {loading ? (
            <div className="flex h-full items-center justify-center text-[13px] text-muted-foreground">
              <Loader2 className="mr-2 size-4 animate-spin" /> Loading…
            </div>
          ) : !listing || listing.entries.length === 0 ? (
            <div className="grid h-full place-items-center text-[13px] text-muted-foreground">
              No sub-folders here.
            </div>
          ) : (
            <ul className="divide-y divide-border/60">
              {listing.entries.map((e) => (
                <li key={e.path}>
                  <button
                    type="button"
                    className="flex w-full items-center gap-2 px-3 py-2 text-left text-[13px] hover:bg-accent/40"
                    disabled={submitting}
                    onClick={() =>
                      e.is_project ? void doOpen(e.path) : navigate(e.path)
                    }
                    title={
                      e.is_project
                        ? "Open this project"
                        : "Browse this folder"
                    }
                  >
                    {e.is_project ? (
                      <FolderCheck className="size-4 shrink-0 text-highlight" />
                    ) : (
                      <Folder className="size-4 shrink-0 text-muted-foreground" />
                    )}
                    <span className="truncate">{e.name}</span>
                    {e.is_project && (
                      <span className="ml-auto rounded bg-highlight/15 px-1.5 py-0.5 text-xs font-medium text-highlight">
                        project
                      </span>
                    )}
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>

        {/* Manual path fallback */}
        <div className="space-y-2">
        <Label htmlFor="open-project-path">Project folder path</Label>
        <Input
          id="open-project-path"
          placeholder="Paste an absolute path"
          value={manual}
          onChange={(e) => setManual(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") void doOpen(manual)
          }}
          className="font-mono text-[13px]"
          disabled={submitting}
        />
        </div>

        <DialogFooter>
          <Button variant="ghost" disabled={submitting} onClick={() => setOpen(false)}>
            Cancel
          </Button>
          <Button
            onClick={() => doOpen(manual)}
            disabled={!manual.trim() || submitting}
          >
            {submitting ? "Opening…" : "Open project"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
