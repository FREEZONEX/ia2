import { useState } from "react"
import { ChevronRight, FolderTree, Plus, Trash2, X } from "@/components/ui/icons"

import { Button } from "@/components/ui/button"
import { EnumSelect } from "@/components/ui/enum-select"
import { Field } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { NumberCell } from "@/components/ui/number-cell"
import { opcuaBrowse } from "@/lib/api"
import type { Device } from "@/types/generated/Device"
import type { OpcuaAccess } from "@/types/generated/OpcuaAccess"
import type { OpcuaBrowseNode } from "@/types/generated/OpcuaBrowseNode"
import type { OpcuaChannel } from "@/types/generated/OpcuaChannel"
import type { OpcuaDataType } from "@/types/generated/OpcuaDataType"

import {
  DeviceSaveBar,
  EmptyBox,
  LinkedToCell,
  SectionHeader,
  useDeviceDraft,
  type DeviceEditorProps,
} from "./deviceEditorShared"

const OPCUA_DATA_TYPES: { value: OpcuaDataType; label: string }[] = [
  { value: "bool", label: "Boolean" },
  { value: "i16", label: "Int16" },
  { value: "u16", label: "UInt16" },
  { value: "i32", label: "Int32" },
  { value: "u32", label: "UInt32" },
  { value: "f32", label: "Float (f32)" },
  { value: "f64", label: "Double (f64)" },
]

export function OpcuaDeviceEditor({ device, onSave, link }: DeviceEditorProps) {
  const { draft, setDraft, dirty } = useDeviceDraft(device)
  if (draft.protocol !== "opcua") return null

  const update = (patch: Partial<typeof draft>) =>
    setDraft({ ...draft, ...patch } as Device)

  const setChannel = (idx: number, patch: Partial<OpcuaChannel>) => {
    const next = [...draft.channels]
    next[idx] = { ...next[idx], ...patch }
    update({ channels: next })
  }
  const addChannel = () => {
    update({
      channels: [
        ...draft.channels,
        {
          name: `tag_${draft.channels.length}`,
          node_id: "ns=2;s=",
          data_type: "f32",
          access: "read",
          failsafe: null,
        },
      ],
    })
  }
  const removeChannel = (idx: number) => {
    update({ channels: draft.channels.filter((_, i) => i !== idx) })
  }
  /** Browse picker → new tag. Name derives from the display name,
   *  cleaned to the channel-name convention; type from the hint. */
  const addFromBrowse = (node: OpcuaBrowseNode) => {
    const base =
      node.display_name
        .toLowerCase()
        .replace(/[^a-z0-9_]+/g, "_")
        .replace(/^_+|_+$/g, "") || "tag"
    let name = base
    let i = 1
    while (draft.channels.some((c) => c.name === name)) {
      name = `${base}_${i}`
      i += 1
    }
    update({
      channels: [
        ...draft.channels,
        {
          name,
          node_id: node.node_id,
          data_type: node.suggested_type ?? "f32",
          access: "read",
          failsafe: null,
        },
      ],
    })
  }

  const auth = draft.auth ?? { kind: "anonymous" }

  return (
    <>
      <DeviceSaveBar
        name={device.name}
        protocol="OPC UA"
        dirty={dirty}
        onSave={() => onSave(draft)}
      />

      <div className="min-h-0 flex-1 space-y-6 overflow-auto p-4">
        <section>
          <SectionHeader title="Server" />
          <div className="ia2-field-grid">
            <Field label="Endpoint URL">
              <Input
                value={draft.endpoint_url}
                onChange={(e) => update({ endpoint_url: e.target.value })}
                placeholder="opc.tcp://10.0.0.10:4840"
                className="font-mono"
              />
            </Field>
            <Field label="Poll interval (ms)">
              <NumberCell
                min={50}
                step={50}
                value={draft.poll_interval_ms}
                onChange={(n) => update({ poll_interval_ms: n })}
              />
            </Field>
            <Field label="Authentication">
              <EnumSelect<"anonymous" | "user_password">
                value={auth.kind}
                onValueChange={(v) =>
                  update({
                    auth:
                      v === "anonymous"
                        ? { kind: "anonymous" }
                        : { kind: "user_password", username: "", password: "" },
                  })
                }
                options={[
                  { value: "anonymous", label: "Anonymous" },
                  { value: "user_password", label: "User / password" },
                ]}
                className="h-9 w-full"
              />
            </Field>
            {auth.kind === "user_password" && (
              <>
                <Field label="Username">
                  <Input
                    value={auth.username}
                    onChange={(e) =>
                      update({ auth: { ...auth, username: e.target.value } })
                    }
                  />
                </Field>
                <Field label="Password">
                  <Input
                    type="password"
                    value={auth.password}
                    onChange={(e) =>
                      update({ auth: { ...auth, password: e.target.value } })
                    }
                  />
                </Field>
              </>
            )}
          </div>
        </section>

        <BrowsePanel deviceName={device.name} onAdd={addFromBrowse} />

        <section>
          <SectionHeader
            title={`Tags (${draft.channels.length})`}
            action={
              <Button size="sm" variant="ghost" onClick={addChannel}>
                <Plus className="mr-1 size-3" />
                Add tag
              </Button>
            }
          />
          {draft.channels.length === 0 ? (
            <EmptyBox>
              No tags. Each tag maps one server NodeId (e.g.{" "}
              <span className="font-mono">ns=2;s=FT0202.PV</span>) to an iomap
              channel.
            </EmptyBox>
          ) : (
            <div className="min-w-0 overflow-x-auto">
              <table className="ia2-table w-full min-w-[800px]">
                <thead className="text-xs text-muted-foreground">
                  <tr className="border-b border-border">
                    <th className="px-2 py-1.5 text-left">Name</th>
                    <th className="px-2 py-1.5 text-left">NodeId</th>
                    <th className="px-2 py-1.5 text-left">Type</th>
                    <th className="px-2 py-1.5 text-left">Access</th>
                    <th
                      className="px-2 py-1.5 text-left"
                      title="Optional value written on runtime shutdown/trip. Empty = leave the DCS tag untouched (recommended for a supervisory layer)."
                    >
                      Failsafe
                    </th>
                    <th className="px-2 py-1.5 text-left">Linked to</th>
                    <th className="px-2 py-1.5"></th>
                  </tr>
                </thead>
                <tbody>
                  {draft.channels.map((ch, i) => (
                    <tr
                      key={i}
                      className="border-b border-border align-top last:border-0"
                    >
                      <td className="px-2 py-1.5">
                        <Input
                          value={ch.name}
                          onChange={(e) => setChannel(i, { name: e.target.value })}
                          className="h-8 w-36"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <Input
                          value={ch.node_id}
                          onChange={(e) =>
                            setChannel(i, { node_id: e.target.value })
                          }
                          className="h-8 w-56 font-mono"
                          placeholder="ns=2;s=FT0202.PV"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <EnumSelect<OpcuaDataType>
                          value={ch.data_type}
                          onValueChange={(v) => setChannel(i, { data_type: v })}
                          options={OPCUA_DATA_TYPES}
                          className="h-8 w-32"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <EnumSelect<OpcuaAccess>
                          value={ch.access ?? "read"}
                          onValueChange={(v) => setChannel(i, { access: v })}
                          options={[
                            { value: "read", label: "read" },
                            { value: "write", label: "write" },
                          ]}
                          className="h-8 w-28"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <Input
                          value={ch.failsafe ?? ""}
                          onChange={(e) => {
                            const t = e.target.value.trim()
                            setChannel(i, {
                              failsafe: t === "" ? null : Number(t),
                            })
                          }}
                          className="h-8 w-20"
                          placeholder="—"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <LinkedToCell channelName={ch.name} link={link} />
                      </td>
                      <td className="px-2 py-1.5 text-right">
                        <button
                          type="button"
                          onClick={() => removeChannel(i)}
                          className="inline-flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-accent/40 hover:text-destructive"
                          title="Remove"
                        >
                          <Trash2 className="size-3.5" />
                        </button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </section>
      </div>
    </>
  )
}

/** Live address-space browser. Collapsed by default (browsing dials the
 *  real endpoint); opening it lists ObjectsFolder, Objects drill down,
 *  Variables land in the tag table via `onAdd`. */
function BrowsePanel({
  deviceName,
  onAdd,
}: {
  deviceName: string
  onAdd: (node: OpcuaBrowseNode) => void
}) {
  const [open, setOpen] = useState(false)
  const [trail, setTrail] = useState<{ label: string; nodeId?: string }[]>([])
  const [nodes, setNodes] = useState<OpcuaBrowseNode[] | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const browse = async (nodeId?: string, label = "Objects") => {
    setBusy(true)
    setError(null)
    try {
      setNodes(await opcuaBrowse(deviceName, nodeId))
      setTrail((t) =>
        nodeId === undefined
          ? [{ label }]
          : [...t, { label, nodeId }],
      )
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }
  const jumpTo = async (idx: number) => {
    const target = trail[idx]
    setBusy(true)
    setError(null)
    try {
      setNodes(await opcuaBrowse(deviceName, target.nodeId))
      setTrail(trail.slice(0, idx + 1))
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  if (!open) {
    return (
      <section>
        <Button
          size="sm"
          variant="outline"
          onClick={() => {
            setOpen(true)
            void browse()
          }}
        >
          <FolderTree className="mr-1.5 size-3.5" />
          Browse server…
        </Button>
      </section>
    )
  }

  return (
    <section className="rounded-md border border-border bg-secondary/30 p-3">
      <div className="flex items-center justify-between">
        <div className="flex min-w-0 items-center gap-1 text-xs">
          <FolderTree className="size-3.5 shrink-0 text-muted-foreground" />
          {trail.map((t, i) => (
            <span key={i} className="flex min-w-0 items-center gap-1">
              {i > 0 && (
                <ChevronRight className="size-3 shrink-0 text-muted-foreground/60" />
              )}
              <button
                type="button"
                onClick={() => void jumpTo(i)}
                className="truncate font-mono text-muted-foreground hover:text-foreground"
              >
                {t.label}
              </button>
            </span>
          ))}
          {busy && <span className="ml-1 text-muted-foreground">…</span>}
        </div>
        <button
          type="button"
          onClick={() => setOpen(false)}
          className="inline-flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-accent/40 hover:text-foreground"
          title="Close browser"
        >
          <X className="size-3.5" />
        </button>
      </div>
      {error && (
        <div className="mt-2 rounded border border-destructive/40 bg-destructive/10 px-2 py-1 text-xs text-destructive">
          {error}
        </div>
      )}
      {nodes && nodes.length === 0 && !error && (
        <div className="mt-2 text-xs text-muted-foreground">
          No child nodes here.
        </div>
      )}
      {nodes && nodes.length > 0 && (
        <div className="mt-2 max-h-56 overflow-auto">
          {nodes.map((n) => (
            <div
              key={n.node_id}
              className="flex items-center gap-2 rounded px-1.5 py-1 text-[12px] hover:bg-accent/30"
            >
              {n.node_class === "Object" ? (
                <button
                  type="button"
                  onClick={() => void browse(n.node_id, n.display_name)}
                  className="flex min-w-0 flex-1 items-center gap-1.5 text-left"
                >
                  <ChevronRight className="size-3 shrink-0 text-muted-foreground" />
                  <span className="truncate text-foreground">
                    {n.display_name}
                  </span>
                </button>
              ) : (
                <span className="flex min-w-0 flex-1 items-center gap-1.5 pl-[18px]">
                  <span className="truncate text-foreground">
                    {n.display_name}
                  </span>
                  <span className="truncate font-mono text-xs text-muted-foreground">
                    {n.node_id}
                    {n.data_type && ` · ${n.data_type}`}
                  </span>
                </span>
              )}
              {n.node_class === "Variable" && (
                <button
                  type="button"
                  onClick={() => onAdd(n)}
                  className="shrink-0 rounded border border-border bg-card px-1.5 py-0.5 text-xs text-muted-foreground hover:text-foreground"
                  title="Add as tag"
                >
                  + Add
                </button>
              )}
            </div>
          ))}
        </div>
      )}
    </section>
  )
}
