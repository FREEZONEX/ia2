import { useEffect, useRef, useState } from "react"
import { Button } from "@/components/ui/button"
import { fetchEngineeringHistory, refreshEngineeringState, type EngineeringScope } from "@/lib/api"
import { useRuntime } from "@/state/runtime"
import type { EngineeringParameter } from "@/types/generated/EngineeringParameter"
import type { EngineeringObservation } from "@/types/generated/EngineeringObservation"

const retention: Record<string, string> = {
  disabled_incremental: "Incremental · position is not retained across power cycles",
  configured_absolute_unverified: "Absolute mode configured · retention not yet verified",
  single_turn_only: "Single-turn · multi-turn reference is unavailable",
  encoder_fault_present: "Encoder fault · reference requires diagnosis",
  unknown: "Position retention unknown",
}
function value(p: EngineeringParameter | undefined): string {
  if (!p?.reading || p.error) return "Unknown"
  return p.data_type.startsWith("i") ? p.reading.signed : p.reading.unsigned
}
function stamp(ms: number): string {
  return `${new Date(ms + 8 * 60 * 60 * 1000).toISOString().slice(0, 23).replace("T", " ")} UTC+8`
}

export function EngineeringState({ device, slaves }: { device: string; slaves: number[] }) {
  const { project, attached } = useRuntime()
  if (!project) return null
  return <EngineeringStateView key={`${project.path}:${attached?.name ?? "local"}:${device}`} scope={{ project: project.name, edge: attached?.name }} device={device} slaves={slaves} />
}

export function EngineeringStateView({ scope, device, slaves }: { scope: EngineeringScope; device: string; slaves: number[] }) {
  const [selected, setSelected] = useState(slaves[0] ?? 0)
  const slave = slaves.includes(selected) ? selected : (slaves[0] ?? 0)
  return <section className="space-y-3 rounded-md border border-border bg-muted/20 p-4" aria-label="Engineering state">
    <div className="flex flex-wrap items-center justify-between gap-3">
      <div><h3 className="text-sm font-semibold">Engineering state</h3><p className="text-xs text-muted-foreground">Observed device facts · {scope.edge ? `Edge: ${scope.edge}` : "Local runtime"}</p></div>
      <label className="text-xs">Slave <select aria-label="Observed slave" className="ml-2 rounded border border-border bg-background p-1" value={slave} onChange={e => setSelected(Number(e.target.value))}>
        {(slaves.length ? slaves : [0]).map(n => <option key={n} value={n}>{n}</option>)}
      </select></label>
    </div>
    <ObservationView key={`${scope.project}:${scope.edge ?? "local"}:${device}:${slave}`} scope={scope} device={device} slave={slave} />
  </section>
}

function ObservationView({ scope, device, slave }: { scope: EngineeringScope; device: string; slave: number }) {
  const [records, setRecords] = useState<EngineeringObservation[]>([])
  const [selectedId, setSelectedId] = useState<string>()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()
  const [unsaved, setUnsaved] = useState<string>()
  const [loading, setLoading] = useState(true)
  const generation = useRef(0)
  const alive = useRef(true)
  const refreshAbort = useRef<AbortController | null>(null)
  const project = scope.project, edge = scope.edge
  useEffect(() => {
    alive.current = true
    const request = ++generation.current
    const controller = new AbortController()
    fetchEngineeringHistory({ project, edge }, controller.signal).then(history => {
      if (!alive.current || request !== generation.current) return
      setRecords(history.observations.filter(o => o.target.device === device && o.target.slave === slave))
    }).catch(e => {
      if (alive.current && request === generation.current) setError(String(e))
    }).finally(() => {
      if (alive.current && request === generation.current) setLoading(false)
    })
    return () => { alive.current = false; controller.abort(); refreshAbort.current?.abort() }
  }, [project, edge, device, slave])

  async function refresh() {
    const request = ++generation.current
    const controller = new AbortController()
    refreshAbort.current = controller
    setBusy(true); setLoading(false); setError(undefined)
    try {
      const result = await refreshEngineeringState({ project, edge }, { device, slave }, controller.signal)
      if (!alive.current || request !== generation.current) return
      setRecords(old => [...old, result.observation].slice(-64))
      setSelectedId(result.observation.id)
      setUnsaved(result.saved ? undefined : `Read completed, but storage is unconfirmed: ${result.storage_error ?? "unknown storage error"}`)
    } catch (e) {
      if (alive.current && request === generation.current) setError(`Refresh unavailable. Displayed records remain historical. ${String(e)}`)
    } finally {
      if (alive.current && request === generation.current) setBusy(false)
    }
  }
  const observation = records.find(o => o.id === selectedId) ?? records.at(-1)
  const prior = observation ? records[records.indexOf(observation) - 1] : undefined
  return <div className="space-y-3 text-xs">
    <div className="flex flex-wrap items-center gap-3">
      <Button size="sm" variant="outline" disabled={busy} onClick={() => void refresh()}>{busy ? "Reading device…" : "Refresh device observation"}</Button>
      <span className="text-muted-foreground">Read-only · existing master · no automatic startup</span>
    </div>
    {error && <p role="alert" className="break-words text-destructive">{error}</p>}
    {unsaved && <p role="alert" className="text-amber-400">{unsaved}</p>}
    {loading && <p className="text-muted-foreground">Loading stored observations…</p>}
    {!loading && !observation && <p className="text-muted-foreground">No observation recorded for this target. Configuration alone does not establish the device’s actual position mode.</p>}
    {observation && <>
      <label className="block text-muted-foreground">Recorded evidence · not live
        <select aria-label="Recorded observation" className="mt-1 block w-full rounded border border-border bg-background p-2 text-foreground" value={observation.id} onChange={e => setSelectedId(e.target.value)}>
          {[...records].reverse().map(o => <option key={o.id} value={o.id}>{stamp(o.finished_at_unix_ms)} · {o.complete ? "all selected reads returned" : "partial / unavailable"} · {o.id}</option>)}
        </select>
      </label>
      <p className="text-sm font-medium">{retention[observation.retention] ?? observation.retention}</p>
      <p className="text-muted-foreground">Machine reference: {observation.mechanical_reference} · Bus continuity: {observation.bus_continuity}</p>
      <ul className="space-y-2 border-l-2 border-border pl-3">
        {observation.findings.map(f => <li key={f.code} className={f.severity === "warning" ? "text-amber-400" : "text-muted-foreground"}>{f.message}</li>)}
      </ul>
      <details><summary className="cursor-pointer font-medium">Parameter evidence and previous recorded values</summary>
        <p className="my-2 break-all text-muted-foreground">Observed project: {observation.project} · Profile: {observation.profile} · Runtime session: {observation.runtime_session} · Identity strength: {observation.identity_strength}. Previous values are historical; no coordinate continuity is inferred.</p>
        <div className="overflow-x-auto"><table className="w-full text-left text-xs">
          <thead className="text-muted-foreground"><tr><th className="p-2">Parameter</th><th className="p-2">Observed / error</th><th className="p-2">Previous record</th><th className="p-2">Evidence</th></tr></thead>
          <tbody>{observation.parameters.map(p => {
            const previous = prior?.parameters.find(row => row.key === p.key)
            const i = p.reading?.identity
            return <tr key={p.key} className="border-t border-border align-top">
              <td className="p-2">{p.label}<br /><code className="text-muted-foreground">{p.object} · {p.data_type}</code></td>
              <td className="p-2">{p.error ? <span className="text-amber-400">{p.error_kind}: {p.error}</span> : <code>{value(p)}</code>}</td>
              <td className="p-2 font-mono">{previous ? value(previous) : "—"}</td>
              <td className="min-w-56 p-2 text-muted-foreground">{p.reading ? <>
                <code>{p.reading.bytes.map(b => b.toString(16).padStart(2, "0")).join(" ")}</code><br />
                {stamp(p.reading.sampled_at_unix_ms)}<br />
                {i && `Vendor ${i.vendor_id.toString(16)} / Product ${i.product_id.toString(16)} / Rev ${i.revision} / Serial ${i.serial}`}
              </> : "No device value"}</td>
            </tr>
          })}</tbody>
        </table></div>
      </details>
    </>}
  </div>
}
