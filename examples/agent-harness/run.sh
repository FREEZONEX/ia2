#!/bin/bash
# run.sh — agent-harness runner.
#
# Runs ONE coding-agent adapter against ONE task in an isolated sandbox
# (temp run dir, temp server HOME, dedicated loopback port, PATH-shimmed
# `cs`), captures the transcript and artifacts, then hands the run dir to
# the grader and reports its verdict.
#
#   ./run.sh <agent> <task-id> [--keep|--clean]
#
# Exit codes:
#   0  verdict overall = pass
#   1  verdict overall = fail
#   2  usage error
#   3  infrastructure / blocked (missing binaries, busy port, task or
#      grader not present yet, adapter blocked, no verdict)
#
# macOS bash 3.2 compatible. No files outside the run dir and
# examples/agent-harness/runs/ are ever written.

set -u

usage() {
  cat <<'EOF'
Usage: run.sh <agent> <task-id> [--keep|--clean]

  <agent>    adapter name: a script agents/<agent>.sh (claude-code, codex)
  <task-id>  task directory under tasks/ (e.g. t1-guided)
  --keep     keep the run directory (explicit; keeping is also the default)
  --clean    delete the temp run directory when the run finishes
             (the scrubbed transcript/verdict copy under runs/ is kept)

Environment:
  HARNESS_PORT          isolated server port (default 3901)
  HARNESS_VERIFY_PORT   grader verification-server port
                        (default HARNESS_PORT+1)
  HARNESS_TIMEOUT_SECS  adapter time budget in seconds (default 1200)

The run directory path is printed at the end of every kept run.
EOF
}

# ---------------------------------------------------------------- args
AGENT=""
TASK=""
KEEP=0
CLEAN=0
for arg in "$@"; do
  case "$arg" in
    -h|--help) usage; exit 0 ;;
    --keep)  KEEP=1 ;;
    --clean) CLEAN=1 ;;
    -*)
      echo "run.sh: unknown option: $arg" >&2
      usage >&2
      exit 2
      ;;
    *)
      if [ -z "$AGENT" ]; then
        AGENT="$arg"
      elif [ -z "$TASK" ]; then
        TASK="$arg"
      else
        echo "run.sh: unexpected extra argument: $arg" >&2
        usage >&2
        exit 2
      fi
      ;;
  esac
done
if [ -z "$AGENT" ] || [ -z "$TASK" ]; then
  usage >&2
  exit 2
fi
if [ "$KEEP" -eq 1 ] && [ "$CLEAN" -eq 1 ]; then
  echo "run.sh: --keep and --clean are mutually exclusive" >&2
  exit 2
fi

JQ=/usr/bin/jq

# ------------------------------------------------- 1. repo root + binaries
HARNESS_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$HARNESS_DIR/../.." && pwd)
SERVER_BIN="$REPO_ROOT/target/release/server"
CS_BIN="$REPO_ROOT/target/release/cs"

for bin in "$SERVER_BIN" "$CS_BIN"; do
  if [ ! -x "$bin" ]; then
    echo "run.sh: missing binary: $bin" >&2
    echo "  build it first: cargo build --release -p server -p ia2-cli" >&2
    exit 3
  fi
done

sha256_file() {
  shasum -a 256 "$1" | awk '{print $1}'
}

# Deterministic tree hash: sha256 of the sorted per-file sha256 listing.
tree_sha256() {
  (cd "$1" && find . -type f | LC_ALL=C sort \
     | while IFS= read -r f; do shasum -a 256 "$f"; done \
     | shasum -a 256 | awk '{print $1}')
}

SERVER_SHA=$(sha256_file "$SERVER_BIN")
CS_SHA=$(sha256_file "$CS_BIN")

ADAPTER="$HARNESS_DIR/agents/$AGENT.sh"
if [ ! -f "$ADAPTER" ]; then
  echo "run.sh: unknown agent '$AGENT' (no agents/$AGENT.sh). Available adapters:" >&2
  ls "$HARNESS_DIR/agents" 2>/dev/null | sed -e 's/\.sh$//' -e 's/^/  /' >&2
  exit 2
fi

TASK_DIR="$HARNESS_DIR/tasks/$TASK"
if [ ! -f "$TASK_DIR/prompt.md" ]; then
  echo "run.sh: task '$TASK' not present yet (expected tasks/$TASK/prompt.md)" >&2
  exit 3
fi

# ------------------------------------------------------------- port checks
PORT="${HARNESS_PORT:-3901}"
port_busy() {
  nc -z 127.0.0.1 "$1" >/dev/null 2>&1
}
if port_busy "$PORT"; then
  echo "run.sh: port $PORT is already in use — set HARNESS_PORT to a free port" >&2
  exit 3
fi
PORT_3001_BUSY=false
if port_busy 3001; then
  PORT_3001_BUSY=true
  echo "run.sh: WARNING: something is listening on 127.0.0.1:3001 (a real IA2" >&2
  echo "  server?). This run talks only to 127.0.0.1:$PORT and the workdir's" >&2
  echo "  cs shim pins that URL, but the collision is recorded in meta.json." >&2
fi
SERVER_URL="http://127.0.0.1:$PORT"

# ---------------------------------------------------------------- 2. rundir
RUNDIR=$(mktemp -d "${TMPDIR:-/tmp}/ia2-harness.XXXXXX")
mkdir -p "$RUNDIR/workdir" "$RUNDIR/home" "$RUNDIR/artifacts"
: > "$RUNDIR/transcript.txt"

SERVER_PID=""
ARCHIVE_READY=0

stop_server() {
  if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill -TERM "$SERVER_PID" 2>/dev/null || true
    i=0
    while [ $i -lt 40 ] && kill -0 "$SERVER_PID" 2>/dev/null; do
      sleep 0.25
      i=$((i + 1))
    done
    if kill -0 "$SERVER_PID" 2>/dev/null; then
      kill -KILL "$SERVER_PID" 2>/dev/null || true
    fi
  fi
  SERVER_PID=""
}

# Copy the shareable subset of a finished run (scrubbed transcript, meta,
# verdict, artifacts — never workdir/ or home/) under runs/. runs/ is
# gitignored; publishing anything from it is a separate human decision.
archive_run() {
  stamp=$(date -u +%Y%m%dT%H%M%SZ)
  archive_dir="$HARNESS_DIR/runs/$stamp-$AGENT-$TASK-$$"
  mkdir -p "$archive_dir"
  for f in transcript.txt meta.json verdict.json integrity.json; do
    [ -f "$RUNDIR/$f" ] && cp "$RUNDIR/$f" "$archive_dir/"
  done
  [ -d "$RUNDIR/artifacts" ] && cp -R "$RUNDIR/artifacts" "$archive_dir/artifacts"
  echo "run record: $archive_dir"
}

cleanup() {
  code=$?
  stop_server
  if [ "$ARCHIVE_READY" -eq 1 ]; then
    archive_run
  fi
  if [ "$CLEAN" -eq 1 ]; then
    rm -rf "$RUNDIR"
  else
    echo "run dir: $RUNDIR"
  fi
  exit "$code"
}
trap cleanup EXIT

# ----------------------------------------------------- 3. populate workdir
if [ -d "$TASK_DIR/fixture" ]; then
  if [ -f "$TASK_DIR/fixture/project.toml" ]; then
    # The fixture IS a project (t3/t4): agents find it at workdir/project.
    cp -R "$TASK_DIR/fixture" "$RUNDIR/workdir/project"
  else
    cp -R "$TASK_DIR/fixture/." "$RUNDIR/workdir/"
  fi
fi

# Prompts state the server URL literally (written for the default port
# 3901); rewrite it to THIS run's real URL so a HARNESS_PORT override
# keeps the prompt truthful. Prompts stay static in git.
sed "s|http://127.0.0.1:3901|$SERVER_URL|g" "$TASK_DIR/prompt.md" \
  > "$RUNDIR/workdir/prompt.md"

SKILL_SRC="$REPO_ROOT/.claude/skills/industrial-automation-skill"
if [ ! -d "$SKILL_SRC" ]; then
  echo "run.sh: skill not found at .claude/skills/industrial-automation-skill" >&2
  exit 3
fi
mkdir -p "$RUNDIR/workdir/.claude/skills"
cp -R "$SKILL_SRC" "$RUNDIR/workdir/.claude/skills/industrial-automation-skill"
SKILL_SHA=$(tree_sha256 "$RUNDIR/workdir/.claude/skills/industrial-automation-skill")

cat > "$RUNDIR/workdir/AGENTS.md" <<'EOF'
This directory is a self-contained IA2 exercise: your task is in
`prompt.md` right here — read it first and follow it exactly. The full
IA2 / IEC 61131-3 playbook (the `cs` CLI, POU authoring, simulation,
alarms) is the skill at `.claude/skills/industrial-automation-skill/`;
read its `SKILL.md` and open its `references/*.md` on demand. A `cs`
binary is already on your PATH and reaches the exercise server; finish
by writing `RESULT.md` exactly as `prompt.md` requires.
EOF

# ------------------------------------------------------------- 4. PATH shim
# Safety rail: even if the agent forgets `--server`, this `cs` can only
# reach the isolated harness server — never a user's real :3001 instance.
mkdir -p "$RUNDIR/workdir/bin"
cat > "$RUNDIR/workdir/bin/cs" <<EOF
#!/bin/bash
exec "$CS_BIN" --server "$SERVER_URL" "\$@"
EOF
chmod +x "$RUNDIR/workdir/bin/cs"

# ------------------------------------------- 4b. integrity manifest
# Recorded BEFORE the agent runs. grader/grade.sh recomputes all five
# hashes at grade time and refuses to grade (overall = blocked) on any
# mismatch — a trust root the agent-under-test rewrote must never
# grade pass/fail. Recipes (keep in lockstep with grade.sh):
#   single file  : shasum -a 256 <file>
#   grader pair  : cat grade.sh common.sh (that order) | shasum -a 256
#   fixture tree : shasum -a 256 of the sorted `sha  ./relpath`
#                  per-file listing (tree_sha256 above); empty string
#                  when the task ships no fixture/
EXPECT_SHA=""
if [ -f "$TASK_DIR/expect.sh" ]; then
  EXPECT_SHA=$(sha256_file "$TASK_DIR/expect.sh")
fi
GRADER_SHA=""
if [ -f "$HARNESS_DIR/grader/grade.sh" ] && [ -f "$HARNESS_DIR/grader/common.sh" ]; then
  GRADER_SHA=$(cat "$HARNESS_DIR/grader/grade.sh" "$HARNESS_DIR/grader/common.sh" \
    | shasum -a 256 | awk '{print $1}')
fi
FIXTURE_SHA=""
if [ -d "$TASK_DIR/fixture" ]; then
  FIXTURE_SHA=$(tree_sha256 "$TASK_DIR/fixture")
fi
"$JQ" -n \
  --arg expect_sha256 "$EXPECT_SHA" \
  --arg grader_sha256 "$GRADER_SHA" \
  --arg fixture_sha256 "$FIXTURE_SHA" \
  --arg cs_sha256 "$CS_SHA" \
  --arg server_sha256 "$SERVER_SHA" \
  '{
     expect_sha256: $expect_sha256,
     grader_sha256: $grader_sha256,
     fixture_sha256: $fixture_sha256,
     cs_sha256: $cs_sha256,
     server_sha256: $server_sha256
   }' > "$RUNDIR/integrity.json"

# ------------------------------------------------------------ 5. start server
START_UTC=$(date -u +%Y-%m-%dT%H:%M:%SZ)
# HOME is enough on macOS. On Linux the server's config dir follows
# XDG_CONFIG_HOME first, and its projects dir falls back to ./projects
# under its cwd, so both are pinned to the run dir too. `exec` keeps $!
# the server's own PID.
(cd "$RUNDIR" && exec env HOME="$RUNDIR/home" XDG_CONFIG_HOME="$RUNDIR/home/.config" \
  "$SERVER_BIN" --bind "127.0.0.1:$PORT" --demo-modbus-addr "") \
  > "$RUNDIR/artifacts/server.log" 2>&1 &
SERVER_PID=$!
disown "$SERVER_PID" 2>/dev/null || true  # no job-control noise on teardown

# Liveness FIRST each iteration, and again after the first successful
# probe: a child that died at bind (port lost to a TOCTOU race) must
# never be masked by a foreign listener answering /health on $PORT —
# adopting another process's server would grade against the wrong run.
healthy=0
i=0
while [ $i -lt 50 ]; do
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    break
  fi
  if curl -fsS -m 2 "$SERVER_URL/health" >/dev/null 2>&1; then
    if kill -0 "$SERVER_PID" 2>/dev/null; then
      healthy=1
    fi
    break
  fi
  sleep 0.2
  i=$((i + 1))
done
if [ "$healthy" -ne 1 ]; then
  echo "run.sh: server never became healthy on $SERVER_URL — see artifacts/server.log in the run dir" >&2
  exit 3
fi

# ------------------------------------------------------------ 6. run adapter
AGENT_START=$(date +%s)
(
  cd "$RUNDIR/workdir" && \
  env PATH="$RUNDIR/workdir/bin:$PATH" \
      HARNESS_PROMPT="$RUNDIR/workdir/prompt.md" \
      HARNESS_SERVER_URL="$SERVER_URL" \
      HARNESS_TIMEOUT_SECS="${HARNESS_TIMEOUT_SECS:-1200}" \
      bash "$ADAPTER"
) > "$RUNDIR/transcript.txt" 2>&1
AGENT_EXIT=$?
AGENT_WALL=$(( $(date +%s) - AGENT_START ))

TOOL_VERSION="unknown"
first_line=$(head -n 1 "$RUNDIR/transcript.txt" 2>/dev/null || true)
case "$first_line" in
  "HARNESS_TOOL_VERSION: "*) TOOL_VERSION="${first_line#HARNESS_TOOL_VERSION: }" ;;
esac

# Resolved model (audit F7): adapters that can determine which model
# actually answered emit a trailing HARNESS_RESOLVED_MODEL line (the
# claude adapter lifts it from its stream-json events; codex-cli
# 0.153.4's --json stream exposes no model field, so that adapter
# emits nothing). Absent = null in meta.json — recorded, never guessed.
RESOLVED_MODEL=$(grep '^HARNESS_RESOLVED_MODEL: ' "$RUNDIR/transcript.txt" 2>/dev/null   | tail -n 1 | sed 's/^HARNESS_RESOLVED_MODEL: //')

# --------------------------------- 7. snapshots, teardown, meta.json
curl -s -m 5 "$SERVER_URL/api/runtime/status" \
  > "$RUNDIR/artifacts/runtime-status.json" 2>/dev/null || true
"$CS_BIN" --server "$SERVER_URL" get runtime/forces \
  > "$RUNDIR/artifacts/forces.json" 2>/dev/null || true

stop_server

END_UTC=$(date -u +%Y-%m-%dT%H:%M:%SZ)
"$JQ" -n \
  --arg agent "$AGENT" \
  --arg task "$TASK" \
  --arg started_utc "$START_UTC" \
  --arg finished_utc "$END_UTC" \
  --arg tool_version "$TOOL_VERSION" \
  --arg resolved_model "$RESOLVED_MODEL" \
  --arg server_sha256 "$SERVER_SHA" \
  --arg cs_sha256 "$CS_SHA" \
  --arg skill_tree_sha256 "$SKILL_SHA" \
  --argjson port "$PORT" \
  --argjson agent_exit_code "$AGENT_EXIT" \
  --argjson agent_wall_secs "$AGENT_WALL" \
  --argjson port_3001_busy "$PORT_3001_BUSY" \
  '{
     agent: $agent,
     task: $task,
     started_utc: $started_utc,
     finished_utc: $finished_utc,
     versions: {
       tool: $tool_version,
       resolved_model: (if $resolved_model == "" then null else $resolved_model end)
     },
     hashes: {
       server_sha256: $server_sha256,
       cs_sha256: $cs_sha256,
       skill_tree_sha256: $skill_tree_sha256
     },
     port: $port,
     agent_exit_code: $agent_exit_code,
     agent_wall_secs: $agent_wall_secs,
     warnings: { port_3001_busy: $port_3001_busy }
   }' > "$RUNDIR/meta.json"

# ------------------------------------------------------------ 8. scrub
# The token mask is left-anchored (lookbehind) so hyphenated words like
# "task-skipped" survive; ${HOME:-} keeps set -u happy in HOME-less
# environments (launchd/cron) — the perl guard already handles empty.
H_HOME="${HOME:-}" perl -pi -e '
  s/(?<![A-Za-z0-9_-])sk-[A-Za-z0-9_-]{8,}/sk-[MASKED]/g;
  my $h = $ENV{H_HOME};
  if (defined $h and length $h) { s/\Q$h\E/~/g; }
' "$RUNDIR/transcript.txt"

ARCHIVE_READY=1

if [ "$AGENT_EXIT" -eq 3 ]; then
  echo "run.sh: adapter '$AGENT' reported blocked (exit 3) — tool missing or unusable, not a task failure. See transcript.txt." >&2
  exit 3
fi

# ------------------------------------------------------------ 9. grade
GRADE_SH="$HARNESS_DIR/grader/grade.sh"
if [ ! -f "$GRADE_SH" ]; then
  echo "run.sh: grader not present yet (expected grader/grade.sh) — run captured but ungraded" >&2
  exit 3
fi

# Per the contract the verification server lives on PORT+1 by default,
# so concurrent runs with distinct HARNESS_PORT never collide on 3902;
# an explicit HARNESS_VERIFY_PORT is honored untouched.
export HARNESS_VERIFY_PORT="${HARNESS_VERIFY_PORT:-$((PORT + 1))}"

bash "$GRADE_SH" "$RUNDIR" "$TASK"
GRADE_EXIT=$?

VERDICT="$RUNDIR/verdict.json"
if [ ! -f "$VERDICT" ]; then
  echo "run.sh: grader exited $GRADE_EXIT without writing verdict.json" >&2
  exit 3
fi

OVERALL=$("$JQ" -r '.overall // "unknown"' "$VERDICT" 2>/dev/null || echo unknown)
echo "verdict: task=$TASK agent=$AGENT overall=$OVERALL"
"$JQ" -r '.checks[]? | "  [\(.result)] \(.name)\(if (.detail // "") == "" then "" else ": " + .detail end)"' \
  "$VERDICT" 2>/dev/null || true

case "$OVERALL" in
  pass) exit 0 ;;
  fail) exit 1 ;;
  *)    exit 3 ;;
esac
