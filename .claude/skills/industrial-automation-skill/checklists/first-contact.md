# First contact checklist

Run this before using the IA2 runtime in a session. Repository-only maintenance does not require a server. For explicitly offline preparation, first read `offline-readiness.md`, stay on loopback, and do not probe or discover hardware. Three runtime unknowns remain: where's the `cs` binary, where's the server, and what's open.

## 1. Locate the `cs` binary

**Native Windows:** open the installed **IA2 Terminal** (process-local PATH),
or use `$cs = "$env:LOCALAPPDATA\IA2\bin\cs.exe"; & $cs --version` in
PowerShell. Release output is `target\x86_64-pc-windows-msvc\release\cs.exe`.
To install from source or an extracted Windows package, run
`powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\install-skill.ps1`.
`-SkillOnly` installs real copies in both user discovery paths without
symlink privileges. The Bash examples below apply to Unix shells; do not
run `command`, `lsof`, or heredocs in PowerShell.

```bash
command -v cs && cs --version     # the installer puts cs in ~/.local/bin (on PATH)
ls ./target/release/cs            # ...or it's the build output, if you're in a checkout
```
- On `$PATH` → use `cs`.
- In a checkout but no binary → `cargo build -p ia2-cli --release`, then `./target/release/cs`.
- Skill installed standalone (via `npx skills`), no checkout → clone + build the binaries once:
  `git clone --recursive https://github.com/supcon-international/ia2 && cd ia2 && ./scripts/install-skill.sh` (puts `cs` + `ia2-server` in `~/.local/bin`).
- Set `CS=` to whichever you found so every later command is unambiguous.

## 2. Discover the server URL

On Windows, verify `& $cs api GET /health --server http://127.0.0.1:3001`.
If unavailable, open **IA2 IDE** or run
`Start-Process "$env:LOCALAPPDATA\IA2\bin\IA2.exe"`; wait for the desktop
window and repeat the health check. Closing the window hides it to the
tray and keeps the backend running. Tray Exit refuses while a PLC is
running; stop it explicitly first. Do not start a
duplicate instance merely because `cs` was not on the current PATH.

`cs` defaults to `http://127.0.0.1:3001` — the conventional port for a dev server (`cargo run -p server`) or an installed `ia2-server`. Resolve it:

```bash
# 1. The conventional port?
curl -sf -m 1 http://127.0.0.1:3001/api/health >/dev/null && SRV=http://127.0.0.1:3001

# 2. A server someone started on another port? Find any listening
#    ia2-server (`+c0` so the command name isn't truncated) and confirm
#    /api/health — the health probe also skips the demo-modbus listener
#    the same process may hold:
if [ -z "$SRV" ]; then
  for p in $(lsof -nP -iTCP -sTCP:LISTEN +c0 2>/dev/null | grep ia2-server | grep -oE '127\.0\.0\.1:[0-9]+' | cut -d: -f2); do
    curl -sf -m 1 "http://127.0.0.1:$p/api/health" 2>/dev/null | grep -q '"status":"ok"' && { SRV="http://127.0.0.1:$p"; break; }
  done
fi
echo "SRV=$SRV"
```

If nothing answers, no server is running. Start one: `ia2-server --bind 127.0.0.1:3001 &` (installed) or `cargo run -p server` (checkout), then `SRV=http://127.0.0.1:3001`. Don't proceed without a reachable server. Once `SRV` is set, the CLI-native liveness check is `cs api GET /health --server "$SRV"` (exit 0, prints uptime + scan count) — the same `/health` the curl probes, but through `cs`.

> Tip: some sessions persist the URL in `/tmp/ia2_srv`. Check there first: `SRV=$(cat /tmp/ia2_srv 2>/dev/null)` then validate it with a health probe before trusting it.

## 3. See what's open

```bash
cs ls projects --server "$SRV"
```
- **Zero projects** → you'll `cs project create` or `cs project open` as the first real step.
- **One project** → you can omit `--project` on later commands (the active fallback is correct).
- **Two or more** → you **must** pass `--project NAME` on every command. Note which one the user's IDE window is showing (its URL `?project=`), and target that one, or confirm with the user.

Orient any time with bare `cs ls` — it prints the resource kinds (`pous`, `devices`, `edges`, `hmi`, `library`, `projects`, `device-catalog`), the self-discovery entry point for the quartet (`ls` / `get` / `set` / `rm`).

## 4. If you're about to do multi-step work

Stop and set up a session wrapper (`03-agent-sessions.md`). Don't fire commands one at a time — the overlay strobes. Draft the whole sequence, then:

```bash
cs agent run --label "<what you're about to do>" --server "$SRV" -- bash -c '... whole workflow ...'
```

## Ready check

You're ready to work when all of these are true:
- [ ] `CS` points at a real binary
- [ ] `cs api GET /health --server "$SRV"` exits 0 (`"status":"ok"`)
- [ ] You know how many projects are open and which to target
- [ ] Multi-step work is wrapped in `cs agent run`
