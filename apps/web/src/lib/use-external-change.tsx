import { useCallback, useEffect, useRef, useState } from "react"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"

export type ExternalChangeChoice = "overwrite" | "load" | "cancel"

/** Ask what to do when a write would replace a version of the open program
 *  that this window never saw. Cancel is the default: nothing is written. */
export function useExternalChangePrompt() {
  const resolver = useRef<((choice: ExternalChangeChoice) => void) | null>(null)
  const [path, setPath] = useState<string | null>(null)
  const askExternalChange = useCallback((forPath: string): Promise<ExternalChangeChoice> => {
    if (resolver.current) return Promise.resolve("cancel")
    return new Promise((resolve) => { resolver.current = resolve; setPath(forPath) })
  }, [])
  const finish = useCallback((choice: ExternalChangeChoice) => {
    const resolve = resolver.current
    resolver.current = null
    setPath(null)
    resolve?.(choice)
  }, [])
  useEffect(() => {
    return () => {
      resolver.current?.("cancel")
      resolver.current = null
    }
  }, [])
  const externalChangeDialog = <Dialog open={path !== null} onOpenChange={(value) => { if (!value) finish("cancel") }}>
    <DialogContent>
      <DialogHeader>
        <DialogTitle>Program changed on disk</DialogTitle>
        <DialogDescription>
          <span className="font-mono">{path}</span> was changed by another writer — an agent, the CLI, or another window — while you had unsaved edits.
          Writing now replaces that version with yours, and nothing else will keep it.
        </DialogDescription>
      </DialogHeader>
      <DialogFooter>
        <Button variant="outline" onClick={() => finish("cancel")} autoFocus>Cancel</Button>
        <Button variant="outline" onClick={() => finish("load")}>Load disk version</Button>
        <Button variant="destructive" onClick={() => finish("overwrite")}>Overwrite with mine</Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>
  return { askExternalChange, externalChangeDialog }
}
