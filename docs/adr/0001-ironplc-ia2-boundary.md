# ADR-0001: ironplc / IA2 responsibility boundary

Status: Accepted (2026-06-13)

Updated: 2026-09-23 — unpatched upstream v0.244.0, single IA2 scheduler.
Updated: 2026-09-23 — released upstream v0.246.0; SFC state follows step-name encoding.

## Context

IA2 uses ironplc (parser / analyzer / codegen / container / vm / dsl) via a
vendored submodule. Audit findings:

- `crates/ironplc-bridge` is the **only** crate that imports ironplc
  directly (lsp-launcher merely calls `ironplc-cli::lsp::start()`).
  server / cli / runtime see only the bridge's `Container` /
  `ProgramHandle` / `VarSnapshot` types — no leakage.
- The vendored ironplc was previously **unmodified upstream** (pinned
  31c40c69, v0.212.0 line).
- The original bridge covered upstream gaps in four places, and "who owns
  what" was never written down as a decision:
  1. codegen doesn't populate `container.task_table` → the VM's
     `next_due_us()` is always None, so the bridge schedules with its own
     tasks.toml-interval sleep (a single cadence);
  2. the VM's `find_program()` executes only the first PROGRAM in a
     container → the server used to reject multi-PROGRAM `tasks.toml`;
  3. codegen drops the `VAR RETAIN` qualifier → the bridge extracts retain
     variable names from the AST before codegen;
  4. the VM write API was only `write_variable(i32)` → LREAL input mapping
     was skipped and RETAIN truncated 64-bit types.

Upstream v0.244.0 now supplies the task table and full-width write API
(gaps 1 and 4). Separate PROGRAM containers and AST-based RETAIN extraction
remain necessary. A populated cyclic task table creates a new hazard:
IA2 and the VM could both gate execution, counting a paused step without
executing it. IA2 therefore normalizes loaded units to freewheeling tasks;
only its `UnitClock` governs cadence, pause, resume and step.

## Decision: boundary principle

**ironplc owns "the language": IEC 61131-3 text → one scan cycle of an
executable unit.**
**IA2 owns "the engineering": orchestrating N executable units into a
plant's control layer.**

| Capability | Owner | Form |
|---|---|---|
| Parse / semantic analysis / problem codes + RST docs | ironplc | bridge passes `CheckDiagnostic` through |
| Bytecode container + debug section | ironplc | bridge consumes `VariableRenderer`; supplies exact parser sources for hashes/line maps |
| VM: execute one container's one scan (`run_round`), variable read/write | ironplc | bridge holds `VmRunning` |
| LSP server (syntax / symbols / semantic tokens) | ironplc | lsp-launcher starts it; **diagnostics do NOT go through the LSP** (single-file view), they go through IA2's project-aware `/api/check` |
| CONFIGURATION synthesis (tasks.toml → IEC text) | IA2 bridge | `synthesize_configuration` |
| Task scheduling (multi-task cadence, multi-PROGRAM orchestration) | **IA2 bridge** | see "multi-PROGRAM design" below |
| RETAIN extraction + persistence + restore | IA2 bridge | AST extraction + `retain.rs` on-disk format |
| I/O: devices, channels, mappings, failsafe, watchdog | IA2 (iocore / iomap-*) | the VM is unaware of it |
| Engineering model: projects / libraries / Edge / deploy / IDE / HTTP API | IA2 | — |

Criterion: anything the IEC 61131-3 standard text defines (syntax, types,
single-POU execution) belongs to ironplc; anything that "makes it a
product beyond the standard" (scheduling policy, persistence format,
hardware, multi-project, IDE) belongs to IA2. **Don't push engineering
concepts into the vendor, and don't reimplement the language in IA2.**

## Decision: vendor strategy (released upstream pin, no active patches)

The submodule points directly at `https://github.com/ironplc/ironplc.git`,
tag [v0.246.0](https://github.com/ironplc/ironplc/releases/tag/v0.246.0),
commit `6f4a796736576b29cde5d163e75c311639028e15` — an upstream **release**
(not a pre-release). There are no IA2 source patches in the submodule.

v0.246.0 over the v0.244.0 pre-release that IA2 #61 surveyed:

- A `STRING` literal holding a character outside Latin-1 is rejected
  (P4052, ironplc [#1733](https://github.com/ironplc/ironplc/issues/1733))
  instead of silently keeping each character's low byte. The SFC
  transpiler therefore lowers a chart whose step names are not all
  Latin-1 to `WSTRING` state and literals; Latin-1 charts keep the
  historical `STRING` lowering unchanged.
- New analysis errors, some for programs that used to compile into
  silently wrong code: a name declared twice in one scope, including
  names that differ only in case (P4014); the same function block or type
  declared in two POU files, where the last one used to win (P4013,
  P2007); a `FUNCTION` holding a function block instance (P4054); a
  `CASE` selector that is not an integer or enumeration, bit strings
  included (P4053); an inverted `CASE` range (P4051). Every project under
  `examples/` and the process-control library validate with identical
  diagnostics on both versions.
- A configuration and a type of the same name are now a duplicate
  declaration, so the synthesized CONFIGURATION is named
  `__ia2_configuration` (it was `config`, which rejected a project's own
  `TYPE Config`).
- P9999 diagnostics carry the source span of the construct
  (ironplc [#1734](https://github.com/ironplc/ironplc/issues/1734)).
- No bridge API change.

Historical fork patches (the old fork/ref is not rewritten, so prior IA2
commits remain reproducible):

| # | Patch | Motivation | Disposition |
|---|---|---|---|
| 1 | `vm: write_variable_raw(VarIndex, u64)` (d06a646c) | Lossless RETAIN restore and 64-bit I/O | Dropped: upstream [#1387](https://github.com/ironplc/ironplc/pull/1387) provides it |
| 2 | `codegen: keep user FUNCTION ids clear of user FB function ids` (f8f135b1) | Prevent FUNCTION/FB function-id collision | Dropped: upstream [#1100](https://github.com/ironplc/ironplc/pull/1100) fixes it |
| 3 | `codegen: pointed diagnostic for nested FB instances in FB bodies` (72d6ac42) | More specific wording for an unsupported construct | Dropped: upstream still rejects it with P9999; retain regression coverage, not a fork solely for wording |

Upgrade flow: select and inspect an upstream release tag, update the
submodule gitlink, adapt only the bridge and run the repository gates.
Do not edit vendor sources or force-push historical fork refs. Any future
patch must be a narrow upstream-worthy API/fix, registered here and
submitted upstream; no IA2 engineering semantics belong in the vendor.

One `parser_options()` constructor preserves IA2's existing allowances:
empty VAR blocks, top-level VAR_GLOBAL and untyped integer literals in
bit-string expressions. Every check and compile entry point uses it.
The bridge passes the actual parser inputs to `SourceLookup`, including
generated/transformed ST; hashes for generated sources identify that ST,
not the original graphical JSON. Anonymous isolated-run compilation hashes
the assembled source. Snapshots use upstream `VariableRenderer` for
STRING/WSTRING, enum and aggregate display, retaining raw VM bits unchanged.

## Decision: multi-PROGRAM / multi-task implemented on the IA2 side

The bridge runs **one
Container + one VM per PROGRAM instance, round-robin scheduled on a single
scan thread**. This is implemented (commit fc4addd):

- **Compile**: each `tasks.toml` program entry gets its own container,
  assembled at the AST level — the target `ProgramDeclaration` hoisted to
  the front (ironplc supports one PROGRAM per container) + every
  non-PROGRAM declaration from all POU files (cross-file FBs resolve) + a
  synthesized single-task CONFIGURATION. Foreign PROGRAM declarations are
  excluded, so each unit's debug map stays free of other programs'
  variables (this also dissolves the "debug_section only names the first
  instance" problem) and a second PROGRAM in one file becomes schedulable.
- **Schedule**: before VM load, every unit's container task is made
  `Freewheeling` with zero interval, including direct-compile callers.
  Each unit has its own `next_due` anchor from its task
  interval; the thread runs every unit whose deadline is due, then sleeps
  to the nearest. Priority then declaration order breaks same-tick ties.
- **I/O routing**: `Mapping.application` selects the target unit
  (case-insensitive instance match); a bare/unknown application falls back
  to the first owning unit, warning only when N > 1. Devices stay
  thread-owned and units share them sequentially — no concurrency.
- **Snapshots** merge across units; a name colliding between units renders
  as `instance.variable`, while single-unit projects keep bare names.
- **RETAIN** keys gain the instance prefix when N > 1. Load accepts both
  spellings for the unit's own instance, so adding *or removing* a PROGRAM
  carries the values across; a bare key never adopts another instance's
  value. Only `PROGRAM` and `VAR_GLOBAL` retain vars are persisted — an
  FB-internal one has no debug-map slot to address.
- **Constraint**: cross-PROGRAM `VAR_GLOBAL` sharing is not supported
  (separate containers isolate the address spaces); `/api/run` and
  `/api/project/validate` detect it and return a clear error.
- Hardware authority is unchanged: the server runs one project at a time.

If upstream later lands multi-PROGRAM container semantics, the
bridge can collapse "round-robin many VMs" back to "one container, many
tasks" with no change to the layers above.

## Follow-ups (upstream candidates)

1. Multi-PROGRAM containers and per-instance debug names:
   [ironplc #1613](https://github.com/ironplc/ironplc/issues/1613).
2. Preserve RETAIN qualifiers in compiler metadata.
3. Nested FB instances inside FUNCTION_BLOCK bodies:
   [ironplc #1553](https://github.com/ironplc/ironplc/issues/1553).
   Until supported, hoist those instances into the PROGRAM.

IA2-side follow-up from the v0.246.0 upgrade: the LD/FBD transpilers
de-duplicate generated instance and temporary names case-sensitively and
do not check them against the POU's own variables. IEC names are
case-insensitive, so such a clash now surfaces as P4014 on generated ST
rather than as a pointed error on the diagram element.

Offline compile, VM, pause/step/resume and simulation tests are upgrade
evidence, not real fieldbus or timing acceptance. Rebuild and test hardware
artifacts separately before deployment.
