import { useEffect, useState } from "react"

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
import { useRuntime } from "@/state/runtime"

type Section = "applications" | "devices" | "edges"

type Props = {
  /** Whether the dialog is shown. The parent owns this so right-click
   * handlers can pop it open and pass `parent` in one go. */
  open: boolean
  onOpenChange: (next: boolean) => void
  section: Section
  /** Pre-filled parent folder (empty = top-level). The new folder is created
   * under this path. */
  parent: string
}

export function NewFolderDialog({ open, onOpenChange, section, parent }: Props) {
  const { createPouFolder, createDeviceFolder, createEdgeFolder } = useRuntime()
  const [leaf, setLeaf] = useState("")
  const [submitting, setSubmitting] = useState(false)

  // Reset when the dialog re-opens for a different parent.
  useEffect(() => {
    if (open) setLeaf("")
  }, [open, parent])

  const trimmed = leaf.trim()
  const fullPath = parent ? `${parent}/${trimmed}` : trimmed

  const submit = async () => {
    if (!trimmed || submitting) return
    setSubmitting(true)
    const ok =
      section === "applications"
        ? await createPouFolder(fullPath)
        : section === "devices"
          ? await createDeviceFolder(fullPath)
          : await createEdgeFolder(fullPath)
    setSubmitting(false)
    // Stay open on failure; the error shows in the global toast.
    if (ok) onOpenChange(false)
  }

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!submitting) onOpenChange(next) }}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            New folder{" "}
            <span className="break-all font-mono text-xs text-muted-foreground">
              under {parent || "/"}
            </span>
          </DialogTitle>
          <DialogDescription>Organize project resources in a folder.</DialogDescription>
        </DialogHeader>
        <div className="space-y-2">
          <Label htmlFor="folder-leaf">Folder name</Label>
          <Input
            id="folder-leaf"
            placeholder="pid_loops"
            value={leaf}
            onChange={(e) => setLeaf(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") submit()
            }}
            autoFocus
          />
          {trimmed && (
            <div className="break-all font-mono text-xs text-muted-foreground">
              {section}/{fullPath}
            </div>
          )}
        </div>
        <DialogFooter>
          <Button variant="ghost" disabled={submitting} onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button onClick={submit} disabled={!trimmed || submitting}>
            {submitting ? "Creating…" : "Create"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
