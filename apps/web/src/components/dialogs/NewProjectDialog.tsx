import { useState } from "react"

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
import { useRuntime } from "@/state/runtime"

type Props = {
  trigger?: React.ReactNode
  open?: boolean
  onOpenChange?: (open: boolean) => void
}

export function NewProjectDialog({ trigger, open: controlledOpen, onOpenChange }: Props) {
  const { createProject } = useRuntime()
  const [internalOpen, setInternalOpen] = useState(false)
  const open = controlledOpen ?? internalOpen
  const setOpen = (next: boolean) => {
    if (controlledOpen === undefined) setInternalOpen(next)
    onOpenChange?.(next)
  }
  const [name, setName] = useState("")
  const [submitting, setSubmitting] = useState(false)
  const trimmed = name.trim()

  const submit = async () => {
    if (!trimmed || submitting) return
    setSubmitting(true)
    const ok = await createProject(trimmed)
    setSubmitting(false)
    // Stay open on failure so the user can correct the name.
    if (ok) {
      setOpen(false)
      setName("")
    }
  }

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!submitting) setOpen(next) }}>
      {trigger && <DialogTrigger asChild>{trigger}</DialogTrigger>}
      <DialogContent>
        <DialogHeader>
          <DialogTitle>New project</DialogTitle>
          <DialogDescription>
            Creates a project in the default IA2 projects folder.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-2">
          <Label htmlFor="project-name">Project name</Label>
          <Input
            id="project-name"
            placeholder="my-controller"
            value={name}
            onChange={(e) => setName(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") submit()
            }}
            autoFocus
          />
        </div>
        <DialogFooter>
          <Button variant="ghost" disabled={submitting} onClick={() => setOpen(false)}>
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
