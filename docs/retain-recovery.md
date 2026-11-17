# RETAIN recovery and durable checkpoints

The shared bridge admits recovery before connecting field devices. Failed
required-data recovery sets a runtime fault and the existing output inhibit;
resume/step cannot bypass it. A valid checkpoint and an explicitly permitted
first start need no per-restart acknowledgement. Runtime liveness alone is
never evidence that retained data recovered or reached durable storage.

## Project policy and identity

Declare the durable project's identity in `project.toml`:

```toml
[retain]
project_id = "9d7d8213-example-controller-application"
instance_id = "controller-01"
required = ["main_inst.total"]
allow_cold_start = ["missing"]
sample_interval_ms = 5000
max_loss_ms = 15000
on_loss = "inhibit" # "warn" reports/alarm only; default is "warn"
```

Choose and retain a unique `project_id` across application releases. Give
independently operated controller instances distinct `instance_id` values
(default `default` is suitable only for one instance). Copying an ID asserts
continuity: a display name is not identity. `content_sha256` identifies compiled
content for provenance, separately from the stable pair; it is not a blanket
compatibility gate. The active policy is returned in `RetainStatus.policy`.

An undeclared IDE run remains ephemeral and reports `disabled`. A persistent
edge run with retained declarations requires an explicit `project_id`; existing
projects are not silently assigned an identity from their name or directory.
Add the identity and deliberately migrate schema 1/2, or initialize current
defaults. This is a one-time legacy transition, not a repeated approval gate.
Unknown or misspelled policy keys are rejected. Required keys are qualified
`PROGRAM-instance.variable`, case insensitive, and must exist in the selected
run. Isolating a PROGRAM does not silently waive another required key.

`required` data must be present in a compatible checkpoint. Allowing cold
start for a condition does not waive that requirement. An explicit stopped
cold start can initialize required defaults with `initialize_required=true`.
If required defaults are not valid for the application, supply validated data
through migration instead. Initializers are application decisions, not proof
of physical machine state. Historical retained positions do not establish
mechanical coordinates or permit motion.

`allow_cold_start` may include `missing`, `corrupt`, `unsupported_version`,
`legacy_unverified`, `identity_mismatch`, and `layout_mismatch`. Only `missing`
is permitted by default. A policy-authorized abnormal cold start records the
original cause, archives the source and raises RETAIN attention; it is never
reported as a normal restore. I/O, lock, configuration and preservation failures
are not cold-start conditions and cannot authorize overwriting a source.

## On-disk and compatibility contract

Schema 3 carries `identity`, compiled `content_sha256`, wall-clock save time,
scan count, data generation, optional explicit-resolution provenance and a
sorted map of qualified variables. Each value has `{layout:{type_name,width_bits},
bits:"unsigned-decimal-u64"}`. JSON strings preserve all 64 bits through CLI,
HTTP and JavaScript without parsing displayed IEC values.

Supported values are scalar BOOL, SINT/INT/DINT/LINT, USINT/UINT/UDINT/ULINT,
BYTE/WORD/DWORD/LWORD, REAL and LREAL. ARRAY, STRING/WSTRING, STRUCT, FB instance,
user-defined/enum/subrange and time/date shapes are rejected for persistent
storage; their slot may be a pointer or omit declaration constraints. No array
or string shape is claimed to be supported. Internal FB `VAR RETAIN` retains
the compiler bridge's explicit warning and is not a persisted PROGRAM slot;
if named as required it fails declaration validation. Move such data into
supported PROGRAM/global scalars. Multiple scheduled PROGRAMs always use
qualified keys, even when only one remains.

| Change or source | Automatic decision | Explicit handling |
| --- | --- | --- |
| No file, no required data, `missing` permitted | First start, initializer defaults | None |
| No file and required data | Denied, no scan/device connection | Verified initialization or migration |
| Same identities/types, different logic hash or initializer | Restore saved bits | None |
| Optional variable/PROGRAM added | Restore old qualified keys; initialize additions | Make additions required if defaults are invalid |
| Optional variable/PROGRAM removed | Restore surviving keys; archive original before writing reduced layout | Rollback can migrate from archived source |
| Variable or PROGRAM renamed | New qualified key initializes if optional; old key listed removed | Explicit complete mapping preserves renamed values |
| Same key changes type or width | Whole restore rejected before any VM write | Exact mapping or lossless DINT→REAL conversion |
| Different project/instance identity | Rejected | Cold start only if deliberately authorized; no cross-identity migration |
| schema 1/2 | Legacy-unverified, source preserved | Attest identity and each source layout from independent evidence; schema 1 sign-extends i32 (already truncated historical high bits cannot be recovered) |
| Corrupt JSON / unsupported version | Rejected and archived before any writer opens | Deliberate cold start/discard; unknown versions have no migration decoder |
| Unreadable path, another writer, archive failure | Denied; no overwrite | Repair the specific storage condition and retry |
| Unsupported data shape | Configuration error | Remodel into explicitly supported scalars |

Migration `exact` preserves raw bits only for identical supported types and
widths. `dint_to_real` performs numeric conversion and refuses integers not
exactly representable as REAL. Other numerical conversions are not provided.
Migration is all-or-nothing; every destination needs an explicit rule by default.
No fuzzy name fallback can copy a single-PROGRAM value into a newly added instance.

## Original-file protection and operation

The edge retains its configured `--state-dir/retain.json` outside versioned
project releases. The IDE stores declared durable state in
`<project>/retain-state/retain.json`; deployment snapshots exclude that entire
local storage directory. Export checkpoints explicitly when composing a
migration package. History, alarm journals and engineering observations are
separate and are never deleted by RETAIN operations.

A cooperating process holds an exclusive OS file lock on the stable
`retain.lock` inode through recovery, periodic writes and final save. Do not
remove lock files to clear contention. Locks release after process termination;
a delayed blocking writer keeps its lock even if its owner stopped waiting.
Every write also compares the current source hash to its preceding observation
and rejects external edits. These checks assume a local filesystem with working
advisory locks and atomic rename; uncooperative external writers can still race
a hash check. Do not edit active storage externally.

Before the first replacement of an existing checkpoint, retain its full source
under `retain-archive/<source-sha256>.json` with a metadata sidecar recording
source path, hash, reason and wall time. Required archive writes and directory
sync must succeed. The original is untouched on failure, and subsequent normal
Stop does not create a writer. Existing archive bytes must match their content
address. Archives survive restarts and support rollback; retention/pruning is
an explicit maintenance operation, not an automatic deletion policy.

Use `cs get runtime/retain --project NAME --server URL` and
`cs get runtime/retain/checkpoint --project NAME --server URL` for the declared
project. Resolve a stopped server run with the existing `cs api POST
/api/runtime/retain/resolve --from request.json` surface (see CLI help for the
body input). The body is defined in [the API contract](api.md#retain-recovery-and-persistence).
For a stopped edge process, the offline entry takes the same body and lock:

```sh
ia2-runtime --project-dir /absolute/project \
  --state-dir /absolute/install/state --retain-resolve /absolute/project/request.json
```

These paths are examples of explicit operator-selected locations. Keep local
review copies, requests and evidence inside the current project. A healthy
process must be stopped before this maintenance command; never run a second
fieldbus master. The command itself performs no device connections or scans.

Resolution returns its own action/receipt and requires restart. It does not
clear any previous fault latch or resume a failed operation. Expected-source
SHA256 prevents a stale request from modifying a newer checkpoint. Retrying the
same request after success conflicts with its now-changed source, rather than
silently applying it again. For first initialization without a file, use SHA256
of empty bytes (`e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`).

A rollback to another schema-3-capable release uses this matrix. To restore
removed values, stop and migrate with `migration_source_sha256` selecting the archived source
and `expected_source_sha256` guarding the current checkpoint. The matching old
layout is compiled from the intended release and both sources are preserved.
A rollback to a pre-schema-3 binary is unsafe against live schema 3: that binary
can ignore and overwrite it. Keep the newer recovery-capable runtime or prepare
a separately reviewed legacy export and separate state directory. There is no
automatic downgrade writer.

## Persistence health and loss budget

The scan publishes complete scalar samples to a nonblocking, capacity-one
mailbox; one sample may also be in flight. A full mailbox skips that sample;
the next interval samples current values. It never queues unlimited retries or
waits for storage. Data generation conservatively advances with every completed
scan across all PROGRAMs, plus accepted writes/pulse resets to retained slots
(display scan count remains the maximum). Paused writes also become pending
and are periodically saved; pause does not imply that VM data cannot change.
The status separates sampled generation, replacement and durable completion.
The checkpoint export includes both parsed state and exact `checkpoint_json`
bytes; verify the receipt SHA256 against those bytes when packaging a checkpoint.
The last durable sample's age includes time before sampling, queued time and
storage latency; 5 seconds is the default sample interval, not a maximum loss
window. Pending age stops only when all observed generations are durable.

`max_loss_ms` is an engineering budget, not a guarantee about a drive's cache.
With `warn`, overrun raises `__supervision/retain` and UI attention. With
`inhibit`, the scan also sets the existing output latch and schedules adapter
failsafe attempts. This decision is local to the scan and uses atomic counters,
so disk or HTTP status readers cannot delay it. The latch requires restart;
a later successful save does not automatically re-arm outputs. Adapter
acknowledgements do not prove physical safe state.

A file is fsynced before atomic rename. The separate receipt reports directory
sync success/failure. Existing `project::write_atomic` callers preserve their
historical best-effort directory-sync contract; RETAIN uses the new receipt and
never treats an unconfirmed rename as durable. A final sample is taken only from
completed, non-aborted scans, after which devices drain before the writer is
awaited. The final wait is bounded at two seconds and timeout is retained in the
API/log receipt; a blocked kernel syscall can outlive that timeout and keeps the
exclusive lease until it returns. SIGKILL cannot produce a final receipt.
Residual dot temporaries are ignored, never interpreted as committed checkpoints.

## Offline validation and remaining evidence

`examples/retain_recovery` proves permitted first start with `cs sim run`;
`examples/retain_required` proves missing required data faults before scanning.
The CLI e2e test uses fresh copies so persisted example state cannot make a
missing-data scenario pass accidentally. Rust tests cover compatibility,
64-bit slots, independent PROGRAMs, explicit migrations, source protection,
contention, a stalled writer, failure receipts and final timeout. The project
atomic-writer tests inject write/fsync/rename/directory-sync boundary failures.
`scripts/retain-process-regression.py` checks the original three failures using
actual device-free processes, periodic saves, SIGTERM, SIGKILL/restart, residual
temporaries, concurrent writers and directory permissions, saving all evidence
under an explicitly selected project-local directory.

These tests do not certify storage controller flush semantics, sudden loss of
power, NVRAM/UPS behavior, Linux scheduling or mechanical coordinate recovery.
Physical medium power-cut qualification remains separate. Integration 02 should
consume `RecoveryResult.admitted` and `RetainStatus.policy`; 03 should consume
`compatibility` and durability-confirmed `CheckpointExport`, rather than adding
another name/type compatibility algorithm.
