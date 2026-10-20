# R11 offline systemd verification fixture

**Status: V — pending real systemd evidence.** Preparing this fixture,
passing its offline tests, and verifying its hashes do not reproduce or
close R11. No production deploy/recovery behavior changes in this branch.
Normal automatic restarts and reconnections remain allowed by their
existing contracts.

## Question and source boundary

The deploy generator in `crates/server/src/edges.rs` switches `current` to
the new version, tries the system service restart, then tries a user service
restart if the first failed. If both fail, it restores the prior link and
returns 3. The question is whether a real manager still has a pending
automatic restart that subsequently follows that restored link and starts
the old version. Link restoration alone is not evidence of that start.

The fixture exports **the actual `remote_deploy_script` generator** through
an ignored Rust test. It keeps the generated shell bytes, production service
snapshot, source commit/dirty flag, and SHA-256 hashes. At execution, only
the sentinel `INSTALL_DIR='__R11_INSTALL_ROOT__'` assignment is substituted;
the staging, fallback, and link-restoration logic is unchanged. The export
passes no web-asset directory, so the generated shell lacks the `web` move
branch; that branch is outside the restart and link-restoration path under test.

`infra/ia2.service` currently leaves `Type` unspecified. With `ExecStart`
present it defaults to `simple`, which can report a successful start before
the executable succeeds. `Type=exec` waits for execution. These are separate
cases, not interchangeable evidence. See the official
[service type documentation](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html#Type=).
Record the tested systemd version; the linked documentation describes the
current release, not necessarily the installed guest.

## Offline preparation on macOS or Linux

From a checkout with the pinned `vendor/ironplc` submodule and the Rust
toolchain/dependencies available:

```bash
python3 -m unittest discover -s scripts/r11-systemd -p 'test_*.py' -v
python3 scripts/r11-systemd/fixture.py prepare --output "$PWD/target/r11-bundle"
python3 target/r11-bundle/fixture.py verify --bundle "$PWD/target/r11-bundle"
tar -czf target/r11-bundle.tar.gz -C target r11-bundle
(cd target && shasum -a 256 r11-bundle.tar.gz | tee r11-bundle.tar.gz.sha256)
```

Record the archive hash when the archive is made and check it again after the
copy (`shasum -a 256 -c r11-bundle.tar.gz.sha256`). Inside the archive, the
integrity anchor is `manifest.json`. Make the bundle from a committed tree:
`export/source.json` records `source_dirty`, and `true` means the bundle does
not identify the code it was built from.

Use a new output directory for each export; preparation never replaces one.
It compiles the server test target and invokes only
`edges::tests::r11_fixture_export::export_production_deploy_template`.
It does not start an IA2 server, use SSH, query systemd, contact an edge,
or execute the exported deploy script. Rust dependency fetching may be
needed on a fresh checkout; use the normal prepopulated Cargo cache for an
air-gapped preparation.

The bundle contains:

- `export/deploy.sh.in`: actual generated deploy shell, with one install
  placeholder; keep it as data and execute it only through the runner.
- `export/production.service` and `export/source.json`: pinned unit/source
  identity and exact script hashes.
- `fixture.py` and `fake_runtime.py`: portable Python 3.9+ runner and version
  witnesses; no Rust toolchain is needed on the test guest.
- `INSTRUCTIONS.md`: this execution/evidence handoff.
- `manifest.json`: file hashes and the nine cases, all **NOT_RUN**. Its
  `V_PENDING_REAL_SYSTEMD` classification is independent of offline success.
- `export.log`: export/build diagnostics. The evidence bundle is an internal
  verification artifact, not a runtime deployment package.

The fake runtime only appends its embedded `old`/`new` identity, PID,
monotonic time, manager, and systemd invocation ID, then waits for SIGTERM.
It never opens sockets, runs PLC code, starts subprocesses, or accesses a
fieldbus. Its crash-once variant exits 23 once, then stays running when the
manager restarts it. A missing-interpreter payload separately tests exec
failure. Offline tests check these witnesses and the runner guards; mocks
in those tests are not systemd results.

## Real execution later: disposable guest only

This step is **NOT RUN during offline preparation**. Use a disposable Linux
VM with systemd as PID 1, no attached PLC/drive/bus, and no deployed IA2 unit.
A normal test-user login with an active user manager exercises both scopes.
Required: Python 3.9+ at `/usr/bin/python3`, bash, GNU tar/mv, systemctl,
journalctl, and noninteractive sudo access to systemctl/journalctl.
`systemd-analyze` is used when present; without it that evidence is NOT_RUN. A
root-only session can exercise system cases, but missing user-manager cases
remain NOT_RUN. No account, lingering session, container, or VM is created
by the runner.

After copying/extracting the bundle to that guest, verify it before use:

```bash
python3 r11-bundle/fixture.py verify --bundle "$PWD/r11-bundle"

# Run a positive control first. This is the explicit real-systemd action.
python3 r11-bundle/fixture.py run --bundle "$PWD/r11-bundle" \
  --isolated-host --case healthy

# Each invocation creates a fresh run ID; this runs the complete matrix.
python3 r11-bundle/fixture.py run --bundle "$PWD/r11-bundle" --isolated-host
```

The runner rejects macOS, a non-systemd PID 1, missing opt-in, a mismatched
bundle hash, or a pre-existing generated unit. It creates only its own
`ia2-r11-<run-id>-<case>.service` units, runtime enablement links, and
`/var/tmp/ia2-r11-<run-id>/` evidence directory. It never invokes a real
`ia2.service`. Production `systemctl … ia2` calls pass through a restricted
alias relay that forwards only the three known argument shapes to the
unique fixture unit in the **real** system/user manager. Manager stdout,
stderr, and return codes are preserved. The `sudo -n` wrapper delegates to
the same restricted relay; the relay uses actual noninteractive sudo for
system-manager access. Authentication and timing overhead are consequently
fixture-specific and recorded, not evidence about a production sudo policy.

Every case first starts the old witness as a positive control. Observers
then record the link and manager properties while the generated deploy shell
consumes a device-free tar payload. They continue for at least 12 seconds
(four configured 3-second restart delays) **before cleanup**. Increase
`--observe-seconds` on a slow guest and repeat cases; one run without an old
start does not establish absence of a race.

### Matrix

All rows start as NOT_RUN; variant observations apply only to the stated
unit configuration.

| Case | Setup | Required observation |
|---|---|---|
| `healthy` | Unspecified Type, normal new witness | Restart result, new link, actual new start. Positive control must pass. |
| `crash-once` | New witness exits once, then runs | New-version automatic restart works; do not call an authorized same-version restart a rollback. |
| `bad-exec-simple` | Production default Type, missing interpreter | Actual restart return code and whether link restoration is reached; do not assume nonzero. |
| `bad-exec-exec` | Explicit Type=exec variant | Same observations, kept separate from default-Type evidence. |
| `pre-no-retry` | New-version-only ExecStartPre failure; Restart=no | Failed deploy/restored link, no new old start during the window; effective Restart=no must be read back. |
| `pre-retry` | Same injected precondition, Restart=on-failure | Both restart results, auto-restart state, restoration time, and any subsequent old start. |
| `pre-rate-limit` | Precondition failure, explicit Unit-section burst=2/interval=60 | Effective limits, Result/start-limit state, retry count and actual launches; read back, do not infer from the file. |
| `dual-user-succeeds` | System precondition fails; user instance can run new | Which manager actually succeeded, both process identities, link state and remaining system retries. |
| `dual-both-fail` | Both managers fail the new-version precondition | Both results, restoration, and later launches attributed separately to each manager. |

The witnesses do not bind API ports. Dual-manager rows test manager selection
with two configured witnesses; they do not prove that two real runtimes using
the same bind address can coexist. The runtime's bind-before-device behavior
and any shared-port conflict require their own evidence.

The default-Type rows preserve the production unit's restart/type and
start-limit line placement. The shipped unit puts `StartLimitIntervalSec=60`
and `StartLimitBurst=5` under `[Service]`. These keys belong to the common
`[Unit]` options, and the upstream `[Service]` table is understood to accept
only the older `StartLimitInterval` spelling there. If so, the 60 s interval is
ignored with an "unknown key" warning and the default interval applies. With
`RestartSec=3s`, five starts would then never fall inside one interval, so a
retry loop would not be rate-limited. **This is a hypothesis about the
guest's systemd, not a result.** Each case saves:

- `<manager>.analyze-verify.txt`: `systemd-analyze verify` output for the
  fixture unit, where a rejected key is reported;
- `start_limit` in `result.json`: the start-limit lines found in the unit
  text with their sections, the limits the manager reports, and
  `interval_matches_declared` (`false` is a readback to review, not a verdict
  on the unit, and `null` means a side could not be read).

The explicit rate-limit row adds `[Unit]`-section settings so its effective
limits can be compared with the baseline rows. Save warnings and effective
values rather than silently correcting the production unit. See the official
[rate-limit documentation](https://www.freedesktop.org/software/systemd/man/latest/systemd.unit.html#StartLimitIntervalSec=interval).

Fixture-only adaptations: install paths use the case directory, system
service User is the invoking UID, CAP_NET_RAW is removed, PrivateTmp is
disabled so `/var/tmp` witnesses remain visible, and the system case adds
a private network/AF_UNIX restriction and a closed device policy. User
instances omit privileged namespace/hardening directives and use
default.target. ExecStartPre, Type=exec, Restart=no and the explicit rate
limit are only the labeled variants. Unit texts and effective properties
are saved so those differences cannot be mistaken for the shipped unit.

## Evidence and interpretation

For each case, keep `result.json`, both generated unit files where present,
their `*.analyze-verify.txt`, `deploy.sh`, `deploy.stdout/stderr`, `deploy-result.json`, `commands.jsonl`,
`observations.jsonl`, `events.jsonl`, and per-manager journal output. The
run's `environment.json` records systemd/kernel versions, UID, bundle
manifest and whether a user manager existed. It deliberately does not save
manager environment variables or credentials. The command trace includes
selected unit properties and list-jobs results (the latter may mention
other guest jobs), so keep the evidence on the isolated guest/internal
verification record.

`old_start_after_rollback_observed` requires an embedded old witness start
strictly **after** the deploy subprocess completed with the link restored.
That conservative timestamp can miss a start in the small restore-to-exit
window; absence is inconclusive. A restored link, a scheduled job, or
NRestarts alone is not a version-start witness. `control_result=PASS` means
the stated fixture control behaved as expected, not that R11 is disproven.
Linux runs retain `V_PENDING_EVIDENCE_REVIEW`; even a variant reproducer
does not automatically establish a defect in every deployment/default unit.

Exit 0 means the selected controls ran without recorded failures; inspect
every row for NOT_RUN, NA, actual findings, and cleanup errors. In particular
a missing user manager never counts as dual-manager coverage. Setup/guard
errors return 2; observed fixture/control failures return 1.

The post-restart **health gate** is a separate path from shell restart/link
restoration. Existing real Rust health-gate tests can be run with:

```bash
cargo test -p server a_restarted_program_that_traps_is_faulted
cargo test -p server a_restarted_program_that_never_scans_is_unconfirmed
```

They are software controls, not a systemd/health-check integration result.
That integration row stays NOT_RUN until a real deploy-health observation
records the new version remaining current, no implicit old-version start,
and the failed health verdict. Do not manufacture a shell health failure:
the production shell does not perform the Rust health check.

## Cleanup and later decision

The runner collects evidence before stopping, disabling runtime enablement,
resetting failure state, and reloading **only its recorded fixture units**.
It keeps files for review and reports any cleanup failure. It does not stop
or alter a production unit. A signal during the case follows the same
finally cleanup for SIGINT, SIGTERM and SIGHUP (for example a dropped SSH
session). A second SIGTERM or SIGHUP during that cleanup is ignored so it
can finish (a second Ctrl-C still interrupts it). SIGKILL or a guest failure
requires checking the saved unit files/command trace after restart. An
interrupted case still writes its own `result.json`, and `results.json` marks
it ERROR.

For interrupted-run cleanup, obtain the exact generated name from a case's
`result.json` or its `system/ia2-r11-….service` file, then on that same guest:

```bash
R11_UNIT=ia2-r11-REPLACE_WITH_RECORDED_RUN_ID-REPLACE_WITH_CASE.service
# Substitute the exact recorded name. Never use ia2.service or wildcard stop.
sudo -n systemctl stop "$R11_UNIT"
sudo -n systemctl disable --runtime "$R11_UNIT"
sudo -n systemctl reset-failed "$R11_UNIT"
sudo -n systemctl daemon-reload
# Only for a case that created a user unit:
systemctl --user stop "$R11_UNIT"
systemctl --user disable --runtime "$R11_UNIT"
systemctl --user reset-failed "$R11_UNIT"
systemctl --user daemon-reload
```

Before choosing a production fix, review whether the target default/custom
unit actually reaches the failed-restart branch, whether retries survived,
what version really started, and which manager controlled it. Decide any
change to stop/cancel/restart handling separately; stopping a possibly live
old process is a process-control decision. This fixture changes none of
those policies, adds no automatic-recovery prohibition, and does not modify
X03 bench scripts, HMI, or field protocol behavior.
