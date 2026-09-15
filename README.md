# IA2

A simple, agent-first IDE + runtime for IEC 61131-3 PLC programming.

> Positioned against Codesys / TwinCAT / Step 7 — same standard,
> 1/50 the complexity. **Agents (Claude Code, Codex, Cursor) are
> first-class users alongside humans.** Every feature reachable via
> GUI is also reachable via the `cs` CLI and the HTTP API.

https://github.com/user-attachments/assets/59e9a583-5064-4f33-918a-2a66c53274ae

[Video credits](docs/assets/showreel-credits.md).


## Install it for your coding agent

IA2 is built so a coding agent drives it (Claude Code, Cursor, Codex…). **Two ways to install the skill:**

**A · Just tell your agent** — it runs everything below for you (installs the skill, builds the binaries, starts the server):

> **"Install the industrial-automation-skill from https://github.com/supcon-international/ia2"**

**B · Run `npx skills` yourself:**

```bash
npx skills add https://github.com/supcon-international/ia2/tree/main/.claude/skills/industrial-automation-skill
```

That's the [vercel-labs/skills](https://github.com/vercel-labs/skills) installer — it drops the skill **and its `references/` + `checklists/`** where your agent looks: `.claude/skills/` for Claude Code, the [Agent Skills](https://agentskills.io) standard `.agents/skills/` for most others (add `-g` for every project; `-a claude-code`, `-a codex`, `-a kimi-code-cli`, `-a cursor`… to pin the agent).

### The binaries — `cs` + `ia2-server`

The skill drives a small Rust CLI and a local server. Route **A** builds them for you; on route **B** (or to do it by hand) build once — needs the Rust toolchain ([rustup.rs](https://rustup.rs)):

```bash
git clone --recursive https://github.com/supcon-international/ia2
cd ia2 && ./scripts/install-skill.sh
```

`scripts/install-skill.sh` builds `cs` + `ia2-server` (plus its `lsp-launcher` sidecar for editor language support) into `~/.local/bin` (it also installs the skill, so it doubles as a no-`npx`, do-everything one-shot). Then:

Already have a clone and only want local agent discovery, with no build or network access? Run `./scripts/install-skill.sh --skill-only`. It copies the complete skill and creates the Codex-native `~/.agents/skills/industrial-automation-skill` discovery link without touching binaries or hardware.

For repository work in Codex, open the **IA2 Git root** as the workspace, or launch with `codex --cd /path/to/ia2`. Codex discovers `AGENTS.md` and repository skills by walking from its current directory up to the Git root; launching from a parent folder that merely contains the nested `ia2` checkout will not load IA2's repository contract.

1. **Start the server:** `ia2-server --bind 127.0.0.1:3001 &`
2. **Restart your agent session** so it discovers the skill.

Now just ask your agent to build a PLC program — it will author ST / LD / FBD / SFC, compile, wire Modbus / EtherCAT / OPC UA / CANopen I/O, run and debug the scan loop, and deploy to edge boxes, all through `cs`. Start with `cs --help` and the skill under `.claude/skills/industrial-automation-skill/`.


## Quickstart

### Run the IDE

```bash
# one-time
. "$HOME/.cargo/env"
pnpm install
cargo test -p server   # populates apps/web/src/types/generated/

# dev mode — two terminals
pnpm --filter @cs/web dev      # → http://localhost:3000
cargo run -p server            # → http://localhost:3001

# OR single origin: server hosts the built UI itself
pnpm --filter @cs/web build
cargo run -p server --release -- --static-dir apps/web/dist
```

### Drive it from the CLI

```bash
cargo build -p ia2-cli
alias cs=./target/debug/cs

# the mental model is bash-sized: 4 resource verbs + an API escape hatch
cs ls                                   # what resource kinds exist? (start here)
cs ls pous                              # enumerate any collection
cs get devices/plc1                     # read any resource (JSON, or raw POU source)
cs set devices/plc1 --from cfg.json     # create-or-replace (get → edit → set)
cs rm  hmi/overview                     # delete
cs api POST /api/edges/pi/attach        # anything else in docs/api.md — full parity

# author + validate (offline where possible)
cs check pous/safe_start.ld.json        # validate any language; `cs check P0002` explains a code
cs project check ~/Documents/IA2/demo   # full compile
printf '...' | cs set pous/motor.ld.json --from -   # extension picks the language
cs library import process-control --blocks fb_pid.st

# wiring / scheduling / alarms — single-doc configs, one shape each
cs set iomap  --from iomap.json         # variable ↔ device.channel bindings
cs set tasks  --from tasks.json         # PROGRAM ↔ task schedule
cs set alarms --from alarms.json        # declarative alarm definitions (alarms.toml)

# run · simulate · debug
cs run                                  # schedule everything in tasks.toml
cs sim run scenarios/fill.toml          # PROVE behaviour against the sim device layer — CI-ready
cs runtime snapshot --vars level,pump   # live values
cs runtime force pump_pct 50.0          # type-aware: REAL bit-packed, BOOL as 0/1
cs get runtime/alarms && cs runtime ack level_high
cs get runtime/history --query vars=level   # 1 Hz historian, ~2 h window
cs stop

# ship it
cs set edges/field_pi --host pi@plc.local
cs deploy field_pi                      # tar → ssh → versioned swap → systemd restart — and it
                                        # FAILS honestly if the unit didn't restart

# wrap a whole multi-step workflow in one steady takeover session
cs agent run --label "build my_line" -- bash -c 'cs project create my_line; cs set pous/...; cs run'
```

Every command's `--help` explains when to call it and
what to call next — written for agent readers.

## Project on disk

```
~/Documents/IA2/
└── my_project/
    ├── project.toml         metadata + version
    ├── tasks.toml           task → program scheduling
    ├── iomap.toml           variable ↔ device-channel wiring
    ├── pous/
    │   ├── cascade_pid.st         IEC Structured Text
    │   ├── motor_seal.ld.json     Ladder Diagram (JSON authored)
    │   ├── click_counter.fbd.json Function Block Diagram
    │   └── batch_sequence.sfc.json Sequential Function Chart
    ├── devices/             Modbus / EtherCAT / OPC UA / CANopen devices
    └── edges/               Deploy targets (Linux edge boxes)
```

JSON for graphical languages (not PLCopen XML) so agents and
`git diff` can read it. LD / FBD / SFC are transpiled to ST before
reaching ironplc; the intermediate ST is observable via
`cs transpile foo.ld.json`. `alarms.toml` declares alarm conditions
(limit + deadband + delay + severity) that the runtime's alarm engine
evaluates — with an ISA-18.2-shaped ack/journal lifecycle — and
`scenarios/*.toml` hold `cs sim` scenarios that prove the program's
behaviour against the simulated device layer before deploy.

## Edge deployment

`crates/runtime/` builds the `ia2-runtime` binary — headless, designed
for Linux edge boxes. Install the systemd unit (`infra/ia2.service`);
its `INSTALL_DIR` is the single source of truth for where the runtime
and project live, and the edge's `install_dir` must match it (`cs
deploy` warns on drift). `cs deploy <edge>` then tars the project over
SSH, swaps the `current` symlink atomically, and restarts the unit.

The server reaches a deployed runtime over `ssh + curl` to its HTTP
monitor: it tries the configured `runtime_port`, then falls back to
systemd — the unit's `--bind` port and `ActiveState` — so a wrong or
changed port (or a stopped service) gives a clear answer instead of a
blind failure. A transient EtherCAT bring-up timeout is retried rather
than left dead until a manual restart. See `docs/edge-deploy.md`.

## Agent skill (Claude Code · Codex · Kimi Code · Cursor · …)

The repo ships an [Agent Skills](https://agentskills.io)-format skill at
**`.claude/skills/industrial-automation-skill/`** (canonical copy),
mirrored at **`.agents/skills/industrial-automation-skill`** — the
standard path [Codex](https://developers.openai.com/codex/skills), Kimi Code, Cursor, Gemini CLI and others scan.
The mirror is a committed symlink, so it needs a symlink-capable
checkout (on Windows: `core.symlinks=true` / Developer Mode) —
without one, point your agent at the canonical `.claude/skills/`
copy, which always works.
Repo-root **`AGENTS.md`** (which `CLAUDE.md` imports) is the working
contract for changing this repo and points agents whose harness doesn't
scan skill directories at the skill by hand.

In Claude Code the skill auto-loads on PLC/automation work matching the
skill's description (words like "IEC 61131-3", "PLC", "Modbus",
"EtherCAT", "cs CLI"); in
Codex / Kimi Code it's discovered from `.agents/skills` (implicitly by
description match, or explicitly as `$industrial-automation-skill` in
Codex, or `/skill:industrial-automation-skill` in hosts that use that
syntax). Either way it teaches the agent
the whole `cs` workflow: the meta-primitive mental model (`ls/get/set/rm` + `api`),
the mandatory `cs agent run` session pattern, end-to-end recipes
including scenario simulation and alarms/history, the exact
device/iomap/tasks/alarms JSON shapes, IEC 61131-3 quirks ironplc
actually accepts, and a troubleshooting table. It also carries three
checklists: `first-contact` (find the local server port, see what's
open), `offline-readiness` (separate simulation proof from bench
evidence), and `handoff` (compile clean, release forces, report state)
— so an agent starts and finishes a task the right way.

It's committed (not gitignored) so every contributor and CI agent gets
the same playbook. Skim `SKILL.md` to see what an agent is told. Run
`./scripts/check-agent-adaptation.sh` for an offline contract check of
`AGENTS.md`, skill metadata, repository discovery, and the skill-only
installer; the check never contacts a server, network, or hardware.


## License

IA2 is licensed under [Apache-2.0](LICENSE). Vendored IronPLC carries its own
[MIT license](vendor/ironplc/LICENSE); other dependencies retain their respective licenses.
