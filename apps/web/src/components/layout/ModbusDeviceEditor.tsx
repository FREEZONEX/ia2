import { Info, Plus, Trash2 } from "@/components/ui/icons"
import { useEffect, useState } from "react"

import { Button } from "@/components/ui/button"
import { EnumSelect } from "@/components/ui/enum-select"
import { Field } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { NumberCell } from "@/components/ui/number-cell"
import type { Device } from "@/types/generated/Device"
import type { ModbusChannel } from "@/types/generated/ModbusChannel"
import type { ModbusChannelKind } from "@/types/generated/ModbusChannelKind"
import type { ModbusDataBits } from "@/types/generated/ModbusDataBits"
import type { ModbusDataType } from "@/types/generated/ModbusDataType"
import type { ModbusParity } from "@/types/generated/ModbusParity"
import type { ModbusStopBits } from "@/types/generated/ModbusStopBits"
import type { ModbusTransport } from "@/types/generated/ModbusTransport"
import type { ModbusWordOrder } from "@/types/generated/ModbusWordOrder"
import type { ModbusAccess } from "@/types/generated/ModbusAccess"

import {
  DeviceSaveBar,
  EmptyBox,
  LinkedToCell,
  SectionHeader,
  useDeviceDraft,
  type DeviceEditorProps,
} from "./deviceEditorShared"

export function ModbusDeviceEditor({ device, onSave, link }: DeviceEditorProps) {
  const { draft, setDraft, dirty } = useDeviceDraft(device)
  if (draft.protocol !== "modbus") return null

  const update = (patch: Partial<typeof draft>) =>
    setDraft({ ...draft, ...patch } as Device)

  const setChannel = (idx: number, patch: Partial<ModbusChannel>) => {
    const next = [...draft.channels]
    next[idx] = { ...next[idx], ...patch }
    update({ channels: next })
  }

  const addChannel = () => {
    update({
      channels: [
        ...draft.channels,
        {
          name: `ch_${draft.channels.length}`,
          kind: "coil",
          address: 0,
          data_type: "u16",
          word_order: "hi_lo",
          access: "write",
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
        protocol="Modbus"
        dirty={dirty}
        onSave={() => onSave(draft)}
      />

      <div className="min-h-0 flex-1 space-y-6 overflow-auto p-4">
        <section>
          <SectionHeader title="Connection" />
          <ModbusTransportEditor
            transport={draft.transport}
            onTransport={(t) => update({ transport: t })}
          />
          <div className="mt-3 ia2-field-grid">
            <Field label="Slave ID">
              <NumberCell
                value={draft.slave_id}
                onChange={(n) => update({ slave_id: n })}
              />
            </Field>
            <Field label="Poll interval (ms)">
              <NumberCell
                value={draft.poll_interval_ms}
                onChange={(n) => update({ poll_interval_ms: n })}
              />
            </Field>
          </div>
        </section>

        <section>
          <SectionHeader
            title="Channels"
            action={
              <Button size="sm" variant="ghost" onClick={addChannel}>
                <Plus className="mr-1 size-3" />
                Add channel
              </Button>
            }
          />
          {draft.channels.length === 0 ? (
            <EmptyBox>
              No channels. Click <span className="font-mono">+ Add channel</span>{" "}
              to define one.
            </EmptyBox>
          ) : (
            <div className="min-w-0 overflow-x-auto">
              <table className="ia2-table w-full min-w-[800px]">
                <thead className="text-xs text-muted-foreground">
                  <tr className="border-b border-border">
                    <th className="px-2 py-1.5 text-left">Name</th>
                    <th className="px-2 py-1.5 text-left">Kind</th>
                    <th className="px-2 py-1.5 text-left">Address</th>
                    <th
                      className="px-2 py-1.5 text-left"
                      title="Register interpretation. 32-bit types (u32/i32/f32) span TWO consecutive registers — the norm for instrument floats and totalizers. Ignored for coils/discretes."
                    >
                      Type
                    </th>
                    <th
                      className="px-2 py-1.5 text-left"
                      title="Word order for 32-bit types: hi_lo = ABCD (Modbus default), lo_hi = CDAB (common on Chinese instruments)."
                    >
                      Words
                    </th>
                    <th className="px-2 py-1.5 text-left">Access</th>
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
                        <EnumSelect<ModbusChannelKind>
                          value={ch.kind}
                          onValueChange={(v) => setChannel(i, { kind: v })}
                          options={[
                            { value: "coil", label: "Coil" },
                            { value: "discrete_input", label: "Discrete Input" },
                            { value: "holding_register", label: "Holding Register" },
                            { value: "input_register", label: "Input Register" },
                          ]}
                          className="h-8 w-44"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <NumberCell
                          value={ch.address}
                          onChange={(n) => setChannel(i, { address: n })}
                          className="h-8 w-24"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <EnumSelect<ModbusDataType>
                          value={ch.data_type ?? "u16"}
                          onValueChange={(v) => setChannel(i, { data_type: v })}
                          disabled={
                            ch.kind === "coil" || ch.kind === "discrete_input"
                          }
                          options={[
                            { value: "u16", label: "u16" },
                            { value: "i16", label: "i16" },
                            { value: "u32", label: "u32 (2reg)" },
                            { value: "i32", label: "i32 (2reg)" },
                            { value: "f32", label: "f32 (2reg)" },
                          ]}
                          className="h-8 w-24"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <EnumSelect<ModbusWordOrder>
                          value={ch.word_order ?? "hi_lo"}
                          onValueChange={(v) => setChannel(i, { word_order: v })}
                          disabled={
                            ch.kind === "coil" ||
                            ch.kind === "discrete_input" ||
                            ch.data_type === "u16" ||
                            ch.data_type === "i16" ||
                            ch.data_type == null
                          }
                          options={[
                            { value: "hi_lo", label: "hi-lo (ABCD)" },
                            { value: "lo_hi", label: "lo-hi (CDAB)" },
                          ]}
                          className="h-8 w-24"
                        />
                      </td>
                      <td className="px-2 py-1.5">
                        <EnumSelect<ModbusAccess>
                          value={
                            ch.kind === "discrete_input" || ch.kind === "input_register"
                              ? "read"
                              : ch.access ?? "write"
                          }
                          onValueChange={(v) => setChannel(i, { access: v })}
                          disabled={
                            ch.kind === "discrete_input" ||
                            ch.kind === "input_register"
                          }
                          options={[
                            { value: "write", label: "write" },
                            { value: "read", label: "read-only" },
                          ]}
                          className="h-8 w-24"
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

// ============================================================
//  Modbus transport editor (TCP vs RTU picker)
// ============================================================

/**
 * Two-mode form: TCP shows host/port, RTU shows serial device +
 * baud + parity + stop bits + data bits. Switching transports
 * preserves whatever you'd already typed in the other one (the
 * collapsed branch is kept in a closure so flipping back and forth
 * doesn't wipe the user's input).
 *
 * Common Modbus RTU defaults: 9600-8-N-1, slave_id ranges 1-247.
 */
function ModbusTransportEditor({
  transport,
  onTransport,
}: {
  transport: ModbusTransport
  onTransport: (t: ModbusTransport) => void
}) {
  // Hold the "other" branch's last-edited shape so toggling kind
  // doesn't lose work in progress.
  const [tcpDraft, setTcpDraft] = useState(() =>
    transport.kind === "tcp"
      ? { host: transport.host, port: transport.port }
      : { host: "127.0.0.1", port: 502 },
  )
  const [rtuDraft, setRtuDraft] = useState(() =>
    transport.kind === "rtu"
      ? {
          serial_device: transport.serial_device,
          baud_rate: transport.baud_rate,
          data_bits: transport.data_bits,
          stop_bits: transport.stop_bits,
          parity: transport.parity,
          rs485: transport.rs485,
        }
      : {
          serial_device: defaultSerialPath(),
          baud_rate: 9600,
          data_bits: "eight" as ModbusDataBits,
          stop_bits: "one" as ModbusStopBits,
          parity: "none" as ModbusParity,
          rs485: null,
        },
  )

  // Mirror the upstream transport into the relevant draft so save +
  // reset still works correctly.
  useEffect(() => {
    if (transport.kind === "tcp") {
      setTcpDraft({ host: transport.host, port: transport.port })
    } else {
      setRtuDraft({
        serial_device: transport.serial_device,
        baud_rate: transport.baud_rate,
        data_bits: transport.data_bits,
        stop_bits: transport.stop_bits,
        parity: transport.parity,
        rs485: transport.rs485,
      })
    }
  }, [transport])

  return (
    <>
      <div className="mb-3 flex items-center gap-2">
        <span className="text-xs text-muted-foreground">
          Transport
        </span>
        <div className="inline-flex overflow-hidden rounded-md border border-border text-xs">
          <button
            type="button"
            aria-pressed={transport.kind === "tcp"}
            onClick={() => onTransport({ kind: "tcp", ...tcpDraft })}
            className={
              "px-3 py-1 transition-colors " +
              (transport.kind === "tcp"
                ? "bg-accent text-foreground"
                : "text-muted-foreground hover:bg-accent/40")
            }
          >
            TCP
          </button>
          <button
            type="button"
            aria-pressed={transport.kind === "rtu"}
            onClick={() => onTransport({ kind: "rtu", ...rtuDraft })}
            className={
              "border-l border-border px-3 py-1 transition-colors " +
              (transport.kind === "rtu"
                ? "bg-accent text-foreground"
                : "text-muted-foreground hover:bg-accent/40")
            }
          >
            RTU (serial)
          </button>
        </div>
      </div>

      {transport.kind === "tcp" ? (
        <div className="ia2-field-grid">
          <Field label="Host">
            <Input
              value={transport.host}
              onChange={(e) =>
                onTransport({
                  kind: "tcp",
                  host: e.target.value,
                  port: transport.port,
                })
              }
            />
          </Field>
          <Field label="Port">
            <NumberCell
              value={transport.port}
              onChange={(n) =>
                onTransport({ kind: "tcp", host: transport.host, port: n })
              }
            />
          </Field>
        </div>
      ) : (
        <div className="ia2-field-grid">
          <Field label="Serial device">
            <Input
              value={transport.serial_device}
              placeholder={defaultSerialPath()}
              onChange={(e) =>
                onTransport({ ...transport, serial_device: e.target.value })
              }
            />
          </Field>
          <Field label="Baud rate">
            <EnumSelect
              value={String(transport.baud_rate)}
              onValueChange={(v) =>
                onTransport({ ...transport, baud_rate: Number(v) || 9600 })
              }
              options={BAUD_RATES.map((b) => ({
                value: String(b),
                label: String(b),
              }))}
              className="h-9 w-full"
            />
          </Field>
          <Field label="Parity">
            <EnumSelect<ModbusParity>
              value={transport.parity}
              onValueChange={(v) => onTransport({ ...transport, parity: v })}
              options={[
                { value: "none", label: "None" },
                { value: "even", label: "Even" },
                { value: "odd", label: "Odd" },
              ]}
              className="h-9 w-full"
            />
          </Field>
          <Field label="Data bits">
            <EnumSelect<ModbusDataBits>
              value={transport.data_bits}
              onValueChange={(v) => onTransport({ ...transport, data_bits: v })}
              options={[
                { value: "five", label: "5" },
                { value: "six", label: "6" },
                { value: "seven", label: "7" },
                { value: "eight", label: "8 (standard)" },
              ]}
              className="h-9 w-full"
            />
          </Field>
          <Field label="Stop bits">
            <EnumSelect<ModbusStopBits>
              value={transport.stop_bits}
              onValueChange={(v) => onTransport({ ...transport, stop_bits: v })}
              options={[
                { value: "one", label: "1" },
                { value: "two", label: "2" },
              ]}
              className="h-9 w-full"
            />
          </Field>
          <Field label="RS485 direction (Linux)">
            <EnumSelect<"on" | "off">
              value={transport.rs485 ? "on" : "off"}
              onValueChange={(v) =>
                onTransport({
                  ...transport,
                  rs485:
                    v === "on"
                      ? (transport.rs485 ?? {
                          rts_on_send: true,
                          rx_during_tx: false,
                          delay_rts_before_send_ms: 0,
                          delay_rts_after_send_ms: 0,
                        })
                      : null,
                })
              }
              options={[
                { value: "off", label: "Off (auto-direction)" },
                { value: "on", label: "On (RTS-gated adapter)" },
              ]}
              className="h-9 w-full"
            />
          </Field>
          {transport.rs485 && (
            <>
              <Field label="RTS on send">
                <EnumSelect<"high" | "low">
                  value={transport.rs485.rts_on_send ? "high" : "low"}
                  onValueChange={(v) =>
                    onTransport({
                      ...transport,
                      rs485: { ...transport.rs485!, rts_on_send: v === "high" },
                    })
                  }
                  options={[
                    { value: "high", label: "High while TX" },
                    { value: "low", label: "Low while TX" },
                  ]}
                  className="h-9 w-full"
                />
              </Field>
              <Field label="RX during TX">
                <EnumSelect<"on" | "off">
                  value={transport.rs485.rx_during_tx ? "on" : "off"}
                  onValueChange={(v) =>
                    onTransport({
                      ...transport,
                      rs485: {
                        ...transport.rs485!,
                        rx_during_tx: v === "on",
                      },
                    })
                  }
                  options={[
                    { value: "off", label: "Off" },
                    { value: "on", label: "On (echo-tolerant)" },
                  ]}
                  className="h-9 w-full"
                />
              </Field>
              <Field label="RTS delay before (ms)">
                <NumberCell
                  min={0}
                  value={transport.rs485.delay_rts_before_send_ms}
                  onChange={(n) =>
                    onTransport({
                      ...transport,
                      rs485: {
                        ...transport.rs485!,
                        delay_rts_before_send_ms: n,
                      },
                    })
                  }
                />
              </Field>
              <Field label="RTS delay after (ms)">
                <NumberCell
                  min={0}
                  value={transport.rs485.delay_rts_after_send_ms}
                  onChange={(n) =>
                    onTransport({
                      ...transport,
                      rs485: {
                        ...transport.rs485!,
                        delay_rts_after_send_ms: n,
                      },
                    })
                  }
                />
              </Field>
            </>
          )}
          <div className="col-span-2 mt-1 flex items-start gap-2 text-xs text-muted-foreground">
            <Info className="mt-0.5 size-3 shrink-0" />
            <span>
              Device path: macOS{" "}
              <span className="font-mono">/dev/cu.usbserial-*</span>, Linux{" "}
              <span className="font-mono">/dev/ttyUSB0</span>, Windows{" "}
              <span className="font-mono">COM3</span>. Turn{" "}
              <span className="font-mono">RS485 direction</span> on (Linux only)
              if requests time out with correct baud/parity/wiring — RTS-gated
              USB-485 adapters never drive the bus in plain serial mode.
            </span>
          </div>
        </div>
      )}
    </>
  )
}

const BAUD_RATES = [1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200] as const

/** Best-guess default serial device path for the current OS, used
 * as a placeholder so first-time users see what to type. The actual
 * default value stored on the device stays empty until the user
 * explicitly enters a path — better to fail loudly than connect to
 * the wrong port. */
function defaultSerialPath(): string {
  // navigator.platform is deprecated but still the most reliable
  // sync platform identification in browsers. Falls back to a
  // POSIX-shaped guess if unavailable.
  const plat =
    typeof navigator !== "undefined" ? navigator.platform.toLowerCase() : ""
  if (plat.includes("mac")) return "/dev/cu.usbserial-1410"
  if (plat.includes("win")) return "COM3"
  return "/dev/ttyUSB0"
}
