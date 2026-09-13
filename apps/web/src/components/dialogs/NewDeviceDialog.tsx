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
import type { Protocol } from "@/types/generated/Protocol"

type ControlledProps = {
  trigger?: undefined
  open: boolean
  onOpenChange: (next: boolean) => void
  parent?: string
}

type UncontrolledProps = {
  trigger: React.ReactNode
  open?: undefined
  onOpenChange?: undefined
  parent?: string
}

type Props = ControlledProps | UncontrolledProps

export function NewDeviceDialog(props: Props) {
  const { createDevice } = useRuntime()
  const [internalOpen, setInternalOpen] = useState(false)
  const open = props.open ?? internalOpen
  const setOpen = props.onOpenChange ?? setInternalOpen
  const parent = props.parent ?? ""
  const [name, setName] = useState("")
  const [protocol, setProtocol] = useState<Protocol>("modbus")
  const [submitting, setSubmitting] = useState(false)

  useEffect(() => {
    if (open) {
      setName("")
      setProtocol("modbus")
    }
  }, [open, parent])

  const trimmed = name.trim()
  const fullPath = parent ? `${parent}/${trimmed}` : trimmed

  const submit = async () => {
    if (!trimmed || submitting) return
    setSubmitting(true)
    const ok = await createDevice(fullPath, protocol)
    setSubmitting(false)
    // Stay open on failure; the error shows in the global toast.
    if (ok) setOpen(false)
  }

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!submitting) setOpen(next) }}>
      {props.trigger ? <DialogTrigger asChild>{props.trigger}</DialogTrigger> : null}
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            New device{" "}
            {parent && (
              <span className="break-all font-mono text-xs text-muted-foreground">
                under {parent}
              </span>
            )}
          </DialogTitle>
          <DialogDescription>Choose a protocol, then configure its connection and channels.</DialogDescription>
        </DialogHeader>
        <div className="space-y-3">
          <div className="space-y-2">
            <Label htmlFor="device-name">Name</Label>
            <Input
              id="device-name"
              placeholder="tank1"
              value={name}
              onChange={(e) => setName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") submit()
              }}
              autoFocus
            />
            {trimmed && parent && (
              <div className="break-all font-mono text-xs text-muted-foreground">
                devices/{fullPath}.toml
              </div>
            )}
          </div>
          <div className="space-y-2">
            <Label htmlFor="device-protocol">Protocol</Label>
            <Select
              value={protocol}
              onValueChange={(v) => setProtocol(v as Protocol)}
            >
              <SelectTrigger id="device-protocol">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="modbus">Modbus TCP</SelectItem>
                <SelectItem value="ethercat">EtherCAT</SelectItem>
                <SelectItem value="opcua">OPC UA (client)</SelectItem>
                <SelectItem value="canopen">CANopen</SelectItem>
              </SelectContent>
            </Select>
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
