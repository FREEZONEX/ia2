import { useEffect, useState } from "react"

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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { useRuntime } from "@/state/runtime"
import type { PouLanguage } from "@/types/generated/PouLanguage"
import type { PouType } from "@/types/generated/PouType"

type ControlledProps = {
  trigger?: undefined
  open: boolean
  onOpenChange: (next: boolean) => void
  /** Optional parent folder; the new POU is placed under it. */
  parent?: string
}

type UncontrolledProps = {
  trigger: React.ReactNode
  open?: undefined
  onOpenChange?: undefined
  parent?: string
}

type Props = ControlledProps | UncontrolledProps

export function NewPouDialog(props: Props) {
  const { createPou } = useRuntime()
  const [internalOpen, setInternalOpen] = useState(false)
  const open = props.open ?? internalOpen
  const setOpen = props.onOpenChange ?? setInternalOpen
  const parent = props.parent ?? ""
  const [name, setName] = useState("")
  const [kind, setKind] = useState<PouType>("program")
  const [language, setLanguage] = useState<PouLanguage>("st")
  const [submitting, setSubmitting] = useState(false)

  // Clear inputs each time the dialog opens, so re-opening for a
  // different folder doesn't leak last submission's text.
  useEffect(() => {
    if (open) {
      setName("")
      setKind("program")
      setLanguage("st")
    }
  }, [open, parent])

  const trimmed = name.trim()
  const fullPath = parent ? `${parent}/${trimmed}` : trimmed
  const extension =
    language === "ld"
      ? "ld.json"
      : language === "fbd"
        ? "fbd.json"
        : language === "sfc"
          ? "sfc.json"
          : "st"

  const submit = async () => {
    if (!trimmed || submitting) return
    setSubmitting(true)
    const ok = await createPou(fullPath, kind, language)
    setSubmitting(false)
    // Keep the dialog open on failure (e.g. name collision) so the typed
    // name isn't lost; the error shows in the global toast.
    if (ok) setOpen(false)
  }

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!submitting) setOpen(next) }}>
      {props.trigger ? <DialogTrigger asChild>{props.trigger}</DialogTrigger> : null}
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            New POU{" "}
            {parent && (
              <span className="break-all font-mono text-xs text-muted-foreground">
                under {parent}
              </span>
            )}
          </DialogTitle>
          <DialogDescription>Choose the program type and language. It will be added to this project.</DialogDescription>
        </DialogHeader>
        <div className="space-y-3">
          <div className="space-y-2">
            <Label htmlFor="pou-name">Name</Label>
            <Input
              id="pou-name"
              placeholder="valve_logic"
              value={name}
              onChange={(e) => setName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") submit()
              }}
              autoFocus
            />
            {trimmed && (
              <div className="break-all font-mono text-xs text-muted-foreground">
                pous/{fullPath}.{extension}
              </div>
            )}
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-2">
              <Label htmlFor="pou-kind">Type</Label>
              <Select
                value={kind}
                onValueChange={(v) => setKind(v as PouType)}
              >
                <SelectTrigger id="pou-kind">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="program">Program</SelectItem>
                  <SelectItem value="function_block">
                    Function Block
                  </SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className="space-y-2">
              <Label htmlFor="pou-language">Language</Label>
              <Select
                value={language}
                onValueChange={(v) => setLanguage(v as PouLanguage)}
              >
                <SelectTrigger id="pou-language">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="st">Structured Text</SelectItem>
                  <SelectItem value="ld">Ladder Diagram</SelectItem>
                  <SelectItem value="fbd">Function Block Diagram</SelectItem>
                  <SelectItem value="sfc">Sequential Function Chart</SelectItem>
                </SelectContent>
              </Select>
            </div>
          </div>
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
