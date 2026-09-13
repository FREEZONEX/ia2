import { Plus, Trash2 } from "@/components/ui/icons"

import { Button } from "@/components/ui/button"
import { EnumSelect } from "@/components/ui/enum-select"
import { Field } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { NumberCell } from "@/components/ui/number-cell"
import type { CanopenAccess } from "@/types/generated/CanopenAccess"
import type { CanopenChannel } from "@/types/generated/CanopenChannel"
import type { CanopenDataType } from "@/types/generated/CanopenDataType"
import type { Device } from "@/types/generated/Device"

import {
  DeviceSaveBar,
  EmptyBox,
  LinkedToCell,
  SectionHeader,
  useDeviceDraft,
  type DeviceEditorProps,
} from "./deviceEditorShared"

const CANOPEN_DATA_TYPES: { value: CanopenDataType; label: string }[] = [
  { value: "bool", label: "Boolean" },
  { value: "i8", label: "Int8" },
  { value: "u8", label: "UInt8" },
  { value: "i16", label: "Int16" },
  { value: "u16", label: "UInt16" },
  { value: "i32", label: "Int32" },
  { value: "u32", label: "UInt32" },
  { value: "f32", label: "Float (f32)" },
]

/** Transport shown as one compact select; slot/offset appear for PDO. */
type TransportKind = "sdo" | "tpdo" | "rpdo"

export function CanopenDeviceEditor({ device, onSave, link }: DeviceEditorProps) {
  const { draft, setDraft, dirty } = useDeviceDraft(device)
  if (draft.protocol !== "canopen") return null

  const update = (patch: Partial<typeof draft>) =>
    setDraft({ ...draft, ...patch } as Device)

  const setChannel = (idx: number, patch: Partial<CanopenChannel>) => {
    const next = [...draft.channels]
    next[idx] = { ...next[idx], ...patch }
    update({ channels: next })
  }
  const addChannel = () => {
    update({
      channels: [
        ...draft.channels,
        {
          name: `obj_${draft.channels.length}`,
          index: 0x2000,
          sub_index: 0,
          data_type: "i16",
          access: "read",
          transport: { kind: "sdo" },
          failsafe: null,
        },
      ],
    })
  }
  const removeChannel = (idx: number) => {
    update({ channels: draft.channels.filter((_, i) => i !== idx) })
  }

  const setTransportKind = (idx: number, kind: TransportKind) => {
    const cur = draft.channels[idx].transport
    if (kind === "sdo") {
      setChannel(idx, { transport: { kind: "sdo" } })
    } else {
      const slot = cur.kind !== "sdo" ? cur.slot : 1
      const byte_offset = cur.kind !== "sdo" ? cur.byte_offset : 0
      setChannel(idx, { transport: { kind, slot, byte_offset } })
    }
  }

  return (
    <>
      <DeviceSaveBar
        name={device.name}
        protocol="CANopen"
        dirty={dirty}
        onSave={() => onSave(draft)}
      />

      <div className="min-h-0 flex-1 space-y-6 overflow-auto p-4">
        <section>
          <SectionHeader title="Bus" />
          <div className="ia2-field-grid">
            <Field label="CAN interface">
              <Input
                value={draft.interface}
                onChange={(e) => update({ interface: e.target.value })}
                placeholder='can0 / gs_usb:VID:PID:serial:0 / _sim'
                className="font-mono"
              />
            </Field>
            <Field label="Node id (1–127)">
              <NumberCell
                min={1}
                max={127}
                value={draft.node_id}
                onChange={(n) => update({ node_id: n })}
              />
            </Field>
            <Field label="SDO poll interval (ms)">
              <NumberCell
                min={10}
                step={10}
                value={draft.poll_interval_ms}
                onChange={(n) => update({ poll_interval_ms: n })}
              />
            </Field>
            <Field label="Heartbeat timeout (ms, 0 = off)">
              <NumberCell
                min={0}
                step={100}
                value={draft.heartbeat_timeout_ms}
                onChange={(n) => update({ heartbeat_timeout_ms: n })}
              />
            </Field>
            <Field
              label={
                draft.interface.startsWith("gs_usb:")
                  ? "Bitrate (required, bit/s)"
                  : "Bitrate (informational)"
              }
            >
              <Input
                value={draft.bitrate ?? ""}
                onChange={(e) => {
                  const t = e.target.value.trim()
                  update({ bitrate: t === "" ? null : Number(t) })
                }}
                placeholder="500000"
                className="font-mono"
              />
            </Field>
            <Field label="NMT on connect">
              <EnumSelect<"start" | "leave">
                value={draft.start_on_connect ? "start" : "leave"}
                onValueChange={(v) => update({ start_on_connect: v === "start" })}
                options={[
                  { value: "start", label: "Start remote node" },
                  { value: "leave", label: "Leave state alone" },
                ]}
                className="h-9 w-full"
              />
            </Field>
          </div>
        </section>

        <section>
          <SectionHeader
            title={`Objects (${draft.channels.length})`}
            action={
              <Button size="sm" variant="ghost" onClick={addChannel}>
                <Plus className="mr-1 size-3" />
                Add object
              </Button>
            }
          />
          {draft.channels.length === 0 ? (
            <EmptyBox>
              No objects. Each row binds one object-dictionary entry (e.g.{" "}
              <span className="font-mono">0x6041:00</span> statusword) as an
              iomap channel — over SDO polling or a PDO slot.
            </EmptyBox>
          ) : (
            <div className="overflow-x-auto">
            <div className="min-w-0 overflow-x-auto">
              <table className="ia2-table w-full min-w-[800px]">
                <thead className="text-xs text-muted-foreground">
                  <tr className="border-b border-border">
                    <th className="px-2 py-1.5 text-left">Name</th>
                    <th className="px-2 py-1.5 text-left">Index</th>
                    <th className="px-2 py-1.5 text-left">Sub</th>
                    <th className="px-2 py-1.5 text-left">Type</th>
                    <th className="px-2 py-1.5 text-left">Access</th>
                    <th
                      className="px-2 py-1.5 text-left"
                      title="SDO = request/response at the poll interval. TPDO/RPDO = process data on the predefined COB-IDs; slot 1–4 and the byte offset inside the ≤8-byte frame."
                    >
                      Transport
                    </th>
                    <th
                      className="px-2 py-1.5 text-left"
                      title="Optional value written on runtime shutdown/trip. Empty = leave the object untouched."
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
                          className="h-8 w-32"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <HexCell
                          value={ch.index}
                          onChange={(n) => setChannel(i, { index: n })}
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <NumberCell
                          min={0}
                          max={255}
                          value={ch.sub_index}
                          onChange={(n) => setChannel(i, { sub_index: n })}
                          className="w-16"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <EnumSelect<CanopenDataType>
                          value={ch.data_type}
                          onValueChange={(v) => setChannel(i, { data_type: v })}
                          options={CANOPEN_DATA_TYPES}
                          className="h-8 w-28"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <EnumSelect<CanopenAccess>
                          value={ch.access ?? "read"}
                          onValueChange={(v) => setChannel(i, { access: v })}
                          options={[
                            { value: "read", label: "read" },
                            { value: "write", label: "write" },
                          ]}
                          className="h-8 w-24"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <div className="flex items-center gap-1.5">
                          <EnumSelect<TransportKind>
                            value={ch.transport.kind}
                            onValueChange={(v) => setTransportKind(i, v)}
                            options={[
                              { value: "sdo", label: "SDO" },
                              { value: "tpdo", label: "TPDO" },
                              { value: "rpdo", label: "RPDO" },
                            ]}
                            className="h-8 w-24"
                          />
                          {ch.transport.kind !== "sdo" && (
                            <>
                              <NumberCell
                                min={1}
                                max={4}
                                value={ch.transport.slot}
                                onChange={(n) =>
                                  setChannel(i, {
                                    transport: { ...ch.transport, slot: n } as CanopenChannel["transport"],
                                  })
                                }
                                className="w-14"
                                title="PDO slot 1–4"
                              />
                              <NumberCell
                                min={0}
                                max={7}
                                value={ch.transport.byte_offset}
                                onChange={(n) =>
                                  setChannel(i, {
                                    transport: {
                                      ...ch.transport,
                                      byte_offset: n,
                                    } as CanopenChannel["transport"],
                                  })
                                }
                                className="w-14"
                                title="Byte offset in the PDO frame"
                              />
                            </>
                          )}
                        </div>
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
            </div>
          )}
        </section>
      </div>
    </>
  )
}

/** Hex-rendered u16 field (object indexes read as hex everywhere in
 *  CANopen literature; storing decimal but showing 0x keeps both). */
function HexCell({ value, onChange }: { value: number; onChange: (n: number) => void }) {
  return (
    <Input
      value={`0x${value.toString(16).toUpperCase().padStart(4, "0")}`}
      onChange={(e) => {
        const t = e.target.value.trim().replace(/^0x/i, "")
        const n = parseInt(t, 16)
        if (!Number.isNaN(n) && n >= 0 && n <= 0xffff) onChange(n)
      }}
      className="h-8 w-24 font-mono"
    />
  )
}
