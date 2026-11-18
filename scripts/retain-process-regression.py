#!/usr/bin/env python3
"""No-device native-process RETAIN regression. All artifacts stay below --evidence-dir."""
import argparse
from datetime import datetime, timedelta, timezone
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import time
import urllib.request


def digest(data):
    return hashlib.sha256(data).hexdigest()


def save(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    stamp = datetime.now(timezone(timedelta(hours=8))).strftime("%Y%m%dT%H%M%S%f")
    root = args.evidence_dir.resolve() / ("retain-process-" + stamp)
    root.mkdir(parents=True)
    (root / "tmp").mkdir()
    results = {}
    repo = Path(__file__).resolve().parents[1]
    provenance = {
        "time": datetime.now(timezone(timedelta(hours=8))).isoformat(),
        "scope": "Native no-device processes; not physical power-cut or Linux timing acceptance",
        "binary": str(binary), "binary_sha256": digest(binary.read_bytes()),
        "head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(),
        "diff_sha256": digest(subprocess.check_output(["git", "diff", "HEAD"], cwd=repo)),
    }
    save(root / "provenance.json", provenance)

    def project(name, iec_type="DINT", initial="42", extra=""):
        case = root / name
        for directory in ["project/pous", "project/devices", "state"]:
            (case / directory).mkdir(parents=True, exist_ok=True)
        (case / "project/project.toml").write_text('name = "retain_process"\n[retain]\nproject_id = "process-fixture"\ninstance_id = "simulator"\n' + extra)
        (case / "project/tasks.toml").write_text('[[tasks]]\nname = "cycle"\ninterval_ms = 100\npriority = 1\n[[programs]]\ninstance = "main_inst"\nprogram = "main"\ntask = "cycle"\n')
        (case / "project/iomap.toml").write_text("mappings = []\n")
        (case / "project/pous/main.st").write_text(f"PROGRAM main\nVAR RETAIN value : {iec_type} := {initial}; END_VAR\nEND_PROGRAM\n")
        return case

    def request(port, endpoint="status"):
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/{endpoint}", timeout=1) as response:
            return json.load(response)

    def run(case, label, expected, hold=0.25, kill=False, during=None):
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        command = [str(binary), "--project-dir", str(case / "project"), "--state-dir", str(case / "state"), "--bind", f"127.0.0.1:{port}"]
        log_path = case / f"{label}.log"
        with log_path.open("wb") as log:
            process = subprocess.Popen(command, cwd=case, env=dict(os.environ, TMPDIR=str(root / "tmp"), RUST_LOG="info", NO_COLOR="1"), stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline:
                    if process.poll() is not None:
                        raise RuntimeError(f"early exit: {log_path.read_text()}")
                    try:
                        status = request(port)
                        recovery = status["retain"]["recovery"]
                        if recovery["kind"] != "pending" and (not recovery["admitted"] or status["scan_count"] > 0):
                            break
                    except (OSError, ValueError):
                        pass
                    time.sleep(0.02)
                else:
                    raise AssertionError("no recovery conclusion")
                assert recovery["kind"] == expected, status
                assert status["devices"] == []
                if not recovery["admitted"]:
                    assert status["watchdog_tripped"] and status["fault"] and status["scan_count"] == 0, status
                save(case / f"{label}-initial.json", status)
                if during:
                    during(case, port)
                time.sleep(hold)
                status = request(port)
                save(case / f"{label}-before-stop.json", status)
                path = case / "state/retain.json"
                if path.is_file():
                    (case / f"{label}-before-stop.retain").write_bytes(path.read_bytes())
            finally:
                if kill:
                    process.kill()
                else:
                    process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=3)
                    raise AssertionError("normal stop exceeded 10s")
        assert process.returncode == (-signal.SIGKILL if kill else 0), log_path.read_text()
        final = None
        for line in log_path.read_text().splitlines():
            if "RETAIN final receipt" in line and "retain=" in line:
                final, _ = json.JSONDecoder().raw_decode(line.split("retain=", 1)[1])
        if not kill:
            assert final is not None, log_path.read_text()
        result = {"command": command, "exit_code": process.returncode, "status": status, "final": final}
        results[label] = result
        save(case / f"{label}-result.json", result)
        return result

    for name, raw, expected in [("corrupt", b"not valid JSON\n", "corrupt"), ("future", b'{"schema":999,"vars":{"value":900}}', "unsupported_version"), ("legacy", b'{"schema":2,"vars":{"value":1000}}', "legacy_unverified")]:
        case = project(name)
        path = case / "state/retain.json"
        path.write_bytes(raw)
        result = run(case, name, expected, hold=5.25)
        assert path.read_bytes() == raw
        assert (case / f"{name}-before-stop.retain").read_bytes() == raw
        assert Path(result["status"]["retain"]["recovery"]["archive_path"]).read_bytes() == raw

    case = project("type-change", initial="1000")
    run(case, "dint", "first_start")
    old = (case / "state/retain.json").read_bytes()
    (case / "original.retain").write_bytes(old)
    project("type-change", "REAL", "2.5")
    result = run(case, "real-rejected", "layout_mismatch", hold=5.25)
    assert (case / "state/retain.json").read_bytes() == old
    assert (case / "real-rejected-before-stop.retain").read_bytes() == old
    assert Path(result["status"]["retain"]["recovery"]["archive_path"]).read_bytes() == old

    case = project("cold", extra='allow_cold_start = ["missing", "corrupt"]\n')
    (case / "state/retain.json").write_bytes(b"old corrupt data")
    result = run(case, "authorized-cold", "cold_start", hold=5.25)
    assert result["status"]["last_snapshot"]["vars"][0]["bits"] == 42
    assert result["status"]["retain"]["recovery"]["source_kind"] == "corrupt"
    assert result["final"]["persistence"]["final_save"] == "durable"
    assert Path(result["status"]["retain"]["recovery"]["archive_path"]).read_bytes() == b"old corrupt data"

    case = project("required", extra='required = ["main_inst.value"]\n')
    run(case, "required-missing", "first_start")
    assert not (case / "state/retain.json").exists()
    request_file = case / "resolve.json"
    save(request_file, {"expected_source_sha256": digest(b""), "action": "cold_start", "initialize_required": True, "attested_source_identity": None, "rules": []})
    command = [str(binary), "--project-dir", str(case / "project"), "--state-dir", str(case / "state"), "--retain-resolve", str(request_file)]
    resolved = subprocess.run(command, cwd=case, env=dict(os.environ, TMPDIR=str(root / "tmp")), capture_output=True, text=True, timeout=10)
    (case / "resolve-output.txt").write_text(resolved.stdout + resolved.stderr)
    assert resolved.returncode == 0, resolved.stderr
    run(case, "required-resolved", "cold_start")

    case = project("restart", extra='sample_interval_ms = 100\n')
    result = run(case, "killed-after-checkpoint", "first_start", hold=0.4, kill=True)
    assert result["status"]["retain"]["persistence"]["last_durable"] is not None
    (case / "state/.retain.json.abandoned.tmp").write_bytes(b"partial")
    run(case, "restart-after-kill", "restored")
    assert (case / "state/.retain.json.abandoned.tmp").read_bytes() == b"partial"

    case = project("writers", extra='sample_interval_ms = 100\n')
    run(case, "writer-owner", "first_start", during=lambda case, port: run(case, "writer-rejected", "writer_busy"))

    case = project("permissions", extra='sample_interval_ms = 100\nmax_loss_ms = 200\non_loss = "inhibit"\n')
    def readonly(case, port):
        (case / "state").chmod(0o500)
    try:
        result = run(case, "readonly-during-run", "first_start", hold=0.6, during=readonly)
        assert result["status"]["retain"]["persistence"]["last_error"]
        assert result["status"]["retain"]["persistence"]["budget_exceeded"]
        assert result["status"]["watchdog_tripped"]
        assert result["final"]["persistence"]["final_save"] == "failed"
    finally:
        (case / "state").chmod(0o700)

    fingerprints = {str(path.relative_to(root)): digest(path.read_bytes()) for path in root.rglob("*") if path.is_file()}
    save(root / "sha256.json", fingerprints)
    save(root / "summary.json", {"status": "PASS", "provenance": provenance, "cases": list(results), "results": results})
    print(root / "summary.json")


if __name__ == "__main__":
    main()
