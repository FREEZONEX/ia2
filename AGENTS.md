# IA2 — instructions for coding agents

IA2 is an agent-first IEC 61131-3 PLC IDE + runtime (`README.md` for
the product story, `MEMORY/principles.md` for the full design
doctrine). This file is the working contract for changing THIS repo —
whichever agent you are (Claude Code loads it via `CLAUDE.md`'s
`@AGENTS.md` import; Codex, Kimi Code, Cursor, Gemini CLI, Copilot and
friends read `AGENTS.md` natively).

For Codex, use this Git root (or a directory below it) as the current
workspace. A parent workspace that merely contains this nested checkout
does not discover this file or the repository skill below.

## Agent skill — deep IA2 / PLC procedures

The full "how to drive IA2" playbook is an
[Agent Skills](https://agentskills.io)-format skill at
**`.claude/skills/industrial-automation-skill/`** (canonical copy),
mirrored for standard discovery at
**`.agents/skills/industrial-automation-skill`** (symlink; needs a
symlink-capable checkout — on Windows, `core.symlinks=true` also needs
Developer Mode or permission to create symlinks. Otherwise the Git
placeholder is not discoverable: use the canonical `.claude/skills/`
path, or `scripts/install-skill.ps1 -SkillOnly` to install real copies
at both user discovery paths without elevation). When doing
IEC 61131-3 / `cs` CLI / device-wiring / sim / deploy work, read that
`SKILL.md` first and open its `references/*.md` on demand — do not
preload them. If your harness auto-loaded the skill already, follow it;
this section exists for agents whose harness does not scan skill
directories.

## Orientation

- Rust workspace under `crates/` (`server` = axum backend :3001,
  `runtime` = headless edge binary, `cli` = the `cs` binary,
  `ironplc-bridge` = compiler/VM wrapper + shared `monitor` layer
  (debug ops, historian, alarm engine), `project` = on-disk schema,
  `iomap-*` = field-protocol adapters, `esi`/`iocore` = support,
  `desktop` = Windows `IA2.exe` window/tray + owned server lifecycle).
  `vendor/ironplc` is a git submodule — never edit it directly; the
  patch registry is `docs/adr/0001-ironplc-ia2-boundary.md`.
- Web IDE in `apps/web` (React 19 + Vite + Tailwind 4; single SPA plus
  the `hmi.html` standalone operator panel entry).
- The HTTP API (`docs/api.md`) is the canonical contract; the CLI and
  the IDE are both clients of it.

## Non-negotiable principles (enforced in review)

1. **API-first.** Every feature is a complete HTTP endpoint before it
   is a GUI affordance. `docs/api.md` coverage is TEST-ENFORCED
   (`crates/server/tests/api_doc_coverage.rs`) — add the doc row in the
   same change as the route.
2. **CLI stays bash-sized (meta-primitives).** The quartet
   `cs ls/get/set/rm <resource-path>` + `cs api METHOD /path` must
   cover new resources BY CONSTRUCTION — extend the path grammar in
   `crates/cli/src/resource.rs`, don't add noun subcommands. Only
   actions with real domain semantics (safety, exit codes, type-aware
   encoding) earn a verb. New leaf subcommands count AGAINST a change
   in review.
3. **Truthfulness everywhere.** Outcomes are reported honestly at every
   layer: deploy fails when the service didn't restart; the CLI prints
   server error bodies verbatim (exit 2 for 4xx, 3 for infra); the HMI
   never fakes state ("Stop unconfirmed", no e-stop cosplay). Docs that
   contradict code are release blockers — fix the doc or the code in
   the same change.
4. **Proof before hardware.** Behavioural changes ship with a
   `cs sim run` scenario (`scenarios/*.toml`; reference example
   `examples/sim_smoke/`). Alarm-worthy conditions are declared in
   `alarms.toml` next to the logic. "It compiles" ≠ "it works".
5. **Text-first storage.** Everything a project holds is grep-able
   TOML / JSON / ST on disk. No binary blobs, no hidden state.
6. **One implementation per semantic.** Shared behaviour lives in one
   place (`ironplc_bridge::monitor` for debug ops / pulse-reset /
   historian / alarms used by BOTH server and edge runtime). If you
   find yourself copying a handler between `crates/server` and
   `crates/runtime`, extract it instead.
7. **No customer information in the repo** — no company names, project
   numbers, or customer document references in code, comments, or
   commits; libraries carry no vendor-benchmark names.

## Quality gate (there is NO CI — run this locally before finishing)

```bash
./scripts/check-agent-adaptation.sh
cargo fmt --all
cargo clippy --workspace        # zero warnings expected
cargo test  --workspace         # includes the sim e2e + deploy-script tests
cargo build --release -p server -p ia2-cli -p ia2-runtime -p lsp-launcher
pnpm --filter @cs/web build && pnpm --filter @cs/web test
```

On native Windows, use `scripts/check-windows.ps1` (PowerShell 5.1):
`powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\check-windows.ps1`.
It checks adaptation, formatting, Clippy (workspace warnings denied),
workspace tests, five release binaries, and web build/tests. It builds
`server.exe` before CLI simulation tests; a missing server is a failure,
not a skipped test. `scripts/build-windows.ps1` builds explicit x64 MSVC
release artifacts with a static CRT, without changing global Cargo config.
`-AdaptationOnly` runs only the offline skill installation checks.
Linux remote Bash execution tests still require Unix coverage. See
`docs/windows.md` for platform scope and installed-artifact acceptance.

`cargo test -p server` also regenerates `apps/web/src/types/generated/`
(ts-rs, via the `TS_RS_EXPORT_DIR` in `.cargo/config.toml`). The
generated files are gitignored build artifacts (only `.gitkeep` is
tracked) — run the test whenever a `#[ts(export)]` type changes so
your local copies stay current; there is nothing to commit.

## Gotchas that bite

- ironplc reserves `DT` — never name an ST variable `dt`/`DT` (P0002).
- POU slugs are extension-less server-side; a `.st` file may declare
  several POUs (file ≠ declaration).
- ironplc's debug section names only the FIRST PROGRAM instance;
  multi-program runs show later instances nameless in the Monitor.
- The FB-library registry resolves `--library-dir`, `IA2_LIBRARY_DIR`,
  `./library`, the executable's adjacent `../library`, then the legacy
  `~/.local/share/ia2/library`. Windows installs at
  `%LOCALAPPDATA%\IA2\library`; its launcher passes both library and web
  paths explicitly. Keep projects outside the replaceable install tree.
- Windows real fieldbus paths are EtherCrab with optional Npcap and
  CANopen over gs_usb/WinUSB; use `docs/windows-can-ethercat.md` for
  selectors, dependencies and bench-pending limits. Compilation and
  simulation do not prove real bus traffic or hard real-time. Windows
  RTU uses COM ports and rejects Linux
  `rs485` direction-control settings. A Windows `.exe` is never a Linux
  deployment payload: use a matching Linux ELF or the provisioned runtime.
- Windows `IA2.exe` hosts the local UI in WebView2. Closing hides to the
  tray; it never stops a PLC. Exit uses the owned server's token-gated
  `/api/desktop/shutdown`, which atomically refuses running/starting PLCs.
  Keep the desktop profile/logs outside the replaceable installation tree.
  Monaco and its worker are bundled locally; do not restore CDN loading.
- Snapshot `bits` is the raw VM slot (REAL = IEEE-754 bits); decode
  with `ironplc_bridge::monitor::typed_value` — never re-parse display
  strings.
- Alarm state machine uses scan time for debounce but WALL-CLOCK for
  journal/raised_at stamps; don't mix the bases.
- The skill under `.claude/skills/industrial-automation-skill/` teaches
  agents the `cs` surface — when you change CLI or API behaviour,
  update the skill (esp. `references/02-cli-reference.md`) in the same
  change, or the next agent session inherits a lie.

## Docs that must move with code

| You changed… | Also update… |
|---|---|
| a server route | `docs/api.md` (test-enforced) + skill 02 if agent-facing |
| the `cs` surface | skill `references/02-cli-reference.md` + README quickstart |
| deploy/edge behaviour | `docs/edge-deploy.md` |
| HMI schema/nodes | `docs/hmi-design.md` + skill `references/08-hmi.md` |
| sim / alarms / history | skill `references/09-sim-alarms.md` |
| agent onboarding (this file, skill discovery paths, skill metadata, installer) | README "Install it for your coding agent" + both installer headers + `scripts/check-agent-adaptation.sh` + `scripts/check-windows.ps1` |
