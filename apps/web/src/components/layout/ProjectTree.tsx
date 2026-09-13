import { useMemo, useState } from "react"
import {
  ChevronDown,
  ChevronRight,
  Cpu,
  FileCode2,
  Folder,
  FolderOpen,
  FolderPlus,
  Library,
  Network,
  Plus,
  Server,
} from "@/components/ui/icons"

import { ImportLibraryDialog } from "@/components/dialogs/ImportLibraryDialog"
import { NewDeviceDialog } from "@/components/dialogs/NewDeviceDialog"
import { NewEdgeDialog } from "@/components/dialogs/NewEdgeDialog"
import { NewFolderDialog } from "@/components/dialogs/NewFolderDialog"
import { NewPouDialog } from "@/components/dialogs/NewPouDialog"
import { importLibrary, removeLibrary } from "@/lib/api"
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from "@/components/ui/context-menu"
import { cn } from "@/lib/utils"
import { buildTree, type TreeNode } from "@/lib/project-tree"
import { useRuntime } from "@/state/runtime"
import { HmiSection } from "./HmiSection"
import type { PouType } from "@/types/generated/PouType"
import type { PouFile } from "@/types/generated/PouFile"
import type { Device } from "@/types/generated/Device"
import type { Edge } from "@/types/generated/Edge"
import type { Protocol } from "@/types/generated/Protocol"

export function ProjectTree() {
  const {
    project,
    view,
    currentPou,
    currentDevice,
    currentEdge,
    attached,
    selectPou,
    selectDevice,
    selectEdge,
    openIoMap,
    openTasks,
    deletePou,
    deleteDevice,
    deleteEdge,
  } = useRuntime()

  // Section open/close. Persists across re-renders so toggling a sibling
  // folder doesn't collapse the section.
  const [appsOpen, setAppsOpen] = useState(true)
  const [librariesOpen, setLibrariesOpen] = useState(true)
  const [devicesOpen, setDevicesOpen] = useState(true)
  const [edgesOpen, setEdgesOpen] = useState(true)
  const [importDialogOpen, setImportDialogOpen] = useState(false)

  // Folder open-state keyed by `section/path` so each section is independent.
  const [openFolders, setOpenFolders] = useState<Record<string, boolean>>({})
  const toggleFolder = (key: string) =>
    setOpenFolders((prev) => ({ ...prev, [key]: !(prev[key] ?? true) }))

  // Controlled NewFolder / NewPou / NewDevice dialogs — opened from the
  // folder context menu with a known parent path.
  const [folderDialog, setFolderDialog] = useState<{
    section: "applications" | "devices" | "edges"
    parent: string
  } | null>(null)
  const [pouDialog, setPouDialog] = useState<{ parent: string } | null>(null)
  const [deviceDialog, setDeviceDialog] = useState<{ parent: string } | null>(
    null,
  )
  const [edgeDialog, setEdgeDialog] = useState<{ parent: string } | null>(null)

  // Imported library blocks live under the reserved `lib/<library>/`
  // POU subtree. They're project POUs as far as compile/deploy are
  // concerned, but the tree presents them as a separate read-only
  // "Libraries" section so they don't drown the user's own programs.
  const { ownPous, ownPouFolders, libraryGroups } = useMemo(() => {
    const pous = project?.pous ?? []
    const folders = project?.pou_folders ?? []
    const own = pous.filter((p) => !p.path.startsWith("lib/"))
    const groups = new Map<string, PouFile[]>()
    for (const p of pous) {
      if (!p.path.startsWith("lib/")) continue
      const lib = p.path.split("/")[1] ?? ""
      if (!groups.has(lib)) groups.set(lib, [])
      groups.get(lib)!.push(p)
    }
    for (const blocks of groups.values()) {
      blocks.sort((a, b) => a.path.localeCompare(b.path))
    }
    return {
      ownPous: own,
      ownPouFolders: folders.filter(
        (f) => f !== "lib" && !f.startsWith("lib/"),
      ),
      libraryGroups: [...groups.entries()].sort(([a], [b]) =>
        a.localeCompare(b),
      ),
    }
  }, [project])

  const pouTree = useMemo(
    () =>
      project
        ? buildTree<PouFile>(ownPous, ownPouFolders, (p) => p.path)
        : [],
    [project, ownPous, ownPouFolders],
  )

  const deviceTree = useMemo(
    () =>
      project
        ? buildTree<Device>(project.devices, project.device_folders, (d) => d.name)
        : [],
    [project],
  )

  const edgeTree = useMemo(
    () =>
      project
        ? buildTree<Edge>(project.edges, project.edge_folders, (e) => e.name)
        : [],
    [project],
  )

  if (!project) return null

  return (
    <div className="py-1 text-[13px]">
      <SectionHeader
        label="POUs"
        open={appsOpen}
        count={ownPous.length}
        onToggle={() => setAppsOpen(!appsOpen)}
        action={
          <SectionActions
            onAddItem={() => setPouDialog({ parent: "" })}
            onAddFolder={() =>
              setFolderDialog({ section: "applications", parent: "" })
            }
            itemTitle="New POU"
            folderTitle="New folder"
          />
        }
      />
      {appsOpen && (
        <TreeChildren
          nodes={pouTree}
          depth={0}
          renderItem={(node) => (
            <PouItem
              node={node}
              active={view === "app" && currentPou?.path === node.path}
              onOpen={() => selectPou(node.path)}
              onDelete={() => {
                if (confirm(`Delete POU file "${node.path}.st"?`)) {
                  void deletePou(node.path)
                }
              }}
            />
          )}
          folderContextMenu={(folder) => (
            <>
              <ContextMenuItem
                onSelect={() => setPouDialog({ parent: folder.path })}
              >
                New POU here…
              </ContextMenuItem>
              <ContextMenuItem
                onSelect={() =>
                  setFolderDialog({
                    section: "applications",
                    parent: folder.path,
                  })
                }
              >
                New folder here…
              </ContextMenuItem>
            </>
          )}
          openFolders={openFolders}
          toggleFolder={toggleFolder}
          section="apps"
          emptyHint={
            ownPous.length === 0 && ownPouFolders.length === 0
              ? "No POUs yet — click + to create one."
              : null
          }
        />
      )}

      <SectionHeader
        label="Libraries"
        open={librariesOpen}
        count={libraryGroups.length}
        onToggle={() => setLibrariesOpen(!librariesOpen)}
        action={
          <button
            type="button"
            title="Import library blocks…"
            aria-label="Import library blocks"
            onClick={(e) => {
              e.stopPropagation()
              setImportDialogOpen(true)
            }}
            className="flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring"
          >
            <Plus className="size-3.5" />
          </button>
        }
      />
      {librariesOpen && (
        <>
          {libraryGroups.length === 0 && (
            <div
              className="px-2 py-2 text-xs leading-relaxed text-muted-foreground"
              style={{ paddingLeft: pad(1) }}
            >
              No libraries imported — click + to browse.
            </div>
          )}
          {libraryGroups.map(([lib, blocks]) => (
            <LibraryGroup
              key={lib}
              name={lib}
              blocks={blocks}
              open={openFolders[`libs/${lib}`] ?? true}
              onToggle={() => toggleFolder(`libs/${lib}`)}
              activePath={view === "app" ? currentPou?.path ?? null : null}
              onOpenBlock={(path) => selectPou(path)}
              onUpdate={() => {
                void importLibrary(lib).catch((e) =>
                  alert(`Update failed: ${e}`),
                )
              }}
              onRemove={() => {
                if (
                  confirm(
                    `Remove library "${lib}" (${blocks.length} block file(s)) from this project?\nPOUs that call its blocks will stop compiling until you re-import.`,
                  )
                ) {
                  void removeLibrary(lib).catch((e) =>
                    alert(`Remove failed: ${e}`),
                  )
                }
              }}
            />
          ))}
        </>
      )}

      <SectionHeader
        label="Devices"
        open={devicesOpen}
        count={project.devices.length}
        onToggle={() => setDevicesOpen(!devicesOpen)}
        action={
          <SectionActions
            onAddItem={() => setDeviceDialog({ parent: "" })}
            onAddFolder={() =>
              setFolderDialog({ section: "devices", parent: "" })
            }
            itemTitle="New device"
            folderTitle="New folder"
          />
        }
      />
      {devicesOpen && (
        <TreeChildren
          nodes={deviceTree}
          depth={0}
          renderItem={(node) => (
            <DeviceItem
              node={node}
              active={
                view === "device" && currentDevice?.name === node.path
              }
              onOpen={() => selectDevice(node.path)}
              onDelete={() => {
                if (confirm(`Delete device "${node.path}"?`)) {
                  void deleteDevice(node.path)
                }
              }}
            />
          )}
          folderContextMenu={(folder) => (
            <>
              <ContextMenuItem
                onSelect={() => setDeviceDialog({ parent: folder.path })}
              >
                New device here…
              </ContextMenuItem>
              <ContextMenuItem
                onSelect={() =>
                  setFolderDialog({
                    section: "devices",
                    parent: folder.path,
                  })
                }
              >
                New folder here…
              </ContextMenuItem>
            </>
          )}
          openFolders={openFolders}
          toggleFolder={toggleFolder}
          section="devices"
          emptyHint={
            project.devices.length === 0 && project.device_folders.length === 0
              ? "No devices yet — click + to add one."
              : null
          }
        />
      )}

      <SectionHeader
        label="Edges"
        open={edgesOpen}
        count={project.edges.length}
        onToggle={() => setEdgesOpen(!edgesOpen)}
        action={
          <SectionActions
            onAddItem={() => setEdgeDialog({ parent: "" })}
            onAddFolder={() =>
              setFolderDialog({ section: "edges", parent: "" })
            }
            itemTitle="New edge"
            folderTitle="New folder"
          />
        }
      />
      {edgesOpen && (
        <TreeChildren
          nodes={edgeTree}
          depth={0}
          renderItem={(node) => (
            <EdgeItem
              node={node}
              active={view === "edge" && currentEdge?.name === node.path}
              attached={attached?.name === node.path}
              onOpen={() => selectEdge(node.path)}
              onDelete={() => {
                if (confirm(`Delete edge "${node.path}"?`)) {
                  void deleteEdge(node.path)
                }
              }}
            />
          )}
          folderContextMenu={(folder) => (
            <>
              <ContextMenuItem
                onSelect={() => setEdgeDialog({ parent: folder.path })}
              >
                New edge here…
              </ContextMenuItem>
              <ContextMenuItem
                onSelect={() =>
                  setFolderDialog({ section: "edges", parent: folder.path })
                }
              >
                New folder here…
              </ContextMenuItem>
            </>
          )}
          openFolders={openFolders}
          toggleFolder={toggleFolder}
          section="edges"
          emptyHint={
            project.edges.length === 0 && project.edge_folders.length === 0
              ? "No edges yet — click + to add a deploy target."
              : null
          }
        />
      )}

      <HmiSection />

      {/* Tasks and IO Mapping are project-level singletons (one
          tasks.toml and one iomap.toml per project), so they don't
          carry the toggle / "+ item" / "+ folder" affordances the
          three sections above do. But they ARE the same kind of
          first-class project content as POUs / Devices / Edges, so
          their header row sits at the same visual weight. */}
      <SingletonSectionHeader
        label="Tasks"
        count={`${project.tasks.tasks.length}·${project.tasks.programs.length}`}
        countTitle={`${project.tasks.tasks.length} tasks · ${project.tasks.programs.length} program bindings`}
        active={view === "tasks"}
        onOpen={() => void openTasks()}
      />

      <SingletonSectionHeader
        label="IO Mapping"
        count={project.iomap.mappings.length}
        countTitle={`${project.iomap.mappings.length} variable ↔ channel bindings`}
        active={view === "iomap"}
        onOpen={() => void openIoMap()}
      />

      {/* Controlled dialogs popped open by folder context menus. */}
      {folderDialog && (
        <NewFolderDialog
          open
          onOpenChange={(o) => {
            if (!o) setFolderDialog(null)
          }}
          section={folderDialog.section}
          parent={folderDialog.parent}
        />
      )}
      {pouDialog && (
        <NewPouDialog
          open
          onOpenChange={(o) => {
            if (!o) setPouDialog(null)
          }}
          parent={pouDialog.parent}
        />
      )}
      {deviceDialog && (
        <NewDeviceDialog
          open
          onOpenChange={(o) => {
            if (!o) setDeviceDialog(null)
          }}
          parent={deviceDialog.parent}
        />
      )}
      {edgeDialog && (
        <NewEdgeDialog
          open
          onOpenChange={(o) => {
            if (!o) setEdgeDialog(null)
          }}
          parent={edgeDialog.parent}
        />
      )}
      {importDialogOpen && (
        <ImportLibraryDialog
          open
          onOpenChange={(o) => {
            if (!o) setImportDialogOpen(false)
          }}
        />
      )}
    </div>
  )
}

/// One imported library in the Libraries section: a folder-style header
/// row (name + block count) with Update / Remove in its context menu,
/// and the block files as read-only POU rows beneath.
function LibraryGroup({
  name,
  blocks,
  open,
  onToggle,
  activePath,
  onOpenBlock,
  onUpdate,
  onRemove,
}: {
  name: string
  blocks: PouFile[]
  open: boolean
  onToggle: () => void
  activePath: string | null
  onOpenBlock: (path: string) => void
  onUpdate: () => void
  onRemove: () => void
}) {
  return (
    <div>
      <ContextMenu>
        <ContextMenuTrigger asChild>
          <button
            type="button"
            onClick={onToggle}
            aria-expanded={open}
            className="flex h-8 w-full min-w-0 items-center gap-1.5 pr-2 text-left text-[13px] text-muted-foreground transition-colors hover:bg-accent/50 hover:text-foreground focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring"
            style={{ paddingLeft: pad(0) }}
          >
            {open ? (
              <ChevronDown className="size-3 shrink-0" />
            ) : (
              <ChevronRight className="size-3 shrink-0" />
            )}
            <Library className="size-3.5 shrink-0 text-muted-foreground" />
            <span className="min-w-0 flex-1 truncate text-foreground" title={name}>
              {name}
            </span>
            <span className="shrink-0 text-xs text-muted-foreground">
              {blocks.length} blocks
            </span>
          </button>
        </ContextMenuTrigger>
        <ContextMenuContent>
          <ContextMenuItem onSelect={onUpdate}>
            Update (re-import all)
          </ContextMenuItem>
          <ContextMenuSeparator />
          <ContextMenuItem variant="destructive" onSelect={onRemove}>
            Remove library…
          </ContextMenuItem>
        </ContextMenuContent>
      </ContextMenu>
      {open && (
        <div>
          {blocks.map((b) => {
            const leaf = b.path.split("/").pop() ?? b.path
            return (
              <div key={b.path} style={{ paddingLeft: pad(1) }}>
                <PouItem
                  node={{ name: leaf, path: b.path, item: b }}
                  active={activePath === b.path}
                  onOpen={() => onOpenBlock(b.path)}
                  onDelete={() => {}}
                  readOnly
                />
              </div>
            )
          })}
        </div>
      )}
    </div>
  )
}

// ============================================================
//  Tree internals
// ============================================================

function TreeChildren<T>({
  nodes,
  depth,
  renderItem,
  folderContextMenu,
  openFolders,
  toggleFolder,
  section,
  emptyHint,
}: {
  nodes: TreeNode<T>[]
  depth: number
  renderItem: (node: Extract<TreeNode<T>, { kind: "item" }>) => React.ReactNode
  folderContextMenu: (
    folder: Extract<TreeNode<T>, { kind: "folder" }>,
  ) => React.ReactNode
  openFolders: Record<string, boolean>
  toggleFolder: (key: string) => void
  section: string
  emptyHint?: string | null
}) {
  if (nodes.length === 0) {
    return emptyHint ? (
      <div
        className="px-2 py-2 text-xs leading-relaxed text-muted-foreground"
        style={{ paddingLeft: pad(depth + 1) }}
      >
        {emptyHint}
      </div>
    ) : null
  }

  return (
    <>
      {nodes.map((node) => {
        if (node.kind === "folder") {
          const key = `${section}/${node.path}`
          const isOpen = openFolders[key] ?? true
          return (
            <div key={key}>
              <ContextMenu>
                <ContextMenuTrigger asChild>
                  <button
                    type="button"
                    onClick={() => toggleFolder(key)}
                    aria-expanded={isOpen}
                    className="flex h-8 w-full min-w-0 items-center gap-1.5 pr-2 text-left text-[13px] text-muted-foreground transition-colors hover:bg-accent/50 hover:text-foreground focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring"
                    style={{ paddingLeft: pad(depth) }}
                  >
                    {isOpen ? (
                      <ChevronDown className="size-3 shrink-0" />
                    ) : (
                      <ChevronRight className="size-3 shrink-0" />
                    )}
                    {isOpen ? (
                      <FolderOpen className="size-3.5 shrink-0 text-muted-foreground" />
                    ) : (
                      <Folder className="size-3.5 shrink-0 text-muted-foreground" />
                    )}
                    <span
                      className="min-w-0 flex-1 truncate text-foreground"
                      title={node.name}
                    >
                      {node.name}
                    </span>
                  </button>
                </ContextMenuTrigger>
                <ContextMenuContent>
                  {folderContextMenu(node)}
                </ContextMenuContent>
              </ContextMenu>
              {isOpen && (
                <TreeChildren
                  nodes={node.children}
                  depth={depth + 1}
                  renderItem={renderItem}
                  folderContextMenu={folderContextMenu}
                  openFolders={openFolders}
                  toggleFolder={toggleFolder}
                  section={section}
                />
              )}
            </div>
          )
        }
        return (
          <div
            key={`${section}-item-${node.path}`}
            style={{ paddingLeft: pad(depth) }}
          >
            {renderItem(node)}
          </div>
        )
      })}
    </>
  )
}

/** Pixel padding for a given tree depth — keeps icons aligned across levels. */
function pad(depth: number): number {
  return 12 + depth * 14
}

function SectionHeader({
  label,
  open,
  count,
  onToggle,
  action,
}: {
  label: string
  open: boolean
  count: number
  onToggle: () => void
  action: React.ReactNode
}) {
  return (
    <div className="flex min-h-8 items-center justify-between gap-1 pl-1 pr-1.5">
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={open}
        className="flex h-8 min-w-0 flex-1 items-center gap-1.5 text-left text-xs font-medium text-foreground focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring"
      >
        {open ? (
          <ChevronDown className="size-3" />
        ) : (
          <ChevronRight className="size-3" />
        )}
        {label}
        <span className="shrink-0 text-xs font-normal text-muted-foreground">
          {count}
        </span>
      </button>
      {action}
    </div>
  )
}

/**
 * Section header for project-level singletons (Tasks, IO Mapping).
 * Same visual weight as `SectionHeader` so the five top-level concepts
 * (POUs / Devices / Edges / Tasks / IO Mapping) read as peers, but
 * with no toggle / "+ item" / "+ folder" — those wouldn't make sense
 * for a one-off project resource.
 *
 * The chevron slot is occupied by a spacer so the labels still line
 * up vertically with the expandable sections above.
 */
function SingletonSectionHeader({
  label,
  count,
  countTitle,
  active,
  onOpen,
}: {
  label: string
  count: number | string
  countTitle?: string
  active: boolean
  onOpen: () => void
}) {
  return (
    <div className="flex min-h-8 items-center justify-between gap-1 pl-1 pr-1.5">
      <button
        type="button"
        onClick={onOpen}
        aria-current={active ? "page" : undefined}
        className={cn(
          "flex h-8 min-w-0 flex-1 items-center gap-1.5 text-left text-xs font-medium hover:bg-accent/50 focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring",
          active ? "bg-accent text-foreground" : "text-foreground",
        )}
      >
        {/* Spacer matching the chevron in SectionHeader so the labels
            line up vertically with the expandable sections above. */}
        <span className="size-3 shrink-0" />
        {label}
        <span
          className="shrink-0 text-xs font-normal text-muted-foreground"
          title={countTitle}
        >
          {count}
        </span>
      </button>
    </div>
  )
}

function SectionActions({
  onAddItem,
  onAddFolder,
  itemTitle,
  folderTitle,
}: {
  onAddItem: () => void
  onAddFolder: () => void
  itemTitle: string
  folderTitle: string
}) {
  return (
    <div className="flex items-center">
      <button
        type="button"
        title={folderTitle}
        aria-label={folderTitle}
        onClick={(e) => {
          e.stopPropagation()
          onAddFolder()
        }}
        className="flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring"
      >
        <FolderPlus className="size-3.5" />
      </button>
      <button
        type="button"
        title={itemTitle}
        aria-label={itemTitle}
        onClick={(e) => {
          e.stopPropagation()
          onAddItem()
        }}
        className="flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring"
      >
        <Plus className="size-3.5" />
      </button>
    </div>
  )
}

/// A POU file in the tree. Renders as one row if the file declares
/// exactly one POU and that POU's IEC name matches the file's leaf
/// segment (the typical 1-POU-per-file case); otherwise renders the
/// file as a header row plus one child per declaration. The icon
/// reflects the IEC POU type (PRG / FB / FUN), and a small badge
/// shows the language ("st" for now). All variants click-to-open the
/// same file in the editor.
function PouItem({
  node,
  active,
  onOpen,
  onDelete,
  readOnly = false,
}: {
  node: { name: string; path: string; item: PouFile }
  active: boolean
  onOpen: () => void
  onDelete: () => void
  /** Library blocks: no Delete in the context menu (the whole library
   *  is removed via its own row; single files via /api/library). */
  readOnly?: boolean
}) {
  const decls = node.item.declarations
  // IEC identifiers are case-insensitive — `fb_pid.st` declaring
  // `FB_PID` is the 1-POU-per-file convention, not a mismatch.
  const simple =
    decls.length === 1 &&
    decls[0].name.toLowerCase() === node.name.toLowerCase()
  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>
        <div>
          <button
            type="button"
            onClick={onOpen}
            aria-current={active ? "page" : undefined}
            className={cn(
              "flex h-8 w-full min-w-0 items-center gap-1.5 pl-3 pr-2 text-left text-[13px] transition-colors hover:bg-accent/50 focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring",
              active && "bg-accent text-foreground",
            )}
          >
            {simple ? (
              <PouTypeIcon type={decls[0].type} />
            ) : (
              <FileCode2 className="size-3.5 shrink-0 text-muted-foreground" />
            )}
            <span className="min-w-0 flex-1 truncate" title={node.name}>
              {node.name}
            </span>
            {simple ? (
              <PouTypeBadge type={decls[0].type} language={decls[0].language} />
            ) : decls.length === 0 ? (
              <span className="shrink-0 text-xs text-muted-foreground">
                empty
              </span>
            ) : (
              <span className="shrink-0 text-xs text-muted-foreground">
                {decls.length} POUs
              </span>
            )}
          </button>
          {/* Multi-POU file: each declaration as a sibling sub-row.
              Indented so it's obvious they live inside the file above. */}
          {!simple && decls.length > 0 && (
            <ul>
              {decls.map((d) => (
                <li key={d.name}>
                  <button
                    type="button"
                    onClick={onOpen}
                    aria-current={active ? "page" : undefined}
                    className="flex h-8 w-full min-w-0 items-center gap-1.5 pl-9 pr-2 text-left text-xs text-muted-foreground transition-colors hover:bg-accent/50 hover:text-foreground focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring"
                  >
                    <PouTypeIcon type={d.type} />
                    <span className="min-w-0 flex-1 truncate" title={d.name}>
                      {d.name}
                    </span>
                    <PouTypeBadge type={d.type} language={d.language} />
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      </ContextMenuTrigger>
      <ContextMenuContent>
        <ContextMenuItem onSelect={onOpen}>Open</ContextMenuItem>
        {!readOnly && (
          <>
            <ContextMenuSeparator />
            <ContextMenuItem variant="destructive" onSelect={onDelete}>
              Delete
            </ContextMenuItem>
          </>
        )}
      </ContextMenuContent>
    </ContextMenu>
  )
}

function PouTypeIcon({ type: _type }: { type: PouType }) {
  // We used to colour the icon by POU type (PRG sky / FB violet / FN
  // amber) so users could scan the tree. The little `prg` / `fb` / `fn`
  // text badge from `PouTypeBadge` carries the same signal, so the
  // colour was redundant — and per the workspace design language we
  // try to keep category labels in the muted tier, reserving colour for
  // state (running / modified / error).
  return <FileCode2 className="size-3.5 shrink-0 text-muted-foreground" />
}

function PouTypeBadge({
  type,
  language,
}: {
  type: PouType
  language: string
}) {
  const label =
    type === "function_block" ? "FB" : type === "function" ? "FN" : "PRG"
  return (
    <span className="flex shrink-0 items-center gap-1.5 text-xs text-muted-foreground">
      <span>{label}</span>
      <span className="text-xs">
        {language.toUpperCase()}
      </span>
    </span>
  )
}

function DeviceItem({
  node,
  active,
  onOpen,
  onDelete,
}: {
  node: { name: string; path: string; item: Device }
  active: boolean
  onOpen: () => void
  onDelete: () => void
}) {
  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>
        <button
          type="button"
          onClick={onOpen}
          aria-current={active ? "page" : undefined}
          className={cn(
            "flex h-8 w-full min-w-0 items-center gap-1.5 pl-3 pr-2 text-left text-[13px] transition-colors hover:bg-accent/50 focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring",
            active && "bg-accent text-foreground",
          )}
        >
          <ProtocolIcon protocol={node.item.protocol} />
          <span className="min-w-0 flex-1 truncate" title={node.name}>{node.name}</span>
          <span className="shrink-0 text-xs text-muted-foreground">
            {node.item.protocol}
          </span>
        </button>
      </ContextMenuTrigger>
      <ContextMenuContent>
        <ContextMenuItem onSelect={onOpen}>Open</ContextMenuItem>
        <ContextMenuSeparator />
        <ContextMenuItem variant="destructive" onSelect={onDelete}>
          Delete
        </ContextMenuItem>
      </ContextMenuContent>
    </ContextMenu>
  )
}

function EdgeItem({
  node,
  active,
  attached,
  onOpen,
  onDelete,
}: {
  node: { name: string; path: string; item: Edge }
  active: boolean
  attached: boolean
  onOpen: () => void
  onDelete: () => void
}) {
  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>
        <button
          type="button"
          onClick={onOpen}
          aria-current={active ? "page" : undefined}
          className={cn(
            "flex h-8 w-full min-w-0 items-center gap-1.5 pl-3 pr-2 text-left text-[13px] transition-colors hover:bg-accent/50 focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring",
            active && "bg-accent text-foreground",
          )}
        >
          <Server className="size-3.5 shrink-0 text-muted-foreground" />
          <span className="min-w-0 flex-1 truncate" title={node.name}>{node.name}</span>
          {attached && (
            <span
              className="shrink-0 text-xs text-highlight"
              title="IDE is attached to this edge"
            >
              attached
            </span>
          )}
          <span className="max-w-[45%] truncate text-xs text-muted-foreground"
            title={node.item.host}>
            {node.item.host}
          </span>
        </button>
      </ContextMenuTrigger>
      <ContextMenuContent>
        <ContextMenuItem onSelect={onOpen}>Open</ContextMenuItem>
        <ContextMenuSeparator />
        <ContextMenuItem variant="destructive" onSelect={onDelete}>
          Delete
        </ContextMenuItem>
      </ContextMenuContent>
    </ContextMenu>
  )
}

function ProtocolIcon({ protocol }: { protocol: Protocol }) {
  if (protocol === "modbus") {
    return (
      <Network className="size-3.5 shrink-0 text-muted-foreground" />
    )
  }
  return (
    <Cpu className="size-3.5 shrink-0 text-muted-foreground" />
  )
}

