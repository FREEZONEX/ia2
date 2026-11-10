"""Exercise the real runner with controlled tool commands in a scratch repo."""

import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

RUNNER = Path(__file__).with_name("offline-regression.py").resolve()
TOOL = """#!/usr/bin/env python3
import os, pathlib, signal, subprocess, sys, time
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
if args == ['--version']:
    print(name + ' fixture')
    sys.exit(0)
with open(os.environ['IA2_FIXTURE_CALLS'], 'a') as log:
    print(name, *args, file=log)
if args and args[0] == 'clippy':
    print('stdout marker', flush=True)
    print('stderr marker', file=sys.stderr, flush=True)
    if os.getenv('IA2_FIXTURE_FAIL'):
        sys.exit(17)
    if os.getenv('IA2_FIXTURE_INTERRUPT'):
        stopped = os.environ['IA2_FIXTURE_STOPPED']
        child_code = '''
import pathlib, signal, sys
def stop(*_):
    pathlib.Path(sys.argv[1]).write_text('stopped')
    sys.exit(0)
signal.signal(signal.SIGTERM, stop)
print('armed', flush=True)
signal.pause()
'''
        child = subprocess.Popen([sys.executable, '-c', child_code, stopped], stdout=subprocess.PIPE)
        assert child.stdout.readline() == b'armed\\n'
        pathlib.Path(os.environ['IA2_FIXTURE_READY']).touch()
        child.wait()
if args[:3] == ['test', '--locked', '--workspace']:
    print('test manual_fixture ... ignored, manual only')
"""


class RegressionRunnerTest(unittest.TestCase):
    def setUp(self):
        target = RUNNER.parent.parent / "target"
        target.mkdir(exist_ok=True)
        self.tmp = tempfile.TemporaryDirectory(prefix="runner-fixture-", dir=target)
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        scripts = self.root / "scripts"
        scripts.mkdir()
        shutil.copy(RUNNER, scripts)
        (scripts / "test-offline-regression.py").write_text("pass\n")
        adaptation = scripts / "check-agent-adaptation.sh"
        adaptation.write_text("#!/bin/sh\nexit 0\n")
        adaptation.chmod(0o700)
        for name in ("Cargo.lock", "pnpm-lock.yaml"):
            (self.root / name).touch()
        self.git("init", "-q")
        self.git("add", ".")
        self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.test",
                 "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null",
                 "commit", "-qm", "fixture")
        tools = self.root / "tools"
        tools.mkdir()
        for name in ("cargo", "rustc", "node", "pnpm"):
            tool = tools / name
            tool.write_text(TOOL)
            tool.chmod(0o700)
        self.calls = self.root / "calls"
        self.env = dict(
            os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"],
            IA2_FIXTURE_CALLS=str(self.calls),
        )
        self.env.pop("IA2_FIXTURE_FAIL", None)
        self.env.pop("IA2_FIXTURE_INTERRUPT", None)
        self.evidence = self.root / "target" / "offline-regression"
        self.git("config", "core.excludesFile", "/dev/null")
        (self.root / ".git" / "info" / "exclude").write_text("/target/\n/tools/\n/calls\n")

    def git(self, *args):
        subprocess.run(["git", *args], cwd=self.root, check=True)

    def command(self, suite):
        return [sys.executable, "scripts/offline-regression.py", "--suite", suite, "--base", "HEAD"]

    def run_suite(self, suite):
        return subprocess.run(self.command(suite), cwd=self.root, env=self.env,
                              capture_output=True, text=True, timeout=30)

    def summary(self):
        run, = self.evidence.glob("run-*")
        return run, json.loads((run / "summary.json").read_text())

    def test_full_records_actual_order_and_ignores(self):
        run = self.run_suite("full")
        self.assertEqual(run.returncode, 0, run.stderr)
        _, report = self.summary()
        passed = [step["name"] for step in report["steps"] if step["state"] == "PASS"]
        self.assertEqual(passed, [
            "adaptation", "runner-tests", "format", "clippy", "debug-server", "workspace-tests",
            "release", "web-typecheck", "web-build", "web-tests", "diff",
        ])
        calls = self.calls.read_text().splitlines()
        self.assertLess(calls.index("cargo build --locked -p server"),
                        calls.index("cargo test --locked --workspace"))
        workspace = next(step for step in report["steps"] if step["name"] == "workspace-tests")
        self.assertEqual(workspace["ignored_or_skipped"], ["test manual_fixture ... ignored, manual only"])

    def test_cli_scope_has_debug_server_and_real_exit_status(self):
        run = self.run_suite("cli")
        self.assertEqual(run.returncode, 0, run.stderr)
        _, report = self.summary()
        self.assertEqual(report["suite"], "cli")
        self.assertEqual(self.calls.read_text().splitlines(), [
            "cargo build --locked -p server", "cargo test --locked -p ia2-cli --test sim_e2e",
        ])
        self.assertTrue(all(step["state"] == "NOT_RUN" for step in report["steps"][3:]))

    def test_failed_command_preserves_both_streams_and_original_code(self):
        self.env["IA2_FIXTURE_FAIL"] = "1"
        run = self.run_suite("full")
        self.assertEqual(run.returncode, 17, run.stderr)
        path, report = self.summary()
        steps = {step["name"]: step for step in report["steps"]}
        failed = steps["clippy"]
        self.assertEqual((failed["state"], failed["exit_code"]), ("FAIL", 17))
        log = (path / failed["log"]).read_text()
        self.assertIn("stdout marker", log)
        self.assertIn("stderr marker", log)
        self.assertEqual(steps["debug-server"]["state"], "NOT_RUN")

    def test_interrupt_stops_descendant_and_leaves_independent_process_alone(self):
        ready = self.root / "target" / "ready"
        stopped = self.root / "target" / "stopped"
        self.env.update(IA2_FIXTURE_INTERRUPT="1", IA2_FIXTURE_READY=str(ready),
                        IA2_FIXTURE_STOPPED=str(stopped))
        bystander = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
        runner = subprocess.Popen(self.command("full"), cwd=self.root, env=self.env,
                                  stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 15
            while not ready.exists():
                self.assertIsNone(runner.poll())
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.02)
            runner.send_signal(signal.SIGINT)
            output = runner.communicate(timeout=5)[0]
            self.assertEqual(runner.returncode, 130, output)
            self.assertEqual(stopped.read_text(), "stopped")
            self.assertIsNone(bystander.poll())
            path, report = self.summary()
            self.assertEqual(report["signal"], signal.SIGINT)
            self.assertIn("stderr marker", (path / "clippy.log").read_text())
            steps = {step["name"]: step["state"] for step in report["steps"]}
            self.assertEqual(steps["clippy"], "FAIL")
            self.assertEqual(steps["workspace-tests"], "NOT_RUN")
        finally:
            if runner.poll() is None:
                runner.terminate()
                runner.communicate(timeout=5)
            bystander.kill()
            bystander.wait()


if __name__ == "__main__":
    unittest.main()
