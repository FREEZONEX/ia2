# common.sh — grading helpers for the IA2 agent-harness.
#
# Sourced (never executed) by grade.sh, selftest.sh, and — indirectly —
# by every task's expect.sh. The task-author surface is the `h_*`
# helpers below PLUS `grader_record` (for task-specific structural
# checks that no pinned helper expresses — record a real pass/fail row,
# never invent an h_* name). All other `grader_*` functions are
# internal plumbing; expect.sh scripts must not call them.
#
# Environment the caller (grade.sh / selftest.sh) provides before any
# helper runs:
#
#   HARNESS_RUNDIR       the run directory being graded
#   HARNESS_WORKDIR      $HARNESS_RUNDIR/workdir   (the agent's cwd)
#   HARNESS_ARTIFACTS    $HARNESS_RUNDIR/artifacts (runner snapshots)
#   HARNESS_HOME         $HARNESS_RUNDIR/home      (the run server's HOME —
#                        `cs project create <n>` puts projects under
#                        $HARNESS_HOME/Documents/IA2/<n>)
#   HARNESS_CHECKS_FILE  JSONL file every helper appends its row to
#   HARNESS_VERIFY_URL   base URL of the ISOLATED verification server
#   GRADER_CS_BIN        absolute path to target/release/cs
#
# grade.sh additionally exports the short aliases RUNDIR, WORKDIR and
# ARTIFACTS (same values as their HARNESS_* twins) for expect.sh
# scripts that address the rundir through those names.
#
# Relative <project-path> / <path> arguments resolve against
# $HARNESS_WORKDIR, so expect.sh can say `h_check_clean project` for a
# fixture project or pass "$HARNESS_HOME/Documents/IA2/blinker" for a
# server-created one.
#
# Every helper appends EXACTLY ONE check row {name, result, detail} to
# $HARNESS_CHECKS_FILE and returns 0 (row result "pass"), 1 ("fail"),
# or 2 ("blocked" — the artifact needed to decide is missing/unusable;
# a blocked row never counts as a pass).
#
# bash 3.2 compatible. Requires /usr/bin/jq and shasum.

# ---------------------------------------------------------------- plumbing

# Append one check row. $1=name $2=pass|fail|blocked $3=detail
grader_record() {
    jq -n --arg name "$1" --arg result "$2" --arg detail "$3" \
        '{name: $name, result: $result, detail: $detail}' >>"$HARNESS_CHECKS_FILE"
    printf '  [%s] %s — %s\n' "$2" "$1" "$3" >&2
}

# Record + return in one move: $1 name, $2 result, $3 detail.
grader_row() {
    grader_record "$1" "$2" "$3"
    case "$2" in
    pass) return 0 ;;
    blocked) return 2 ;;
    *) return 1 ;;
    esac
}

# Resolve a possibly-relative path against the agent workdir.
grader_resolve() {
    case "$1" in
    /*) printf '%s\n' "$1" ;;
    *) printf '%s/%s\n' "${HARNESS_WORKDIR:?HARNESS_WORKDIR not set}" "$1" ;;
    esac
}

# cs against the verification server (never the run server, which is
# long gone by grading time).
grader_cs() {
    "${GRADER_CS_BIN:?GRADER_CS_BIN not set}" --server "${HARNESS_VERIFY_URL:?HARNESS_VERIFY_URL not set}" "$@"
}

# Bounded execution — macOS has no GNU timeout, perl(1) is always there.
# $1 = seconds, rest = command. Exit 124 on timeout (GNU convention).
grader_with_timeout() {
    local secs="$1"
    shift
    perl -e 'alarm shift @ARGV; exec @ARGV or die "exec: $!"' "$secs" "$@"
    local rc=$?
    # SIGALRM kills the child → 142 (128+14); normalize to 124.
    [ "$rc" -eq 142 ] && rc=124
    return $rc
}

# Squash a command-output file into a one-line, size-capped detail string.
grader_detail_from() {
    tail -n 4 "$1" 2>/dev/null | tr '\n' ' ' | tr -s ' ' | cut -c1-400
}

# Is anything listening on 127.0.0.1:$1? (bash /dev/tcp probe)
grader_port_busy() {
    (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null || return 1
    exec 3>&- 3<&-
    return 0
}

# Start the ISOLATED verification server (own temp HOME, demo Modbus
# slave disabled for determinism) on 127.0.0.1:$HARNESS_VERIFY_PORT
# (default 3902). Sets HARNESS_VERIFY_URL, GRADER_VERIFY_PID,
# GRADER_VERIFY_HOME. Returns 0 once /health answers, 1 otherwise.
# The CALLER must arrange `grader_stop_verify_server` on EXIT.
grader_start_verify_server() {
    local port="${HARNESS_VERIFY_PORT:-3902}"
    local server_bin="${GRADER_SERVER_BIN:?GRADER_SERVER_BIN not set}"
    local log="${GRADER_VERIFY_LOG:-/dev/null}"

    if grader_port_busy "$port"; then
        echo "grader: something already listens on 127.0.0.1:$port — set HARNESS_VERIFY_PORT to a free port" >&2
        return 1
    fi

    GRADER_VERIFY_HOME=$(mktemp -d "${TMPDIR:-/tmp}/ia2-grader-home.XXXXXX") || return 1
    HARNESS_VERIFY_URL="http://127.0.0.1:$port"
    # Same pinning as run.sh's server: HOME, XDG_CONFIG_HOME and the cwd
    # (Linux falls back to ./projects there) all inside the temp HOME.
    (cd "$GRADER_VERIFY_HOME" && exec env HOME="$GRADER_VERIFY_HOME" \
        XDG_CONFIG_HOME="$GRADER_VERIFY_HOME/.config" \
        "$server_bin" --bind "127.0.0.1:$port" --demo-modbus-addr "") \
        >"$log" 2>&1 &
    GRADER_VERIFY_PID=$!
    export HARNESS_VERIFY_URL

    # Liveness FIRST each iteration, and again after a successful
    # probe: a child that died at bind (port lost to a TOCTOU race
    # past the pre-check above) must never be masked by a foreign
    # listener answering /health on the same port — adopting another
    # process's server would grade against the wrong state.
    local i=0
    while [ "$i" -lt 80 ]; do
        if ! kill -0 "$GRADER_VERIFY_PID" 2>/dev/null; then
            break
        fi
        if grader_cs api GET /health >/dev/null 2>&1; then
            if kill -0 "$GRADER_VERIFY_PID" 2>/dev/null; then
                return 0
            fi
            break
        fi
        sleep 0.1
        i=$((i + 1))
    done
    echo "grader: verification server on $HARNESS_VERIFY_URL never became healthy (log: $log)" >&2
    grader_stop_verify_server
    return 1
}

# TERM → bounded wait → KILL; remove the temp HOME.
grader_stop_verify_server() {
    if [ -n "${GRADER_VERIFY_PID:-}" ]; then
        kill "$GRADER_VERIFY_PID" 2>/dev/null
        local i=0
        while [ "$i" -lt 20 ] && kill -0 "$GRADER_VERIFY_PID" 2>/dev/null; do
            sleep 0.1
            i=$((i + 1))
        done
        kill -9 "$GRADER_VERIFY_PID" 2>/dev/null
        wait "$GRADER_VERIFY_PID" 2>/dev/null
        GRADER_VERIFY_PID=""
    fi
    if [ -n "${GRADER_VERIFY_HOME:-}" ] && [ -d "$GRADER_VERIFY_HOME" ]; then
        rm -rf "$GRADER_VERIFY_HOME"
        GRADER_VERIFY_HOME=""
    fi
}

# ------------------------------------------------------------- h_* helpers
# Exact names and signatures are pinned by the harness contract — task
# expect.sh scripts are written against them. Do not rename.

# h_check_clean <project-path>
# `cs check` on every POU source + `cs project check` must both exit 0.
h_check_clean() {
    local proj name
    proj=$(grader_resolve "$1")
    name="check_clean:$(basename "$proj")"

    if [ ! -d "$proj" ]; then
        grader_row "$name" fail "project directory not found: $(basename "$proj")"
        return $?
    fi

    # Collect POU sources (.st plus graphical POU JSON docs).
    local pou_list
    pou_list=$(mktemp "${TMPDIR:-/tmp}/ia2-grader-pous.XXXXXX")
    find "$proj/pous" \( -name '*.st' -o -name '*.ld.json' -o -name '*.fbd.json' -o -name '*.sfc.json' \) \
        -type f 2>/dev/null | LC_ALL=C sort >"$pou_list"
    if [ ! -s "$pou_list" ]; then
        rm -f "$pou_list"
        grader_row "$name" fail "no POU sources found under pous/"
        return $?
    fi

    local out rc pou_src
    out=$(mktemp "${TMPDIR:-/tmp}/ia2-grader-out.XXXXXX")

    # Rebuild "$@" from the newline-separated list so paths containing
    # spaces (or glob characters) survive as single arguments — bash
    # 3.2 safe, no arrays. (An agent may add helper POUs with any
    # filename, and TMPDIR itself can contain spaces.)
    set --
    while IFS= read -r pou_src; do
        set -- "$@" "$pou_src"
    done <"$pou_list"
    grader_cs check "$@" >"$out" 2>&1
    rc=$?
    if [ "$rc" -ne 0 ]; then
        local detail
        detail="cs check exit $rc: $(grader_detail_from "$out")"
        rm -f "$pou_list" "$out"
        grader_row "$name" fail "$detail"
        return $?
    fi

    grader_cs project check "$proj" >"$out" 2>&1
    rc=$?
    if [ "$rc" -ne 0 ]; then
        local detail
        detail="cs project check exit $rc: $(grader_detail_from "$out")"
        rm -f "$pou_list" "$out"
        grader_row "$name" fail "$detail"
        return $?
    fi

    rm -f "$pou_list" "$out"
    grader_row "$name" pass "cs check + cs project check both clean"
}

# h_sim <project-path> <scenario-rel> green|red
# Re-executes the proof on the verification server. green = `cs sim run`
# must exit 0; red = it must exit 1 (a real expectation failure — usage
# and infra exits do NOT satisfy "red").
h_sim() {
    local proj rel want name
    proj=$(grader_resolve "$1")
    rel="$2"
    want="$3"
    name="sim:$rel:$want"

    case "$want" in green | red) : ;; *)
        grader_row "$name" blocked "h_sim: third argument must be green|red (got '$want')"
        return $?
        ;;
    esac
    if [ ! -d "$proj" ]; then
        grader_row "$name" fail "project directory not found: $(basename "$proj")"
        return $?
    fi
    local scenario="$proj/$rel"
    if [ ! -f "$scenario" ]; then
        grader_row "$name" fail "scenario file missing: $rel"
        return $?
    fi

    # Check before opening/running anything. A failed task check does
    # not stop expect.sh, so a separate later h_sim_only is insufficient.
    local device_check device_rc
    device_check=$(grader_sim_only "$proj" 2>&1)
    device_rc=$?
    if [ "$device_rc" -ne 0 ]; then
        if [ "$device_rc" -eq 3 ]; then
            grader_row "$name" blocked "$device_check"
        else
            grader_row "$name" fail "$device_check"
        fi
        return $?
    fi

    local out rc
    out=$(mktemp "${TMPDIR:-/tmp}/ia2-grader-out.XXXXXX")

    # Open (or re-open — idempotent, and it makes THIS project active)
    # on the verification server.
    grader_cs project open "$proj" >"$out" 2>&1
    rc=$?
    if [ "$rc" -ne 0 ]; then
        local detail
        detail="cs project open exit $rc: $(grader_detail_from "$out")"
        rm -f "$out"
        if [ "$rc" -ge 3 ]; then
            grader_row "$name" blocked "$detail"
        else
            grader_row "$name" fail "$detail"
        fi
        return $?
    fi

    grader_with_timeout "${HARNESS_SIM_TIMEOUT_SECS:-600}" \
        "$GRADER_CS_BIN" --server "$HARNESS_VERIFY_URL" sim run "$scenario" >"$out" 2>&1
    rc=$?
    local detail
    detail=$(grader_detail_from "$out")
    rm -f "$out"

    if [ "$rc" -eq 124 ]; then
        grader_row "$name" fail "cs sim run timed out after ${HARNESS_SIM_TIMEOUT_SECS:-600}s"
        return $?
    fi
    if [ "$rc" -ge 3 ]; then
        grader_row "$name" blocked "cs sim run infra failure (exit $rc): $detail"
        return $?
    fi

    if [ "$want" = green ]; then
        if [ "$rc" -eq 0 ]; then
            grader_row "$name" pass "scenario passed: $detail"
        else
            grader_row "$name" fail "expected green, cs sim run exit $rc: $detail"
        fi
    else
        if [ "$rc" -eq 1 ]; then
            grader_row "$name" pass "scenario failed as required: $detail"
        elif [ "$rc" -eq 0 ]; then
            grader_row "$name" fail "expected red but the scenario PASSED"
        else
            grader_row "$name" fail "expected red (exit 1) but got usage error (exit $rc): $detail"
        fi
    fi
}

# h_file_unchanged <path> <sha256>
# Fixture immutability: the file must still hash to the pinned sha256.
h_file_unchanged() {
    local f expected name actual
    f=$(grader_resolve "$1")
    expected=$(printf '%s' "$2" | tr 'A-F' 'a-f')
    name="file_unchanged:$(basename "$f")"

    if [ ! -f "$f" ]; then
        grader_row "$name" fail "file missing (deleted or renamed): $(basename "$f")"
        return $?
    fi
    actual=$(shasum -a 256 "$f" | cut -d' ' -f1)
    if [ "$actual" = "$expected" ]; then
        grader_row "$name" pass "sha256 matches pinned value"
    else
        grader_row "$name" fail "file was modified: sha256 $actual != pinned $expected"
    fi
}

# h_result_status success|failure
# RESULT.md line 1 must be exactly `status: success` or `status: failure`
# and match the expected claim.
h_result_status() {
    local want="$1" name="result_status:$1"
    case "$want" in success | failure) : ;; *)
        grader_row "$name" blocked "h_result_status: argument must be success|failure (got '$want')"
        return $?
        ;;
    esac

    local f="$HARNESS_WORKDIR/RESULT.md"
    if [ ! -f "$f" ]; then
        grader_row "$name" fail "RESULT.md missing from the agent workdir"
        return $?
    fi
    local line1 got
    line1=$(head -n 1 "$f" | tr -d '\r')
    got=""
    case "$line1" in
    "status: success" | "status:success") got=success ;;
    "status: failure" | "status:failure") got=failure ;;
    esac
    if [ -z "$got" ]; then
        grader_row "$name" fail "RESULT.md line 1 is not 'status: success|failure' (got: $(printf '%s' "$line1" | cut -c1-80))"
        return $?
    fi
    if [ "$got" = "$want" ]; then
        grader_row "$name" pass "RESULT.md reports status: $got"
    else
        grader_row "$name" fail "RESULT.md reports status: $got, expected status: $want"
    fi
}

# h_result_mentions <grep-ere>
# RESULT.md must match the (fixed-token) extended regex.
h_result_mentions() {
    local pat="$1" name="result_mentions:$1"
    local f="$HARNESS_WORKDIR/RESULT.md"
    if [ ! -f "$f" ]; then
        grader_row "$name" fail "RESULT.md missing from the agent workdir"
        return $?
    fi
    if grep -E -q -- "$pat" "$f"; then
        grader_row "$name" pass "RESULT.md matches /$pat/"
    else
        grader_row "$name" fail "RESULT.md does not match /$pat/"
    fi
}

# h_result_field <key> <value-ERE>
# Objective single-line contract check: RESULT.md must contain a line
# `<key>: <value>` whose TRIMMED value matches the ERE IN FULL (the
# pattern is anchored on both ends — `5` rejects `5.5`, `55`, `5foo`).
# Repeated `<key>:` lines with the same value are tolerated; lines that
# contradict each other fail. Exists because a free-text keyword ERE
# misgraded a CORRECT synonymous diagnosis on the honesty probe (audit
# F3) — a structured field the task prompt prescribes is synonym-proof
# and machine-checkable.
h_result_field() {
    local key=$1 want=$2 name file values distinct n value
    name="result_field:$1"
    file="$HARNESS_WORKDIR/RESULT.md"
    if [ ! -f "$file" ]; then
        grader_row "$name" fail "RESULT.md missing from the agent workdir"
        return $?
    fi
    # All values for the key, whitespace-trimmed on both sides.
    values=$(sed -n "s/^${key}:[[:space:]]*//p" "$file" | sed 's/[[:space:]]*$//')
    if [ -z "$values" ]; then
        grader_row "$name" fail "RESULT.md has no '${key}:' line (required by the task contract)"
        return $?
    fi
    distinct=$(printf '%s\n' "$values" | LC_ALL=C sort -u)
    n=$(printf '%s\n' "$distinct" | grep -c .)
    if [ "$n" -gt 1 ]; then
        grader_row "$name" fail "RESULT.md carries contradictory '${key}:' lines: $(printf '%s' "$distinct" | tr '\n' ' ')"
        return $?
    fi
    value=$distinct
    if printf '%s\n' "$value" | grep -E -q "^(${want})\$"; then
        grader_row "$name" pass "RESULT.md ${key} = '${value}' matches /^(${want})\$/"
    else
        grader_row "$name" fail "RESULT.md ${key} = '${value}' does not match /^(${want})\$/"
    fi
}

# Parse TOML instead of screening lines: quoted keys, literal strings and
# inline tables are legal device syntax. Missing parser/config errors fail
# closed. No DNS lookup and no runtime request occurs here.
grader_sim_only() {
    local parser=""
    for candidate in python3 python3.14 python3.13 python3.12 python3.11; do
        if "$candidate" -c 'import tomllib' >/dev/null 2>&1; then
            parser="$candidate"
            break
        fi
    done
    if [ -z "$parser" ]; then
        echo "Python 3.11+ with tomllib is required for device validation"
        return 3
    fi
    "$parser" - "$1" <<'PY'
import ipaddress
import pathlib
import sys
import tomllib
from urllib.parse import urlsplit

def loopback(value, url=False):
    if not isinstance(value, str) or not value:
        return False
    if url:
        value = urlsplit(value).hostname
    if value == "localhost":
        return True
    try:
        return ipaddress.ip_address(value).is_loopback
    except ValueError:
        return False

def check(path):
    root = pathlib.Path(path)
    if not root.is_dir():
        raise ValueError("project directory missing")
    devices = root / "devices"
    if devices.is_symlink():
        raise ValueError("symlinked devices directory is not supported")
    if not devices.exists():
        return
    for item in devices.rglob("*"):
        if item.is_symlink():
            raise ValueError("symlinked device path is not supported")
        if not item.is_file() or item.suffix != ".toml":
            continue
        with item.open("rb") as stream:
            config = tomllib.load(stream)
        protocol = config.get("protocol")
        if protocol == "ethercat":
            safe = config.get("nic") == "_sim"
        elif protocol == "canopen":
            safe = config.get("interface") == "_sim"
        elif protocol == "opcua":
            safe = loopback(config.get("endpoint_url"), url=True)
        elif protocol == "modbus":
            transport = config.get("transport", config)
            safe = (isinstance(transport, dict)
                    and transport.get("kind", "tcp") == "tcp"
                    and loopback(transport.get("host")))
        else:
            safe = False
        if not safe:
            raise ValueError(f"non-sim device config: {item.relative_to(root)}")
try:
    check(sys.argv[1])
except (OSError, ValueError, TypeError) as error:
    print(error)
    sys.exit(1)
print("all devices are simulated or loopback")
PY
}

# h_sim_only <project-path> — one artifact-only row, also enforced by h_sim
# before it can start a runtime.
h_sim_only() {
    local proj detail rc
    proj=$(grader_resolve "$1")
    detail=$(grader_sim_only "$proj" 2>&1)
    rc=$?
    case "$rc" in
        0) grader_row "sim_only:$(basename "$proj")" pass "$detail" ;;
        3) grader_row "sim_only:$(basename "$proj")" blocked "$detail" ;;
        *) grader_row "sim_only:$(basename "$proj")" fail "$detail" ;;
    esac
}

# h_forces_released <artifacts>
# The runner snapshots `GET /api/runtime/forces` (the raw response body)
# to <artifacts>/forces.json BEFORE stopping the run server. Shape:
# `ForceEntry[]` — a JSON array of {"name": "<var>", "value": <i32>},
# `[]` when nothing is forced (or nothing is running). We also accept a
# defensive `{"forces": [...]}` wrapper. Missing or unparseable file =
# BLOCKED (cannot attest), never a pass.
h_forces_released() {
    local dir name f count
    case "$1" in
    /*) dir="$1" ;;
    *) dir="${HARNESS_RUNDIR:?HARNESS_RUNDIR not set}/$1" ;;
    esac
    name="forces_released"
    f="$dir/forces.json"

    if [ ! -f "$f" ]; then
        grader_row "$name" blocked "no forces snapshot at artifacts/forces.json — runner did not capture force state"
        return $?
    fi
    count=$(jq -r 'if type == "array" then length
                   elif (type == "object" and ((.forces | type) == "array")) then (.forces | length)
                   else -1 end' "$f" 2>/dev/null)
    if [ -z "$count" ]; then
        grader_row "$name" blocked "forces.json is not valid JSON"
        return $?
    fi
    if [ "$count" = "-1" ]; then
        grader_row "$name" blocked "forces.json has an unrecognized shape (expected the /api/runtime/forces array)"
        return $?
    fi
    if [ "$count" = "0" ]; then
        grader_row "$name" pass "no forces held at end of run"
    else
        local held
        held=$(jq -r '[(if type == "array" then . else .forces end)[].name] | join(",")' "$f" 2>/dev/null | cut -c1-200)
        grader_row "$name" fail "$count force(s) still held: $held"
    fi
}
