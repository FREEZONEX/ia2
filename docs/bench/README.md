# Bench evidence

IA2's claim is not "it compiles" but "the sim gate predicts the real
machine". This directory is the receipts: what was measured on real
hardware, under what conditions, with what evidence behind each number.

Every number in these documents carries an evidence grade:

| Grade | Meaning |
|---|---|
| **RAW** | A machine-produced artifact (CSV/log) is committed under `data/`, and `data/recompute.py` re-derives the published number from it. If the script exits non-zero, the evidence is invalid/incomplete or an encoded number no longer reproduces — treat that as a release blocker. |
| **TRANSCRIPT** | Values were read live off the bench and quoted verbatim in a linked PR comment or committed report; the underlying journal/CSV was not retained. Strong when the protocol is committed and falsifiable, but you cannot re-derive the digits. |
| **PROSE** | An assertion with neither artifact nor quoted samples behind it. These are labelled where they appear and are not load-bearing. |
| **CONFIG** | The number IS a committed configuration file in this repo (e.g. a register map) — the evidence is the shipped artifact plus the recorded method that verified it. |

Re-run the verification (stdlib Python only, no IA2 code imported):

```bash
python3 docs/bench/data/recompute.py
python3 -m unittest discover -s docs/bench/data -p 'test_*.py' -v
```

Capture schemas, known legacy duplicates, rounding-only tolerances and
negative tests are documented in [`data-validation.md`](data-validation.md).
Passing this gate is not hardware acceptance or validation of every prose
claim. Raw capture files are never silently cleaned up to obtain a pass.

## Documents

- [`ethercat-dc-qualification-20261106.md`](ethercat-dc-qualification-20261106.md)
  — supplemental DC pipeline tests, recurrence with 50 us + full preemption,
  and a 30-minute disabled qualification with 2 us + full; measurement
  transcript and archive hashes, with motion still unqualified.
- [`ethercat-2ms-dc-sync.md`](ethercat-2ms-dc-sync.md) — 2 ms scan
  cadence on a non-RT kernel, DC-SYNC0 CiA402 motion, electronic-gear
  accuracy, watchdog latch A/B, bus-loss self-heal.
- [`modbus-rtu.md`](modbus-rtu.md) — field-verified register map on a
  real RTU coupler, concurrent EtherCAT+RTU operation, analog-loop
  calibration through the coupler.
- [`write-governance-on-hardware.md`](write-governance-on-hardware.md)
  — the per-tag write governance (allowlist / clamp / deny) and the
  write-audit ring exercised against the live edge runtime.
- [`watchdog-latch-runbook.md`](watchdog-latch-runbook.md) — the
  executable bench procedure behind the watchdog A/B result.

## Ground rules these documents follow

- Numbers come from raw data or verbatim bench transcripts — never
  reconstructed from report titles or memory.
- Negative results and unmet gates are listed next to the positives;
  a bench doc that only contains successes is marketing, not evidence.
- Hostnames, addresses, and operator identities are redacted; device
  vendor/model names are kept (they are the test conditions).
