"""Offline tests of the witness and guards, never evidence about systemd."""

import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
from unittest.mock import patch

import fixture


class FixtureTests(unittest.TestCase):
    def make_integrity_example(self, root):
        # Hash/guard plumbing only: this is deliberately not a deploy or
        # systemd behavioral test. The real Rust exporter is exercised separately.
        export = root / "export"
        export.mkdir()
        (export / "deploy.sh.in").write_text(fixture.PLACEHOLDER + "\n")
        (export / "production.service").write_text("[Service]\nExecStart=/example\n")
        source = {"evidence": "OFFLINE_EXPORT_ONLY", "r11_status": fixture.PENDING,
                  "deploy_template_sha256": fixture.digest(export / "deploy.sh.in"),
                  "service_sha256": fixture.digest(export / "production.service")}
        fixture.write_json(export / "source.json", source)
        manifest = {"evidence": "OFFLINE_PREPARED", "r11_status": fixture.PENDING,
                    "source": source, "cases": [dict(case, status="NOT_RUN") for case in fixture.CASES],
                    "files": {p.relative_to(root).as_posix(): fixture.digest(p)
                              for p in root.rglob("*") if p.is_file()}}
        fixture.write_json(root / "manifest.json", manifest)

    def test_bundle_integrity_detects_drift_and_does_not_upgrade_not_run_cases(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            self.make_integrity_example(root)
            self.assertEqual(fixture.verify_bundle(root)["r11_status"], fixture.PENDING)
            (root / "export/deploy.sh.in").write_text("changed")
            with self.assertRaisesRegex(ValueError, "hash mismatch"):
                fixture.verify_bundle(root)
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            self.make_integrity_example(root)
            manifest = json.loads((root / "manifest.json").read_text())
            manifest["cases"][0]["status"] = "PASS"
            fixture.write_json(root / "manifest.json", manifest)
            with self.assertRaisesRegex(ValueError, "NOT_RUN"):
                fixture.verify_bundle(root)

    def test_bundle_rejects_added_files_and_paths_outside_the_bundle(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            self.make_integrity_example(root)
            (root / "extra.py").write_text("extra")
            with self.assertRaisesRegex(ValueError, "inventory mismatch"):
                fixture.verify_bundle(root)
            (root / "extra.py").unlink()
            target = root.parent / (root.name + "-outside")
            target.write_text("outside")
            try:
                (root / "linked").symlink_to(target)
                manifest = json.loads((root / "manifest.json").read_text())
                manifest["files"]["linked"] = fixture.digest(target)
                fixture.write_json(root / "manifest.json", manifest)
                with self.assertRaisesRegex(ValueError, "escapes its root"):
                    fixture.verify_bundle(root)
            finally:
                target.unlink()

    def test_proxy_only_routes_the_production_shapes_to_its_unique_unit(self):
        unit = "ia2-r11-123456abcdef-pre-retry.service"
        for request, manager, args in [
            (["is-enabled", "--quiet", "ia2"], "system", ["is-enabled", "--quiet", unit]),
            (["restart", "ia2"], "system", ["restart", unit]),
            (["--user", "restart", "ia2"], "user", ["restart", unit]),
        ]:
            self.assertEqual(fixture.proxy_request(request, "ia2", unit), (manager, args))
        for request in [["stop", "ia2"], ["restart", "another"], ["restart", "*"],
                        ["restart", "ia2", "another"], ["--host=remote", "restart", "ia2"]]:
            with self.assertRaises(ValueError):
                fixture.proxy_request(request, "ia2", unit)
        with self.assertRaises(ValueError):
            fixture.proxy_request(["restart", "ia2"], "ia2", "ia2.service")

    def test_proxy_returns_the_real_manager_output_and_failure_without_synthesizing_success(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            config = {"alias": "ia2", "unit": "ia2-r11-123456abcdef-pre-retry.service",
                      "trace": str(root / "trace.jsonl"), "current": str(root / "current"),
                      "system_prefix": ["/usr/bin/sudo", "-n", "/usr/bin/systemctl"],
                      "user_prefix": ["/usr/bin/systemctl", "--user"]}
            fixture.write_json(root / "config.json", config)
            class Output:
                buffer = io.BytesIO()
            stdout, stderr = Output(), Output()
            stdout.buffer, stderr.buffer = io.BytesIO(), io.BytesIO()
            with patch.dict(os.environ, {"IA2_R11_PROXY_CONFIG": str(root / "config.json")}), \
                    patch.object(fixture.subprocess, "run", return_value=subprocess.CompletedProcess([], 5, b"out", b"denied")) as call, \
                    patch.object(fixture.sys, "stdout", stdout), patch.object(fixture.sys, "stderr", stderr):
                code = fixture.proxy_main("sudo", ["-n", "systemctl", "restart", "ia2"])
            self.assertEqual(code, 5)
            self.assertEqual(stdout.buffer.getvalue(), b"out")
            self.assertEqual(stderr.buffer.getvalue(), b"denied")
            self.assertEqual(call.call_args[0][0][-1], config["unit"])
            self.assertEqual(fixture.read_rows(root / "trace.jsonl")[0]["returncode"], 5)

    def test_only_the_install_assignment_is_rendered_and_shell_metacharacters_stay_literal(self):
        template = "set -eu\n" + fixture.PLACEHOLDER + '\nprintf "%s" "$INSTALL_DIR"\n'
        path = Path("/var/tmp/r11-'$(touch not-executed)`echo x`/install")
        rendered = fixture.render_deploy(template, path)
        result = subprocess.run(["bash", "-c", rendered], stdout=subprocess.PIPE, check=True)
        self.assertEqual(result.stdout.decode(), str(path))
        for broken in ["no placeholder", template + fixture.PLACEHOLDER]:
            with self.assertRaises(ValueError):
                fixture.render_deploy(broken, path)

    def test_default_and_variant_units_keep_their_distinct_start_contracts(self):
        production = (Path(__file__).resolve().parents[2] / "infra/ia2.service").read_text()
        root = Path("/var/tmp/ia2-r11-123456abcdef/healthy")
        baseline = fixture.render_unit(production, root, "system", fixture.CASES[0], 1000)
        self.assertNotIn("\nType=", baseline)
        self.assertIn("Restart=on-failure", baseline)
        self.assertIn("RestartSec=3s", baseline)
        self.assertIn("PrivateTmp=false", baseline)
        self.assertNotIn("CAP_NET_RAW\n", baseline)
        self.assertIn(str(root / "install/current/runtime"), baseline)
        variant = next(case for case in fixture.CASES if case["id"] == "bad-exec-exec")
        self.assertIn("Type=exec", fixture.render_unit(production, root, "system", variant, 1000))
        user = fixture.render_unit(production, root, "user", fixture.CASES[0], 1000)
        self.assertNotIn("\nUser=", user)
        self.assertIn("WantedBy=default.target", user)

    def test_tar_payload_is_device_free_and_uses_the_production_bundle_names(self):
        with tarfile.open(fileobj=io.BytesIO(fixture.payload(b"fake"))) as archive:
            self.assertEqual(set(archive.getnames()), {"r11-project/project.toml", "r11-project/version.txt", "r11-runtime"})
            self.assertEqual(archive.extractfile("r11-project/version.txt").read(), b"new\n")

    def test_systemd_execution_guards_run_before_any_manager_command(self):
        with patch.object(fixture.subprocess, "run") as call:
            with self.assertRaisesRegex(ValueError, "--isolated-host"):
                fixture.execute("/does/not/exist", False, [], 12)
            with patch.object(fixture.platform, "system", return_value="Darwin"):
                with self.assertRaisesRegex(ValueError, "NOT_RUN"):
                    fixture.execute("/does/not/exist", True, [], 12)
            call.assert_not_called()

    def test_late_old_start_is_not_inferred_from_the_link_or_start_before_rollback(self):
        case = next(case for case in fixture.CASES if case["id"] == "pre-retry")
        events = [{"event": "start", "version": "old", "manager": "system", "monotonic_ns": 10}]
        self.assertFalse(fixture.summarize(case, 3, 20, events, [], [])["old_start_after_rollback_observed"])
        events.append(dict(events[0], monotonic_ns=30))
        self.assertTrue(fixture.summarize(case, 3, 20, events, [], [])["old_start_after_rollback_observed"])
        self.assertFalse(fixture.summarize(case, 0, None, events, [], [])["old_start_after_rollback_observed"])

    def test_a_missing_witness_or_effective_restart_setting_is_not_a_passing_control(self):
        case = next(case for case in fixture.CASES if case["id"] == "pre-no-retry")
        self.assertEqual(fixture.summarize(case, 3, 20, [], [], [])["control_result"], "FAIL")

    def test_an_unobserved_production_restart_path_is_an_error_not_completed_coverage(self):
        case = next(case for case in fixture.CASES if case["id"] == "pre-retry")
        result = fixture.summarize(case, 3, 20, [], [], [])
        self.assertEqual(result["status"], "ERROR")
        self.assertFalse(result["production_restart_path_observed"])

    def test_fake_runtime_records_embedded_identity_and_stops_on_sigterm(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            entry = root / "runtime"
            entry.write_bytes(fixture.render_runtime(Path(__file__).with_name("fake_runtime.py").read_text(), "old", "steady"))
            events = root / "events.jsonl"
            env = dict(os.environ, IA2_R11_EVENTS=str(events), IA2_R11_MANAGER="system")
            child = subprocess.Popen([sys.executable, str(entry), "--bind", "127.0.0.1:13001"], env=env)
            try:
                fixture.wait_for_start(events, "system", "old", timeout=3)
                child.terminate()
                self.assertEqual(child.wait(timeout=3), 0)
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait()
            rows = fixture.read_rows(events)
            self.assertEqual([row["event"] for row in rows], ["start", "stop"])
            self.assertEqual(rows[0]["pid"], child.pid)

    def test_crash_once_witness_distinguishes_new_restart_from_old_version_start(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            entry = root / "runtime"
            entry.write_bytes(fixture.render_runtime(Path(__file__).with_name("fake_runtime.py").read_text(), "new", "crash-once"))
            env = dict(os.environ, IA2_R11_EVENTS=str(root / "events.jsonl"), IA2_R11_MANAGER="system")
            first = subprocess.run([sys.executable, str(entry)], env=env, timeout=3)
            self.assertEqual(first.returncode, 23)
            child = subprocess.Popen([sys.executable, str(entry)], env=env)
            try:
                deadline = time.monotonic() + 3
                while time.monotonic() < deadline and len(fixture.read_rows(root / "events.jsonl")) < 3:
                    time.sleep(0.01)
                self.assertGreaterEqual(len(fixture.read_rows(root / "events.jsonl")), 3)
                child.terminate()
                self.assertEqual(child.wait(timeout=3), 0)
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait()
            self.assertEqual({row["version"] for row in fixture.read_rows(root / "events.jsonl")}, {"new"})


    def test_termination_signals_run_cleanup_once_and_restore_handlers(self):
        numbers = (signal.SIGTERM, signal.SIGHUP)
        before = {number: signal.getsignal(number) for number in numbers}
        ran = []
        with fixture.cleanup_on_termination():
            with self.assertRaises(SystemExit) as caught:
                try:
                    os.kill(os.getpid(), signal.SIGTERM)
                    time.sleep(1)
                finally:
                    ran.append("cleanup")
            self.assertEqual(caught.exception.code, 128 + signal.SIGTERM)
            # A second signal while cleanup runs must not interrupt it.
            os.kill(os.getpid(), signal.SIGHUP)
            time.sleep(0.1)
        self.assertEqual(ran, ["cleanup"])
        self.assertEqual({number: signal.getsignal(number) for number in numbers}, before)

    def test_the_shipped_unit_keeps_its_start_limit_keys_in_the_service_section(self):
        production = (Path(__file__).resolve().parents[2] / "infra/ia2.service").read_text()
        declared = fixture.declared_start_limits(production)
        # Where each key sits is the fact the guest's systemd must then confirm or contradict.
        self.assertIn({"section": "Service", "key": "StartLimitIntervalSec", "value": "60"}, declared)
        self.assertIn({"section": "Service", "key": "StartLimitBurst", "value": "5"}, declared)
        case = next(case for case in fixture.CASES if case["id"] == "pre-rate-limit")
        variant = fixture.render_unit(production, Path("/var/tmp/ia2-r11-123456abcdef/x"), "system", case, 1000)
        self.assertIn({"section": "Unit", "key": "StartLimitBurst", "value": "2"},
                      fixture.declared_start_limits(variant))

    def test_time_spans_parse_only_when_fully_recognised(self):
        for text, seconds in [("1min", 60.0), ("10s", 10.0), ("1min 30s", 90.0), ("500ms", 0.5),
                              ("60", 60.0), ("0", 0.0)]:
            self.assertEqual(fixture.parse_span(text), seconds, text)
        for text in ["infinity", "", None, "5 parsecs", "1min soon"]:
            self.assertIsNone(fixture.parse_span(text), text)

    def test_start_limit_readback_reports_a_difference_without_judging_it(self):
        unit = "[Service]\nStartLimitIntervalSec=60\nStartLimitBurst=5\n"
        snapshot = {"manager": "system", "properties": {
            "LoadState": "loaded", "StartLimitIntervalUSec": "10s", "StartLimitBurst": "5"}}
        readback = fixture.start_limit_readback({"system": unit}, [snapshot])["system"]
        self.assertIs(readback["interval_matches_declared"], False)
        self.assertEqual(readback["effective"]["StartLimitIntervalUSec"], "10s")
        snapshot["properties"]["StartLimitIntervalUSec"] = "1min"
        self.assertIs(fixture.start_limit_readback({"system": unit}, [snapshot])["system"]
                      ["interval_matches_declared"], True)
        self.assertIsNone(fixture.start_limit_readback({"system": unit}, [])["system"]
                          ["interval_matches_declared"])

    def test_a_dual_manager_control_needs_the_old_witness_in_both_managers(self):
        case = next(case for case in fixture.CASES if case["id"] == "dual-user-succeeds")
        def start(manager, version, at):
            return {"event": "start", "manager": manager, "version": version, "monotonic_ns": at}
        def command(argv, manager, code):
            return {"event": "production-command", "original_argv": argv, "manager": manager,
                    "returncode": code}
        trace = [command(["is-enabled", "--quiet", "ia2"], "system", 0),
                 command(["-n", "systemctl", "restart", "ia2"], "system", 1),
                 command(["--user", "restart", "ia2"], "user", 0)]
        snapshots = [{"manager": "system", "properties": {"LoadState": "loaded"}}]
        only_system = [start("system", "old", 1), start("user", "new", 5)]
        self.assertEqual(fixture.summarize(case, 0, None, only_system, trace, snapshots)["control_result"], "FAIL")
        both = [start("system", "old", 1), start("user", "old", 2), start("user", "new", 5)]
        self.assertEqual(fixture.summarize(case, 0, None, both, trace, snapshots)["control_result"], "PASS")

    def test_analyze_verify_output_is_kept_and_never_fails_the_case(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            fixture.save_analyze_verify("system", root / "u.service", None, root / "none.txt")
            self.assertIn("NOT_RUN", (root / "none.txt").read_text())
            script = root / "analyze"
            script.write_text("#!/bin/sh\necho \"$@\"\necho 'Unknown key' >&2\nexit 1\n")
            script.chmod(0o755)
            fixture.save_analyze_verify("user", root / "u.service", str(script), root / "user.txt")
            text = (root / "user.txt").read_text()
            self.assertIn("returncode: 1", text)
            self.assertIn("Unknown key", text)
            self.assertIn("--user verify", text)
            fixture.save_analyze_verify("system", root / "u.service", str(root / "missing"), root / "gone.txt")
            self.assertIn("NOT_RUN", (root / "gone.txt").read_text())

    def test_an_old_python_or_an_interrupt_ends_with_a_plain_exit_code(self):
        message = io.StringIO()
        with patch.object(fixture.sys, "version_info", (3, 8, 0)), \
                patch.object(fixture, "verify_bundle", return_value={"r11_status": fixture.PENDING}), \
                patch.object(fixture.sys, "stderr", message):
            self.assertEqual(fixture.main(["verify", "--bundle", "/does/not/exist"]), 2)
        self.assertIn("3.9", message.getvalue())
        with patch.object(fixture, "verify_bundle", side_effect=KeyboardInterrupt), \
                patch.object(fixture.sys, "stderr", io.StringIO()):
            self.assertEqual(fixture.main(["verify", "--bundle", "/does/not/exist"]), 130)


    def test_a_termination_signal_marks_the_interrupted_case_and_prints_the_evidence_path(self):
        with tempfile.TemporaryDirectory() as folder:
            def interrupted_case(_bundle, work, case, *_rest):
                try:
                    os.kill(os.getpid(), signal.SIGTERM)
                    time.sleep(1)
                finally:
                    (work / "cleaned").write_text(case["id"])
            printed = io.StringIO()
            done = subprocess.CompletedProcess([], 0, b"", b"")
            with patch.object(fixture, "real_systemd_host", return_value=True), \
                    patch.object(fixture, "verify_bundle", return_value={"r11_status": fixture.PENDING}), \
                    patch.object(fixture, "EVIDENCE_ROOT", Path(folder)), \
                    patch.object(fixture.shutil, "which", side_effect=lambda name, path=None: "/usr/bin/" + name), \
                    patch.object(fixture.subprocess, "run", return_value=done), \
                    patch.object(fixture.subprocess, "check_output", return_value="systemd 255\n"), \
                    patch.object(fixture, "run_case", interrupted_case), \
                    patch.object(fixture.sys, "stdout", printed):
                with self.assertRaises(SystemExit) as caught:
                    fixture.execute(folder, True, ["healthy"], 12)
            self.assertEqual(caught.exception.code, 128 + signal.SIGTERM)
            work = next(Path(folder).glob("ia2-r11-*"))
            self.assertEqual((work / "cleaned").read_text(), "healthy")
            index = json.loads((work / "results.json").read_text())
            self.assertEqual([(row["id"], row["status"]) for row in index["cases"]], [("healthy", "ERROR")])
            self.assertIn("Evidence: " + str(work), printed.getvalue())


if __name__ == "__main__":
    unittest.main()
