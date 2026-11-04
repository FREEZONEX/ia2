#!/usr/bin/env python3
"""Two offline flows: a real exported bundle and real witness processes."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

import fixture

HERE = Path(__file__).resolve().parent


def run(*args, code=0, timeout=30, **kwargs):
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout, **kwargs)
    assert result.returncode == code, f"{args}\n{result.stdout}\n{result.stderr}"
    return result


def bundle_flow(root):
    bundle = root / "bundle"
    run(sys.executable, str(HERE / "fixture.py"), "prepare", "--output", str(bundle), timeout=900)
    # Use the copied CLI outside the checkout, with no Rust dependency for verify.
    cli = (sys.executable, str(bundle / "fixture.py"))
    result = json.loads(run(*cli, "verify", "--bundle", str(bundle), cwd=root).stdout)
    assert result == {"integrity": "PASS", "r11_status": "V_PENDING_REAL_SYSTEMD",
                      "systemd_cases": "NOT_RUN"}
    manifest = json.loads((bundle / "manifest.json").read_text())
    assert manifest["cases"] and all(case["status"] == "NOT_RUN" for case in manifest["cases"])
    head = run("git", "-C", str(HERE), "rev-parse", "HEAD").stdout.strip()
    assert manifest["source"]["source_commit"] == head
    run(*cli, "prepare", "--output", str(bundle), code=2)
    run(*cli, "verify", "--bundle", str(bundle))
    denied = run(*cli, "run", "--bundle", str(bundle), code=2)
    assert "--isolated-host" in denied.stderr
    if sys.platform != "linux" or Path("/proc/1/comm").read_text().strip() != "systemd":
        denied = run(*cli, "run", "--bundle", str(bundle), "--isolated-host", code=2)
        assert "NOT_RUN" in denied.stderr
    # Check a transported copy, then a corrupt copy. Never execute the deploy shell.
    copied = Path(shutil.copytree(bundle, root / "copied"))
    run(sys.executable, str(copied / "fixture.py"), "verify", "--bundle", str(copied))
    with (copied / "export/production.service").open("a") as file:
        file.write("\n# transport corruption\n")
    rejected = run(*cli, "verify", "--bundle", str(copied), code=2)
    assert "hash mismatch" in rejected.stderr
    print("PASS: export → portable verify; overwrite, execution and corruption guards")


def witness_flow(root):
    entry, events = root / "runtime", root / "events.jsonl"
    env = dict(os.environ, IA2_R11_EVENTS=str(events), IA2_R11_MANAGER="system")
    template = (HERE / "fake_runtime.py").read_text()
    rows = []
    for version, behavior in [("old", "steady"), ("new", "crash-once")]:
        entry.write_bytes(fixture.render_runtime(template, version, behavior))
        if behavior == "crash-once":
            run(sys.executable, str(entry), code=23, env=env)
        child = subprocess.Popen([sys.executable, str(entry)], env=env)
        try:
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                rows = fixture.read_rows(events)
                if any(row["event"] == "start" and row["pid"] == child.pid for row in rows):
                    break
                time.sleep(0.02)
            else:
                raise AssertionError(f"{version} witness did not start")
            child.terminate()
            assert child.wait(timeout=5) == 0
        finally:
            if child.poll() is None:
                child.kill()
                child.wait(timeout=5)
    rows = fixture.read_rows(events)
    assert [(row["version"], row["event"]) for row in rows] == [
        ("old", "start"), ("old", "stop"), ("new", "start"),
        ("new", "injected-crash"), ("new", "start"), ("new", "stop")]
    print("PASS: old start/stop → new crash → relaunch/stop (no systemd)")


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="ia2-r11-smoke-") as directory:
        root = Path(directory)
        bundle_flow(root)
        witness_flow(root)
    print("Real systemd cases: NOT_RUN; R11: V_PENDING_REAL_SYSTEMD")
