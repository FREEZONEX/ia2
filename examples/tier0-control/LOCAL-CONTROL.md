# Signed local control MCP

This example implements D11–D14 and a real MCP stdio server on a local simulation. The bridge submits governed setpoint requests to an IA2 runtime on loopback; an independent read-only process samples `/status`; the MCP verifies both sources before deriving the outcome. Tier0 transport, a service deployment and hardware acceptance are outside this local implementation.

The existing seven-tool read-only MCP is unchanged. This new server exposes exactly two tools:

| Tool | Arguments | Result |
|---|---|---|
| `ia2_set` | `target_key`, numeric/boolean `target`, optional `ttl_ms` (default 20000) | Signed request appended; returns `action_id` and `requested`, not execution success |
| `get_action_status` | `action_id` | Derived status, claim/limitations, observation references and rejected-envelope counts |

No tool accepts a variable name, raw bit pattern, runtime URL, force/pulse operation, arbitrary command, key path or key value. The target registry and file paths are fixed by the process launcher. The local MCP refuses enabled nonsimulation targets. The bridge retains all registry, timing, runtime, governance and journal checks.

## Reproduce the local closed loop

Build `ia2-runtime` from the integrated branch (`cargo build --release -p ia2-runtime` at the repository root); it includes the runtime fix merged in PR #100. Then, from `examples/tier0-control`:

```sh
python3 -I -B lab/run_cases.py --bin ../../target/release/ia2-runtime \
  --out evidence/local-signed-demo --cases F6,F7,F8,MCP,MCPPROD
```

The harness starts only its own loopback runtime and fake Modbus devices. It generates separate disposable request/event/sample keys, starts the bridge and sampler as independent processes, calls the actual stdio MCP, restarts MCP, and checks there was only one runtime write. It saves JSON protocol transcripts, signed trace/samples, the runtime audit, fake-device logs and checksums, then stops its children. Choose a fresh output directory. The `MCPPROD` case uses the standalone package without lab hooks.

The integrated validation's exact source and runtime hash are recorded in [VALIDATION-D11-INTEGRATED.md](VALIDATION-D11-INTEGRATED.md). Older validation records pin a different binary and retain their historical results.

## Launch against your own local simulation

Use a reviewed simulation-only registry and an already running loopback simulation project. Keep that project and registry consistent; the bridge checks governance. Create a private state directory and three fresh key files (run once, from this example directory):

```sh
python3 -B - <<'PY'
import json, os, secrets, time
from pathlib import Path
root = Path('local-state')
root.mkdir(mode=0o700, exist_ok=True)
os.chmod(root, 0o700)
now = int(time.time() * 1000)
for role in ('request', 'event', 'sample'):
    path = root / (role + '-keys.json')
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'w') as f:
        json.dump({role + '-1': {'secret_hex': secrets.token_hex(32),
                               'not_before_ms': now - 60000,
                               'not_after_ms': now + 7 * 86400000}}, f)
PY
```

The files are ignored by Git. Never put their contents in tool arguments, prompts or an MCP configuration. Registry pins are SHA-256 of the exact file bytes. Substitute your registry, its digest, simulation project directory and local runtime port in these commands:

```sh
python3 -I -B bridge_run.py \
  --registry /absolute/path/registry.json --registry-sha256 SHA256 \
  --keys local-state/request-keys.json --event-keys local-state/event-keys.json \
  --runtime 127.0.0.1:13501 --project-dir /absolute/path/simulation-project \
  --journal local-state/journal.jsonl --requests local-state/requests.jsonl \
  --trace local-state/trace.jsonl
```

In another terminal, start the independently signed sampler:

```sh
python3 -I -B sampler_run.py --runtime 127.0.0.1:13501 \
  --sample-keys local-state/sample-keys.json --samples local-state/samples.jsonl
```

Launch MCP through the client's stdio configuration, or a protocol client:

```sh
python3 -I -B control_mcp_run.py \
  --registry /absolute/path/registry.json --registry-sha256 SHA256 \
  --keys local-state/request-keys.json --event-keys local-state/event-keys.json \
  --sample-keys local-state/sample-keys.json --requests local-state/requests.jsonl \
  --trace local-state/trace.jsonl --samples local-state/samples.jsonl
```

For a client configuration, use absolute paths for every file and for the interpreter. The MCP server uses newline-delimited UTF-8 JSON-RPC on stdin/stdout, advertises protocol `2025-11-25`, negotiates `initialize`, and requires `notifications/initialized` before tools. Stdout contains protocol messages only; startup errors go to stderr. Closing stdin ends the MCP process. This follows the official [stdio transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports), [lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle) and [tool](https://modelcontextprotocol.io/specification/2025-11-25/server/tools) contracts.

## Keys, recovery and limits

- HMAC-SHA256 authenticates the exact UTF-8 payload bytes, with distinct `ia2.control/v0`, `ia2.event/v0` and `ia2.sample/v0` domains and independent secrets. Event and sample hints are checked against the authenticated payload and never used as unauthenticated authority.
- Key files are mode 0600. Event/sample key entries require `secret_hex` (32–64 bytes), `not_before_ms` and `not_after_ms`. Use distinct key IDs when rotating. Signing chooses the latest valid key for the record's timestamp; validation checks that signed timestamp, so old evidence survives normal key expiry.
- Each launcher accepts `--revoked-keys /absolute/path/revoked.json`, a JSON array of key IDs (the bridge list covers event keys). Configuration is loaded at startup; restart after key or revocation changes. Keep unrevoked historical verification keys to query old actions. A signer without a valid key cannot publish a signed record. Bridge terminal publication remains pending in the journal and can be retried without repeating the runtime write.
- To revoke a request key at the bridge, remove its ID from the bridge's `--keys` ring and restart; the bridge's `--revoked-keys` list applies only to event keys. Update MCP's request configuration as well. Revocation does not retract a control write already sent or in durable recovery. Use fresh secrets and distinct IDs when rotating, not aliases of an old secret.
- The outcome verifier has no unsigned mode. Old unsigned records are rejected and counted; they cannot supply a trace deduplication ID or trusted journal anchor. Keep old sealed evidence separate and start new signed sessions in new files. This is not an automatic conversion of historical evidence.
- MCP recovers action bindings from authenticated request records after restart. It incrementally reads and authenticates newly appended event/sample lines, caches them, and re-verifies selected evidence when deriving a status. Complete malformed rows are counted; an incomplete final line waits for completion. Replacing or truncating a file rebuilds its index. These files are append-only transports, not a transactional database.
- Local resource budgets are explicit: 64 KiB per MCP frame/request line, 4 MiB per evidence line, and 20,000 records per evidence/request log in an MCP session's index, including rejected rows. At 5 Hz the sample budget is roughly 67 minutes. A query at exactly 20,000 complete rows is allowed; the next complete row makes queries fail with a tool error, including after MCP restart. New submissions refuse before appending when any of the three logs is already full or over budget. The request budget is checked under the append lock, so two MCP processes cannot consume the final slot twice. Independently growing evidence can still cross the limit after submission; this guard does not reserve future proof capacity.

## Complete session rotation at 5 Hz

This is an explicit maintenance operation on the local simulation; there is no automatic rollover or persistent history index. Plan rotation at **18,000 complete rows in any log or 60 minutes, whichever comes first**. These operational thresholds leave room for pending feedback; they do not change the 20,000-row hard limit. Count rejected lines too, and account for other writers or shorter key windows. Stop admission earlier if the remaining capacity cannot cover every pending request's deadline and verification window.

1. Stop new `ia2_set` calls at the caller. Let the bridge and sampler continue. Query every submitted action until it has a known outcome. Treat `requested` / `accepted` as pending; an `unknown` outcome needs explicit review and must never be automatically replayed. A tool error is not an action outcome or permission to retry.
2. Once all actions are accounted for, stop the bridge, then the sampler, then MCP. Save final audit/status evidence and exact runtime identity. Do not restart the runtime merely to rotate a session.
3. Seal the **whole** session: requests, journal, trace, samples, pinned registry, relevant configuration, key/revocation history, transcripts and a file-size/SHA-256 manifest. Preserve access controls. Keep secrets out of commits and reports. Do not truncate a log, replace a single file under live readers, or combine old requests with a new journal.
4. Create a new private directory with fresh request/journal/trace/sample paths and separate role keys. Keep the reviewed simulation project and registry consistent. Restart the bridge, sampler and MCP against the new paths; verify one new action before reopening admission. Do not copy or replay old requests. Record the rotation gap and the mapping of old action IDs to their original session.
5. Query historical actions only with the complete original pinned files and historical verification keys. Never restart a bridge against an archived request log. A new session must refuse unknown old action IDs. An already oversized archive still returns the capacity error; do not cut out evidence to manufacture a successful query. Use a separately reviewed offline verifier or future persistent index for that archive.

The `lab/soak_history.py` acceptance runner exercises the production bridge, standalone sampler at its default 200 ms interval, and real MCP stdio. It requires a fresh output path. A 75-minute unrotated lane crosses the actual limit; a separate 75-minute lane rotates after 35 minutes while keeping its runtime identity. Short runs are pilots only. See [HISTORY-ACCEPTANCE.md](HISTORY-ACCEPTANCE.md) for the recorded evidence and scope.
- Restarting MCP alone does not reset the budget for existing logs. Archive a complete session only after resolving outstanding actions, then use a fresh set of state paths; do not mix an old request file with a new journal or truncate individual evidence files.
- Rejection counters cover the entire indexed event/sample logs, not just the selected action. Rejected envelopes are skipped and counted. An exception caught within outcome derivation returns `unknown` with `isError:false`; rejected-envelope counts may still be zero. `isError:false` does not establish action success or exclude evidence anomalies.
- An exception caught by the outer lookup or a history-budget error returns `isError:true`; the stdio connection remains available. Such a tool error does not prove the action was not executed. If request append/sync fails after bytes may have become visible, `ia2_set` returns `unknown` and `isError:false` with its action ID; query that ID instead of automatically submitting again. See the [four return boundaries](HANDOFF-D11-MCP-20261030.md#异常返回的判读).
- Sampling defaults to 200 ms. The standalone sampler holds an exclusive writer lock on its sample file, bounds response size, and never calls a runtime mutation endpoint. Nonfinite decoded floating values become null and cannot verify a target. Samples are observational evidence; no power-loss durability guarantee is made for them.

HMAC verification secrets also permit forgery. A compromised verifier, bridge or sampler remains a trust-boundary failure. Separate files under the same OS account do not isolate processes. A valid signature authenticates a report, not its truth, physical effect or causation. Additional `/status` load on grey0/#65, real Tier0 access controls, hardware timing and power-loss behavior remain **BENCH PENDING**.

## Package and test

```sh
python3 -I -B tools/package_prod.py prod
python3 -I -B prod/control_mcp_run.py --help
python3 -I -B prod/sampler_run.py --help
python3 -I -B tools/run_tests.py --out evidence/signed-unit
python3 -I -B tools/check_d11_mutations.py --out evidence/signed-mutations
```

`package_prod.py` replaces its destination. The package includes bridge, read side, sampler, MCP, vocabulary and the pinned TOML parser/license; it excludes test fixtures and lab hooks. The `prod` name does not claim deployment or hardware acceptance.
