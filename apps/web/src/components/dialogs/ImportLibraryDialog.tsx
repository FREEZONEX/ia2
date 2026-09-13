import { useEffect, useMemo, useState } from "react"

import { Button } from "@/components/ui/button"
import { ErrorBox } from "@/components/ui/error-box"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog"
import { fetchLibraries, importLibrary } from "@/lib/api"
import type { LibrarySummary } from "@/types/generated/LibrarySummary"

type ControlledProps = {
  trigger?: undefined
  open: boolean
  onOpenChange: (next: boolean) => void
}

type UncontrolledProps = {
  trigger: React.ReactNode
  open?: undefined
  onOpenChange?: undefined
}

type Props = ControlledProps | UncontrolledProps

/**
 * Browse the server's FB-library registry and vendor blocks into the
 * project (`pous/lib/<library>/`). Re-importing an already-imported
 * block overwrites it — that's the update path, surfaced here through
 * the per-block "imported" / "update" badges.
 */
export function ImportLibraryDialog(props: Props) {
  const [internalOpen, setInternalOpen] = useState(false)
  const open = props.open ?? internalOpen
  const setOpen = props.onOpenChange ?? setInternalOpen

  const [libs, setLibs] = useState<LibrarySummary[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  // Which library's blocks are shown (registry usually has one).
  const [activeLib, setActiveLib] = useState<string | null>(null)
  const [selected, setSelected] = useState<Set<string>>(new Set())
  const [submitting, setSubmitting] = useState(false)

  useEffect(() => {
    if (!open) return
    setLibs(null)
    setError(null)
    setSelected(new Set())
    fetchLibraries()
      .then((ls) => {
        setLibs(ls)
        setActiveLib(ls[0]?.name ?? null)
      })
      .catch((e) => setError(String(e)))
  }, [open])

  const lib = useMemo(
    () => libs?.find((l) => l.name === activeLib) ?? null,
    [libs, activeLib],
  )
  const importedStems = useMemo(
    () => new Set(lib?.imported_files ?? []),
    [lib],
  )
  const updateAvailable =
    lib?.imported_version != null && lib.imported_version !== lib.version

  const toggle = (file: string) =>
    setSelected((prev) => {
      const next = new Set(prev)
      if (next.has(file)) next.delete(file)
      else next.add(file)
      return next
    })

  const submit = async (files: string[]) => {
    if (!lib || submitting) return
    setSubmitting(true)
    setError(null)
    try {
      await importLibrary(lib.name, files)
      setOpen(false)
    } catch (e) {
      setError(String(e))
    } finally {
      setSubmitting(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      {props.trigger ? <DialogTrigger asChild>{props.trigger}</DialogTrigger> : null}
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>Import library blocks</DialogTitle>
          <DialogDescription>Choose reusable function blocks. Importing an existing block updates its local copy.</DialogDescription>
        </DialogHeader>

        {error && (
          <div role="alert"><ErrorBox>{error}</ErrorBox></div>
        )}

        {libs === null && !error && (
          <div className="py-6 text-center text-[13px] text-muted-foreground">
            Loading registry…
          </div>
        )}

        {libs !== null && libs.length === 0 && (
          <div className="py-6 text-center text-[13px] text-muted-foreground">
            No libraries are available. Install a function block library before importing blocks.
          </div>
        )}

        {libs !== null && libs.length > 1 && (
          <div className="flex gap-1">
            {libs.map((l) => (
              <button
                key={l.name}
                type="button"
                onClick={() => {
                  setActiveLib(l.name)
                  setSelected(new Set())
                }}
                className={
                  l.name === activeLib
                    ? "rounded bg-accent px-2 py-1 text-xs font-medium"
                    : "rounded px-2 py-1 text-xs text-muted-foreground hover:bg-accent/40"
                }
              >
                {l.name}
              </button>
            ))}
          </div>
        )}

        {lib && (
          <div className="space-y-2">
            <div className="text-xs text-muted-foreground">
              <span className="font-mono text-foreground">
                {lib.name}@{lib.version}
              </span>
              {lib.imported_version && (
                <span className="ml-2 rounded bg-muted/60 px-1.5 py-0.5 font-mono text-xs">
                  imported @{lib.imported_version}
                </span>
              )}
              {updateAvailable && (
                <span className="ml-2 rounded bg-warn/15 px-1.5 py-0.5 font-mono text-xs text-warn">
                  update available
                </span>
              )}
              {lib.description && (
                <div className="mt-1">{lib.description}</div>
              )}
            </div>
            <div className="max-h-72 divide-y divide-border overflow-y-auto border-y border-border">
              {lib.blocks.map((b) => {
                const stem = b.file.replace(/\.st$/, "")
                const isImported = importedStems.has(stem)
                return (
                  <label
                    key={b.file}
                    className="flex cursor-pointer items-start gap-3 px-2 py-3 text-[13px] hover:bg-accent/40"
                  >
                    <input
                      type="checkbox"
                      className="mt-0.5 size-4 shrink-0 accent-primary"
                      checked={selected.has(b.file)}
                      onChange={() => toggle(b.file)}
                    />
                    <span className="min-w-0 flex-1 break-words">
                      <span className="font-mono text-foreground">
                        {b.name}
                      </span>
                      {isImported && (
                        <span className="ml-2 rounded bg-muted/60 px-1 py-0.5 font-mono text-xs text-muted-foreground">
                          imported
                        </span>
                      )}
                      {b.summary && (
                        <span className="block text-muted-foreground">
                          {b.summary}
                        </span>
                      )}
                    </span>
                  </label>
                )
              })}
            </div>
          </div>
        )}

        <DialogFooter>
          <Button variant="ghost" onClick={() => setOpen(false)}>
            Cancel
          </Button>
          <Button
            variant="outline"
            disabled={!lib || submitting}
            onClick={() => void submit([])}
            title="Import every block (re-importing overwrites = update)"
          >
            Import all
          </Button>
          <Button
            disabled={!lib || selected.size === 0 || submitting}
            onClick={() => void submit([...selected])}
          >
            {submitting ? "Importing…" : `Import ${selected.size > 0 ? selected.size + " " : ""}selected`}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
