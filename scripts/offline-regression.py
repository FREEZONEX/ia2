#!/usr/bin/env python3
"""Run IA2's local quality gate with a separate log and status for each command."""

import argparse
from datetime import datetime, timedelta, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import signal
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent
STEPS = {
    "adaptation": ("./scripts/check-agent-adaptation.sh",),
    "runner-tests": ("python3", "-B", "scripts/test-offline-regression.py"),
    "format": ("cargo", "fmt", "--all", "--", "--check"),
    "clippy": ("cargo", "clippy", "--locked", "--workspace", "--all-targets", "--", "-D", "warnings"),
    "debug-server": ("cargo", "build", "--locked", "-p", "server"),
    "workspace-tests": ("cargo", "test", "--locked", "--workspace"),
    "cli-sim": ("cargo", "test", "--locked", "-p", "ia2-cli", "--test", "sim_e2e"),
    "web-types": ("cargo", "test", "--locked", "-p", "server"),
    "release": ("cargo", "build", "--locked", "--release", "-p", "server", "-p", "ia2-cli",
                "-p", "ia2-runtime", "-p", "lsp-launcher"),
    "web-typecheck": ("pnpm", "--filter", "@cs/web", "exec", "tsc", "--noEmit"),
    "web-build": ("pnpm", "--filter", "@cs/web", "build"),
    "web-tests": ("pnpm", "--filter", "@cs/web", "test"),
    "diff": ("git", "diff", "--check"),
}
SUITES = {
    "full": (
        "adaptation", "runner-tests", "format", "clippy", "debug-server", "workspace-tests",
        "release", "web-typecheck", "web-build", "web-tests", "diff",
    ),
    "fmt": ("format",),
    "clippy": ("clippy",),
    "rust": ("debug-server", "workspace-tests", "diff"),
    "cli": ("debug-server", "cli-sim", "diff"),
    "web": ("web-types", "web-typecheck", "web-build", "web-tests", "diff"),
}
UTC8 = timezone(timedelta(hours=8))
SKIPPED = re.compile(
    r"^test .* \.\.\. ignored\b|\b[1-9][0-9]* (?:ignored|skipped|filtered out)\b",
    re.IGNORECASE,
)
RELEVANT_ENV = (
    "CARGO_BUILD_JOBS", "CARGO_BUILD_TARGET", "CARGO_TARGET_DIR", "CARGO_NET_OFFLINE",
    "RUSTFLAGS", "RUST_TEST_THREADS", "TS_RS_EXPORT_DIR", "IA2_TEST_SERVER_LOG_DIR", "TMPDIR",
)
CLEANUP_GRACE = 0.5


def timestamp():
    return datetime.now(UTC8).isoformat(timespec="milliseconds")


def capture(*command, required=True):
    try:
        return subprocess.check_output(command, cwd=ROOT, stderr=subprocess.STDOUT, timeout=15)
    except (OSError, subprocess.SubprocessError):
        if required:
            raise
        return b""


def git(*args, required=True):
    return capture("git", *args, required=required).decode("utf-8", errors="replace").strip()


def identity():
    changes = git("status", "--porcelain=v1", "--untracked-files=all", "--ignore-submodules=none")
    return {
        "head": git("rev-parse", "HEAD"),
        "tree": git("rev-parse", "HEAD^{tree}"),
        "dirty_summary": changes,
        "tracked_diff_sha256": hashlib.sha256(capture("git", "diff", "--binary", "HEAD")).hexdigest(),
        "submodules": git("submodule", "status", "--recursive"),
        "locks": {
            name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
            for name in ("Cargo.lock", "pnpm-lock.yaml")
        },
    }


def save(report, directory):
    pending = directory / "summary.json.tmp"
    pending.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    pending.replace(directory / "summary.json")


def active_session_groups(process, timeout):
    # The test launcher can create groups inside the step's private session.
    # Darwin ps has no SID selector. Match its numeric rows with getsid.
    # Linux ps includes threads so a zombie leader cannot hide live threads.
    darwin = sys.platform == "darwin"
    selection = ("-A",) if darwin else ("--sid", str(process.pid), "-L")
    check = subprocess.run(
        ("ps", *selection, "-o", "pid=,pgid=,stat="),
        capture_output=True, text=True, timeout=timeout,
    )
    if check.returncode == 1 and not check.stdout and not check.stderr:
        return set()  # No matching processes.
    if check.returncode or check.stderr:
        raise OSError(f"ps exited {check.returncode}: {check.stderr.strip()}")
    active = set()
    foreign = set()
    for row in check.stdout.splitlines():
        fields = row.split()
        if len(fields) != 3 or not all(value.isdecimal() for value in fields[:2]):
            raise OSError(f"unexpected ps output: {row!r}")
        pid, pgid = map(int, fields[:2])
        if process.returncode is not None and pid == process.pid and pgid == pid:
            raise OSError(f"cannot verify ownership of session {pid} after reaping its leader")
        if pgid <= 0 or pgid in active or pgid in foreign or fields[2][0] in ("Z", "X"):
            continue
        if darwin:
            try:
                if os.getsid(pid) != process.pid:
                    foreign.add(pgid)
                    continue
                # A process can change groups between the snapshot and getsid.
                pgid = os.getpgid(pid)
            except ProcessLookupError:
                continue
        active.add(pgid)
    return active


def wait_for_session_exit(process):
    deadline = time.monotonic() + CLEANUP_GRACE
    while (remaining := deadline - time.monotonic()) > 0:
        if not active_session_groups(process, remaining):
            return True
        time.sleep(min(0.05, max(0, deadline - time.monotonic())))
    return False


def stop_session(process):
    # Keep the leader unreaped during cleanup whenever possible so its PID
    # cannot be reused. A normal wait can already have reaped it if the
    # interrupt arrived just as the command exited.
    cleanup = {"state": "FAIL", "signals": [], "errors": []}
    for signum in (signal.SIGTERM, signal.SIGKILL):
        try:
            groups = active_session_groups(process, CLEANUP_GRACE)
        except (OSError, subprocess.SubprocessError) as error:
            cleanup["errors"].append(f"checking session membership: {error}")
            if process.returncode is not None:
                return cleanup
            # The unreaped leader still reserves its original group ID.
            groups = {process.pid}
        if not groups:
            cleanup["state"] = "PASS"
            return cleanup
        cleanup["signals"].append({"signal": signum.name, "groups": sorted(groups)})
        for pgid in sorted(groups):
            try:
                os.killpg(pgid, signum)
            except ProcessLookupError:
                pass  # Verify the entire session below.
            except OSError as error:
                cleanup["errors"].append(f"{signum.name} for group {pgid}: {error}")
        try:
            if wait_for_session_exit(process):
                cleanup["state"] = "PASS"
                return cleanup
        except (OSError, subprocess.SubprocessError) as error:
            cleanup["errors"].append(f"checking session after {signum.name}: {error}")
    cleanup["errors"].append("session exit was not confirmed before the cleanup deadline")
    return cleanup


def run_step(step, directory, env, interrupted):
    with (directory / step["log"]).open("wb") as output:
        try:
            process = subprocess.Popen(
                step["argv"], cwd=ROOT, env=env, stdin=subprocess.DEVNULL,
                stdout=output, stderr=subprocess.STDOUT, start_new_session=True,
            )
        except OSError as error:
            step.update(exit_code=127, status_error=f"could not start command: {error}")
            output.write(f"{step['status_error']}\n".encode())
            return
        try:
            while not interrupted:
                try:
                    step["exit_code"] = process.wait(timeout=0.2)
                    break
                except subprocess.TimeoutExpired:
                    pass
        except (OSError, subprocess.SubprocessError) as error:
            step["status_error"] = f"waiting for command exit: {error}"
        finally:
            if interrupted or step["exit_code"] is None:
                try:
                    step["cleanup"] = stop_session(process)
                except Exception as error:
                    step["cleanup"] = {
                        "state": "FAIL", "signals": [], "errors": [f"{type(error).__name__}: {error}"],
                    }
                finally:
                    try:
                        step["exit_code"] = process.wait(timeout=CLEANUP_GRACE)
                    except (OSError, subprocess.SubprocessError) as error:
                        step["status_error"] = f"could not obtain exit status after cleanup: {error}"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--suite", choices=SUITES, default="full")
    parser.add_argument("--evidence-dir", type=Path, default=ROOT / "target" / "offline-regression")
    parser.add_argument("--base", help="comparison commit (defaults to the merge base with origin/main)")
    args = parser.parse_args()

    parent = args.evidence_dir.resolve()
    parent.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix="run-", dir=parent))
    (directory / "tmp").mkdir()
    env = os.environ.copy()
    env.setdefault("CARGO_TARGET_DIR", str(ROOT / "target"))
    env["CARGO_NET_OFFLINE"] = "true"
    env["TS_RS_EXPORT_DIR"] = str(ROOT / "apps/web/src/types/generated")
    env["IA2_TEST_SERVER_LOG_DIR"] = str(directory / "test-servers")
    env["TMPDIR"] = str(directory / "tmp")
    env["NO_COLOR"] = "1"

    selected = SUITES[args.suite]
    steps = [
        {
            "name": name, "argv": STEPS[name], "log": f"{name}.log", "state": "NOT_RUN",
            "reason": "awaiting prerequisites" if name in selected else "outside selected suite",
            "start": None, "end": None, "exit_code": None, "status_error": None, "cleanup": None,
            "ignored_or_skipped": [],
        }
        for name in (*selected, *(name for name in STEPS if name not in selected))
    ]
    interrupted = []

    def mark_interrupted(signum, _frame):
        interrupted.append(signum)

    signal.signal(signal.SIGINT, mark_interrupted)
    signal.signal(signal.SIGTERM, mark_interrupted)
    report = {
        "suite": args.suite, "start": timestamp(), "repo": str(ROOT),
        "env": {name: env.get(name) for name in RELEVANT_ENV}, "steps": steps,
        "result": "INCOMPLETE", "exit_code": None,
    }
    save(report, directory)
    print(f"IA2 {args.suite} evidence in {directory}", flush=True)
    result = 0
    try:
        report["source"] = identity()
        report["base"] = (
            git("rev-parse", "--verify", f"{args.base}^{{commit}}") if args.base
            else git("merge-base", "HEAD", "origin/main", required=False) or None
        )
        diff_step = next(step for step in steps if step["name"] == "diff")
        diff_step["argv"] += (report["base"] or "HEAD",)
        report["tools"] = {
            name: capture(name, "--version", required=False).decode(errors="replace").strip()
            or "unavailable"
            for name in ("python3", "rustc", "cargo", "node", "pnpm", "git")
        }
        for step in steps[:len(selected)]:
            if interrupted:
                break
            step.update(state="RUNNING", start=timestamp(), reason=None)
            save(report, directory)
            completed = False
            try:
                print(f"{step['name']}  {shlex.join(step['argv'])}  [{step['log']}]", flush=True)
                run_step(step, directory, env, interrupted)
                with (directory / step["log"]).open(encoding="utf-8", errors="replace") as output:
                    step["ignored_or_skipped"] = [line.strip() for line in output if SKIPPED.search(line)]
                completed = True
            except Exception as error:
                step["error"] = f"{type(error).__name__}: {error}"
                raise
            finally:
                if interrupted:
                    step["reason"] = f"interrupted by signal {interrupted[0]}"
                cleanup = step["cleanup"]
                passed = (completed and step["exit_code"] == 0 and not interrupted
                          and not step["status_error"] and (cleanup is None or cleanup["state"] == "PASS"))
                step.update(state="PASS" if passed else "FAIL", end=timestamp())
                save(report, directory)
            if step["state"] == "FAIL":
                code = step["exit_code"]
                result = 128 - code if code is not None and code < 0 else code or 1
                break
        if not interrupted and identity() != report["source"]:
            report["error"] = "source identity or lock files changed while the gate ran"
            result = result or 1
    except Exception as error:
        report["error"] = f"{type(error).__name__}: {error}"
        result = result or 1
    finally:
        if interrupted:
            report["signal"] = interrupted[0]
            result = 128 + interrupted[0]
        report.update(end=timestamp(), result="FAIL" if result else "PASS", exit_code=result)
        save(report, directory)
        if result:
            print(report.get("error", f"IA2 {args.suite} failed (exit {result})"), file=sys.stderr)
        print(directory / "summary.json", flush=True)
    return result


if __name__ == "__main__":
    sys.exit(main())
