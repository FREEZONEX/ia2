import {
  Activity,
  AlertCircle,
  CheckCircle2,
  Cpu,
  Gauge,
  Link2,
  Link2Off,
  Loader2,
  Network,
  Pause,
  Pin,
  PinOff,
  Play,
  RefreshCw,
  Rocket,
  Save,
  ScrollText,
  StepForward,
} from "@/components/ui/icons"
import { useEffect, useRef, useState } from "react"

import { StatusBadge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { ErrorBox } from "@/components/ui/error-box"
import { Field } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { PaneHeader } from "@/components/ui/pane-header"
import { EmptyState } from "@/components/ui/empty-state"
import { Label } from "@/components/ui/label"
import {
  deployEdge,
  discoverEdge,
  edgeRuntimeOp,
  fetchEdgeLogs,
  fetchEdge,
  fetchEdgeStatus,
  fetchEdgeSystem,
  probeEdge,
  type DeviceReport,
  type EdgeStatus,
  type EdgeSystem,
} from "@/lib/api"
import { deployOutcome } from "@/lib/deploy-outcome"
import { useRuntime } from "@/state/runtime"
import { DocumentReloadButton, useDocumentDraft } from "@/lib/use-document-draft"
import type { DeployReport } from "@/types/generated/DeployReport"
import type { Edge } from "@/types/generated/Edge"
import type { EdgeProbe } from "@/types/generated/EdgeProbe"

type EdgeTab = "config" | "logs" | "discover" | "system" | "debug"

export function EdgePane() {
  const {
    currentEdge,
    attached,
    saveEdge,
    attachEdge,
    detachEdge,
  } = useRuntime()

  if (!currentEdge) {
    return (
      <main className="ia2-pane">
        <PaneHeader title="Edges" description="Deploy and monitor remote controllers" />
        <EmptyState title="Select an edge" description="Choose an edge in the project tree to configure its connection or inspect its runtime." />
      </main>
    )
  }

  return (
    <main className="ia2-pane">
      <Editor
        key={currentEdge.name}
        edge={currentEdge}
        attached={attached?.name === currentEdge.name}
        onSave={saveEdge}
        onAttach={() => attachEdge(currentEdge.name)}
        onDetach={detachEdge}
      />
    </main>
  )
}

function Editor({
  edge,
  attached,
  onSave,
  onAttach,
  onDetach,
}: {
  edge: Edge
  attached: boolean
  onSave: (e: Edge) => Promise<void>
  onAttach: () => Promise<void>
  onDetach: () => Promise<void>
}) {
  const { draft, setDraft, dirty, conflict, reload } = useDocumentDraft(edge, edge.name)
  const [tab, setTab] = useState<EdgeTab>("config")
  const [probe, setProbe] = useState<EdgeProbe | null>(null)
  const [probing, setProbing] = useState(false)
  const [deploying, setDeploying] = useState(false)
  const [saving, setSaving] = useState(false)
  const [attachmentPending, setAttachmentPending] = useState(false)
  const [deployLog, setDeployLog] = useState<DeployReport | null>(null)
  const [deployError, setDeployError] = useState<string | null>(null)

  useEffect(() => {
    setProbe(null)
    setDeployLog(null)
    setDeployError(null)
  }, [edge])

  // Auto-probe on entry and every 10s while the pane is open. Cheap (one
  // ssh + curl) and keeps the status badge fresh without user clicks.
  const probeName = edge.name
  const setProbeRef = useRef(setProbe)
  setProbeRef.current = setProbe
  useEffect(() => {
    let cancelled = false
    const tick = async () => {
      if (cancelled) return
      try {
        const result = await probeEdge(probeName)
        if (!cancelled) setProbeRef.current(result)
      } catch {
        /* ignore — backend may have moved between project loads */
      }
    }
    void tick()
    const handle = window.setInterval(tick, 10_000)
    return () => {
      cancelled = true
      window.clearInterval(handle)
    }
  }, [probeName])

  const update = (patch: Partial<Edge>) => setDraft({ ...draft, ...patch })

  const probeNow = async () => {
    setProbing(true)
    try {
      setProbe(await probeEdge(edge.name))
    } catch (e) {
      setProbe({
        reachable: false,
        scan_count: null,
        uptime_secs: null,
        runtime_version: null,
        fieldbus_healthy: null,
        unhealthy_devices: [],
        watchdog_tripped: null,
        error: String(e),
      })
    } finally {
      setProbing(false)
    }
  }

  const changeAttachment = async () => {
    if (attachmentPending) return
    setAttachmentPending(true)
    try {
      if (attached) await onDetach()
      else await onAttach()
    } finally {
      setAttachmentPending(false)
    }
  }

  const deploy = async () => {
    if (!confirm(
      `Deploy this project to ${edge.host}?\n` +
        `The runtime will be restarted; the previous version is kept for rollback.`,
    )) {
      return
    }
    setDeploying(true)
    setDeployError(null)
    setDeployLog(null)
    try {
      const report = await deployEdge(edge.name)
      setDeployLog(report)
      // Re-probe so the user sees the new uptime + scan count climb.
      await probeNow()
    } catch (e) {
      setDeployError(String(e))
    } finally {
      setDeploying(false)
    }
  }

  return (
    <>
      <PaneHeader
        title={<span title={edge.name}>{edge.name}</span>}
        description={<span className="font-mono" title={edge.host}>{edge.host}</span>}
        meta={<><ReachBadge probe={probe} probing={probing} />{dirty && <span className="text-warn">Unsaved changes</span>}</>}
        actions={<>
          <DocumentReloadButton reload={() => reload(() => fetchEdge(edge.name))} conflict={conflict} />
          <Button size="sm" disabled={!dirty || saving} aria-busy={saving} onClick={async () => {
          if (saving) return
          setSaving(true)
          try { await onSave(draft) } catch { /* provider displays the error */ } finally { setSaving(false) }
        }}><Save className="size-4" />{saving ? "Saving…" : "Save changes"}</Button></>}
      />

      <EdgeTabs tab={tab} setTab={setTab} />

      {tab === "config" && (
      <div className="min-h-0 flex-1 space-y-6 overflow-auto p-4">
        <section>
          <div className="mb-3 text-xs font-medium text-muted-foreground">
            Connection
          </div>
          <div className="ia2-field-grid">
            <Field label="SSH host / alias">
              <Input
                value={draft.host}
                onChange={(e) => update({ host: e.target.value })}
                placeholder="line1.lan or production-line-1"
              />
            </Field>
            <Field label="SSH port">
              <Input
                type="number"
                value={draft.ssh_port}
                onChange={(e) =>
                  update({ ssh_port: Number(e.target.value) || 22 })
                }
              />
            </Field>
            <Field label="SSH user (optional)">
              <Input
                value={draft.ssh_user}
                placeholder="(default — from ~/.ssh/config)"
                onChange={(e) => update({ ssh_user: e.target.value })}
              />
            </Field>
            <Field label="Runtime port (loopback on edge)">
              <Input
                type="number"
                value={draft.runtime_port}
                onChange={(e) =>
                  update({ runtime_port: Number(e.target.value) || 13001 })
                }
              />
            </Field>
            <Field label="Install dir on edge">
              <Input
                value={draft.install_dir}
                onChange={(e) => update({ install_dir: e.target.value })}
              />
            </Field>
          </div>
          <p className="mt-3 max-w-2xl text-xs text-muted-foreground">
            What runs on this edge is the project's <span className="font-mono">tasks.toml</span>{" "}
            (every PROGRAM instance declared there, on its bound task) — not a
            single POU. Edit the Tasks pane to change the schedule.
          </p>
        </section>

        <section>
          <Label htmlFor="edge-notes" className="text-xs text-muted-foreground">
            Notes
          </Label>
          <textarea
            id="edge-notes"
            value={draft.notes}
            onChange={(e) => update({ notes: e.target.value })}
            placeholder="Free-form: production line 1, hardware revision, on-site contacts…"
            rows={3}
            className="mt-1.5 block w-full max-w-2xl resize-y rounded-md border border-border bg-background px-2 py-1.5 text-[13px] placeholder:text-muted-foreground"
          />
        </section>

        <section>
          <div className="mb-3 text-xs font-medium text-muted-foreground">
            Actions
          </div>
          <div className="flex flex-wrap gap-2">
            <Button size="sm" variant="outline" onClick={probeNow} disabled={probing}>
              {probing ? (
                <Loader2 className="mr-1.5 size-3 animate-spin" />
              ) : (
                <Activity className="mr-1.5 size-3" />
              )}
              Probe now
            </Button>
            <Button
              size="sm"
              variant="default"
              onClick={deploy}
              disabled={deploying || dirty}
              title={
                dirty
                  ? "Save changes before deploying"
                  : "Bundle the project (+ runtime binary if locally built) and push to the edge"
              }
            >
              {deploying ? (
                <Loader2 className="mr-1.5 size-3 animate-spin" />
              ) : (
                <Rocket className="mr-1.5 size-3" />
              )}
              Deploy
            </Button>
            {attached ? (
              <Button
                size="sm"
                variant="outline"
                onClick={() => void changeAttachment()}
                disabled={attachmentPending}
                className="border-highlight/40 text-highlight"
              >
                <Link2Off className="mr-1.5 size-3" />
                {attachmentPending ? "Detaching…" : "Detach"}
              </Button>
            ) : (
              <Button
                size="sm"
                variant="outline"
                onClick={() => void changeAttachment()}
                disabled={probe?.reachable !== true || attachmentPending}
                title={
                  probe?.reachable
                    ? "Open SSH port-forward and stream live variables from the edge runtime"
                    : "Edge unreachable — probe first"
                }
              >
                <Link2 className="mr-1.5 size-3" />
                {attachmentPending ? "Attaching…" : "Attach"}
              </Button>
            )}
          </div>
        </section>

        <section>
          <div className="mb-3 text-xs font-medium text-muted-foreground">
            Runtime status
          </div>
          {probe ? (
            <StatusGrid probe={probe} />
          ) : (
            <p className="text-xs text-muted-foreground">
              Probing… (or click <span className="font-mono">Probe now</span>)
            </p>
          )}
          {attached && (
            <div className="mt-3 inline-flex items-center gap-1.5 rounded-md border border-highlight/30 bg-highlight/10 px-2 py-1 text-[12px] text-highlight">
              <Link2 className="size-3" />
              IDE is attached — Monitor and Variables panes are now showing
              live data from this edge.
            </div>
          )}
        </section>

        {(deployLog || deployError) && (
          <section>
            <div className="mb-2 text-xs font-medium text-muted-foreground">
              Last deploy
            </div>
            {deployError ? (
              <ErrorBox className="max-w-full p-3 text-xs">{deployError}</ErrorBox>
            ) : deployLog ? (
              <div>
                <DeployVerdict report={deployLog} />
                <pre className="max-h-64 max-w-full overflow-auto rounded-md border border-border bg-muted/40 p-3 text-xs text-muted-foreground">
                  {deployLog.log}
                </pre>
              </div>
            ) : null}
          </section>
        )}
      </div>
      )}

      {tab === "logs" && <LogsPanel name={edge.name} />}
      {tab === "discover" && <DiscoverPanel name={edge.name} />}
      {tab === "system" && <SystemPanel name={edge.name} />}
      {tab === "debug" && <DebugPanel name={edge.name} />}
    </>
  )
}

function EdgeTabs({
  tab,
  setTab,
}: {
  tab: EdgeTab
  setTab: (t: EdgeTab) => void
}) {
  const tabs: [EdgeTab, string, React.ComponentType<{ className?: string }>][] = [
    ["config", "Config", Save],
    ["logs", "Logs", ScrollText],
    ["discover", "Discover", Network],
    ["system", "System", Cpu],
    ["debug", "Debug", Gauge],
  ]
  return (
    <div className="ia2-toolbar overflow-x-auto">
      {tabs.map(([id, label, Icon]) => (
        <button
          key={id}
          type="button"
          aria-pressed={tab === id}
          onClick={() => setTab(id)}
          className={`-mb-px flex items-center gap-1.5 border-b-2 px-2.5 py-1.5 whitespace-nowrap text-[13px] transition-colors ${
            tab === id
              ? "border-foreground text-foreground"
              : "border-transparent text-muted-foreground hover:text-foreground"
          }`}
        >
          <Icon className="size-3" />
          {label}
        </button>
      ))}
    </div>
  )
}

/** Live edge-runtime log — polls GET /logs every 2s. */
function LogsPanel({ name }: { name: string }) {
  const [lines, setLines] = useState<string[]>([])
  const [error, setError] = useState<string | null>(null)
  const [paused, setPaused] = useState(false)
  const pausedRef = useRef(false)
  pausedRef.current = paused
  useEffect(() => {
    let cancelled = false
    const tick = async () => {
      if (cancelled || pausedRef.current) return
      try {
        const r = await fetchEdgeLogs(name, 400)
        if (!cancelled) {
          setLines(r.lines)
          setError(null)
        }
      } catch (e) {
        if (!cancelled) setError(String(e))
      }
    }
    void tick()
    const h = window.setInterval(tick, 2000)
    return () => {
      cancelled = true
      window.clearInterval(h)
    }
  }, [name])
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center justify-between border-b border-border px-3 py-1.5 text-xs text-muted-foreground">
        <span className="font-mono">Runtime log · updates every 2 seconds</span>
        <button
          onClick={() => setPaused((p) => !p)}
          className="rounded border border-border px-1.5 py-0.5 hover:text-foreground"
        >
          {paused ? "Resume" : "Pause"}
        </button>
      </div>
      {error && (
        <div className="border-b border-destructive/30 bg-destructive/5 px-3 py-1.5 text-xs text-destructive">
          {error}
        </div>
      )}
      <pre className="flex-1 overflow-auto whitespace-pre-wrap break-all bg-muted/20 p-3 font-mono text-xs leading-relaxed">
        {lines.length ? lines.join("\n") : "(no log lines yet)"}
      </pre>
    </div>
  )
}

/** Per-device connect status + discovered EtherCAT topology (GET /discover). */
function DiscoverPanel({ name }: { name: string }) {
  const [devs, setDevs] = useState<DeviceReport[] | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const load = async () => {
    setLoading(true)
    setError(null)
    try {
      setDevs(await discoverEdge(name))
    } catch (e) {
      setError(String(e))
    } finally {
      setLoading(false)
    }
  }
  useEffect(() => {
    void load()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [name])
  return (
    <div className="flex-1 space-y-4 overflow-auto p-4">
      <div className="flex items-center justify-between">
        <span className="text-xs text-muted-foreground">
          Bus discovery
        </span>
        <Button size="sm" variant="outline" onClick={() => void load()} disabled={loading}>
          {loading ? (
            <Loader2 className="mr-1.5 size-3 animate-spin" />
          ) : (
            <RefreshCw className="mr-1.5 size-3" />
          )}
          Scan
        </Button>
      </div>
      {error && <ErrorBox>{error}</ErrorBox>}
      {devs?.map((d) => (
        <div key={d.name} className="overflow-hidden rounded-md border border-border">
          <div className="flex items-center gap-2 border-b border-border bg-muted/30 px-3 py-2 text-[13px]">
            {d.connected ? (
              <CheckCircle2 className="size-3.5 text-highlight" />
            ) : (
              <AlertCircle className="size-3.5 text-destructive" />
            )}
            <span className="font-mono">{d.name}</span>
            <span className="rounded border border-border px-1.5 py-0.5 font-mono text-xs text-muted-foreground">
              {d.protocol}
            </span>
            {!d.connected && d.error && (
              <span
                className="truncate text-xs text-destructive"
                title={d.error}
              >
                {d.error}
              </span>
            )}
          </div>
          {d.connected &&
            (d.slaves.length ? (
              <div className="min-w-0 overflow-x-auto">
                <table className="ia2-table w-full min-w-[640px]">
                  <thead>
                    <tr className="text-left text-xs text-muted-foreground">
                      <th className="px-3 py-1 font-medium">#</th>
                      <th className="py-1 font-medium">Name</th>
                      <th className="py-1 font-medium">Vendor</th>
                      <th className="py-1 font-medium">Product</th>
                      <th className="py-1 font-medium">In</th>
                      <th className="py-1 pr-3 font-medium">Out</th>
                    </tr>
                  </thead>
                  <tbody className="font-mono">
                    {d.slaves.map((s) => (
                      <tr key={s.index} className="border-t border-border/50">
                        <td className="px-3 py-1">{s.index}</td>
                        <td className="py-1">{s.name}</td>
                        <td className="py-1">
                          0x{s.vendor_id.toString(16).padStart(8, "0")}
                        </td>
                        <td className="py-1">
                          0x{s.product_id.toString(16).padStart(8, "0")}
                        </td>
                        <td className="py-1">{s.input_bytes}B</td>
                        <td className="py-1 pr-3">{s.output_bytes}B</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            ) : (
              <div className="px-3 py-2 text-xs text-muted-foreground">
                no slaves on the bus
              </div>
            ))}
        </div>
      ))}
      {devs && devs.length === 0 && (
        <div className="text-xs text-muted-foreground">no devices in project</div>
      )}
    </div>
  )
}

/** Edge interfaces / serial ports / arch (GET /system). */
function SystemPanel({ name }: { name: string }) {
  const [sys, setSys] = useState<EdgeSystem | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const load = async () => {
    setLoading(true)
    setError(null)
    try {
      setSys(await fetchEdgeSystem(name))
    } catch (e) {
      setError(String(e))
    } finally {
      setLoading(false)
    }
  }
  useEffect(() => {
    void load()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [name])
  return (
    <div className="flex-1 space-y-4 overflow-auto p-4">
      <div className="flex items-center justify-between">
        <span className="text-xs text-muted-foreground">
          System
          {sys && (
            <span className="ml-1 font-mono normal-case lowercase">
              · {sys.os}/{sys.arch}
            </span>
          )}
        </span>
        <Button size="sm" variant="outline" onClick={() => void load()} disabled={loading}>
          {loading ? (
            <Loader2 className="mr-1.5 size-3 animate-spin" />
          ) : (
            <RefreshCw className="mr-1.5 size-3" />
          )}
          Refresh
        </Button>
      </div>
      {error && <ErrorBox>{error}</ErrorBox>}
      {sys && (
        <>
          <div>
            <div className="mb-1.5 text-xs text-muted-foreground">
              Network interfaces (pick one for an EtherCAT nic)
            </div>
            <div className="overflow-hidden rounded-md border border-border">
              <div className="min-w-0 overflow-x-auto">
                <table className="ia2-table w-full min-w-[640px]">
                  <tbody className="font-mono">
                    {sys.nics.map((n) => (
                      <tr key={n.name} className="border-b border-border/50 last:border-0">
                        <td className="px-3 py-1.5">{n.name}</td>
                        <td className="py-1.5 text-muted-foreground">{n.operstate}</td>
                        <td className="py-1.5">
                          {n.carrier ? (
                            <span className="text-highlight">
                              carrier
                            </span>
                          ) : (
                            <span className="text-muted-foreground">no-carrier</span>
                          )}
                        </td>
                        <td className="py-1.5 pr-3 text-muted-foreground">{n.mac}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </div>
          </div>
          <div>
            <div className="mb-1.5 text-xs text-muted-foreground">
              Serial ports (pick one for a Modbus RTU device)
            </div>
            {sys.serial_ports.length ? (
              <ul className="space-y-0.5 font-mono text-xs">
                {sys.serial_ports.map((p) => (
                  <li key={p} className="rounded border border-border px-2 py-1">
                    {p}
                  </li>
                ))}
              </ul>
            ) : (
              <div className="text-xs text-muted-foreground">(none detected)</div>
            )}
          </div>
        </>
      )}
    </div>
  )
}

/** Online-debug control for the edge: mode + Pause/Resume/Step + live
 *  variables with force/unforce. Drives the same server-proxy routes as
 *  `cs runtime --edge` (/api/edges/{name}/runtime/{op} + /status). */
function DebugPanel({ name }: { name: string }) {
  const [status, setStatus] = useState<EdgeStatus | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [forceVar, setForceVar] = useState("")
  const [forceVal, setForceVal] = useState("")

  const refresh = async () => {
    try {
      setStatus(await fetchEdgeStatus(name))
      setError(null)
    } catch (e) {
      setError(String(e))
    }
  }
  useEffect(() => {
    let cancelled = false
    const tick = async () => {
      if (!cancelled) await refresh()
    }
    void tick()
    const h = window.setInterval(tick, 1500)
    return () => {
      cancelled = true
      window.clearInterval(h)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [name])

  const op = async (
    o: "pause" | "resume" | "step" | "force" | "unforce",
    body?: Record<string, unknown>,
  ) => {
    setBusy(true)
    try {
      await edgeRuntimeOp(name, o, body)
      await refresh()
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  const mode = status?.mode.kind ?? "—"
  const halted = mode === "paused" || mode === "step"
  const forced = new Set((status?.forces ?? []).map((f) => f.name))
  const vars = status?.last_snapshot?.vars ?? []

  return (
    <div className="flex-1 space-y-4 overflow-auto p-4">
      <div className="flex items-center gap-2">
        <StatusBadge
          className={
            mode === "running"
              ? "bg-highlight/15 text-highlight"
              : "bg-warn/15 text-warn"
          }
        >
          {mode}
          {status?.mode.remaining != null ? ` ${status.mode.remaining}` : ""}
        </StatusBadge>
        <span className="text-xs text-muted-foreground">
          scan {status ? status.scan_count.toLocaleString() : "—"}
        </span>
        <div className="ml-auto flex gap-1.5">
          {halted ? (
            <Button size="sm" variant="outline" disabled={busy} onClick={() => void op("resume")}>
              <Play className="mr-1.5 size-3" />
              Resume
            </Button>
          ) : (
            <Button size="sm" variant="outline" disabled={busy} onClick={() => void op("pause")}>
              <Pause className="mr-1.5 size-3" />
              Pause
            </Button>
          )}
          <Button
            size="sm"
            variant="outline"
            disabled={busy}
            onClick={() => void op("step", { cycles: 1 })}
          >
            <StepForward className="mr-1.5 size-3" />
            Step
          </Button>
        </div>
      </div>

      {error && <ErrorBox>{error}</ErrorBox>}

      <div className="overflow-hidden rounded-md border border-border">
        <div className="min-w-0 overflow-x-auto">
          <table className="ia2-table w-full min-w-[640px]">
            <thead>
              <tr className="text-left text-xs text-muted-foreground">
                <th className="px-3 py-1 font-medium">Variable</th>
                <th className="py-1 font-medium">Type</th>
                <th className="py-1 font-medium">Value</th>
                <th className="py-1 pr-3" />
              </tr>
            </thead>
            <tbody className="font-mono">
              {vars.length === 0 && (
                <tr>
                  <td colSpan={4} className="px-3 py-2 text-muted-foreground">
                    no variables yet (runtime not reporting a snapshot)
                  </td>
                </tr>
              )}
              {vars.map((v) => (
                <tr key={v.name} className="border-t border-border/50">
                  <td className="px-3 py-1">{v.name}</td>
                  <td className="py-1 text-muted-foreground">{v.type_name}</td>
                  <td className="py-1">
                    {v.value}
                    {forced.has(v.name) && (
                      <span className="ml-1.5 text-warn">forced</span>
                    )}
                  </td>
                  <td className="py-1 pr-3 text-right">
                    {forced.has(v.name) && (
                      <button
                        title="Unforce"
                        disabled={busy}
                        onClick={() => void op("unforce", { name: v.name })}
                        className="text-muted-foreground hover:text-foreground"
                      >
                        <PinOff className="size-3.5" />
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>

      <div className="flex items-end gap-2">
        <div className="flex-1 space-y-1.5">
          <Label className="text-xs text-muted-foreground">
            Force a variable (integer value)
          </Label>
          <select
            value={forceVar}
            onChange={(e) => setForceVar(e.target.value)}
            className="block w-full rounded-md border border-border bg-background px-2 py-1.5 text-[13px]"
          >
            <option value="">Select…</option>
            {vars.map((v) => (
              <option key={v.name} value={v.name}>
                {v.name} ({v.type_name})
              </option>
            ))}
          </select>
        </div>
        <Input
          className="w-28"
          placeholder="value"
          value={forceVal}
          onChange={(e) => setForceVal(e.target.value)}
        />
        <Button
          size="sm"
          disabled={busy || !forceVar || forceVal === "" || Number.isNaN(Number(forceVal))}
          onClick={() =>
            void op("force", { name: forceVar, value: Number(forceVal) }).then(() =>
              setForceVal(""),
            )
          }
        >
          <Pin className="mr-1.5 size-3" />
          Force
        </Button>
      </div>
    </div>
  )
}

/** The headline of the last deploy: live, installed but not running, or
 *  failed — see `deployOutcome` — plus the report's warning. */
function DeployVerdict({ report }: { report: DeployReport }) {
  const outcome = deployOutcome(report)
  return (
    <div className="mb-1 flex flex-col gap-1 text-[12px]">
      {outcome.kind === "live" ? (
        <div className="inline-flex items-center gap-1.5 text-highlight">
          <CheckCircle2 className="size-3.5" />
          <span>
            Version <span className="font-mono">{outcome.version}</span> live
            {outcome.detail && <span className="text-muted-foreground"> — {outcome.detail}</span>}
          </span>
        </div>
      ) : outcome.kind === "unconfirmed" ? (
        <div className="inline-flex items-center gap-1.5 text-warn">
          <AlertCircle className="size-3.5" />
          <span>
            Version <span className="font-mono">{outcome.version}</span> installed;
            program state unconfirmed: {outcome.detail}. Check runtime and plant state
            before restarting or rolling back.
          </span>
        </div>
      ) : outcome.kind === "not_running" ? (
        <ErrorBox className="max-w-full whitespace-pre-wrap break-words p-2 text-xs">
          Version <span className="font-mono">{outcome.version}</span> is installed and
          current, but continuous operation failed: {outcome.detail}. Inspect runtime and
          plant state before restarting or rolling back — the log names the previous version.
        </ErrorBox>
      ) : (
        <ErrorBox className="max-w-full whitespace-pre-wrap p-2 text-xs">
          Deploy failed — read the log below.
        </ErrorBox>
      )}
      {report.warning && (
        <div className="inline-flex items-center gap-1.5 text-warn">
          <AlertCircle className="size-3.5" />
          {report.warning}
        </div>
      )}
    </div>
  )
}

function ReachBadge({
  probe,
  probing,
}: {
  probe: EdgeProbe | null
  probing: boolean
}) {
  if (probing) {
    return (
      <StatusBadge className="bg-muted/50 text-muted-foreground">
        <Loader2 className="size-3 animate-spin" />
        probing
      </StatusBadge>
    )
  }
  if (!probe) {
    return (
      <StatusBadge className="bg-muted/50 text-muted-foreground">
        unknown
      </StatusBadge>
    )
  }
  if (probe.reachable) {
    // Reachable is not the same as working: the runtime answers /health
    // and keeps scanning while a fieldbus is down. Say so, rather than
    // showing the same green as a fully healthy edge.
    const down = probe.unhealthy_devices
    if (down.length > 0) {
      return (
        <StatusBadge
          className="bg-warn/15 text-warn"
          title={`Runtime is up, but ${down.length === 1 ? "this device is" : "these devices are"} down (inputs frozen, outputs dropped): ${down.join(", ")}`}
        >
          <AlertCircle className="size-3" />
          degraded
        </StatusBadge>
      )
    }
    return (
      <StatusBadge
        className="bg-highlight/15 text-highlight"
        title="Edge runtime is responding"
      >
        <CheckCircle2 className="size-3" />
        running
      </StatusBadge>
    )
  }
  return (
    <StatusBadge
      className="bg-destructive/15 text-destructive"
      title={probe.error ?? ""}
    >
      <AlertCircle className="size-3" />
      unreachable
    </StatusBadge>
  )
}

function StatusGrid({ probe }: { probe: EdgeProbe }) {
  return (
    <dl className="ia2-field-grid text-[13px]">
      <Stat
        label="Uptime"
        value={
          probe.uptime_secs == null
            ? "—"
            : formatDuration(Number(probe.uptime_secs))
        }
      />
      <Stat
        label="Scan count"
        value={
          probe.scan_count == null ? "—" : Number(probe.scan_count).toLocaleString()
        }
      />
      <Stat label="Runtime version" value={probe.runtime_version ?? "—"} />
      {!probe.reachable && probe.error && (
        <div className="col-span-full">
          <ErrorBox>{probe.error}</ErrorBox>
        </div>
      )}
    </dl>
  )
}

function Stat({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div className="min-w-0 border-b border-border py-3">
      <dt className="text-xs text-muted-foreground">
        {label}
      </dt>
      <dd className="mt-1 break-words font-mono text-[13px] tabular-nums">{value}</dd>
    </div>
  )
}

function formatDuration(secs: number): string {
  if (!Number.isFinite(secs) || secs < 0) return "—"
  const h = Math.floor(secs / 3600)
  const m = Math.floor((secs % 3600) / 60)
  const s = Math.floor(secs % 60)
  if (h > 0) return `${h}h ${m}m`
  if (m > 0) return `${m}m ${s}s`
  return `${s}s`
}
