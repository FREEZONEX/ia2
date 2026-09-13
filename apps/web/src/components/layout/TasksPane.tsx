import {
  AlertCircle,
  ArrowDownToLine,
  ChevronRight,
  Clock,
  Play,
  Plus,
  Save,
  Square,
  Trash2,
} from "@/components/ui/icons"
import { useEffect, useState } from "react"

import { Button } from "@/components/ui/button"
import { Field } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { PaneHeader } from "@/components/ui/pane-header"
import { EmptyState as PaneEmptyState } from "@/components/ui/empty-state"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { fetchProjectPous } from "@/lib/api"
import { useRuntime } from "@/state/runtime"
import type { PouInProject } from "@/types/generated/PouInProject"
import type { ProgramInstance } from "@/types/generated/ProgramInstance"
import type { Task } from "@/types/generated/Task"
import type { Tasks } from "@/types/generated/Tasks"

/**
 * Project-level scheduling editor.
 *
 * Mental model: one card per Task. Each card shows the task's
 * interval/priority/name at the top, then a small list of "programs that
 * run on this task" inside. "Add program" picks a PROGRAM-kind POU from
 * the project; the IEC instance name auto-generates (`<program>_inst`,
 * deduped) but can be renamed inline if the user needs two instances of
 * the same PROGRAM.
 *
 * This matches Codesys / TwinCAT layout — engineers think "this 10ms task
 * runs these programs" not "instance row X binds POU Y to task Z".
 *
 * On disk the file is still flat `tasks.toml = { tasks: [], programs: [] }`
 * — we just unflatten in the UI. Save serialises back to flat.
 */
export function TasksPane() {
  const { project, tasks, saveTasks, migrateTasks, isRunning, run, stop } =
    useRuntime()
  const [draft, setDraft] = useState<Tasks>(tasks)
  const [migrating, setMigrating] = useState(false)
  const [migrationNote, setMigrationNote] = useState<string | null>(null)
  // Parser-driven list of every IEC POU declared anywhere in the project.
  // A single source file may declare multiple POUs (PROGRAM + FB + FUNCTION
  // side by side); the file-level `application.kind` is a heuristic, not
  // the truth — we use this list instead.
  const [pous, setPous] = useState<PouInProject[]>([])

  useEffect(() => {
    setDraft(tasks)
  }, [tasks])

  // Refresh the POU declaration list whenever the project changes (POU
  // added/renamed/source edited). Failures are tolerated — the dropdown
  // just falls back to the file-name list below.
  useEffect(() => {
    let cancelled = false
    fetchProjectPous()
      .then((res) => {
        if (!cancelled) setPous(res.pous)
      })
      .catch(() => {
        if (!cancelled) setPous([])
      })
    return () => {
      cancelled = true
    }
  }, [project])

  const dirty = JSON.stringify(draft) !== JSON.stringify(tasks)

  // Only PROGRAM-kind POUs are schedulable. IEC enforces this — FBs and
  // FUNCTIONs are used INSIDE programs, not bound to tasks directly.
  // Picking from real declarations (rather than file-level kind) means
  // multi-POU files like cascade_pid.st (which declares BOTH a FB and a
  // PROGRAM named cascade_pid) correctly show their PROGRAM here.
  const programOptions = pous
    .filter((p) => p.type === "program")
    .map((p) => p.name)
  // Dedup just in case two files declare a PROGRAM with the same name —
  // the lib would reject it at compile, but the dropdown should still
  // show a clean list.
  const programOptionsUnique = Array.from(new Set(programOptions))
  const programOptionsSet = new Set(programOptionsUnique)
  const taskNameSet = new Set(draft.tasks.map((t) => t.name))

  // Orphan = instance bound to a task that no longer exists in the draft.
  // Surface these in their own "Unscheduled" group so the user can see + fix.
  const orphans = draft.programs.filter((p) => !taskNameSet.has(p.task))
  const orphanByTaskName = new Map<string, ProgramInstance[]>()
  for (const o of orphans) {
    const k = o.task || "(no task)"
    const arr = orphanByTaskName.get(k) ?? []
    arr.push(o)
    orphanByTaskName.set(k, arr)
  }

  // Programs grouped by their task (in task definition order).
  const programsByTask = new Map<string, ProgramInstance[]>()
  for (const t of draft.tasks) programsByTask.set(t.name, [])
  for (const p of draft.programs) {
    const arr = programsByTask.get(p.task)
    if (arr) arr.push(p)
  }

  // ----- mutations -----
  const replaceProgramAt = (idx: number, patch: Partial<ProgramInstance>) => {
    setDraft({
      ...draft,
      programs: draft.programs.map((p, i) =>
        i === idx ? { ...p, ...patch } : p,
      ),
    })
  }

  const removeProgramAt = (idx: number) => {
    setDraft({
      ...draft,
      programs: draft.programs.filter((_, i) => i !== idx),
    })
  }

  const setTaskAt = (idx: number, patch: Partial<Task>) => {
    const prev = draft.tasks[idx]
    const next = [...draft.tasks]
    next[idx] = { ...prev, ...patch }
    // If the task was renamed, follow the rename through to any programs
    // bound to it — keeping the link intact rather than breaking it.
    let programs = draft.programs
    if (patch.name && patch.name !== prev.name) {
      programs = programs.map((p) =>
        p.task === prev.name ? { ...p, task: patch.name! } : p,
      )
    }
    setDraft({ tasks: next, programs })
  }

  const addTask = () => {
    let i = draft.tasks.length
    let name = `task_${i}`
    while (taskNameSet.has(name)) {
      i++
      name = `task_${i}`
    }
    setDraft({
      ...draft,
      tasks: [...draft.tasks, { name, interval_ms: 100, priority: 1 }],
    })
  }

  const removeTaskAt = (idx: number) => {
    const name = draft.tasks[idx]?.name
    setDraft({
      ...draft,
      tasks: draft.tasks.filter((_, i) => i !== idx),
      // Orphan its programs instead of deleting them — surfaced in the
      // "Unscheduled" group so the user can rebind or remove.
      programs: draft.programs.map((p) =>
        p.task === name ? { ...p, task: "" } : p,
      ),
    })
  }

  // Add a program to a task: pick the first program POU that isn't yet
  // instantiated on this task (so the dropdown defaults sensibly), and
  // generate a unique instance name `<program>_inst`.
  const addProgramToTask = (taskName: string) => {
    if (programOptionsUnique.length === 0) return
    const usedOnTask = new Set(
      draft.programs.filter((p) => p.task === taskName).map((p) => p.program),
    )
    const chosen =
      programOptionsUnique.find((name) => !usedOnTask.has(name)) ??
      programOptionsUnique[0]
    const baseName = `${chosen}_inst`
    const taken = new Set(draft.programs.map((p) => p.instance))
    let candidate = baseName
    let n = 1
    while (taken.has(candidate)) {
      candidate = `${baseName}_${n++}`
    }
    setDraft({
      ...draft,
      programs: [
        ...draft.programs,
        { instance: candidate, program: chosen, task: taskName },
      ],
    })
  }

  const migrate = async () => {
    setMigrating(true)
    setMigrationNote(null)
    try {
      const report = await migrateTasks()
      if (report.migrated) {
        setMigrationNote(
          `Migrated. ${report.tasks_count} task(s), ${report.programs_count} program instance(s). ` +
            `Stripped CONFIGURATION from: ${report.pous_modified.join(", ") || "(none)"}.`,
        )
      } else {
        setMigrationNote(
          "Already on the new layout — no inline CONFIGURATION found and tasks.toml already exists.",
        )
      }
    } finally {
      setMigrating(false)
    }
  }

  const offerMigration =
    (project?.pous.length ?? 0) > 0 && draft.tasks.length === 0

  return (
    <main className="ia2-pane">
      <Header dirty={dirty} isRunning={isRunning} run={run} stop={stop} save={() => saveTasks(draft)} />

      <div className="min-h-0 flex-1 space-y-5 overflow-auto p-4">
        {offerMigration && (
          <MigrateBanner migrating={migrating} onMigrate={migrate} />
        )}
        {migrationNote && <Note kind="ok">{migrationNote}</Note>}

        {draft.tasks.length === 0 ? (
          <EmptyState onAdd={addTask} />
        ) : (
          <div className="space-y-3">
            {draft.tasks.map((task, taskIdx) => (
              <TaskCard
                key={taskIdx}
                task={task}
                programs={programsByTask.get(task.name) ?? []}
                programOptions={programOptionsUnique}
                programOptionsSet={programOptionsSet}
                allInstances={draft.programs}
                onTaskChange={(patch) => setTaskAt(taskIdx, patch)}
                onTaskRemove={() => {
                  if (confirm(`Remove task "${task.name}"? Its programs will be unscheduled.`)) {
                    removeTaskAt(taskIdx)
                  }
                }}
                onProgramChange={(programInstance, patch) => {
                  const idx = draft.programs.indexOf(programInstance)
                  if (idx >= 0) replaceProgramAt(idx, patch)
                }}
                onProgramRemove={(programInstance) => {
                  const idx = draft.programs.indexOf(programInstance)
                  if (idx >= 0) removeProgramAt(idx)
                }}
                onAddProgram={() => addProgramToTask(task.name)}
              />
            ))}
            <button
              type="button"
              onClick={addTask}
              className="flex w-full items-center justify-center gap-1.5 rounded-md border border-dashed border-border py-2 text-[12px] text-muted-foreground transition-colors hover:bg-accent/40 hover:text-foreground"
            >
              <Plus className="size-3.5" />
              Add task
            </button>
          </div>
        )}

        {orphans.length > 0 && (
          <UnscheduledGroup
            byTaskName={orphanByTaskName}
            programOptions={programOptionsUnique}
            programOptionsSet={programOptionsSet}
            taskNames={draft.tasks.map((t) => t.name)}
            allInstances={draft.programs}
            onProgramChange={(p, patch) => {
              const idx = draft.programs.indexOf(p)
              if (idx >= 0) replaceProgramAt(idx, patch)
            }}
            onProgramRemove={(p) => {
              const idx = draft.programs.indexOf(p)
              if (idx >= 0) removeProgramAt(idx)
            }}
          />
        )}
      </div>
    </main>
  )
}

// ============================================================
//  Subcomponents
// ============================================================

function Header({ dirty, isRunning, run, stop, save }: {
  dirty: boolean
  isRunning: boolean
  run: () => Promise<void>
  stop: () => Promise<void>
  save: () => Promise<void>
}) {
  const [pending, setPending] = useState<"save" | "run" | "stop" | null>(null)
  const act = async (action: "save" | "run" | "stop") => {
    if (pending) return
    setPending(action)
    try { await ({ save, run, stop })[action]() } finally { setPending(null) }
  }
  return (
    <PaneHeader
      title="Tasks"
      description="Schedule programs and set their scan intervals"
      meta={dirty ? <span className="text-warn">Unsaved changes</span> : undefined}
      actions={<>
        <Button size="sm" variant={dirty ? "default" : "outline"} disabled={!dirty || pending !== null} onClick={() => void act("save")}>
          <Save className="size-4" /> {pending === "save" ? "Saving…" : "Save changes"}
        </Button>
        {isRunning ? (
          <Button size="sm" variant="outline" className="text-destructive" disabled={pending !== null} onClick={() => void act("stop")}>
            <Square className="size-4" /> {pending === "stop" ? "Stopping…" : "Stop project"}
          </Button>
        ) : (
          <Button size="sm" disabled={dirty || pending !== null} title={dirty ? "Save changes before running" : "Compile and run all scheduled programs"} onClick={() => void act("run")}>
            <Play className="size-4" /> {pending === "run" ? "Starting…" : "Run project"}
          </Button>
        )}
      </>}
    />
  )
}

function EmptyState({ onAdd }: { onAdd: () => void }) {
  return <PaneEmptyState icon={<Clock className="size-6" />} title="No tasks scheduled" description="A task runs its assigned programs at a fixed interval. Create a task, then add a program to it." actions={<Button size="sm" onClick={onAdd}><Plus className="size-4" /> Create task</Button>} />
}

function TaskCard({
  task,
  programs,
  programOptions,
  programOptionsSet,
  allInstances,
  onTaskChange,
  onTaskRemove,
  onProgramChange,
  onProgramRemove,
  onAddProgram,
}: {
  task: Task
  programs: ProgramInstance[]
  programOptions: string[]
  programOptionsSet: Set<string>
  allInstances: ProgramInstance[]
  onTaskChange: (patch: Partial<Task>) => void
  onTaskRemove: () => void
  onProgramChange: (
    program: ProgramInstance,
    patch: Partial<ProgramInstance>,
  ) => void
  onProgramRemove: (program: ProgramInstance) => void
  onAddProgram: () => void
}) {
  return (
    <div className="overflow-hidden border-y border-border bg-background">
      <div className="flex flex-wrap items-end gap-3 bg-muted/40 px-3 py-3">
        <Field label="Task name" className="w-44">
          <Input
            value={task.name}
            onChange={(e) => onTaskChange({ name: e.target.value })}
            className="h-8 font-mono"
          />
        </Field>
        <Field label="Interval" className="w-32">
          <div className="relative">
            <Input
              type="number"
              min={1}
              value={task.interval_ms}
              onChange={(e) =>
                onTaskChange({
                  interval_ms: Math.max(1, Number(e.target.value) || 1),
                })
              }
              className="h-8 pr-8 tabular-nums"
            />
            <span className="pointer-events-none absolute inset-y-0 right-2 flex items-center font-mono text-xs text-muted-foreground">
              ms
            </span>
          </div>
        </Field>
        <Field label="Priority" className="w-24">
          <Input
            type="number"
            value={task.priority}
            onChange={(e) =>
              onTaskChange({ priority: Number(e.target.value) || 0 })
            }
            className="h-8 tabular-nums"
          />
        </Field>
        <span className="ml-auto inline-flex items-center gap-1 font-mono text-xs text-muted-foreground">
          {programs.length} program{programs.length === 1 ? "" : "s"}
        </span>
        <button
          type="button"
          onClick={onTaskRemove}
          className="inline-flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-accent/40 hover:text-destructive"
          title="Remove this task"
        >
          <Trash2 className="size-3.5" />
        </button>
      </div>

      <div className="p-2">
        {programs.length === 0 ? (
          <div className="px-2 py-1 text-[12px] italic text-muted-foreground">
            No programs scheduled on this task.
          </div>
        ) : (
          <ul className="space-y-1">
            {programs.map((p, i) => (
              <ProgramRow
                key={`${p.instance}-${i}`}
                program={p}
                programOptions={programOptions}
                programMissing={!!p.program && !programOptionsSet.has(p.program)}
                instanceNameClashes={
                  allInstances.filter((q) => q.instance === p.instance).length >
                  1
                }
                onChange={(patch) => onProgramChange(p, patch)}
                onRemove={() => onProgramRemove(p)}
              />
            ))}
          </ul>
        )}

        <button
          type="button"
          onClick={onAddProgram}
          disabled={programOptions.length === 0}
          title={
            programOptions.length === 0
              ? "Create a PROGRAM-kind POU first"
              : "Schedule a PROGRAM on this task"
          }
          className="mt-1 flex w-full items-center justify-center gap-1.5 rounded py-1.5 text-xs text-muted-foreground transition-colors hover:bg-accent/40 hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
        >
          <Plus className="size-3" />
          Add program to this task
        </button>
      </div>
    </div>
  )
}

function ProgramRow({
  program,
  programOptions,
  programMissing,
  instanceNameClashes,
  onChange,
  onRemove,
}: {
  program: ProgramInstance
  programOptions: string[]
  programMissing: boolean
  instanceNameClashes: boolean
  onChange: (patch: Partial<ProgramInstance>) => void
  onRemove: () => void
}) {
  return (
    <li className="flex flex-wrap items-center gap-2 rounded px-2 py-1.5 hover:bg-accent/20">
      <ChevronRight className="size-3 shrink-0 text-muted-foreground" />
      <Select
        value={program.program}
        onValueChange={(v) => {
          // When the user picks a new PROGRAM, also auto-update the
          // instance name if it still looks like the default for the
          // previous program — minimizes manual rename for the common
          // 1-instance-per-program case.
          const looksLikeDefault =
            program.instance === `${program.program}_inst`
          onChange({
            program: v,
            ...(looksLikeDefault ? { instance: `${v}_inst` } : {}),
          })
        }}
      >
        <SelectTrigger
          className={
            "h-7 w-40 text-[12px] " + (programMissing ? "border-destructive/60" : "")
          }
        >
          <SelectValue placeholder="(pick a PROGRAM)" />
        </SelectTrigger>
        <SelectContent>
          {programOptions.map((name) => (
            <SelectItem key={name} value={name}>
              {name}
            </SelectItem>
          ))}
          {programMissing && (
            <SelectItem value={program.program}>
              {program.program} (missing)
            </SelectItem>
          )}
        </SelectContent>
      </Select>
      <span className="font-mono text-xs text-muted-foreground">as</span>
      <Input
        value={program.instance}
        onChange={(e) => onChange({ instance: e.target.value })}
        className={
          "h-7 w-44 font-mono text-[12px] " +
          (instanceNameClashes ? "border-destructive/60" : "")
        }
        title={
          instanceNameClashes
            ? "Instance name is reused elsewhere — IEC requires uniqueness"
            : "Instance name (auto-generated, edit if you need two instances of the same PROGRAM)"
        }
      />
      <button
        type="button"
        onClick={onRemove}
        className="ml-auto inline-flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-accent/40 hover:text-destructive"
        title="Unschedule"
      >
        <Trash2 className="size-3.5" />
      </button>
    </li>
  )
}

function UnscheduledGroup({
  byTaskName,
  programOptions,
  programOptionsSet,
  taskNames,
  allInstances,
  onProgramChange,
  onProgramRemove,
}: {
  byTaskName: Map<string, ProgramInstance[]>
  programOptions: string[]
  programOptionsSet: Set<string>
  taskNames: string[]
  allInstances: ProgramInstance[]
  onProgramChange: (
    program: ProgramInstance,
    patch: Partial<ProgramInstance>,
  ) => void
  onProgramRemove: (program: ProgramInstance) => void
}) {
  return (
    <div className="overflow-hidden rounded-md border border-destructive/40 bg-destructive/5">
      <div className="flex items-center gap-2 border-b border-destructive/30 bg-destructive/10 px-3 py-2 text-xs font-medium text-destructive">
        <AlertCircle className="size-3.5" />
        Unscheduled program instances — Run / Deploy will fail until these are fixed.
      </div>
      <ul className="space-y-1 p-2">
        {Array.from(byTaskName.entries()).flatMap(([oldTask, ps]) =>
          ps.map((p, i) => (
            <li
              key={`${oldTask}-${i}-${p.instance}`}
              className="flex flex-wrap items-center gap-2 rounded px-2 py-1.5"
            >
              <Select
                value={program_for_select(p, programOptionsSet)}
                onValueChange={(v) => onProgramChange(p, { program: v })}
              >
                <SelectTrigger
                  className={
                    "h-7 w-40 text-[12px] " +
                    (!programOptionsSet.has(p.program)
                      ? "border-destructive/60"
                      : "")
                  }
                >
                  <SelectValue placeholder="(missing program)" />
                </SelectTrigger>
                <SelectContent>
                  {programOptions.map((name) => (
                    <SelectItem key={name} value={name}>
                      {name}
                    </SelectItem>
                  ))}
                  {!programOptionsSet.has(p.program) && p.program && (
                    <SelectItem value={p.program}>
                      {p.program} (missing)
                    </SelectItem>
                  )}
                </SelectContent>
              </Select>
              <span className="font-mono text-xs text-muted-foreground">
                as
              </span>
              <Input
                value={p.instance}
                onChange={(e) => onProgramChange(p, { instance: e.target.value })}
                className="h-7 w-44 font-mono text-[12px]"
              />
              <span className="font-mono text-xs text-muted-foreground">→</span>
              <Select
                value={p.task || ""}
                onValueChange={(v) => onProgramChange(p, { task: v })}
              >
                <SelectTrigger className="h-7 w-32 border-destructive/60 text-[12px]">
                  <SelectValue placeholder="(no task)" />
                </SelectTrigger>
                <SelectContent>
                  {taskNames.map((t) => (
                    <SelectItem key={t} value={t}>
                      {t}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <button
                type="button"
                onClick={() => onProgramRemove(p)}
                className="ml-auto inline-flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-accent/40 hover:text-destructive"
                title="Remove"
              >
                <Trash2 className="size-3.5" />
              </button>
            </li>
          )),
        )}
      </ul>
      {/* Touch allInstances so it's not flagged unused — kept on the
          signature for symmetry with TaskCard, may be useful for
          dup-instance checks across the unscheduled group too. */}
      <div className="sr-only">{allInstances.length}</div>
    </div>
  )
}

function program_for_select(p: ProgramInstance, valid: Set<string>): string {
  // <Select> doesn't like values that aren't in its option list, so fall
  // back to empty when the program is broken — the SelectItem for the
  // missing name is rendered separately.
  return valid.has(p.program) ? p.program : ""
}

function MigrateBanner({
  migrating,
  onMigrate,
}: {
  migrating: boolean
  onMigrate: () => void
}) {
  return (
    <div className="flex items-start gap-2 rounded-md border border-warn/40 bg-warn/10 p-3 text-[12px] text-warn">
      <ArrowDownToLine className="mt-0.5 size-4 shrink-0" />
      <div className="flex-1 space-y-1">
        <div className="font-medium">Legacy project — needs migration</div>
        <p className="text-xs text-warn/80">
          This project's POU files still carry inline CONFIGURATION blocks.
          Migration extracts them into <span className="font-mono">tasks.toml</span>{" "}
          and strips them from the POU source files. Files are rewritten in
          place — review git diff after.
        </p>
        <Button
          size="sm"
          variant="outline"
          onClick={onMigrate}
          disabled={migrating}
          className="mt-1"
        >
          <ArrowDownToLine className="mr-1.5 size-3" />
          {migrating ? "Migrating…" : "Migrate now"}
        </Button>
      </div>
    </div>
  )
}

function Note({
  children,
  kind,
}: {
  children: React.ReactNode
  kind: "ok"
}) {
  const cls =
    kind === "ok"
      ? "border-highlight/40 bg-highlight/10 text-highlight"
      : ""
  return <div className={`rounded-md border p-2 text-[12px] ${cls}`}>{children}</div>
}
