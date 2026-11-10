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


def stop_group(process):
    # Each command owns a process group. Keep descendants (e.g. a test server)
    # in scope even when the command that launched them exits on SIGTERM first.
    for signum in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(process.pid, signum)
        except ProcessLookupError:
            break
        if signum == signal.SIGTERM:
            time.sleep(0.5)
    return process.wait()


def run_step(step, directory, env, interrupted):
    with (directory / step["log"]).open("wb") as output:
        try:
            process = subprocess.Popen(
                step["argv"], cwd=ROOT, env=env, stdin=subprocess.DEVNULL,
                stdout=output, stderr=subprocess.STDOUT, start_new_session=True,
            )
        except OSError as error:
            output.write(f"could not start command: {error}\n".encode())
            return 127
        try:
            while not interrupted:
                try:
                    code = process.wait(timeout=0.2)
                except subprocess.TimeoutExpired:
                    continue
                return stop_group(process) if interrupted else code
        except BaseException:
            stop_group(process)
            raise
        return stop_group(process)


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
            "start": None, "end": None, "exit_code": None, "ignored_or_skipped": [],
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
            step.update(start=timestamp(), reason=None)
            save(report, directory)
            print(f"{step['name']}  {shlex.join(step['argv'])}  [{step['log']}]", flush=True)
            code = run_step(step, directory, env, interrupted)
            if interrupted:
                step["reason"] = f"interrupted by signal {interrupted[0]}"
            step.update(state="FAIL" if code or interrupted else "PASS", end=timestamp(), exit_code=code)
            with (directory / step["log"]).open(encoding="utf-8", errors="replace") as output:
                step["ignored_or_skipped"] = [line.strip() for line in output if SKIPPED.search(line)]
            save(report, directory)
            if code:
                result = code if code > 0 else 128 - code
                break
        if not interrupted and identity() != report["source"]:
            report["error"] = "source identity or lock files changed while the gate ran"
            result = result or 1
    except (OSError, subprocess.SubprocessError) as error:
        report["error"] = str(error)
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
