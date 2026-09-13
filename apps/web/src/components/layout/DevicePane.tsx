import { useEffect, useState } from "react"

import { EmptyState } from "@/components/ui/empty-state"
import { PaneHeader } from "@/components/ui/pane-header"

import { fetchPouVariables } from "@/lib/api"
import { useRuntime } from "@/state/runtime"
import type { VariableInfo } from "@/types/generated/VariableInfo"

import { CanopenDeviceEditor } from "./CanopenDeviceEditor"
import { EthercatDeviceEditor } from "./EthercatDeviceEditor"
import { ModbusDeviceEditor } from "./ModbusDeviceEditor"
import { OpcuaDeviceEditor } from "./OpcuaDeviceEditor"
import type { LinkProps } from "./deviceEditorShared"

/**
 * Thin dispatcher: prefetch the per-POU variable lists (so the inline
 * add-binding form has autocomplete without per-row latency), then hand
 * off to the protocol-specific editor. The per-protocol column sets differ
 * enough (Modbus registers vs EtherCAT PDO offsets vs OPC UA NodeIds vs
 * CANopen object entries) that each editor owns its own table. Genuinely
 * shared bits (draft scaffold, save bar, LinkedToCell) live in
 * `deviceEditorShared`.
 */
export function DevicePane() {
  const { currentDevice, project, iomap, saveDevice, saveIomap } = useRuntime()
  const [varsByApp, setVarsByApp] = useState<Record<string, VariableInfo[]>>({})

  // Pre-fetch variables for every POU once, so the inline add-binding form
  // can offer autocomplete without per-row latency.
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
      if (!cancelled) setVarsByApp(Object.fromEntries(entries))
    })
    return () => {
      cancelled = true
    }
  }, [project])

  if (!currentDevice) {
    return (
      <main className="ia2-pane">
        <PaneHeader title="Devices" description="Connections, channels and variable bindings" />
        <EmptyState title="Select a device" description="Choose a device in the project tree to configure its connection and channels." />
      </main>
    )
  }

  // Editing through the Linked-to column commits straight to iomap.toml.
  // The Mapping wire-format identifies the device by name, so we just
  // splice the entries for this device.
  const linkProps: LinkProps = {
    deviceName: currentDevice.name,
    iomap,
    saveIomap,
    apps: project?.pous.map((p) => p.path) ?? [],
    varsByApp,
  }

  return (
    <main className="ia2-pane">
      {currentDevice.protocol === "modbus" ? (
        <ModbusDeviceEditor
          device={currentDevice}
          onSave={saveDevice}
          link={linkProps}
        />
      ) : currentDevice.protocol === "opcua" ? (
        <OpcuaDeviceEditor
          device={currentDevice}
          onSave={saveDevice}
          link={linkProps}
        />
      ) : currentDevice.protocol === "canopen" ? (
        <CanopenDeviceEditor
          device={currentDevice}
          onSave={saveDevice}
          link={linkProps}
        />
      ) : (
        <EthercatDeviceEditor
          device={currentDevice}
          onSave={saveDevice}
          link={linkProps}
        />
      )}
    </main>
  )
}
