import { useCallback, useEffect, useRef, useState } from "react"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"

/** Resolve navigation only after the user chooses what happens to their edit. */
export function useDiscardChanges(dirty: boolean) {
  const dirtyRef = useRef(dirty)
  dirtyRef.current = dirty
  const resolver = useRef<((allowed: boolean) => void) | null>(null)
  const [open, setOpen] = useState(false)
  const confirmDiscard = useCallback((): Promise<boolean> => {
    if (resolver.current) return Promise.resolve(false)
    if (!dirtyRef.current) return Promise.resolve(true)
    return new Promise((resolve) => { resolver.current = resolve; setOpen(true) })
  }, [])
  const finish = useCallback((allowed: boolean) => {
    const resolve = resolver.current
    resolver.current = null
    setOpen(false)
    resolve?.(allowed)
  }, [])
  useEffect(() => {
    if (!dirty) return
    const beforeUnload = (event: BeforeUnloadEvent) => {
      if (!dirtyRef.current) return
      event.preventDefault()
      event.returnValue = ""
    }
    window.addEventListener("beforeunload", beforeUnload)
    return () => window.removeEventListener("beforeunload", beforeUnload)
  }, [dirty])
  useEffect(() => {
    return () => {
      resolver.current?.(false)
      resolver.current = null
    }
  }, [])
  const discardDialog = <Dialog open={open} onOpenChange={(value) => { if (!value) finish(false) }}>
    <DialogContent>
      <DialogHeader><DialogTitle>Unsaved changes</DialogTitle><DialogDescription>The open program has unsaved changes. Keep editing to save them, or discard them and continue.</DialogDescription></DialogHeader>
      <DialogFooter><Button variant="outline" onClick={() => finish(false)} autoFocus>Keep editing</Button><Button onClick={() => finish(true)}>Discard changes</Button></DialogFooter>
    </DialogContent>
  </Dialog>
  return { confirmDiscard, discardDialog }
}
