import { useEffect, useRef, useState, type SetStateAction } from "react"
import { Button } from "@/components/ui/button"
import { documentVersion } from "@/lib/api"

const sameContent = (a: object, b: object) => JSON.stringify(a) === JSON.stringify(b)

/** Keep a dirty form and the version it was edited from together. A background
 * GET may refresh the incoming document, never a dirty draft's base. */
export function useDocumentDraft<T extends object>(incoming: T, identity: string) {
  const [state, setState] = useState({ base: incoming, draft: incoming, identity, acknowledged: incoming })
  const currentIdentity = useRef(identity)
  currentIdentity.current = identity
  useEffect(() => {
    setState((current) => {
      if (current.identity !== identity || sameContent(current.draft, current.base) ||
          sameContent(current.draft, incoming)) {
        return { base: incoming, draft: incoming, identity, acknowledged: incoming }
      }
      return current
    })
  }, [incoming, identity])
  const dirty = !sameContent(state.draft, state.base)
  const conflict = dirty && incoming !== state.acknowledged && (
    documentVersion(incoming) !== documentVersion(state.base) || !sameContent(incoming, state.base)
  )
  const setDraft = (next: SetStateAction<T>) => setState((current) => ({
    ...current, draft: typeof next === "function" ? next(current.draft) : next,
  }))
  const reload = async (load: () => Promise<T>) => {
    if (dirty && !window.confirm("Discard local changes and reload the latest document from disk?")) return
    const fresh = await load()
    if (currentIdentity.current === identity) setState({ base: fresh, draft: fresh, identity, acknowledged: incoming })
  }
  return { draft: state.draft, setDraft, dirty, conflict, reload }
}

export function DocumentReloadButton({ reload, conflict = false }: {
  reload: () => Promise<void>
  conflict?: boolean
}) {
  const [pending, setPending] = useState(false)
  const [error, setError] = useState<string | null>(null)
  return <>
    <Button size="sm" variant="outline" disabled={pending} onClick={async () => {
      setPending(true)
      setError(null)
      try { await reload() } catch (e) { setError(String(e)) } finally { setPending(false) }
    }} title="Reload from disk (asks before discarding local changes)">
      {conflict ? "Changed on disk · Reload" : "Reload from disk"}
    </Button>
    {error && <span role="alert" className="text-destructive">{error}</span>}
  </>
}
