import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  type ReactNode,
} from "react"
import type { AppEvent } from "@/types/generated/AppEvent"
import type { MutationEvent } from "@/types/generated/MutationEvent"
import type { CheckDiagnostic } from "@/types/generated/CheckDiagnostic"
import type { Device } from "@/types/generated/Device"
import type { Edge } from "@/types/generated/Edge"
import type { IoMap } from "@/types/generated/IoMap"
import type { MigrationResponse } from "@/types/generated/MigrationResponse"
import type { Pou } from "@/types/generated/Pou"
import type { PouLanguage } from "@/types/generated/PouLanguage"
import type { PouType } from "@/types/generated/PouType"
import type { ProjectListing } from "@/types/generated/ProjectListing"
import type { ProjectTree } from "@/types/generated/ProjectTree"
import type { Protocol } from "@/types/generated/Protocol"
import type { Tasks } from "@/types/generated/Tasks"
import type { VarSnapshot } from "@/types/generated/VarSnapshot"
import { agentActivityStore } from "@/state/agent-activity"
import { hmiLiveStore } from "@/state/hmi-live"
import { liveFeedStore } from "@/state/live-feed"
import { buildProjectFbDefs, setProjectFbs, type FbPin } from "@/lib/ld-fbs"
import { invalidationBus, Topic } from "@/state/invalidation"
import {
  attachEdge as apiAttachEdge,
  checkProgram,
  closeProject as apiCloseProject,
  currentProject,
  createDevice as apiCreateDevice,
  createDeviceFolder as apiCreateDeviceFolder,
  createEdge as apiCreateEdge,
  createEdgeFolder as apiCreateEdgeFolder,
  createPou as apiCreatePou,
  createPouFolder as apiCreatePouFolder,
  createProject as apiCreateProject,
  deleteDevice as apiDeleteDevice,
  deleteEdge as apiDeleteEdge,
  deletePou as apiDeletePou,
  detachEdge as apiDetachEdge,
  eventsUrl,
  fetchDevice,
  fetchEdge,
  fetchIomap,
  fetchPou,
  fetchProject,
  fetchPouVariables,
  fetchProjects as apiFetchProjects,
  fetchRuntimeStatus,
  fetchTasks,
  migrateTasks as apiMigrateTasks,
  openProject as apiOpenProject,
  runProgram,
  savePou,
  stopProgram,
  updateDevice as apiUpdateDevice,
  updateEdge as apiUpdateEdge,
  updateIomap as apiUpdateIomap,
  updateTasks as apiUpdateTasks,
} from "@/lib/api"
import { LspClient, pouDocumentUri } from "@/lib/lsp-client"

export type View = "app" | "device" | "iomap" | "edge" | "tasks" | "hmi"

/** Select the project before the next API request reads its routing header.
 * Creating and opening must both replace a previous window's selector. */
function selectWindowProject(name: string): void {
  if (typeof window === "undefined") return
  const url = new URL(window.location.href)
  url.searchParams.set("project", name)
  window.history.replaceState(null, "", url.toString())
}

/**
 * Handle a single `Mutation` event from `/api/events`.
 *
 * Two outputs only — invalidate the matching cache topic, and (if
 * a new POU was created) auto-jump the editor onto it.
 *
 * Toasts used to live here but were removed once the agent takeover
 * overlay landed. The overlay is the canonical surface for
 * "something happened in the background." When no agent is active,
 * the only things firing mutations are the user's own clicks in the
 * IDE — toasting those was always redundant noise.
 *
 * Auto-jump policy (pou_created): jump to the new POU iff the
 * editor is currently empty OR clean. Never disturb a user with
 * unsaved work — they need to navigate manually in that case.
 */
/** Bumps every time an AGENT-driven change lands in the open editor
 * (SSE per-POU refresh, or the auto-jump to a freshly created POU).
 * The editors listen and play their "generated" reveal. Human typing
 * never routes through here. */
class PouSpawnStore {
  private tick = 0
  private listeners = new Set<() => void>()
  getTick = (): number => this.tick
  subscribe = (l: () => void): (() => void) => {
    this.listeners.add(l)
    return () => {
      this.listeners.delete(l)
    }
  }
  bump(): void {
    this.tick += 1
    this.listeners.forEach((l) => l())
  }
}
export const pouSpawnStore = new PouSpawnStore()
export function usePouSpawnTick(): number {
  return useSyncExternalStore(pouSpawnStore.subscribe, pouSpawnStore.getTick)
}

export function handleMutationEvent(
  event: MutationEvent,
  currentPouRef: React.MutableRefObject<Pou | null>,
  sourceRef: React.MutableRefObject<string>,
  selectPouRef: React.MutableRefObject<
    ((path: string) => Promise<void>) | null
  >,
  displayedProject: string | null,
  onProjectClosed: () => void,
): void {
  // Project-scoping: the SSE channel is single, but server tags every
  // mutation with its `project` so windows can filter. A window with
  // `?project=foo` ignores mutations from `?project=bar` so editing
  // one project doesn't auto-jump or invalidate state in the other.
  //
  // `event.project === ""` means a server still tagged something
  // without a name (defensive) — treat as universal and let it
  // through so we don't lose events from edge cases.
  const ours = currentProject() ?? displayedProject
  // A closed project no longer has a tree to fetch. Consume its explicit
  // lifecycle event before invalidation, and only clear the window that
  // actually displays it (including a window without a URL selector).
  if (event.detail.kind === "project_closed") {
    if (event.project && event.project === ours) onProjectClosed()
    return
  }
  if (ours && event.project && event.project !== ours) {
    return
  }

  // (1) Fan out cache invalidation. Any hook subscribed via
  // useInvalidate refetches its own slice.
  invalidationBus.emit(event.topic)

  // (1b) HMI mutations additionally carry the touched node ids — feed
  // the dedicated store so open canvases reload AND spawn-animate
  // exactly the elements this batch placed.
  if (event.detail.kind === "hmi_upserted") {
    hmiLiveStore.upserted(event.detail.path, event.detail.touched)
  } else if (event.detail.kind === "hmi_deleted") {
    hmiLiveStore.deleted(event.detail.path)
  }

  // (2) Auto-jump for newly-created POUs. Per-POU refetch on
  // updates is handled by the dedicated subscription effect in
  // RuntimeProvider (`Topic.pou(currentPou.path)`).
  if (event.detail.kind === "pou_created") {
    const cur = currentPouRef.current
    const dirty = !!cur && sourceRef.current !== cur.source
    if (!cur || !dirty) {
      void selectPouRef.current?.(event.detail.path)?.then(() => pouSpawnStore.bump())
    }
  }
}

/** Which edge (if any) the IDE is currently attached to. When attached,
 * the SSE source switches from the local bridge to the edge's runtime
 * via the SSH-forwarded port. */
export type AttachedEdge = { name: string; localPort: number } | null

/** What the runtime is currently executing.
 *
 *  - `isolated`: ProgramPane "Run THIS POU" — one PROGRAM, compiled from
 *    a single source file, on a synthetic single-task schedule.
 *  - `scheduled`: TasksPane "Run project" — every PROGRAM instance bound
 *    in `tasks.toml`. We snapshot the list at run-time so the Monitor
 *    header stays accurate even if the user edits tasks.toml afterwards.
 *  - `remote`: attached to an edge runtime. We can't easily know the
 *    edge's running programs without calling its `/status` endpoint, so
 *    show the edge name and let the operator inspect there.
 *  - `null`: nothing running locally and not attached. */
export type RunningInfo =
  | { kind: "isolated"; program: string; filePath: string }
  | { kind: "scheduled"; programs: string[] }
  | { kind: "remote"; edge: string }
  | null

type AppState = {
  // Project
  project: ProjectTree | null
  projectLoading: boolean
  availableProjects: ProjectListing[]

  // Center-pane focus
  view: View | null
  currentPou: Pou | null
  source: string
  setSource: (s: string) => void
  isDirty: boolean
  diagnostics: CheckDiagnostic[]
  /** Bumps on every project-tree refresh. Editors put it in their
   *  diagnostics-effect deps so importing/removing a library re-checks
   *  an already-open POU (its FB references may have just (un)resolved). */
  projectEpoch: number

  currentDevice: Device | null
  /** Selected HMI screen slug when view === "hmi". */
  currentHmi: string | null
  currentEdge: Edge | null
  iomap: IoMap
  tasks: Tasks

  /** Live attachment to an edge runtime (or null when running locally). */
  attached: AttachedEdge

  // Runtime. High-frequency feed state (last snapshot, SSE link) lives
  // in `state/live-feed.ts` — subscribe with useLastSnapshot() /
  // useConnected() instead of through this context, so snapshot ticks
  // don't re-render every context consumer.
  isRunning: boolean
  /** What the runtime is currently executing — see `RunningInfo`. */
  running: RunningInfo

  // Errors
  error: string | null
  /** Dismiss the current global error (used by the toast). */
  clearError: () => void

  // Project actions
  createProject: (name: string) => Promise<boolean>
  openProject: (path: string) => Promise<void>
  closeProject: () => Promise<void>
  refreshProjects: () => Promise<void>
  refreshProject: () => Promise<void>

  // Selection actions
  selectPou: (path: string) => Promise<void>
  selectDevice: (name: string) => Promise<void>
  selectHmi: (path: string) => Promise<void>
  selectEdge: (name: string) => Promise<void>
  openIoMap: () => Promise<void>
  openTasks: () => Promise<void>

  // POU / Device mutations
  saveCurrentPou: () => Promise<void>
  /** Returns true on success; dialogs keep themselves open on false. */
  createPou: (path: string, type_: PouType, language?: PouLanguage) => Promise<boolean>
  deletePou: (path: string) => Promise<void>
  createDevice: (name: string, protocol: Protocol) => Promise<boolean>
  deleteDevice: (name: string) => Promise<void>
  createPouFolder: (path: string) => Promise<boolean>
  createDeviceFolder: (path: string) => Promise<boolean>
  createEdge: (name: string, host: string) => Promise<boolean>
  deleteEdge: (name: string) => Promise<void>
  saveEdge: (edge: Edge) => Promise<void>
  createEdgeFolder: (path: string) => Promise<boolean>

  /** Open an SSH tunnel to the edge's runtime port and switch the SSE
   * stream over to it. Updates `attached`. */
  attachEdge: (name: string) => Promise<void>
  /** Close the SSH tunnel and switch back to the local bridge SSE. */
  detachEdge: () => Promise<void>

  saveTasks: (tasks: Tasks) => Promise<void>
  /** One-shot migration: extract inline CONFIGURATION blocks from POUs
   * into tasks.toml, strip them from the POU source files. */
  migrateTasks: () => Promise<MigrationResponse>
  saveDevice: (device: Device) => Promise<void>
  saveIomap: (iomap: IoMap) => Promise<void>

  // Runtime actions. `run()` runs the project's tasks.toml schedule.
  // `run(program, file_path)` runs JUST that PROGRAM ad-hoc, compiling
  // only the named file's source so Monitor shows exactly the running
  // PROGRAM's variables. Tasks.toml on disk is never touched either way.
  run: (program?: string, file_path?: string) => Promise<void>
  stop: () => Promise<void>
}

const Ctx = createContext<AppState | null>(null)

export function RuntimeProvider({ children }: { children: ReactNode }) {
  const [project, setProject] = useState<ProjectTree | null>(null)
  const [projectLoading, setProjectLoading] = useState(true)
  const [availableProjects, setAvailableProjects] = useState<ProjectListing[]>(
    [],
  )

  const [view, setView] = useState<View | null>(null)
  const [currentPou, setCurrentPou] = useState<Pou | null>(null)
  const [source, setSource] = useState("")
  const [diagnostics, setDiagnostics] = useState<CheckDiagnostic[]>([])
  const [projectEpoch, setProjectEpoch] = useState(0)
  const [currentDevice, setCurrentDevice] = useState<Device | null>(null)
  const [currentHmi, setCurrentHmi] = useState<string | null>(null)
  const [currentEdge, setCurrentEdge] = useState<Edge | null>(null)
  const [attached, setAttached] = useState<AttachedEdge>(null)
  const [iomap, setIomap] = useState<IoMap>({ mappings: [] })
  const [tasks, setTasks] = useState<Tasks>({ tasks: [], programs: [] })

  const [isRunning, setIsRunning] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const clearError = useCallback(() => setError(null), [])
  // What's actually executing right now. Tracked client-side because
  // the bridge's `started` SSE event doesn't carry program names — the
  // call site (ProgramPane Run vs TasksPane Run) is the source of truth
  // for whether this is an ad-hoc isolated run or the full tasks.toml
  // schedule. Cleared on stop / SSE `stopped` so we don't leave a stale
  // "running ..." pill after the program exits.
  const [running, setRunning] = useState<RunningInfo>(null)

  const esRef = useRef<EventSource | null>(null)

  // Refs mirroring state that the SSE Mutation handler needs to read
  // *at event time* without retearing the EventSource on every
  // keystroke. Plain `currentPou`/`source` as effect deps would
  // rebuild the connection ~50× per minute while the user types.
  const currentPouRef = useRef<Pou | null>(null)
  const sourceRef = useRef("")
  const selectPouRef = useRef<((path: string) => Promise<void>) | null>(null)
  const displayedProjectRef = useRef<string | null>(null)
  useEffect(() => {
    displayedProjectRef.current = project?.name ?? null
  }, [project?.name])
  useEffect(() => {
    currentPouRef.current = currentPou
  }, [currentPou])
  useEffect(() => {
    sourceRef.current = source
  }, [source])

  const clearProjectState = useCallback(() => {
    displayedProjectRef.current = null
    currentPouRef.current = null
    sourceRef.current = ""
    setProject(null)
    setCurrentPou(null)
    setCurrentDevice(null)
    setCurrentEdge(null)
    setCurrentHmi(null)
    setView(null)
    setSource("")
    setAttached(null)
    setIsRunning(false)
    setRunning(null)
    liveFeedStore.setSnapshot(null)
  }, [])

  // Register the project's own FUNCTION_BLOCKs (e.g. the imported
  // process-control library) so the graphical FBD / LD editors offer
  // them in the block palette alongside the builtins, with pins from
  // each FB's VAR_INPUT / VAR_OUTPUT. Re-runs whenever the POU set
  // changes (import/create/delete a block).
  const fbSignature = project?.pous
    .flatMap((p) =>
      p.declarations
        .filter((d) => d.type === "function_block")
        .map((d) => `${p.path}:${d.name}`),
    )
    .sort()
    .join(",")
  useEffect(() => {
    let cancelled = false
    const tree = project
    if (!tree) {
      setProjectFbs([])
      return
    }
    // One file → its FB declarations; pins come from the file's
    // input/output vars (library convention is one FB per file, so the
    // mapping is exact). Fetch each FB file's variables once.
    const fbFiles = tree.pous.filter((p) =>
      p.declarations.some((d) => d.type === "function_block"),
    )
    ;(async () => {
      const pinsByType = new Map<string, FbPin[]>()
      const pathByType = new Map<string, string>()
      await Promise.all(
        fbFiles.map(async (p) => {
          try {
            const vars = await fetchPouVariables(p.path)
            const pins: FbPin[] = vars
              .filter((v) => v.direction === "input" || v.direction === "output")
              .map((v) => ({
                pin: v.name,
                direction: v.direction === "output" ? "output" : "input",
                type: v.type_name,
                doc: `${v.direction} ${v.type_name}`,
              }))
            for (const d of p.declarations.filter(
              (d) => d.type === "function_block",
            )) {
              pinsByType.set(d.name, pins)
              pathByType.set(d.name, p.path)
            }
          } catch {
            // Leave this FB pinless rather than dropping it — it can
            // still be placed; pins fill in once it parses.
          }
        }),
      )
      if (cancelled) return
      const defs = buildProjectFbDefs(
        [...pinsByType.keys()].map((type) => ({
          type,
          path: pathByType.get(type),
        })),
        (type) => pinsByType.get(type) ?? [],
      )
      setProjectFbs(defs)
    })()
    return () => {
      cancelled = true
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fbSignature])

  // ---------------- Bootstrap ----------------

  const refreshProject = useCallback(async () => {
    try {
      const tree = await fetchProject()
      setProject(tree)
      setProjectEpoch((e) => e + 1)
      // Drop currentPou if the project lost it (e.g. deleted). Read
      // through the ref (kept in sync above) so this callback keeps ONE
      // identity for the provider's lifetime — fifteen-odd CRUD actions
      // depend on it, and a per-POU-switch identity change used to
      // cascade new identities through all of them.
      const cur = currentPouRef.current
      if (tree && cur && !tree.pous.some((p) => p.path === cur.path)) {
        setCurrentPou(null)
        setSource("")
      }
    } catch (e) {
      setError(String(e))
    }
  }, [])

  const refreshProjects = useCallback(async () => {
    try {
      setAvailableProjects(await apiFetchProjects())
    } catch (e) {
      setError(String(e))
    }
  }, [])

  // Initial fetch.
  useEffect(() => {
    ;(async () => {
      try {
        const tree = await fetchProject()
        setProject(tree)
        if (!tree) {
          setAvailableProjects(await apiFetchProjects())
        }
        // Recover running state from the server in case a program was
        // started before this page loaded. Without this, a hard reload
        // would leave the Monitor header blank even though the bridge
        // is happily streaming snapshots, because the SSE `started`
        // event already fired and we missed it.
        const status = await fetchRuntimeStatus()
        if (status.running) {
          setIsRunning(true)
          if (status.running_info) {
            const info = status.running_info
            if (info.kind === "isolated") {
              setRunning({
                kind: "isolated",
                program: info.program,
                filePath: info.file_path,
              })
            } else {
              setRunning({ kind: "scheduled", programs: info.programs })
            }
          }
        }
      } catch (e) {
        setError(String(e))
      } finally {
        setProjectLoading(false)
      }
    })()
  }, [])

  // Auto-select the first POU when a project loads and nothing is open yet.
  useEffect(() => {
    if (!project || view !== null) return
    if (project.pous.length === 0) return
    void selectPou(project.pous[0].path)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [project])

  // Keep the iomap + tasks state in sync with the tree.
  useEffect(() => {
    if (project) {
      setIomap(project.iomap)
      setTasks(project.tasks)
    }
  }, [project])

  // ---------------- SSE source ----------------
  //
  // Two modes:
  //  (a) Local — subscribe to /api/events, which carries AppEvent JSON.
  //  (b) Attached to an edge — subscribe to the SSH-forwarded edge runtime
  //      at http://127.0.0.1:<localPort>/events, which streams bare
  //      VarSnapshot JSON. We synthesize "started" / "stopped" from the
  //      tunnel coming up and going down.
  //
  // The effect tears down and rebuilds the EventSource whenever the
  // attachment state changes, so existing MonitorPane / VariablesPanel
  // consumers of `lastSnapshot` keep working without knowing about edges.

  useEffect(() => {
    const url = attached
      ? `http://127.0.0.1:${attached.localPort}/events`
      : eventsUrl()
    const es = new EventSource(url)
    esRef.current = es
    es.onopen = () => {
      liveFeedStore.setConnected(true)
      if (attached) {
        // Edge runtime has no "started" event — the tunnel coming up is
        // good enough; the program has been running on the edge for a
        // while already.
        setIsRunning(true)
        setRunning({ kind: "remote", edge: attached.name })
        setError(null)
      }
    }
    es.onerror = () => liveFeedStore.setConnected(false)
    es.onmessage = (msg) => {
      try {
        if (attached) {
          // Edge runtime: payload is a VarSnapshot directly.
          const snap = JSON.parse(msg.data) as VarSnapshot
          liveFeedStore.setSnapshot(snap)
        } else {
          const ev = JSON.parse(msg.data) as AppEvent
          switch (ev.type) {
            case "snapshot":
              liveFeedStore.setSnapshot(ev.data)
              break
            case "started":
              setIsRunning(true)
              setError(null)
              break
            case "stopped":
              setIsRunning(false)
              setRunning(null)
              break
            case "error":
              setError(ev.data)
              break
            case "mutation":
              handleMutationEvent(
                ev.data,
                currentPouRef,
                sourceRef,
                selectPouRef,
                displayedProjectRef.current,
                () => {
                  clearProjectState()
                  void refreshProjects()
                },
              )
              break
            // NOTE: `agent_activity` is intentionally NOT handled here.
            // It's an app-global concern handled by a dedicated, always-on
            // /api/events subscription (see the effect below) so the
            // takeover overlay keeps working even while this stream is
            // repointed at an edge runtime during Debug/attach.
          }
        }
      } catch {
        /* ignore */
      }
    }
    return () => {
      es.close()
      esRef.current = null
    }
  }, [attached, clearProjectState, refreshProjects])

  // Dedicated, always-on /api/events subscription for agent-takeover
  // activity. The main stream above gets repointed at an edge runtime
  // while attached (Edge → Debug), which would otherwise starve the
  // takeover overlay of `agent_activity`. This one never switches, so
  // the overlay reflects reality regardless of edge attach. (The server
  // replays the current agent_activity on connect, so it self-heals on
  // reconnect too.)
  useEffect(() => {
    const es = new EventSource(eventsUrl())
    es.onmessage = (msg) => {
      try {
        const ev = JSON.parse(msg.data) as AppEvent
        if (ev.type === "agent_activity") {
          agentActivityStore.ingest({
            active: ev.data.active,
            command: ev.data.command,
            session: ev.data.session,
            session_label: ev.data.session_label,
          })
        }
      } catch {
        /* ignore malformed frames */
      }
    }
    return () => es.close()
  }, [])

  // ---------------- Editor diagnostics (ST) ----------------

  // One LSP client per opened POU — it still powers Monaco's semantic
  // tokens / symbols, but its publishDiagnostics are IGNORED: the LSP
  // sees one file in isolation, so any reference to a FUNCTION_BLOCK
  // declared in a sibling file (i.e. every library call) would
  // false-positive P2008. The project-aware `/api/check` below is the
  // diagnostics source instead, same as the graphical editors.
  const lspRef = useRef<LspClient | null>(null)
  useEffect(() => {
    if (!currentPou) {
      setDiagnostics([])
      lspRef.current?.dispose()
      lspRef.current = null
      return
    }
    const client = new LspClient({
      uri: pouDocumentUri(currentPou.path),
      languageId: "iec61131",
      onDiagnostics: () => {},
    })
    lspRef.current = client
    return () => {
      client.dispose()
      lspRef.current = null
    }
  }, [currentPou?.path])

  useEffect(() => {
    lspRef.current?.setSource(source)
  }, [source])

  // Project-aware squiggles for ST buffers, 350 ms debounced (the
  // graphical editors run their own identical loop with their JSON
  // languages — see FBDEditor/LDEditor/SFCEditor).
  const currentLanguage = currentPou?.declarations[0]?.language ?? "st"
  useEffect(() => {
    if (!currentPou || currentLanguage !== "st") {
      setDiagnostics([])
      return
    }
    const path = currentPou.path
    let cancelled = false
    const handle = setTimeout(async () => {
      try {
        const diags = await checkProgram(source, "st", path)
        if (!cancelled) setDiagnostics(diags)
      } catch (e) {
        console.warn("ST diagnostics fetch failed:", e)
      }
    }, 350)
    return () => {
      cancelled = true
      clearTimeout(handle)
    }
  }, [source, currentPou?.path, currentLanguage, projectEpoch])

  // ---------------- Invalidation bus subscriptions ----------------
  //
  // The SSE message handler emits to `invalidationBus` on every
  // server-side `Mutation` event. Here we wire each topic to the
  // matching refetch in this provider. Effects run once (mount) and
  // rely on stable refs for the actual fetchers so we don't
  // re-subscribe on every state update.

  const refreshProjectRef = useRef<() => Promise<void>>(() => Promise.resolve())
  useEffect(() => {
    refreshProjectRef.current = refreshProject
  }, [refreshProject])

  useEffect(() => {
    const subs = [
      invalidationBus.subscribe(Topic.PROJECT, () => {
        void refreshProjectRef.current()
      }),
      invalidationBus.subscribe(Topic.PROJECT_META, () => {
        // Re-load both the open project state AND the available list
        // (a project_created event from another client should show
        // up in the picker).
        void refreshProjectRef.current()
        void apiFetchProjects()
          .then(setAvailableProjects)
          .catch(() => {})
      }),
      invalidationBus.subscribe(Topic.IOMAP, () => {
        void fetchIomap().then(setIomap).catch(() => {})
      }),
      invalidationBus.subscribe(Topic.TASKS, () => {
        void fetchTasks().then(setTasks).catch(() => {})
      }),
      invalidationBus.subscribe(Topic.DEVICES, () => {
        // Devices show up in the project tree; refresh that. Per-
        // device editor refetches via the device:<name> topic.
        void refreshProjectRef.current()
      }),
      invalidationBus.subscribe(Topic.EDGES, () => {
        void refreshProjectRef.current()
      }),
    ]
    return () => {
      subs.forEach((u) => u())
    }
  }, [])

  // Per-POU live-reload. Subscribes to `pou:<currentPou.path>` only
  // for as long as the editor is on that POU; tears down when the
  // user switches POUs. Solves the race where an auto-jump's
  // in-flight fetchPou is still resolving when the agent's
  // `pou_updated` mutation arrives — the subscription is established
  // *after* setCurrentPou completes, so the refetch always sees the
  // newest source on disk. If the local buffer has unsaved edits
  // we deliberately skip the silent refetch (the SSE handler will
  // surface a Reload toast instead).
  useEffect(() => {
    const path = currentPou?.path
    if (!path) return
    return invalidationBus.subscribe(Topic.pou(path), () => {
      // Re-read the current dirty state via refs to avoid stale
      // closure values — `currentPou` and `source` snapshot at
      // subscribe time would mis-classify rapid edits.
      const cur = currentPouRef.current
      if (!cur || cur.path !== path) return
      if (sourceRef.current !== cur.source) return  // dirty — let toast handle it
      void fetchPou(path)
        .then((fresh) => {
          if (currentPouRef.current?.path !== path) return  // user moved away
          setCurrentPou(fresh)
          setSource(fresh.source)
          pouSpawnStore.bump()
        })
        .catch(() => {})
    })
  }, [currentPou?.path])

  // Per-device / per-edge live-reload, mirroring the per-POU pattern
  // above: while an editor pane is open on it, an agent-side `cs device
  // set` / `cs set edges/…` refreshes the pane through the `device:<name>`
  // / `edge:<name>` mutation topics the server already emits. Without
  // this the pane kept showing the select-time snapshot.
  useEffect(() => {
    const name = currentDevice?.name
    if (!name) return
    return invalidationBus.subscribe(Topic.device(name), () => {
      void fetchDevice(name)
        .then((fresh) => setCurrentDevice((cur) => (cur?.name === name ? fresh : cur)))
        .catch(() => {})
    })
  }, [currentDevice?.name])

  useEffect(() => {
    const name = currentEdge?.name
    if (!name) return
    return invalidationBus.subscribe(Topic.edge(name), () => {
      void fetchEdge(name)
        .then((fresh) => setCurrentEdge((cur) => (cur?.name === name ? fresh : cur)))
        .catch(() => {})
    })
  }, [currentEdge?.name])

  // ---------------- Project actions ----------------

  const createProject = useCallback(async (name: string): Promise<boolean> => {
    setError(null)
    try {
      const info = await apiCreateProject(name)
      selectWindowProject(info.name)
      const tree = await fetchProject()
      setProject(tree)
      setCurrentPou(null)
      setSource("")
      return true
    } catch (e) {
      setError(String(e))
      return false
    }
  }, [])

  const openProject = useCallback(async (path: string) => {
    setError(null)
    try {
      const info = await apiOpenProject(path)
      selectWindowProject(info.name)
      const tree = await fetchProject()
      setProject(tree)
      setCurrentPou(null)
      setSource("")
    } catch (e) {
      setError(String(e))
    }
  }, [])

  const closeProject = useCallback(async () => {
    setError(null)
    try {
      await apiCloseProject()
      clearProjectState()
      setAvailableProjects(await apiFetchProjects())
    } catch (e) {
      setError(String(e))
    }
  }, [clearProjectState])

  // ---------------- POU / Device actions ----------------

  const selectPou = useCallback(async (path: string) => {
    setError(null)
    try {
      const pou = await fetchPou(path)
      setCurrentPou(pou)
      setSource(pou.source)
      setView("app")
    } catch (e) {
      setError(String(e))
    }
  }, [])

  // Mirror selectPou into the ref so the SSE Mutation handler (which
  // lives outside React's render scope) can call the latest version.
  useEffect(() => {
    selectPouRef.current = selectPou
  }, [selectPou])

  const selectDevice = useCallback(async (name: string) => {
    setError(null)
    try {
      const device = await fetchDevice(name)
      setCurrentDevice(device)
      setView("device")
    } catch (e) {
      setError(String(e))
    }
  }, [])

  const selectHmi = useCallback(async (path: string) => {
    setError(null)
    setCurrentHmi(path)
    setView("hmi")
  }, [])

  const selectEdge = useCallback(async (name: string) => {
    setError(null)
    try {
      const edge = await fetchEdge(name)
      setCurrentEdge(edge)
      setView("edge")
    } catch (e) {
      setError(String(e))
    }
  }, [])

  const openIoMap = useCallback(async () => {
    setError(null)
    try {
      const m = await fetchIomap()
      setIomap(m)
      setView("iomap")
    } catch (e) {
      setError(String(e))
    }
  }, [])

  const openTasks = useCallback(async () => {
    setError(null)
    setView("tasks")
  }, [])

  const saveDevice = useCallback(
    async (device: Device) => {
      setError(null)
      try {
        await apiUpdateDevice(device.name, device)
        setCurrentDevice(device)
        await refreshProject()
      } catch (e) {
        setError(String(e))
      }
    },
    [refreshProject],
  )

  const saveIomap = useCallback(
    async (next: IoMap) => {
      setError(null)
      try {
        await apiUpdateIomap(next)
        setIomap(next)
        await refreshProject()
      } catch (e) {
        setError(String(e))
      }
    },
    [refreshProject],
  )

  const saveTasks = useCallback(
    async (next: Tasks) => {
      setError(null)
      try {
        await apiUpdateTasks(next)
        setTasks(next)
        await refreshProject()
      } catch (e) {
        setError(String(e))
      }
    },
    [refreshProject],
  )

  const migrateTasks = useCallback(async (): Promise<MigrationResponse> => {
    setError(null)
    try {
      const report = await apiMigrateTasks()
      await refreshProject()
      return report
    } catch (e) {
      setError(String(e))
      return {
        migrated: false,
        tasks_count: 0,
        programs_count: 0,
        pous_modified: [],
      }
    }
  }, [refreshProject])

  const saveCurrentPou = useCallback(async () => {
    if (!currentPou) return
    try {
      await savePou(currentPou.path, source)
      setCurrentPou({ ...currentPou, source })
    } catch (e) {
      setError(String(e))
    }
  }, [currentPou, source])

  const createPou = useCallback(
    async (
      path: string,
      type_: PouType,
      language: PouLanguage = "st",
    ): Promise<boolean> => {
      setError(null)
      try {
        const pou = await apiCreatePou(path, type_, language)
        await refreshProject()
        setCurrentPou(pou)
        setSource(pou.source)
        return true
      } catch (e) {
        setError(String(e))
        return false
      }
    },
    [refreshProject],
  )

  const deletePou = useCallback(
    async (path: string) => {
      setError(null)
      try {
        await apiDeletePou(path)
        if (currentPou?.path === path) {
          setCurrentPou(null)
          setSource("")
        }
        await refreshProject()
      } catch (e) {
        setError(String(e))
      }
    },
    [currentPou, refreshProject],
  )

  const createDevice = useCallback(
    async (name: string, protocol: Protocol): Promise<boolean> => {
      setError(null)
      try {
        await apiCreateDevice(name, protocol)
        await refreshProject()
        return true
      } catch (e) {
        setError(String(e))
        return false
      }
    },
    [refreshProject],
  )

  const deleteDevice = useCallback(
    async (name: string) => {
      setError(null)
      try {
        await apiDeleteDevice(name)
        await refreshProject()
      } catch (e) {
        setError(String(e))
      }
    },
    [refreshProject],
  )

  const createPouFolder = useCallback(
    async (path: string): Promise<boolean> => {
      setError(null)
      try {
        await apiCreatePouFolder(path)
        await refreshProject()
        return true
      } catch (e) {
        setError(String(e))
        return false
      }
    },
    [refreshProject],
  )

  const createDeviceFolderCb = useCallback(
    async (path: string): Promise<boolean> => {
      setError(null)
      try {
        await apiCreateDeviceFolder(path)
        await refreshProject()
        return true
      } catch (e) {
        setError(String(e))
        return false
      }
    },
    [refreshProject],
  )

  // ---------------- Edge actions ----------------

  const createEdgeAction = useCallback(
    async (name: string, host: string): Promise<boolean> => {
      setError(null)
      try {
        await apiCreateEdge(name, host)
        await refreshProject()
        return true
      } catch (e) {
        setError(String(e))
        return false
      }
    },
    [refreshProject],
  )

  const deleteEdgeAction = useCallback(
    async (name: string) => {
      setError(null)
      try {
        await apiDeleteEdge(name)
        if (currentEdge?.name === name) setCurrentEdge(null)
        if (attached?.name === name) setAttached(null)
        await refreshProject()
      } catch (e) {
        setError(String(e))
      }
    },
    [currentEdge, attached, refreshProject],
  )

  const saveEdge = useCallback(
    async (edge: Edge) => {
      setError(null)
      try {
        await apiUpdateEdge(edge.name, edge)
        setCurrentEdge(edge)
        await refreshProject()
      } catch (e) {
        setError(String(e))
      }
    },
    [refreshProject],
  )

  const createEdgeFolderAction = useCallback(
    async (path: string): Promise<boolean> => {
      setError(null)
      try {
        await apiCreateEdgeFolder(path)
        await refreshProject()
        return true
      } catch (e) {
        setError(String(e))
        return false
      }
    },
    [refreshProject],
  )

  const attachEdgeAction = useCallback(async (name: string) => {
    setError(null)
    try {
      // If something's already attached, detach it first so we don't leak
      // tunnels server-side.
      await apiDetachEdge(name).catch(() => {})
      const info = await apiAttachEdge(name)
      setAttached({ name, localPort: info.local_port })
    } catch (e) {
      setError(String(e))
    }
  }, [])

  const detachEdgeAction = useCallback(async () => {
    if (!attached) return
    try {
      await apiDetachEdge(attached.name)
    } catch (e) {
      setError(String(e))
    }
    setAttached(null)
    // Detaching means we're back on the local bridge, which may or may
    // not be running. Until a "started" comes through, no running info.
    setRunning(null)
    setIsRunning(false)
  }, [attached])

  // ---------------- Run / Stop ----------------

  const run = useCallback(
    async (program?: string, file_path?: string) => {
      setError(null)
      try {
        // Save the currently-open POU first if it's dirty — the runtime
        // re-reads the project from disk on compile, so unsaved edits
        // would otherwise be silently ignored.
        if (currentPou && source !== currentPou.source) {
          await savePou(currentPou.path, source)
          setCurrentPou({ ...currentPou, source })
        }
        await runProgram(program, file_path)
        // Record what we just kicked off so the Monitor header can
        // label the snapshots. Three cases:
        //   - program + file_path -> ProgramPane isolated run
        //   - program only        -> project-level run with a chosen
        //     PROGRAM (treat as isolated for labelling — Monitor will
        //     show that one name)
        //   - neither             -> full tasks.toml schedule
        if (program && file_path) {
          setRunning({ kind: "isolated", program, filePath: file_path })
        } else if (program) {
          setRunning({ kind: "isolated", program, filePath: program })
        } else {
          setRunning({
            kind: "scheduled",
            programs: tasks.programs.map((p) => p.program),
          })
        }
      } catch (e) {
        setError(String(e))
      }
    },
    [currentPou, source, tasks],
  )

  const stop = useCallback(async () => {
    try {
      await stopProgram()
      // Clear immediately for snappy UI; the SSE `stopped` event will
      // also clear, but the round-trip is several hundred ms.
      setRunning(null)
    } catch (e) {
      setError(String(e))
    }
  }, [])

  const isDirty = !!currentPou && currentPou.source !== source

  // Memoised: an inline literal would be a NEW object on every one of
  // the provider's setState calls, re-rendering all consumers whether or
  // not the fields they read changed. With the high-frequency feed moved
  // to `live-feed.ts` and the actions holding stable identities, this
  // now changes only when actual context data changes.
  const value = useMemo(
    () => ({
      project,
      projectLoading,
      availableProjects,
      view,
      currentPou,
      source,
      setSource,
      isDirty,
      diagnostics,
      projectEpoch,
      currentDevice,
      currentHmi,
      currentEdge,
      attached,
      iomap,
      tasks,
      isRunning,
      running,
      error,
      clearError,
      createProject,
      openProject,
      closeProject,
      refreshProjects,
      refreshProject,
      selectPou,
      selectDevice,
      selectHmi,
      selectEdge,
      openIoMap,
      openTasks,
      saveCurrentPou,
      createPou,
      deletePou,
      createDevice,
      deleteDevice,
      createPouFolder,
      createDeviceFolder: createDeviceFolderCb,
      createEdge: createEdgeAction,
      deleteEdge: deleteEdgeAction,
      saveEdge,
      createEdgeFolder: createEdgeFolderAction,
      attachEdge: attachEdgeAction,
      detachEdge: detachEdgeAction,
      saveTasks,
      migrateTasks,
      saveDevice,
      saveIomap,
      run,
      stop,
    }),
    [
      project,
      projectLoading,
      availableProjects,
      view,
      currentPou,
      source,
      setSource,
      isDirty,
      diagnostics,
      projectEpoch,
      currentDevice,
      currentHmi,
      currentEdge,
      attached,
      iomap,
      tasks,
      isRunning,
      running,
      error,
      clearError,
      createProject,
      openProject,
      closeProject,
      refreshProjects,
      refreshProject,
      selectPou,
      selectDevice,
      selectHmi,
      selectEdge,
      openIoMap,
      openTasks,
      saveCurrentPou,
      createPou,
      deletePou,
      createDevice,
      deleteDevice,
      createPouFolder,
      createDeviceFolderCb,
      createEdgeAction,
      deleteEdgeAction,
      saveEdge,
      createEdgeFolderAction,
      attachEdgeAction,
      detachEdgeAction,
      saveTasks,
      migrateTasks,
      saveDevice,
      saveIomap,
      run,
      stop,
    ],
  )

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>
}

export function useRuntime() {
  const ctx = useContext(Ctx)
  if (!ctx) throw new Error("useRuntime must be used inside RuntimeProvider")
  return ctx
}
