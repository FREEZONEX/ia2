import { Info, Plus, Trash2 } from "@/components/ui/icons"

import { Button } from "@/components/ui/button"
import { EnumSelect } from "@/components/ui/enum-select"
import { Field } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { NumberCell } from "@/components/ui/number-cell"
import type { Device } from "@/types/generated/Device"
import type { EthercatChannel } from "@/types/generated/EthercatChannel"
import type { EthercatDataType } from "@/types/generated/EthercatDataType"
import type { EthercatDcSync } from "@/types/generated/EthercatDcSync"
import type { EthercatSlave } from "@/types/generated/EthercatSlave"

import {
  DeviceSaveBar,
  EmptyBox,
  LinkedToCell,
  SectionHeader,
  useDeviceDraft,
  type DeviceEditorProps,
} from "./deviceEditorShared"

const PDO_DATA_TYPES: { value: EthercatDataType; label: string; bits: number }[] =
  [
    { value: "bool", label: "BOOL", bits: 1 },
    { value: "u8", label: "USINT (u8)", bits: 8 },
    { value: "i8", label: "SINT (i8)", bits: 8 },
    { value: "u16", label: "UINT (u16)", bits: 16 },
    { value: "i16", label: "INT (i16)", bits: 16 },
    { value: "u32", label: "UDINT (u32)", bits: 32 },
    { value: "i32", label: "DINT (i32)", bits: 32 },
    { value: "real", label: "REAL (f32)", bits: 32 },
  ]

function defaultBitsFor(t: EthercatDataType): number {
  return PDO_DATA_TYPES.find((d) => d.value === t)?.bits ?? 1
}

/** Render a u16/u32 as 0x-prefixed hex; used for object dictionary indices. */
function toHex(n: number, width: number): string {
  if (!Number.isFinite(n) || n < 0) return "0x0"
  return `0x${n.toString(16).toUpperCase().padStart(width, "0")}`
}

/** Parse user-entered hex (with or without 0x) or decimal. Returns 0 on junk. */
function parseHexOrDec(s: string): number {
  const t = s.trim()
  if (!t) return 0
  const cleaned = t.toLowerCase().startsWith("0x")
    ? t
    : t.match(/^[0-9a-f]+$/i) && /[a-f]/i.test(t)
      ? `0x${t}`
      : t
  const n = Number(cleaned)
  return Number.isFinite(n) ? Math.max(0, Math.trunc(n)) : 0
}

export function EthercatDeviceEditor({
  device,
  onSave,
  link,
}: DeviceEditorProps) {
  const { draft, setDraft, dirty, reload, conflict } = useDeviceDraft(device)
  if (draft.protocol !== "ethercat") return null

  const update = (patch: Partial<typeof draft>) =>
    setDraft({ ...draft, ...patch } as Device)

  // ----- Slaves -----
  const setSlave = (idx: number, patch: Partial<EthercatSlave>) => {
    const next = [...draft.slaves]
    next[idx] = { ...next[idx], ...patch }
    update({ slaves: next })
  }
  const addSlave = () => {
    const used = new Set(draft.slaves.map((s) => s.index))
    let nextIndex = 0
    while (used.has(nextIndex)) nextIndex++
    update({
      slaves: [
        ...draft.slaves,
        {
          index: nextIndex,
          name: `slave_${nextIndex}`,
          vendor_id: 0,
          product_id: 0,
          // Per-slave DC override (null = inherit device dc_sync) and
          // startup SDO writes — default to "none" so a plain coupler
          // needs no extra config; servos set these via the JSON editor.
          dc_sync: null,
          init_sdo: [],
        },
      ],
    })
  }
  const removeSlave = (idx: number) => {
    update({ slaves: draft.slaves.filter((_, i) => i !== idx) })
  }

  // ----- Channels -----
  const setChannel = (idx: number, patch: Partial<EthercatChannel>) => {
    const next = [...draft.channels]
    next[idx] = { ...next[idx], ...patch }
    update({ channels: next })
  }
  const addChannel = () => {
    const firstSlave = draft.slaves[0]?.index ?? 0
    update({
      channels: [
        ...draft.channels,
        {
          name: `pdo_${draft.channels.length}`,
          slave_index: firstSlave,
          direction: "tx_pdo",
          pdo_index: 0x6000,
          sub_index: 1,
          bit_length: 16,
          data_type: "u16",
          // Default to (0, 0): correct for the first PDO entry on a
          // single-channel slave. Multi-channel slaves need real
          // numbers from the slave's ESI / vendor docs.
          pdi_byte_offset: 0,
          pdi_bit_offset: 0,
        },
      ],
    })
  }
  const removeChannel = (idx: number) => {
    update({ channels: draft.channels.filter((_, i) => i !== idx) })
  }

  return (
    <>
      <DeviceSaveBar
        name={device.name}
        protocol="EtherCAT"
        dirty={dirty}
        onSave={() => onSave(draft)}
        reload={reload}
        conflict={conflict}
      />

      <div className="min-h-0 flex-1 space-y-6 overflow-auto p-4">
        <section>
          <SectionHeader title="MainDevice" />
          <div className="ia2-field-grid">
            <Field label="Network interface">
              <Input
                value={draft.nic}
                onChange={(e) => update({ nic: e.target.value })}
                placeholder="en0 / eth0"
              />
            </Field>
            <Field label="Cycle time (µs)">
              <NumberCell
                min={50}
                step={50}
                value={draft.cycle_us}
                onChange={(n) => update({ cycle_us: n })}
              />
            </Field>
            <Field label="Distributed clock (DC)">
              <EnumSelect<EthercatDcSync>
                value={draft.dc_sync ?? "off"}
                onValueChange={(v) => update({ dc_sync: v })}
                options={[
                  { value: "off", label: "Off (free-run)" },
                  { value: "sync0", label: "SYNC0" },
                ]}
                className="h-9 w-full"
              />
            </Field>
            <div className="col-span-2 flex items-start gap-2 text-xs text-muted-foreground">
              <Info className="mt-0.5 size-3 shrink-0" />
              <span>
                <span className="font-mono">SYNC0</span> enables the
                distributed-clock pulse (period = cycle time) — servo drives
                (e.g. Inovance SV660N) need it to reach{" "}
                <span className="font-mono">OP</span>. Leave{" "}
                <span className="font-mono">Off</span> for simple I/O couplers;
                enabling DC on a slave that doesn't support it fails to
                configure.
              </span>
            </div>
            <Field label="Bring-up">
              <EnumSelect<"auto" | "esi_modular">
                value={draft.bringup?.mode ?? "auto"}
                onValueChange={(v) =>
                  update({
                    bringup:
                      v === "esi_modular"
                        ? {
                            mode: "esi_modular",
                            esi_path:
                              draft.bringup?.mode === "esi_modular"
                                ? draft.bringup.esi_path
                                : "",
                          }
                        : { mode: "auto" },
                  })
                }
                options={[
                  { value: "auto", label: "Auto (CoE PDO discovery)" },
                  { value: "esi_modular", label: "ESI modular" },
                ]}
                className="h-9 w-full"
              />
            </Field>
            {draft.bringup?.mode === "esi_modular" && (
              <Field label="ESI file (project-relative)">
                <Input
                  value={draft.bringup.esi_path}
                  placeholder="esi/coupler.xml"
                  onChange={(e) =>
                    update({
                      bringup: { mode: "esi_modular", esi_path: e.target.value },
                    })
                  }
                />
              </Field>
            )}
            {draft.bringup?.mode === "esi_modular" && (
              <div className="col-span-2 flex items-start gap-2 text-xs text-muted-foreground">
                <Info className="mt-0.5 size-3 shrink-0" />
                <span>
                  <span className="font-mono">ESI modular</span> builds the
                  process image from the device's ESI file + the modules it
                  reports at <span className="font-mono">0xF050</span> — for
                  modular couplers whose module PDOs never appear over runtime
                  CoE. Channels are assembled from the ESI rather than
                  hand-entered.
                </span>
              </div>
            )}
          </div>
        </section>

        <section>
          <SectionHeader
            title={`SubDevices (${draft.slaves.length})`}
            action={
              <Button size="sm" variant="ghost" onClick={addSlave}>
                <Plus className="mr-1 size-3" />
                Add slave
              </Button>
            }
          />
          {draft.slaves.length === 0 ? (
            <EmptyBox>
              No slaves declared. Click{" "}
              <span className="font-mono">+ Add slave</span> to describe one
              SubDevice on the ring.
            </EmptyBox>
          ) : (
            <div className="min-w-0 overflow-x-auto">
              <table className="ia2-table w-full min-w-[800px]">
                <thead className="text-xs text-muted-foreground">
                  <tr className="border-b border-border">
                    <th className="px-2 py-1.5 text-left">Index</th>
                    <th className="px-2 py-1.5 text-left">Name</th>
                    <th className="px-2 py-1.5 text-left">Vendor ID</th>
                    <th className="px-2 py-1.5 text-left">Product code</th>
                    <th className="px-2 py-1.5"></th>
                  </tr>
                </thead>
                <tbody>
                  {draft.slaves.map((s, i) => (
                    <tr key={i} className="border-b border-border last:border-0">
                      <td className="px-2 py-1.5">
                        <NumberCell
                          min={0}
                          value={s.index}
                          onChange={(n) => setSlave(i, { index: n })}
                          className="h-8 w-20"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <Input
                          value={s.name}
                          onChange={(e) => setSlave(i, { name: e.target.value })}
                          className="h-8 w-40"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <Input
                          value={toHex(s.vendor_id, 8)}
                          onChange={(e) =>
                            setSlave(i, { vendor_id: parseHexOrDec(e.target.value) })
                          }
                          className="h-8 w-32 font-mono"
                          placeholder="0x00000000"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <Input
                          value={toHex(s.product_id, 8)}
                          onChange={(e) =>
                            setSlave(i, {
                              product_id: parseHexOrDec(e.target.value),
                            })
                          }
                          className="h-8 w-32 font-mono"
                          placeholder="0x00000000"
                        />
                      </td>
                      <td className="px-2 py-1.5 text-right">
                        <button
                          type="button"
                          onClick={() => removeSlave(i)}
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

        <section>
          <SectionHeader
            title={`PDO Channels (${draft.channels.length})`}
            action={
              <Button
                size="sm"
                variant="ghost"
                onClick={addChannel}
                disabled={draft.slaves.length === 0}
                title={
                  draft.slaves.length === 0
                    ? "Add at least one slave first"
                    : "Add a PDO channel"
                }
              >
                <Plus className="mr-1 size-3" />
                Add channel
              </Button>
            }
          />
          {draft.channels.length === 0 ? (
            <EmptyBox>
              {draft.slaves.length === 0
                ? "Declare a slave first, then bind PDO entries here."
                : "No channels. Each channel maps a single PDO entry (object dictionary index + sub-index) on one slave."}
            </EmptyBox>
          ) : (
            <div className="min-w-0 overflow-x-auto">
              <table className="ia2-table w-full min-w-[800px]">
                <thead className="text-xs text-muted-foreground">
                  <tr className="border-b border-border">
                    <th className="px-2 py-1.5 text-left">Name</th>
                    <th className="px-2 py-1.5 text-left">Slave</th>
                    <th className="px-2 py-1.5 text-left">Direction</th>
                    <th className="px-2 py-1.5 text-left">PDO index</th>
                    <th className="px-2 py-1.5 text-left">Sub</th>
                    <th className="px-2 py-1.5 text-left">Type</th>
                    <th className="px-2 py-1.5 text-left">Bits</th>
                    <th
                      className="px-2 py-1.5 text-left"
                      title="Byte offset within the SubDevice's PDI region for this direction. Required when running against real hardware; ignored in sim mode."
                    >
                      Byte&nbsp;off
                    </th>
                    <th
                      className="px-2 py-1.5 text-left"
                      title="Bit offset within the byte at Byte off. Only meaningful for sub-byte (1-bit) channels like digital I/O. 0 = LSB."
                    >
                      Bit&nbsp;off
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
                          className="h-8 w-40"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <EnumSelect
                          value={String(ch.slave_index)}
                          onValueChange={(v) =>
                            setChannel(i, { slave_index: Number(v) })
                          }
                          options={draft.slaves.map((s) => ({
                            value: String(s.index),
                            label: `[${s.index}] ${s.name}`,
                          }))}
                          className="h-8 w-40"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <EnumSelect<EthercatChannel["direction"]>
                          value={ch.direction}
                          onValueChange={(v) => setChannel(i, { direction: v })}
                          options={[
                            { value: "tx_pdo", label: "TxPDO (in)" },
                            { value: "rx_pdo", label: "RxPDO (out)" },
                          ]}
                          className="h-8 w-36"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <Input
                          value={toHex(ch.pdo_index, 4)}
                          onChange={(e) =>
                            setChannel(i, {
                              pdo_index: parseHexOrDec(e.target.value),
                            })
                          }
                          className="h-8 w-24 font-mono"
                          placeholder="0x6000"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <NumberCell
                          min={0}
                          max={255}
                          value={ch.sub_index}
                          onChange={(n) => setChannel(i, { sub_index: n })}
                          className="h-8 w-16"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <EnumSelect<EthercatDataType>
                          value={ch.data_type}
                          onValueChange={(v) =>
                            setChannel(i, {
                              data_type: v,
                              // Snap bit length to the type's natural width.
                              bit_length: defaultBitsFor(v),
                            })
                          }
                          options={PDO_DATA_TYPES}
                          className="h-8 w-36"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <NumberCell
                          min={1}
                          max={64}
                          value={ch.bit_length}
                          onChange={(n) => setChannel(i, { bit_length: n })}
                          className="h-8 w-16"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <NumberCell
                          min={0}
                          max={65535}
                          value={ch.pdi_byte_offset}
                          onChange={(n) => setChannel(i, { pdi_byte_offset: n })}
                          className="h-8 w-16"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <NumberCell
                          min={0}
                          max={7}
                          value={ch.pdi_bit_offset}
                          onChange={(n) => setChannel(i, { pdi_bit_offset: n })}
                          className="h-8 w-14"
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
