# ADR-0001: ironplc / IA2 responsibility boundary

Status: Accepted (2026-06-13)

## Context

IA2 uses ironplc (parser / analyzer / codegen / container / vm / dsl) via a
vendored submodule. Audit findings:

- `crates/ironplc-bridge` is the **only** crate that imports ironplc
  directly (lsp-launcher merely calls `ironplc-cli::lsp::start()`).
  server / cli / runtime see only the bridge's `Container` /
  `ProgramHandle` / `VarSnapshot` types — no leakage.
- The vendored ironplc was previously **unmodified upstream** (pinned
  31c40c69, v0.212.0 line).
- But the bridge papers over upstream gaps in four places, and "who owns
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

## Decision: boundary principle

**ironplc owns "the language": IEC 61131-3 text → one scan cycle of an
executable unit.**
**IA2 owns "the engineering": orchestrating N executable units into a
plant's control layer.**

| Capability | Owner | Form |
|---|---|---|
| Parse / semantic analysis / problem codes + RST docs | ironplc | bridge passes `CheckDiagnostic` through |
| Bytecode container + debug section | ironplc | bridge reads only (`build_var_debug_map`) |
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

## Decision: vendor strategy (fork + minimal-patch registry)

The submodule points at the fork `supcon-international/ironplc`, branch
`ia2-patches`, based on the upstream pinned commit. Patch admission rule:
**only narrow APIs upstream ought to provide** — never any IA2 business
semantics. Each patch is registered in the table below and offered back
upstream as a PR; once upstream merges it, the corresponding patch is
rebased out.

| # | Patch | Motivation | Upstream PR |
|---|---|---|---|
| 1 | `vm: write_variable_raw(VarIndex, u64)` (d06a646c) | `read_variable_raw` has a u64 read but no symmetric write; RETAIN restore and 64-bit I/O mapping need a non-truncating write | pending |
| 2 | `codegen: keep user FUNCTION ids clear of user FB function ids` (f8f135b1) | FB pre-scan and user-FUNCTION loop both assigned function ids from 2; the container function directory is positional, so a program using both a user FUNCTION and a user FB executed FB bytecode on `CALL` (InvalidVariableIndex trap or silent corruption) | pending |
| 3 | `codegen: pointed diagnostic for nested FB instances in FB bodies` (72d6ac42) | An FB field that is itself an FB instance (e.g. TON inside a user FB) used to fail at the call site as a generic P9999 todo pointing at compiler source; now rejected at the field declaration with the instance, type, enclosing FB, and the hoist-into-PROGRAM workaround named | pending |

Upgrade flow: `git fetch upstream && git rebase upstream/main ia2-patches`;
a conflicting patch is re-evaluated for whether it is still needed.

## Decision: multi-PROGRAM / multi-task implemented on the IA2 side

Rather than wait for upstream task_table codegen, the bridge runs **one
Container + one VM per PROGRAM instance, round-robin scheduled on a single
scan thread**. This is implemented (commit fc4addd):

- **Compile**: each `tasks.toml` program entry gets its own container,
  assembled at the AST level — the target `ProgramDeclaration` hoisted to
  the front (ironplc's codegen compiles the first PROGRAM it finds) + every
  non-PROGRAM declaration from all POU files (cross-file FBs resolve) + a
  synthesized single-task CONFIGURATION. Foreign PROGRAM declarations are
  excluded, so each unit's debug map stays free of other programs'
  variables (this also dissolves the "debug_section only names the first
  instance" problem) and a second PROGRAM in one file becomes schedulable.
- **Schedule**: each unit has its own `next_due` anchor from its task
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

If upstream later lands task_table + multi-PROGRAM container semantics, the
bridge can collapse "round-robin many VMs" back to "one container, many
tasks" with no change to the layers above.

## Follow-ups (upstream candidates)

1. PR: `write_variable_raw` (patch #1).
2. Issue/PR: have codegen populate `container.task_table` (the VM's
   `scheduler.rs` skeleton is already there).
3. Issue: have the debug_section name variables per PROGRAM instance.
4. PR: function-id collision fix (patch #2) — a straight bug upstream
   will want regardless of IA2.
5. Issue: nested FB instances inside FUNCTION_BLOCK bodies (patch #3
   only improves the error). A real implementation needs per-instance
   recursive data-region layout plus an init story for seeding nested
   instance offsets into the outer instance's block (the data region
   currently starts zeroed; there is no init image or
   store-to-data-region-constant opcode to carry them).
