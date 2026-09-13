# Agent sessions — the mandatory pattern

## Why this exists

When an agent drives IA2, the human watches the IDE. The IDE shows a green "AGENT IN CONTROL" overlay so the human knows not to fight you for state. There are two ways that overlay gets driven:

- **Transient heartbeat** — every *mutating* `cs` command (`set` / `rm` / `run` / `stop` / `deploy`, non-GET `api`, `runtime` pause/step/force/write/ack, `hmi op`/`generate`, `library import`, `project` create/open/close, `sim run`) POSTs `/api/agent/heartbeat`; reads (`ls` / `get` / `check` / `probe` / `runtime status` / `runtime snapshot`) stay **silent** — querying isn't operating. The overlay flashes on with the command name and ages out after 3 s. Run five mutations a few seconds apart and the banner *strobes* — on, off, on, off — with a different label each time. Exhausting to watch.
- **Session** — you POST `/api/agent/session/start { id, label }` once, the overlay stays on with your `label` for the entire workflow, then `/api/agent/session/end { id }` drops it. One steady banner, one message.

**For any workflow longer than a single command, you MUST use a session.** This is the difference between "the agent is thrashing my screen" and "the agent is doing "Building bottling line" and I can see exactly that".

## Attribution beyond `cs` (ADR-0002)

The banner is no longer `cs`-only. Every mutating request may carry `X-IA2-Origin: <operator>` — the IDE sends `gui`, `cs` sends `cs`, the operator panel sends `hmi`, MQTT northbound writes are recorded as `mqtt`; any other declared value is recorded as given (the IDE's edge proxy sanitizes labels to `[A-Za-z0-9._-]`, max 64 chars, before forwarding), and only a request with no header (or an empty one) is recorded as `anonymous`. Three things to know:

- **Origin is a self-declared header, not authentication.** The overlay and the audit trust the label as given — a writer claiming `gui` is treated as the console. The mechanism catches accidents and keeps honest tooling visible; it does not stop an adversary from lying about who they are.
- **Driving the runtime without `cs` still shows up.** On every mutating runtime route (write / force / unforce / pause / resume / step, alarm ack, run / stop, inject-scan-stall, and the edge proxy) the server auto-attributes by origin: no usable label flashes `<op> (unattributed)`; a declared non-`gui` label flashes `<op> — <origin> (self-declared)` — always, even while someone else's session is open; `cs` outside a session flashes `<op> — cs (no session)` (inside a session it just refreshes the session's liveness). So bypassing the session pattern doesn't make you invisible — it makes you look like an unidentified script. Wrap your work in `cs agent run` so the banner says *what* you are, not "(unattributed)".
- **Every edge write is audited.** The runtime keeps a bounded ring (256 entries) of every write attempt — HTTP and MQTT alike — with origin, the requested *and* applied values, and the outcome (`ok` / `clamped` / the denial reason; governance denials are recorded too). Read it as `cs get edges/<n>/audit` (or `GET /api/edges/{name}/audit`; on the box itself, `curl -s http://127.0.0.1:13001/audit`). When a plant behaves unexpectedly, that's the first place to look for *who claimed to write what* — remembering the origin column records the writer's claim, not a verified identity. The ring is in-memory only: it is wiped when the edge runtime restarts (a redeploy restarts it) and rolls over quickly under steady write traffic, so read it early; durable history lives in `/history`.

## The one command you reach for: `cs agent run`

```bash
cs agent run --label "Human-readable description" --server "$SRV" -- bash -c '
  set +e   # decide per-workflow whether one failure should abort the rest
  cs --project foo set pous/main.st --server "'"$SRV"'" --from - <<"ST"
  PROGRAM main ... END_PROGRAM
ST
  cs --project foo project check ...
  cs --project foo run --program main --server "'"$SRV"'"
'
```

What `cs agent run` does, in order:
1. Generates a session id.
2. `POST /api/agent/session/start { id, label }` — overlay comes on.
3. Spawns a **background heartbeat thread** (1 Hz) carrying the session id — keeps the server's session watchdog satisfied.
4. Runs the inner command, with `IA2_AGENT_SESSION=<id>` injected into its environment.
5. On exit — success, failure, **or Ctrl-C** — `POST /api/agent/session/end { id }`. Overlay drops.
6. Exits with the inner command's exit code.

If the inner command cannot start (for example, its executable is
missing), `cs agent run` stops the heartbeat keeper and requests session
closure before reporting the spawn error with exit code 3.

Because `IA2_AGENT_SESSION` is in the child's env, every `cs` call inside the wrapped script attaches its heartbeats to *your* session instead of starting a competing transient one. No flicker, no races.

## Practical shape for heredocs inside `cs agent run`

Quoting gets fiddly when you nest a heredoc inside `bash -c '...'`. Two reliable patterns:

**Pattern A — write a script file, run it (clearest for big workflows):**
```bash
cat > /tmp/ia2_build.sh <<'OUTER'
set -e
SRV="$1"
cs --project foo set pous/main.st --server "$SRV" --from - <<'ST'
PROGRAM main
  VAR x : INT := 0; END_VAR
  x := x + 1;
END_PROGRAM
ST
cs --project foo project check ~/Documents/IA2/foo
cs --project foo run --program main --server "$SRV"
OUTER
cs agent run --label "Build foo" --server "$SRV" -- bash /tmp/ia2_build.sh "$SRV"
```

**Pattern B — inline, escaping the inner `$SRV`** (fine for short sequences; see the `'"$SRV"'` dance above). Pattern A is less error-prone for anything non-trivial.

## Manual enter / leave (only when `run` can't wrap)

Use this if the work spans multiple tool calls you can't put in one `bash -c` (e.g. you need to inspect output between steps and branch):

```bash
export IA2_AGENT_SESSION="$(cs agent enter --label "Investigating fill fault" --server "$SRV")"
# ... now run as many `cs` commands as you like across multiple Bash tool calls;
#     each auto-attaches via the IA2_AGENT_SESSION env var ...
cs agent leave --server "$SRV"     # reads IA2_AGENT_SESSION; or pass --id explicitly
```

⚠️ The risk with manual enter/leave: if your work errors out and you never call `leave`, the session lingers until the server's **30 s no-heartbeat watchdog** ends it (or the human clicks "End session" in the banner). `cs agent run` has no such risk — it always cleans up. **Prefer `run`.**

## What the human sees / can do

- Banner reads **"AGENT SESSION · <your label>"** and stays put.
- The banner's button says **"End session"** — clicking it `POST`s `/api/agent/session/end` with no id, force-closing your session. If that happens mid-workflow, your subsequent `cs` calls still work (they don't require an open session) but the overlay won't reappear unless you start a new one. Treat a force-close as "the human wants control back" — stop and ask.

## Don't

- Don't open a full session for a *single* read (`cs ls projects`, `cs runtime status`) — reads don't heartbeat, so a lone read won't even flash the overlay. It's *mutations* fired one at a time that strobe the banner; wrap a *sequence of those* in `cs agent run`.
- Don't nest sessions (`cs agent run` inside another `cs agent run`) — the inner start replaces the outer; the outer's `end` then no-ops on a stale id. One session per workflow.
- Don't hand-roll `/api/agent/session/start` unless you also guarantee the matching `end` on every exit path. `cs agent run` exists precisely so you don't have to.
