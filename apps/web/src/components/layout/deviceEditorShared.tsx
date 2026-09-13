import { Link2, Save, X } from "@/components/ui/icons"
import { useEffect, useMemo, useState } from "react"

import { Button } from "@/components/ui/button"
import { EnumSelect } from "@/components/ui/enum-select"
import { Input } from "@/components/ui/input"
import { PaneHeader } from "@/components/ui/pane-header"
import { ErrorBox } from "@/components/ui/error-box"
import type { Device } from "@/types/generated/Device"
import type { Direction } from "@/types/generated/Direction"
import type { IoMap } from "@/types/generated/IoMap"
import type { Mapping } from "@/types/generated/Mapping"
import type { VariableInfo } from "@/types/generated/VariableInfo"

// ============================================================
//  Shared plumbing for the per-protocol device editors
// ============================================================

/** Bundled props the Linked-to column needs from the runtime. Passed down
 * to every protocol editor so each channel row can render the same
 * LinkedToCell. */
export type LinkProps = {
  deviceName: string
  iomap: IoMap
  saveIomap: (next: IoMap) => Promise<void>
  apps: string[]
  varsByApp: Record<string, VariableInfo[]>
}

/** Common shape of every protocol editor: the device to edit, a save
 * callback, and the linked-to plumbing. */
export type DeviceEditorProps = {
  device: Device
  onSave: (device: Device) => Promise<void>
  link: LinkProps
}

/**
 * Draft/dirty scaffold shared by all three editors: seed local state from
 * the device, reset when the upstream device changes, and derive `dirty` by
 * value comparison. Each editor keeps its own one-line `update` (typed to
 * its narrowed variant) because `Device` is a discriminated union — a
 * union-wide `Partial<Device>` would only expose the keys common to every
 * protocol.
 */
export function useDeviceDraft(device: Device) {
  const [draft, setDraft] = useState<Device>(device)
  // Reset the draft whenever the upstream device changes (e.g. a different
  // device is selected in the tree).
  useEffect(() => {
    setDraft(device)
  }, [device])
  const dirty = JSON.stringify(draft) !== JSON.stringify(device)
  return { draft, setDraft, dirty }
}

/** The name + protocol + modified badges + Save button strip atop each
 * editor. `protocol` is the display label ("modbus" / "ethercat" /
 * "opc ua"). */
export function DeviceSaveBar({
  name,
  protocol,
  dirty,
  onSave,
}: {
  name: string
  protocol: string
  dirty: boolean
  onSave: () => Promise<void>
}) {
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const save = async () => {
    if (saving || !dirty) return
    setSaving(true)
    setError(null)
    try {
      await onSave()
    } catch (e) {
      setError(String(e))
    } finally {
      setSaving(false)
    }
  }
  return (
    <>
      <PaneHeader
        title={<span title={name}>{name}</span>}
        description={protocol}
        meta={dirty ? <span className="text-warn">Unsaved changes</span> : undefined}
        actions={
          <Button size="sm" onClick={() => void save()} disabled={!dirty || saving} aria-busy={saving}>
            <Save className="size-4" />
            {saving ? "Saving…" : "Save changes"}
          </Button>
        }
      />
      {error && <ErrorBox className="m-4 mb-0">{error}</ErrorBox>}
    </>
  )
}

/** Section title, optionally with a right-aligned action (an
 * "Add …" button). */
export function SectionHeader({
  title,
  action,
}: {
  title: string
  action?: React.ReactNode
}) {
  if (action) {
    return (
      <div className="mb-3 flex items-center justify-between">
        <h2 className="text-[13px] font-medium text-foreground">
          {title}
        </h2>
        {action}
      </div>
    )
  }
  return (
    <h2 className="mb-3 text-[13px] font-medium text-foreground">
      {title}
    </h2>
  )
}

/** Quiet placeholder shown when a channel/slave/tag table is
 * empty. */
export function EmptyBox({ children }: { children: React.ReactNode }) {
  return (
    <div className="bg-muted/40 px-4 py-6 text-[13px] leading-6 text-muted-foreground">
      {children}
    </div>
  )
}

// ============================================================
//  LinkedToCell — shared across every protocol's channel rows
// ============================================================

/** Stable string key for one Mapping → used to identify pills for removal. */
function mappingKey(m: Mapping): string {
  return `${m.application}::${m.variable}::${m.direction}`
}

export function LinkedToCell({
  channelName,
  link,
}: {
  channelName: string
  link: LinkProps
}) {
  const [adding, setAdding] = useState(false)
  const [draft, setDraft] = useState<{
    application: string
    variable: string
    direction: Direction
  }>({
    application: link.apps[0] ?? "",
    variable: "",
    direction: "output",
  })

  const bindings = useMemo(
    () =>
      link.iomap.mappings.filter(
        (m) => m.device === link.deviceName && m.channel === channelName,
      ),
    [link.iomap, link.deviceName, channelName],
  )

  // Variables for the currently-selected app, used for the variable input
  // datalist. Empty array if the app hasn't been fetched yet.
  const appVars = link.varsByApp[draft.application] ?? []
  const varListId = `link-vars-${link.deviceName}-${channelName}-${draft.application || "_"}`

  const remove = async (target: Mapping) => {
    const next: IoMap = {
      mappings: link.iomap.mappings.filter(
        (m) => m.device !== link.deviceName || mappingKey(m) !== mappingKey(target) || m.channel !== channelName,
      ),
    }
    await link.saveIomap(next)
  }

  const commit = async () => {
    const variable = draft.variable.trim()
    if (!variable || !draft.application || !channelName) return
    const newMapping: Mapping = {
      application: draft.application,
      variable,
      direction: draft.direction,
      device: link.deviceName,
      channel: channelName,
    }
    // De-duplicate: if an identical mapping already exists, skip the write.
    const exists = link.iomap.mappings.some(
      (m) =>
        m.application === newMapping.application &&
        m.variable === newMapping.variable &&
        m.direction === newMapping.direction &&
        m.device === newMapping.device &&
        m.channel === newMapping.channel,
    )
    if (!exists) {
      await link.saveIomap({
        mappings: [...link.iomap.mappings, newMapping],
      })
    }
    setDraft((d) => ({ ...d, variable: "" }))
    setAdding(false)
  }

  // Channels with empty names can't be bound (iomap targets by name string).
  if (!channelName) {
    return (
      <span className="text-xs italic text-muted-foreground">
        (name this channel first)
      </span>
    )
  }

  return (
    <div className="flex flex-wrap items-center gap-1">
      {bindings.map((m) => (
        <span
          key={mappingKey(m)}
          className="inline-flex min-h-7 max-w-full items-center gap-1 rounded-md bg-muted/40 px-2 py-1 text-xs font-mono"
          title={`${m.application}.${m.variable} (${m.direction === "input" ? "bus → var" : "var → bus"})`}
        >
          <span
            className={
              m.direction === "input"
                ? "text-muted-foreground"
                : "text-highlight"
            }
          >
            {m.direction === "input" ? "←" : "→"}
          </span>
          <span className="truncate max-w-[14ch]">
            {m.application}.{m.variable}
          </span>
          <button
            type="button"
            onClick={() => void remove(m)}
            className="inline-flex size-6 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-accent/40 hover:text-destructive"
            title="Unlink"
          >
            <X className="size-3" />
          </button>
        </span>
      ))}
      {adding ? (
        <div className="flex flex-wrap items-center gap-2 bg-muted/40 p-2">
          <EnumSelect
            value={draft.application}
            onValueChange={(v) => setDraft({ ...draft, application: v })}
            options={link.apps.map((a) => ({ value: a, label: a }))}
            placeholder="app"
            className="h-7 w-32 text-xs"
          />
          <Input
            list={varListId}
            value={draft.variable}
            onChange={(e) => setDraft({ ...draft, variable: e.target.value })}
            onKeyDown={(e) => {
              if (e.key === "Enter") void commit()
              if (e.key === "Escape") setAdding(false)
            }}
            placeholder="variable"
            className="h-7 w-32 text-xs"
            autoFocus
          />
          <datalist id={varListId}>
            {appVars.map((v) => (
              <option key={v.name} value={v.name}>
                {v.type_name} · {v.direction}
              </option>
            ))}
          </datalist>
          <EnumSelect<Direction>
            value={draft.direction}
            onValueChange={(v) => setDraft({ ...draft, direction: v })}
            options={[
              { value: "output", label: "→ output" },
              { value: "input", label: "← input" },
            ]}
            className="h-7 w-24 text-xs"
          />
          <Button
            size="sm"
            variant="outline"
            onClick={() => void commit()}
            disabled={!draft.variable.trim() || !draft.application}
            className="h-7 px-2 text-xs"
          >
            Link
          </Button>
          <button
            type="button"
            onClick={() => setAdding(false)}
            className="inline-flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-accent/40 hover:text-foreground"
            title="Cancel"
          >
            <X className="size-3" />
          </button>
        </div>
      ) : (
        <button
          type="button"
          onClick={() => {
            setAdding(true)
            // If the previously-chosen app no longer exists, fall back.
            if (!link.apps.includes(draft.application)) {
              setDraft((d) => ({ ...d, application: link.apps[0] ?? "" }))
            }
          }}
          className="inline-flex min-h-8 items-center gap-1 rounded-md px-2 py-1 text-xs text-muted-foreground hover:bg-accent/40 hover:text-foreground"
          title="Link this channel to a POU variable"
        >
          <Link2 className="size-3" />
          Link variable
        </button>
      )}
    </div>
  )
}
