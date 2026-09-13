import { Plus, Save, Trash2 } from "@/components/ui/icons"
import { useEffect, useMemo, useState } from "react"

import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { EmptyState } from "@/components/ui/empty-state"
import { PaneHeader } from "@/components/ui/pane-header"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { fetchPouVariables, fetchDemoSlaveSnapshot } from "@/lib/api"
import { useRuntime } from "@/state/runtime"
import type { DemoSlaveSnapshot } from "@/types/generated/DemoSlaveSnapshot"
import type { Direction } from "@/types/generated/Direction"
import type { IoMap } from "@/types/generated/IoMap"
import type { Mapping } from "@/types/generated/Mapping"
import type { VariableInfo } from "@/types/generated/VariableInfo"

const SLAVE_POLL_MS = 500

export function IoMapPane() {
  const { project, iomap, isRunning, saveIomap } = useRuntime()
  const [draft, setDraft] = useState<IoMap>(iomap)
  const [saving, setSaving] = useState(false)
  const [vars, setVars] = useState<Record<string, VariableInfo[]>>({})
  const [slave, setSlave] = useState<DemoSlaveSnapshot | null>(null)

  useEffect(() => setDraft(iomap), [iomap])

  // Pre-fetch variables for every POU so the variable column has datalist
  // suggestions ready without per-row latency.
  useEffect(() => {
    if (!project) return
    let cancelled = false
    Promise.all(
      project.pous.map((p) =>
        fetchPouVariables(p.path)
          .then((vs) => [p.path, vs] as const)
          .catch(() => [p.path, [] as VariableInfo[]] as const),
      ),
    ).then((entries) => {
      if (!cancelled) setVars(Object.fromEntries(entries))
    })
    return () => {
      cancelled = true
    }
  }, [project])

  // Live-poll the demo slave while the program runs.
  useEffect(() => {
    if (!isRunning) {
      setSlave(null)
      return
    }
    let cancelled = false
    const tick = () => {
      fetchDemoSlaveSnapshot()
        .then((s) => {
          if (!cancelled) setSlave(s)
        })
        .catch(() => {})
    }
    tick()
    const handle = window.setInterval(tick, SLAVE_POLL_MS)
    return () => {
      cancelled = true
      window.clearInterval(handle)
    }
  }, [isRunning])

  const dirty = JSON.stringify(draft) !== JSON.stringify(iomap)

  const set = (idx: number, patch: Partial<Mapping>) => {
    const next = draft.mappings.map((m, i) =>
      i === idx ? { ...m, ...patch } : m,
    )
    setDraft({ mappings: next })
  }

  const add = () => {
    setDraft({
      mappings: [
        ...draft.mappings,
        {
          application: project?.pous[0]?.path ?? "",
          variable: "",
          direction: "output",
          device: project?.devices[0]?.name ?? "",
          channel: "",
        },
      ],
    })
  }

  const remove = (idx: number) => {
    setDraft({ mappings: draft.mappings.filter((_, i) => i !== idx) })
  }

  return (
    <main className="ia2-pane">
      <PaneHeader
        title="IO Mapping"
        description="Bind program variables to device channels"
        meta={dirty ? <span className="text-warn">Unsaved changes</span> : `${draft.mappings.length} mappings`}
        actions={
          <>
            <Button size="sm" variant="outline" onClick={add}>
              <Plus className="size-4" /> Add mapping
            </Button>
            <Button size="sm" disabled={!dirty || saving} aria-busy={saving} onClick={async () => {
              if (saving) return
              setSaving(true)
              try { await saveIomap(draft) } finally { setSaving(false) }
            }}>
              <Save className="size-4" /> {saving ? "Saving…" : "Save changes"}
            </Button>
          </>
        }
      />
      <div className="flex-1 overflow-auto p-4">
        {draft.mappings.length === 0 ? (
          <EmptyState title="No variable bindings" description="Add a mapping, then choose a program variable and the device channel it reads or writes." actions={<Button size="sm" onClick={add}><Plus className="size-4" /> Add mapping</Button>} />
        ) : (
          <div className="min-w-0 overflow-x-auto">
            <table className="ia2-table w-full min-w-[640px]">
              <thead className="text-xs text-muted-foreground">
                <tr className="border-b border-border">
                  <th className="px-2 py-1.5 text-left">Application</th>
                  <th className="px-2 py-1.5 text-left">Variable</th>
                  <th className="px-2 py-1.5 text-left">Direction</th>
                  <th className="px-2 py-1.5 text-left">Device</th>
                  <th className="px-2 py-1.5 text-left">Channel</th>
                  <th className="px-2 py-1.5 text-left">Current</th>
                  <th className="px-2 py-1.5"></th>
                </tr>
              </thead>
              <tbody>
                {draft.mappings.map((m, i) => (
                  <MappingRow
                    key={i}
                    mapping={m}
                    appVars={vars[m.application] ?? []}
                    channelNames={channelsOf(project, m.device)}
                    liveValue={currentValue(m, project, slave)}
                    applications={project?.pous.map((p) => p.path) ?? []}
                    devices={project?.devices.map((d) => d.name) ?? []}
                    isRunning={isRunning}
                    onChange={(patch) => set(i, patch)}
                    onRemove={() => remove(i)}
                  />
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </main>
  )
}

function MappingRow({
  mapping,
  appVars,
  channelNames,
  liveValue,
  applications,
  devices,
  isRunning,
  onChange,
  onRemove,
}: {
  mapping: Mapping
  appVars: VariableInfo[]
  channelNames: string[]
  liveValue: string | null
  applications: string[]
  devices: string[]
  isRunning: boolean
  onChange: (patch: Partial<Mapping>) => void
  onRemove: () => void
}) {
  // Memoize the datalist IDs so they're stable across renders.
  const varListId = useMemo(
    () => `vars-${mapping.application || "_"}`,
    [mapping.application],
  )
  const chListId = useMemo(
    () => `chs-${mapping.device || "_"}`,
    [mapping.device],
  )

  return (
    <tr className="border-b border-border last:border-0">
      <td className="px-2 py-1.5">
        <Select
          value={mapping.application}
          onValueChange={(v) => onChange({ application: v })}
        >
          <SelectTrigger className="h-8 w-44">
            <SelectValue placeholder="(none)" />
          </SelectTrigger>
          <SelectContent>
            {applications.map((a) => (
              <SelectItem key={a} value={a}>
                {a}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </td>
      <td className="px-2 py-1.5">
        <Input
          list={varListId}
          value={mapping.variable}
          onChange={(e) => onChange({ variable: e.target.value })}
          className="h-8 w-40"
          placeholder="counter"
        />
        <datalist id={varListId}>
          {appVars.map((v) => (
            <option key={v.name} value={v.name}>
              {v.type_name} · {v.direction}
            </option>
          ))}
        </datalist>
      </td>
      <td className="px-2 py-1.5">
        <Select
          value={mapping.direction}
          onValueChange={(v) => onChange({ direction: v as Direction })}
        >
          <SelectTrigger className="h-8 w-32">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="input">input (bus → var)</SelectItem>
            <SelectItem value="output">output (var → bus)</SelectItem>
          </SelectContent>
        </Select>
      </td>
      <td className="px-2 py-1.5">
        <Select
          value={mapping.device}
          onValueChange={(v) => onChange({ device: v })}
        >
          <SelectTrigger className="h-8 w-40">
            <SelectValue placeholder="(none)" />
          </SelectTrigger>
          <SelectContent>
            {devices.map((d) => (
              <SelectItem key={d} value={d}>
                {d}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </td>
      <td className="px-2 py-1.5">
        <Input
          list={chListId}
          value={mapping.channel}
          onChange={(e) => onChange({ channel: e.target.value })}
          className="h-8 w-40"
          placeholder="coil_0"
        />
        <datalist id={chListId}>
          {channelNames.map((c) => (
            <option key={c} value={c} />
          ))}
        </datalist>
      </td>
      <td className="px-2 py-1.5 font-mono tabular-nums text-[13px]">
        {isRunning ? (
          liveValue ?? <span className="text-muted-foreground">—</span>
        ) : (
          <span className="text-muted-foreground/60">·</span>
        )}
      </td>
      <td className="px-2 py-1.5 text-right">
        <button
          type="button"
          onClick={onRemove}
          className="inline-flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-accent/40 hover:text-destructive"
          title="Remove"
        >
          <Trash2 className="size-3.5" />
        </button>
      </td>
    </tr>
  )
}

function channelsOf(
  project: ReturnType<typeof useRuntime>["project"],
  deviceName: string,
): string[] {
  const device = project?.devices.find((d) => d.name === deviceName)
  if (!device) return []
  // Every protocol variant carries a `channels` list with `name`s —
  // no per-protocol casing (an earlier version listed protocols and
  // silently returned [] for OPC UA devices).
  return device.channels.map((c) => c.name)
}

/** Resolve a mapping's current bus value from the demo slave snapshot. */
function currentValue(
  mapping: Mapping,
  project: ReturnType<typeof useRuntime>["project"],
  slave: DemoSlaveSnapshot | null,
): string | null {
  if (!slave || !project) return null
  const device = project.devices.find((d) => d.name === mapping.device)
  // EtherCAT sim-mode buffer lives inside the runtime thread and isn't
  // peekable like the demo Modbus slave, so leave the "Current" column
  // empty for ethercat channels rather than showing stale/zero values.
  if (!device || device.protocol !== "modbus") return null
  const channel = device.channels.find((c) => c.name === mapping.channel)
  if (!channel) return null
  const addr = channel.address
  switch (channel.kind) {
    case "coil":
      return slave.coils[addr] !== undefined
        ? slave.coils[addr]
          ? "TRUE"
          : "FALSE"
        : null
    case "discrete_input":
      return slave.discrete_inputs[addr] !== undefined
        ? slave.discrete_inputs[addr]
          ? "TRUE"
          : "FALSE"
        : null
    case "holding_register":
      return slave.holding_registers[addr]?.toString() ?? null
    case "input_register":
      return slave.input_registers[addr]?.toString() ?? null
  }
}
