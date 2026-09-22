import { FileCode2, PanelRight, Play, Plus, RotateCcw, Save, Square } from "@/components/ui/icons"
import { useCallback, useEffect, useRef, useState } from "react"

import { FBDEditor } from "@/components/editor/FBDEditor"
import { LDEditor } from "@/components/editor/LDEditor"
import { SFCEditor } from "@/components/editor/SFCEditor"
import { STEditor } from "@/components/editor/STEditor"
import { Button } from "@/components/ui/button"
import { EmptyState } from "@/components/ui/empty-state"
import { PaneHeader } from "@/components/ui/pane-header"
import { cn } from "@/lib/utils"
import { usePouSpawnTick, useRuntime } from "@/state/runtime"
import { DatasheetView } from "./DatasheetView"
import { VariablesPanel } from "./VariablesPanel"

export function ProgramPane() {
  const {
    currentPou,
    source,
    setSource,
    isDirty,
    externalChange,
    loadExternalChange,
    saveCurrentPou,
    isRunning,
    run,
    stop,
    diagnostics,
    tasks,
    saveTasks,
    clearError,
  } = useRuntime()

  // Right-side Variables panel — defaults open so users discover the
  // binding picker without hunting. Persists across POU switches but not
  // across reloads (keeping the state ephemeral keeps the toolbar simple).
  const [varsOpen, setVarsOpen] = useState(true)
  const [pending, setPending] = useState<"save" | "run" | "stop" | "schedule" | null>(null)
  const pendingRef = useRef(false)
  const [actionError, setActionError] = useState<string | null>(null)
  const perform = useCallback(async (action: NonNullable<typeof pending>, operation: () => Promise<void>) => {
    if (pendingRef.current) return
    pendingRef.current = true
    setPending(action)
    setActionError(null)
    clearError()
    try {
      await operation()
    } catch (error) {
      setActionError(String(error))
    } finally {
      pendingRef.current = false
      setPending(null)
    }
  }, [clearError])

  // Imported library blocks (pous/lib/**) open read-only: the server
  // rejects writes there anyway (they're managed via /api/library), so
  // don't let edits accumulate that can only fail on Save.
  // Agent-generated content reveal: when an SSE-driven update lands in
  // the open editor, sweep an acid scan line down the editor area (all
  // four languages) — the ST editor additionally staggers its lines.
  const spawnTick = usePouSpawnTick()
  const [sweeping, setSweeping] = useState(false)
  useEffect(() => {
    if (spawnTick === 0) return
    setSweeping(true)
    const t = setTimeout(() => setSweeping(false), 900)
    return () => clearTimeout(t)
  }, [spawnTick])

  const isLibrary = currentPou?.path.startsWith("lib/") ?? false
  const libraryName = isLibrary ? currentPou!.path.split("/")[1] : null

  useEffect(() => {
    const save = (event: KeyboardEvent) => {
      if (!currentPou || !(event.metaKey || event.ctrlKey) || event.key.toLowerCase() !== "s") return
      event.preventDefault()
      if (isDirty && !isLibrary) void perform("save", saveCurrentPou)
    }
    window.addEventListener("keydown", save, true)
    return () => window.removeEventListener("keydown", save, true)
  }, [currentPou, isDirty, isLibrary, perform, saveCurrentPou])

  if (!currentPou) {
    return (
      <main className="flex h-full min-h-0 min-w-0 flex-col bg-background">
        <PaneHeader title="Program" />
        <EmptyState
          icon={<FileCode2 />}
          title="Open a program"
          description="Select a POU in the project tree, or use Add POU to create one."
        />
      </main>
    )
  }

  const target = currentPou.declarations.find((declaration) => declaration.type === "program")
  const declaration = currentPou.declarations[0]
  const declarationLabel = currentPou.declarations.length > 1
    ? `${currentPou.declarations.length} POUs`
    : declaration?.type === "function_block" ? "Function block"
      : declaration?.type === "function" ? "Function" : "Program"

  return (
    <main className="flex h-full min-h-0 min-w-0 flex-col bg-background">
      <PaneHeader
        title={<span className="font-mono" title={currentPou.path}>{currentPou.path}</span>}
        meta={<>
          <span>{declarationLabel} · {declaration?.language.toUpperCase() ?? "ST"}</span>
          {isLibrary && <span>Library · Read only</span>}
          {isDirty && <span className="text-warn">Modified</span>}
          {diagnostics.length > 0 && (
            <span className="text-destructive">
              {diagnostics.length} {diagnostics.length === 1 ? "issue" : "issues"}
            </span>
          )}
          <ScheduleHint
            currentPou={currentPou}
            tasks={tasks}
            pending={pending === "schedule"}
            disabled={pending !== null}
            onSchedule={(programName) => perform("schedule", async () => {
              const taskName = tasks.tasks[0]?.name ?? "plc_task"
              const nextTasks = tasks.tasks.length === 0
                ? [{ name: taskName, interval_ms: 100, priority: 1 }]
                : tasks.tasks
              const taken = new Set(tasks.programs.map((program) => program.instance))
              let instance = `${programName}_inst`
              let suffix = 1
              while (taken.has(instance)) instance = `${programName}_inst_${suffix++}`
              await saveTasks({
                ...tasks,
                tasks: nextTasks,
                programs: [...tasks.programs, { instance, program: programName, task: taskName }],
              })
            })}
          />
        </>}
        actions={<>
          {!isLibrary && <>
            <Button
              variant="ghost" size="sm" disabled={!isDirty || pending !== null}
              onClick={() => setSource(currentPou.source)} title="Discard unsaved changes in this file"
            >
              <RotateCcw /> Revert
            </Button>
            <Button
              variant="default" size="sm" disabled={!isDirty || pending !== null}
              onClick={() => void perform("save", saveCurrentPou)} title="Save (Cmd/Ctrl+S)"
              aria-busy={pending === "save"}
            >
              <Save /> {pending === "save" ? "Saving…" : "Save"}
            </Button>
          </>}
          {isRunning ? (
            <Button
              variant="outline" size="sm" disabled={pending !== null}
              onClick={() => void perform("stop", stop)} className="text-destructive"
              aria-busy={pending === "stop"}
            >
              <Square /> {pending === "stop" ? "Stopping…" : "Stop"}
            </Button>
          ) : (
            <Button
              variant="highlight" size="sm" disabled={!target || pending !== null}
              onClick={() => target && void perform("run", () => run(target.name, currentPou.path))}
              title={target
                ? `Save, compile and run PROGRAM ${target.name} from this file in isolation`
                : "No PROGRAM in this file. Use Tasks to run the project schedule."}
              aria-busy={pending === "run"}
            >
              <Play />
              <span className="max-w-48 truncate">{pending === "run" ? "Starting…" : target ? `Run ${target.name}` : "Run"}</span>
            </Button>
          )}
          <Button
            variant="ghost" size="icon-sm" onClick={() => setVarsOpen((open) => !open)}
            title={varsOpen ? "Hide Variables panel" : "Show Variables panel"}
            aria-label={varsOpen ? "Hide Variables panel" : "Show Variables panel"}
            aria-pressed={varsOpen}
            className={varsOpen ? "bg-accent text-foreground" : "text-muted-foreground"}
          >
            <PanelRight />
          </Button>
        </>}
      />
      {actionError && <div role="alert" className="border-b border-border bg-destructive/5 px-4 py-2 text-[13px] text-destructive">{actionError}</div>}
      {externalChange && (
        <div role="alert" className="flex items-center gap-3 border-b border-warn/50 bg-warn/10 px-4 py-2 text-[13px] text-warn">
          <span className="min-w-0 flex-1">
            Changed on disk while you were editing — another writer saved a different version.
            Save and Run will ask before replacing it.
          </span>
          <Button variant="outline" size="sm" className="shrink-0" onClick={loadExternalChange}>
            Load disk version
          </Button>
        </div>
      )}
      <div className="flex min-h-0 flex-1">
        <div className={cn("relative min-h-0 min-w-0 flex-1", sweeping && "pou-sweep")}>
          {/* Library blocks open as a datasheet (interface + docs, with
              the ST source folded away) — a block is a contract to use,
              not code to read. Everything else dispatches to its
              language editor. */}
          {isLibrary ? (
            <DatasheetView source={source} libraryName={libraryName ?? ""} />
          ) : currentPou.declarations[0]?.language === "ld" ? (
            <LDEditor
              value={source}
              onChange={setSource}
              path={currentPou.path}
              readOnly={isLibrary}
            />
          ) : currentPou.declarations[0]?.language === "fbd" ? (
            <FBDEditor
              value={source}
              onChange={setSource}
              path={currentPou.path}
              readOnly={isLibrary}
            />
          ) : currentPou.declarations[0]?.language === "sfc" ? (
            <SFCEditor
              value={source}
              onChange={setSource}
              path={currentPou.path}
              readOnly={isLibrary}
            />
          ) : (
            <STEditor
              value={source}
              onChange={setSource}
              diagnostics={diagnostics}
              readOnly={isLibrary}
              spawnTick={spawnTick}
            />
          )}
        </div>
        {varsOpen && !isLibrary && (
          <div className="hidden min-h-0 w-[240px] shrink-0 md:block">
            <VariablesPanel />
          </div>
        )}
      </div>
    </main>
  )
}

import type { Pou } from "@/types/generated/Pou"
import type { Tasks } from "@/types/generated/Tasks"

/** Scheduling is separate from the isolated file Run action above. */
function ScheduleHint({ currentPou, tasks, onSchedule, pending, disabled }: {
  currentPou: Pou
  tasks: Tasks
  onSchedule: (programName: string) => Promise<void>
  pending: boolean
  disabled: boolean
}) {
  const programs = currentPou.declarations.filter((declaration) => declaration.type === "program")
  if (programs.length === 0) return null
  const scheduled = new Set(tasks.programs.map((program) => program.program))
  const unscheduled = programs.filter((program) => !scheduled.has(program.name))
  if (unscheduled.length === 0) {
    return <span title="Every PROGRAM in this file belongs to the project task schedule.">In task schedule</span>
  }
  const target = unscheduled[0]
  return (
    <Button
      type="button" variant="ghost" size="sm" disabled={disabled} aria-busy={pending}
      onClick={() => void onSchedule(target.name)}
      title={`Add PROGRAM ${target.name} to the project task schedule`}
    >
      <Plus /> {pending ? "Scheduling…" : "Add to task"}
    </Button>
  )
}
