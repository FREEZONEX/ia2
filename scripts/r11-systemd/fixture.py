#!/usr/bin/env python3
"""Prepare a portable R11 witness; real systemd execution is a separate opt-in.

Only prepare/verify and ordinary unit tests are intended on macOS. There is
no SSH, edge address, production runtime, or production service operation.
"""

import argparse
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import uuid

PENDING = "V_PENDING_REAL_SYSTEMD"
EVIDENCE_ROOT = Path("/var/tmp")
START_LIMIT_KEYS = ("StartLimitIntervalSec", "StartLimitInterval", "StartLimitBurst")
SPAN_UNITS = {"us": 1e-6, "ms": 1e-3, "s": 1.0, "min": 60.0, "h": 3600.0, "d": 86400.0}
EXPORT_TEST = "edges::tests::r11_fixture_export::export_production_deploy_template"
PLACEHOLDER = "INSTALL_DIR='__R11_INSTALL_ROOT__'"
UNIT_PATTERN = re.compile(r"ia2-r11-[0-9a-f]{12}-[a-z0-9-]+[.]service\Z")
PROPERTIES = ["LoadState", "ActiveState", "SubState", "MainPID", "Result",
              "ExecMainCode", "ExecMainStatus", "NRestarts", "Job", "Type",
              "Restart", "RestartUSec", "StartLimitBurst", "StartLimitIntervalUSec",
              "FragmentPath", "InvocationID"]
CASES = [
    {"id": "healthy", "kind": "baseline", "expect": "new-start"},
    {"id": "crash-once", "kind": "baseline-runtime-injection", "behavior": "crash-once",
     "expect": "new-restart"},
    {"id": "bad-exec-simple", "kind": "baseline-exec-injection", "behavior": "bad-exec",
     "expect": "observe"},
    {"id": "bad-exec-exec", "kind": "type-exec-variant", "behavior": "bad-exec",
     "type": "exec", "expect": "observe"},
    {"id": "pre-no-retry", "kind": "precondition-variant", "pre": True,
     "restart": "no", "expect": "no-late-old"},
    {"id": "pre-retry", "kind": "precondition-variant", "pre": True,
     "expect": "observe"},
    {"id": "pre-rate-limit", "kind": "rate-limit-variant", "pre": True,
     "burst": 2, "expect": "observe"},
    {"id": "dual-user-succeeds", "kind": "dual-manager-variant", "pre": True,
     "user": True, "user_pre": False, "expect": "user-new-start"},
    {"id": "dual-both-fail", "kind": "dual-manager-variant", "pre": True,
     "user": True, "user_pre": True, "expect": "observe"},
]


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def append_json(path, value):
    fd = os.open(str(path), os.O_APPEND | os.O_WRONLY | os.O_CREAT, 0o600)
    try:
        os.write(fd, (json.dumps(value, sort_keys=True) + "\n").encode())
    finally:
        os.close(fd)


def read_rows(path):
    rows = []
    if Path(path).exists():
        for line in Path(path).read_text().splitlines():
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                # A witness may be appending its last line during a poll.
                continue
    return rows


def declared_start_limits(unit_text):
    """Every start-limit line with the section it sits in.

    The shipped unit keeps these keys under [Service]; whether the guest's
    systemd honours each spelling there is read back, not assumed.
    """
    section, found = None, []
    for raw in unit_text.splitlines():
        line = raw.strip()
        if line.startswith("[") and line.endswith("]"):
            section = line[1:-1]
        elif "=" in line and not line.startswith(("#", ";")):
            key, value = (part.strip() for part in line.split("=", 1))
            if key in START_LIMIT_KEYS:
                found.append({"section": section, "key": key, "value": value})
    return found


def parse_span(text):
    """Seconds in a `systemctl show` time span such as '1min 30s'; None if unrecognised."""
    text = (text or "").strip()
    if re.fullmatch(r"\d+(?:\.\d+)?", text):
        return float(text)  # a bare number is seconds, as in a unit file
    pattern = r"(\d+(?:\.\d+)?)\s*([a-z]+)"
    parts = re.findall(pattern, text)
    if not parts or re.sub(pattern + r"|\s", "", text):
        return None
    if any(unit not in SPAN_UNITS for _, unit in parts):
        return None
    return sum(float(number) * SPAN_UNITS[unit] for number, unit in parts)


def start_limit_readback(unit_texts, snapshots):
    """Declared start-limit lines beside the limits the manager reports.

    `interval_matches_declared` is None when either side cannot be read.
    A False value is a readback to review, not a verdict on the unit.
    """
    readback = {}
    for manager, text in (unit_texts or {}).items():
        declared = declared_start_limits(text)
        loaded = [s["properties"] for s in snapshots
                  if s.get("manager") == manager
                  and s.get("properties", {}).get("LoadState") == "loaded"]
        effective = {key: loaded[-1].get(key) for key in
                     ("StartLimitIntervalUSec", "StartLimitBurst")} if loaded else {}
        intervals = [d["value"] for d in declared
                     if d["key"] in ("StartLimitIntervalSec", "StartLimitInterval")]
        wanted = parse_span(intervals[-1]) if intervals else None
        actual = parse_span(effective.get("StartLimitIntervalUSec"))
        readback[manager] = {
            "declared": declared, "effective": effective,
            "interval_matches_declared": None if wanted is None or actual is None
            else abs(wanted - actual) < 1e-6}
    return readback


@contextlib.contextmanager
def cleanup_on_termination():
    """Turn SIGTERM/SIGHUP into SystemExit so each case's finally block runs.

    One-shot: a second signal must not cut short the cleanup the first one
    started. SIGINT keeps its KeyboardInterrupt behaviour. SIGKILL cannot be
    caught; the saved unit files and command trace then guide manual cleanup.
    """
    caught = (signal.SIGTERM, signal.SIGHUP)
    previous = {number: signal.getsignal(number) for number in caught}

    def handler(number, _frame):
        for other in caught:
            signal.signal(other, signal.SIG_IGN)
        raise SystemExit(128 + number)

    for number in caught:
        signal.signal(number, handler)
    try:
        yield
    finally:
        for number, old in previous.items():
            signal.signal(number, old if old is not None else signal.SIG_DFL)


def save_analyze_verify(manager, unit_path, analyzer, output):
    """Keep systemd-analyze's view of a fixture unit; never fails the case.

    Its warnings (for example a key the section does not accept) are the
    parse-time evidence that `systemctl show` values alone do not carry.
    """
    if analyzer is None:
        Path(output).write_text("NOT_RUN: systemd-analyze was not found\n")
        return
    command = [analyzer] + (["--user"] if manager == "user" else []) + ["verify", str(unit_path)]
    try:
        result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=20)
    except (OSError, subprocess.SubprocessError) as error:
        Path(output).write_text("NOT_RUN: " + str(error) + "\n")
        return
    Path(output).write_text(
        "command: " + " ".join(command) + "\nreturncode: " + str(result.returncode) + "\n"
        "--- stdout\n" + result.stdout.decode(errors="replace") +
        "--- stderr\n" + result.stderr.decode(errors="replace"))


def render_deploy(template, install):
    if template.count(PLACEHOLDER) != 1 or template.count("__R11_INSTALL_ROOT__") != 1:
        raise ValueError("deploy template must have exactly one install placeholder")
    install = Path(install)
    if not install.is_absolute():
        raise ValueError("install directory must be absolute")
    quoted = "'" + str(install).replace("'", "'\\''") + "'"
    return template.replace(PLACEHOLDER, "INSTALL_DIR=" + quoted)


def validate_unit(unit):
    if not UNIT_PATTERN.fullmatch(unit):
        raise ValueError("only an ia2-r11-<run-id>-<case>.service unit is allowed")


def proxy_request(argv, alias, unit):
    """Allow only the three production calls. Never forward an arbitrary unit."""
    validate_unit(unit)
    if argv == ["is-enabled", "--quiet", alias]:
        return "system", ["is-enabled", "--quiet", unit]
    if argv == ["restart", alias]:
        return "system", ["restart", unit]
    if argv == ["--user", "restart", alias]:
        return "user", ["restart", unit]
    raise ValueError("unexpected production systemctl invocation: " + repr(argv))


def proxy_main(program, argv):
    config = json.loads(Path(os.environ["IA2_R11_PROXY_CONFIG"]).read_text())
    original = list(argv)
    if program == "sudo":
        if argv != ["-n", "systemctl", "restart", config["alias"]]:
            raise ValueError("only the production sudo restart shape is allowed")
        argv = argv[2:]
    manager, args = proxy_request(argv, config["alias"], config["unit"])
    command = config["system_prefix"] if manager == "system" else config["user_prefix"]
    start = time.monotonic_ns()
    result = subprocess.run(command + args, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, timeout=20)
    end = time.monotonic_ns()
    append_json(config["trace"], {"event": "production-command", "program": program,
                "original_argv": original, "actual_argv": command + args,
                "manager": manager, "start_ns": start, "end_ns": end,
                "returncode": result.returncode,
                "current_after_command": os.path.realpath(config["current"])})
    sys.stdout.buffer.write(result.stdout)
    sys.stderr.buffer.write(result.stderr)
    return result.returncode if result.returncode >= 0 else 128 - result.returncode


def verify_export(export):
    source = json.loads((export / "source.json").read_text())
    if source["evidence"] != "OFFLINE_EXPORT_ONLY" or source["r11_status"] != PENDING:
        raise ValueError("not an offline export from the production generator")
    if digest(export / "deploy.sh.in") != source["deploy_template_sha256"]:
        raise ValueError("deploy template hash mismatch")
    if digest(export / "production.service") != source["service_sha256"]:
        raise ValueError("production service hash mismatch")
    render_deploy((export / "deploy.sh.in").read_text(), Path("/var/tmp/r11-test/install"))
    return source


def prepare(output):
    output = Path(output).resolve()
    output.mkdir(parents=True, exist_ok=False)
    root = Path(__file__).resolve().parents[2]
    env = dict(os.environ)
    env["IA2_R11_EXPORT_DIR"] = str(output / "export")
    command = ["cargo", "test", "-p", "server", EXPORT_TEST, "--", "--ignored", "--exact"]
    with (output / "export.log").open("w") as log:
        result = subprocess.run(command, cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT)
    if result.returncode:
        raise RuntimeError("Rust export failed; see " + str(output / "export.log"))
    source = verify_export(output / "export")
    for name in ["fixture.py", "fake_runtime.py"]:
        shutil.copyfile(Path(__file__).with_name(name), output / name)
    shutil.copyfile(root / "docs/recovery-r11-offline.md", output / "INSTRUCTIONS.md")
    files = {p.relative_to(output).as_posix(): digest(p)
             for p in output.rglob("*") if p.is_file()}
    manifest = {"schema": 1, "evidence": "OFFLINE_PREPARED",
                "r11_status": PENDING, "source": source, "files": files,
                "cases": [dict(case, status="NOT_RUN") for case in CASES]}
    write_json(output / "manifest.json", manifest)
    print(json.dumps({"bundle": str(output), "evidence": "OFFLINE_PREPARED",
                      "r11_status": PENDING, "systemd_cases": "NOT_RUN"}, indent=2))


def verify_bundle(bundle):
    bundle = Path(bundle).resolve()
    manifest = json.loads((bundle / "manifest.json").read_text())
    if manifest["evidence"] != "OFFLINE_PREPARED" or manifest["r11_status"] != PENDING:
        raise ValueError("bundle must retain its unverified R11 classification")
    if manifest["cases"] != [dict(case, status="NOT_RUN") for case in CASES]:
        raise ValueError("offline case plan must remain NOT_RUN")
    actual = {p.relative_to(bundle).as_posix() for p in bundle.rglob("*")
              if p.is_file() and p.relative_to(bundle).as_posix() != "manifest.json"}
    if actual != set(manifest["files"]):
        raise ValueError("bundle file inventory mismatch")
    for relative, expected in manifest["files"].items():
        path = bundle / relative
        if path.is_symlink() or not path.resolve().is_relative_to(bundle):
            raise ValueError("bundle path escapes its root")
        if digest(path) != expected:
            raise ValueError("bundle hash mismatch: " + relative)
    if verify_export(bundle / "export") != manifest["source"]:
        raise ValueError("bundle provenance mismatch")
    return manifest


def render_runtime(template, version, behavior):
    if template.count('VERSION = "__R11_VERSION__"') != 1:
        raise ValueError("fake runtime version marker is missing")
    if behavior == "bad-exec":
        return b"#!/__ia2_r11_missing_interpreter__\n# new version: exec failure injection\n"
    return template.replace("__R11_VERSION__", version).replace("__R11_BEHAVIOR__", behavior).encode()


def payload(runtime):
    data = io.BytesIO()
    with tarfile.open(fileobj=data, mode="w") as archive:
        for name, body, mode in [
            ("r11-project/project.toml", b'name = "r11-fake"\n', 0o644),
            ("r11-project/version.txt", b"new\n", 0o644),
            ("r11-runtime", runtime, 0o755),
        ]:
            info = tarfile.TarInfo(name)
            info.size, info.mode = len(body), mode
            archive.addfile(info, io.BytesIO(body))
    return data.getvalue()


def render_unit(production, case_root, manager, case, uid):
    """Preserve production restart/type lines; enumerate fixture-only changes."""
    install = case_root / "install"
    result = production.replace("/opt/ia2", str(install))
    result = result.replace("User=root", "User=" + str(uid))
    result = result.replace("AmbientCapabilities=CAP_NET_RAW", "AmbientCapabilities=")
    result = result.replace("ReadWritePaths=" + str(install), "ReadWritePaths=" + str(case_root))
    if manager == "user":
        # User managers cannot assume privileged namespace/capability support.
        keys = ("User=", "AmbientCapabilities=", "PrivateTmp=", "ProtectSystem=",
                "ProtectHome=", "ReadWritePaths=")
        result = "\n".join(line for line in result.splitlines()
                           if not line.startswith(keys)) + "\n"
        result = result.replace("WantedBy=multi-user.target", "WantedBy=default.target")
    result += '\n[Service]\nEnvironment="IA2_R11_EVENTS=' + str(case_root / "events.jsonl") + '"\n'
    result += 'Environment="IA2_R11_MANAGER=' + manager + '"\n'
    result += "Environment=PYTHONDONTWRITEBYTECODE=1\n"
    # The witness lives under /var/tmp; a private tmp namespace hides it.
    result += "PrivateTmp=false\n"
    if manager == "system":
        result += "PrivateNetwork=true\nRestrictAddressFamilies=AF_UNIX\nDevicePolicy=closed\n"
    if case.get("type"):
        result += "Type=" + case["type"] + "\n"
    if case.get("restart"):
        result += "Restart=" + case["restart"] + "\n"
    pre = case.get("pre", False) if manager == "system" else case.get("user_pre", False)
    if pre:
        result += "ExecStartPre=/usr/bin/python3 " + str(case_root / "gate.py")
        result += " " + str(install / "current/project/version.txt") + "\n"
    if case.get("burst"):
        result += "\n[Unit]\nStartLimitIntervalSec=60\nStartLimitBurst=" + str(case["burst"]) + "\n"
    return result


class Unit:
    def __init__(self, manager, unit, path, trace, systemctl, sudo):
        validate_unit(unit)
        self.manager, self.unit, self.path, self.trace = manager, unit, path, trace
        self.prefix = ([sudo, "-n", systemctl] if manager == "system"
                       else [systemctl, "--user"])

    def call(self, *args, timeout=20):
        start = time.monotonic_ns()
        result = subprocess.run(self.prefix + list(args), stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, timeout=timeout)
        append_json(self.trace, {"event": "fixture-command", "manager": self.manager,
                    "actual_argv": self.prefix + list(args), "start_ns": start,
                    "end_ns": time.monotonic_ns(), "returncode": result.returncode,
                    "stdout": result.stdout.decode(errors="replace"),
                    "stderr": result.stderr.decode(errors="replace")})
        return result

    def show(self):
        result = self.call("show", self.unit, "--no-pager", "--property=" + ",".join(PROPERTIES))
        return dict(line.split("=", 1) for line in result.stdout.decode().splitlines() if "=" in line)


def require_success(result, description):
    if result.returncode:
        raise RuntimeError(description + ": " + result.stderr.decode(errors="replace"))


def wait_for_start(events, manager, version, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if any(row["event"] == "start" and row["manager"] == manager and row["version"] == version
               for row in read_rows(events)):
            return
        time.sleep(0.05)
    raise RuntimeError("positive control did not start: " + manager + "/" + version)


def summarize(case, deploy_code, restored_ns, events, trace, snapshots, unit_texts=None):
    starts = [row for row in events if row.get("event") == "start"]
    late_old = [row for row in starts if row["version"] == "old" and restored_ns is not None
                and row["monotonic_ns"] > restored_ns]
    commands = [row for row in trace if row.get("event") == "production-command"]
    pending = any(snapshot.get("properties", {}).get("SubState") == "auto-restart"
                  for snapshot in snapshots)
    control = "NA"
    managers = ["system", "user"] if case.get("user") else ["system"]
    positive_control = all(any(row["manager"] == manager and row["version"] == "old" for row in starts)
                           for manager in managers)
    readable = [s for s in snapshots if s.get("properties", {}).get("LoadState") == "loaded"]
    if case["expect"] in ("new-start", "new-restart", "user-new-start"):
        manager = "user" if case["expect"] == "user-new-start" else "system"
        needed = 2 if case["expect"] == "new-restart" else 1
        control = "PASS" if positive_control and readable and deploy_code == 0 and sum(
            row["manager"] == manager and row["version"] == "new" for row in starts
        ) >= needed else "FAIL"
    elif case["expect"] == "no-late-old":
        no_restart = len(readable) >= 2 and all(s["properties"].get("Restart") == "no" for s in readable)
        control = "PASS" if positive_control and no_restart and deploy_code == 3 and restored_ns is not None and not late_old else "FAIL"
    enabled = any(row.get("original_argv", [])[:1] == ["is-enabled"] and row["returncode"] == 0 for row in commands)
    restarts = [row for row in commands if "restart" in row.get("original_argv", [])]
    system_restart = [row for row in restarts if row["manager"] == "system"]
    route_complete = enabled and bool(system_restart) and (
        system_restart[-1]["returncode"] == 0 or any(row["manager"] == "user" for row in restarts))
    return {"id": case["id"], "kind": case["kind"],
            "status": "OBSERVED" if route_complete and readable else "ERROR",
            "production_restart_path_observed": route_complete,
            "control_result": control, "deploy_returncode": deploy_code,
            "old_start_after_rollback_observed": bool(late_old),
            "auto_restart_state_observed": pending, "late_old_starts": late_old,
            "production_commands": commands,
            "start_limit": start_limit_readback(unit_texts, snapshots),
            "interpretation": "conditional version-switch evidence; review actual unit settings and timing",
            "r11_status": "V_PENDING_EVIDENCE_REVIEW"}


def run_case(bundle, work, case, systemctl, sudo, user_available, observe_seconds, analyzer=None):
    if case.get("user") and not user_available:
        return {"id": case["id"], "status": "NOT_RUN", "reason": "no reachable user manager"}
    root = work / case["id"]
    root.mkdir()
    install = root / "install"
    old = install / "versions/old"
    (old / "project").mkdir(parents=True)
    (old / "project/version.txt").write_text("old\n")
    runtime = (bundle / "fake_runtime.py").read_text()
    (old / "runtime").write_bytes(render_runtime(runtime, "old", "steady"))
    (old / "runtime").chmod(0o755)
    (install / "current").symlink_to(old)
    (root / "gate.py").write_text(
        "import sys\nfrom pathlib import Path\n"
        "raise SystemExit(1 if Path(sys.argv[1]).read_text().strip() == 'new' else 0)\n")
    unit = "ia2-r11-" + work.name.rsplit("-", 1)[-1] + "-" + case["id"] + ".service"
    validate_unit(unit)
    events, trace, observations = root / "events.jsonl", root / "commands.jsonl", root / "observations.jsonl"
    source = verify_export(bundle / "export")
    managers = ["system", "user"] if case.get("user") else ["system"]
    units = []
    owned = []
    snapshots = []
    unit_texts = {}
    stop_monitor = threading.Event()
    monitor = None
    cleanup_errors = []
    result = {"id": case["id"], "status": "ERROR", "r11_status": "V_PENDING_EVIDENCE_REVIEW"}
    try:
        if user_available and "user" not in managers:
            guard = Unit("user", unit, root / "unused-user.service", trace, systemctl, sudo)
            if guard.show().get("LoadState") != "not-found":
                raise RuntimeError("refusing an existing user unit: " + unit)
        for manager in managers:
            # systemctl links a unit by basename; keep distinct directories.
            path = root / manager / unit
            path.parent.mkdir()
            unit_texts[manager] = render_unit((bundle / "export/production.service").read_text(),
                                              root, manager, case, os.getuid())
            path.write_text(unit_texts[manager])
            save_analyze_verify(manager, path, analyzer, root / (manager + ".analyze-verify.txt"))
            managed = Unit(manager, unit, path, trace, systemctl, sudo)
            if managed.show().get("LoadState") != "not-found":
                raise RuntimeError("refusing an existing unit: " + manager + "/" + unit)
            units.append(managed)
        for managed in units:
            # Record ownership before installation so a partial failure is cleaned.
            owned.append(managed)
            require_success(managed.call("enable", "--runtime", str(managed.path)), "install fixture unit")
            require_success(managed.call("daemon-reload"), "reload fixture unit")
            require_success(managed.call("start", managed.unit), "start old positive control")
            wait_for_start(events, managed.manager, "old")
            managed.call("list-jobs", "--no-pager", "--all")

        def poll():
            while not stop_monitor.is_set():
                for managed in units:
                    try:
                        snapshot = {"monotonic_ns": time.monotonic_ns(), "manager": managed.manager,
                                    "current": os.path.realpath(install / "current"),
                                    "properties": managed.show()}
                        snapshots.append(snapshot)
                        append_json(observations, snapshot)
                    except (OSError, subprocess.SubprocessError) as error:
                        append_json(observations, {"observer_error": str(error)})
                stop_monitor.wait(0.1)

        config = {"alias": source["production_unit_alias"], "unit": unit, "trace": str(trace),
                  "current": str(install / "current"), "system_prefix": [sudo, "-n", systemctl],
                  "user_prefix": [systemctl, "--user"]}
        write_json(root / "proxy.json", config)
        shim = root / "shim"
        shim.mkdir()
        for name in ["sudo", "systemctl"]:
            # A small launcher uses the copied, hashed fixture implementation.
            launcher = "#!/usr/bin/python3\nimport sys\nsys.path.insert(0, " + repr(str(bundle)) + ")\n"
            launcher += "from fixture import proxy_main\nraise SystemExit(proxy_main(" + repr(name) + ", sys.argv[1:]))\n"
            (shim / name).write_text(launcher)
            (shim / name).chmod(0o755)
        env = dict(os.environ)
        env["PATH"] = str(shim) + ":/usr/bin:/bin"
        env["IA2_R11_PROXY_CONFIG"] = str(root / "proxy.json")
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        script = render_deploy((bundle / "export/deploy.sh.in").read_text(), install)
        (root / "deploy.sh").write_text(script)
        monitor = threading.Thread(target=poll)
        monitor.start()
        result_process = subprocess.run(["/bin/bash", str(root / "deploy.sh")],
                                        input=payload(render_runtime(runtime, "new", case.get("behavior", "steady"))),
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, timeout=50)
        completed = time.monotonic_ns()
        (root / "deploy.stdout").write_bytes(result_process.stdout)
        (root / "deploy.stderr").write_bytes(result_process.stderr)
        restored = completed if (install / "current").resolve() == old.resolve() else None
        write_json(root / "deploy-result.json", {"returncode": result_process.returncode,
                   "completed_ns": completed, "current": os.path.realpath(install / "current"),
                   "restored_old_link": restored is not None})
        for managed in units:
            managed.call("list-jobs", "--no-pager", "--all")
        stop_monitor.wait(observe_seconds)
        stop_monitor.set()
        monitor.join(timeout=45)
        if monitor.is_alive():
            raise RuntimeError("observer did not stop within its command deadlines")
        for managed in units:
            managed.call("list-jobs", "--no-pager", "--all")
            managed.show()
            journal = ([sudo, "-n", "journalctl"] if managed.manager == "system"
                       else ["journalctl", "--user"])
            captured = subprocess.run(journal + ["--no-pager", "--unit", unit, "-o", "json"],
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=20)
            (root / (managed.manager + ".journal.jsonl")).write_bytes(captured.stdout)
            (root / (managed.manager + ".journal.stderr")).write_bytes(captured.stderr)
        result = summarize(case, result_process.returncode, restored, read_rows(events),
                           read_rows(trace), snapshots, unit_texts)
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        result["error"] = str(error)
    finally:
        stop_monitor.set()
        if monitor is not None:
            monitor.join(timeout=45)
        for managed in reversed(owned):
            for args in [("stop", unit), ("disable", "--runtime", unit), ("reset-failed", unit), ("daemon-reload",)]:
                try:
                    cleanup = managed.call(*args)
                    if cleanup.returncode and args[0] in ("stop", "disable", "daemon-reload"):
                        cleanup_errors.append(managed.manager + ": " + " ".join(args))
                except (OSError, subprocess.SubprocessError) as error:
                    cleanup_errors.append(str(error))
        if result["status"] == "ERROR" and "error" not in result:
            result["error"] = "interrupted before the case completed"
        result["cleanup_errors"] = cleanup_errors
        result["unit"] = unit
        result["managers"] = managers
        result["evidence_directory"] = str(root)
        write_json(root / "result.json", result)
    return result


def real_systemd_host():
    return platform.system() == "Linux" and Path("/proc/1/comm").read_text().strip() == "systemd"


def execute(bundle, isolated_host, selected, observe_seconds):
    # These checks precede every manager query, sudo call, or work directory.
    if not isolated_host:
        raise ValueError("real execution requires --isolated-host on a disposable Linux/systemd guest")
    if not real_systemd_host():
        raise ValueError("NOT_RUN: a real Linux guest with systemd as PID 1 is required")
    manifest = verify_bundle(bundle)
    bundle = Path(bundle).resolve()
    if observe_seconds < 12:
        raise ValueError("observe at least 12 seconds (four production 3-second restart intervals)")
    if selected and not set(selected) <= {case["id"] for case in CASES}:
        raise ValueError("unknown case")
    systemctl = shutil.which("systemctl", path="/usr/bin:/bin")
    sudo = shutil.which("sudo", path="/usr/bin:/bin")
    if not systemctl or not sudo or not Path("/usr/bin/python3").is_file():
        raise ValueError("systemctl, noninteractive sudo, and /usr/bin/python3 are required")
    require_success(subprocess.run([sudo, "-n", systemctl, "show-environment"],
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10), "system manager access")
    user = subprocess.run([systemctl, "--user", "show-environment"],
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
    # Do not save show-environment output: the evidence must not copy credentials.
    analyzer = shutil.which("systemd-analyze", path="/usr/bin:/bin")
    with cleanup_on_termination():
        work = EVIDENCE_ROOT / ("ia2-r11-" + uuid.uuid4().hex[:12])
        work.mkdir(mode=0o755)
        results = []
        try:
            version = subprocess.check_output([systemctl, "--version"], text=True)
            write_json(work / "environment.json", {
                "systemd_version": version, "kernel": platform.release(),
                "uid": os.getuid(), "user_manager_available": user.returncode == 0,
                "systemd_analyze_available": analyzer is not None,
                "bundle_manifest": manifest, "observation_seconds": observe_seconds,
                "evidence": "REAL_SYSTEMD_FAKE_RUNTIME", "r11_status": "V_PENDING_EVIDENCE_REVIEW"})

            def write_index():
                write_json(work / "results.json", {
                    "evidence": "REAL_SYSTEMD_FAKE_RUNTIME",
                    "r11_status": "V_PENDING_EVIDENCE_REVIEW", "cases": results})

            for case in CASES:
                if selected and case["id"] not in selected:
                    results.append({"id": case["id"], "status": "NOT_RUN", "reason": "not selected"})
                else:
                    try:
                        results.append(run_case(bundle, work, case, systemctl, sudo,
                                                user.returncode == 0, observe_seconds, analyzer))
                    except BaseException:
                        # run_case has already cleaned up its own units; keep the index honest.
                        results.append({"id": case["id"], "status": "ERROR",
                                        "reason": "case did not complete (signal or error); see its result.json"})
                        write_index()
                        raise
                write_index()
                print(json.dumps(results[-1]), flush=True)
        finally:
            print("Evidence: " + str(work), flush=True)
    return 1 if any(row["status"] == "ERROR" or row.get("control_result") == "FAIL"
                    or row.get("cleanup_errors") for row in results) else 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("prepare").add_argument("--output", required=True)
    commands.add_parser("verify").add_argument("--bundle", required=True)
    run = commands.add_parser("run")
    run.add_argument("--bundle", required=True)
    run.add_argument("--isolated-host", action="store_true")
    run.add_argument("--case", action="append", default=[])
    run.add_argument("--observe-seconds", type=float, default=12)
    args = parser.parse_args(argv)
    if sys.version_info < (3, 9):
        print("Python 3.9 or newer is required", file=sys.stderr)
        return 2
    try:
        if args.command == "prepare":
            prepare(args.output)
        elif args.command == "verify":
            manifest = verify_bundle(args.bundle)
            print(json.dumps({"integrity": "PASS", "r11_status": manifest["r11_status"],
                              "systemd_cases": "NOT_RUN"}, indent=2))
        else:
            return execute(args.bundle, args.isolated_host, args.case, args.observe_seconds)
    except KeyboardInterrupt:
        print("interrupted; check each case's result.json for cleanup_errors", file=sys.stderr)
        return 130
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(str(error), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
