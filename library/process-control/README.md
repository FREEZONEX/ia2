# process-control — IEC 61131-3 ST process-control FB library

Common process-control function blocks for IA2 (the vendored ironplc dialect),
all pure ST with no standard-FB dependencies (timing is always done by manually
accumulating `dt_s`, so it is offline-testable and portable across dialects).
The whole library passes `cs check pous/*.st` (all files checked together, so the
demo's cross-file FB references resolve) and a full `cs project check` compile
(including codegen).

## FB index (one per line)

| File | FB | One-line description |
|---|---|---|
| `pous/fb_scale.st` | `FB_SCALE` | Raw counts -> engineering units linear conversion; divide-by-zero guard; out-of-range >5% raises an NE43-style open-circuit flag; optional clamp |
| `pous/fb_lag.st` | `FB_LAG` | First-order lag filter PT1 (PT1/LAG); t_s=0 pass-through; reset / first-scan alignment |
| `pous/fb_leadlag.st` | `FB_LEADLAG` | Lead-lag compensation (feedforward dynamic shaping, lead-lag); discrete approximation as an internal PT1 + pass-through mix |
| `pous/fb_ramp.st` | `FB_RAMP` | Setpoint ramp generator: rate-limited approach to target, independent up/down rates, 0=step, track for bumpless alignment, ramping indication |
| `pous/fb_rate_limit.st` | `FB_RATE_LIMIT` | Rate limiter (velocity limiter): follows a continuously changing input, clamps only |delta|/s; differs from RAMP in having no fixed target |
| `pous/fb_sqrt_flow.st` | `FB_SQRT_FLOW` | DP-flow square-root extraction: flow_max*SQRT(dp/100), low-flow cutoff (default 6.25% DP <-> 25% flow) |
| `pous/fb_flow_comp.st` | `FB_FLOW_COMP` | Gas-flow pressure/temperature compensation (ideal gas): ratio=(p_abs/p_ref)*(t_ref/t), square-root DP multiplies by SQRT(ratio), linear volumetric multiplies directly, invalid measurement passes through |
| `pous/fb_char.st` | `FB_CHAR` | Piecewise-linear characterizer: 2..8-point segmented linear interpolation, clamped at both ends; scalar point-pair form (an FB-scope array can't be codegen'd, see gotcha 7) |
| `pous/fb_limit.st` | `FB_LIMIT` | Value limiter: clamp to [lo,hi] + limit indication q_hi/q_lo; when lo>hi outputs the midpoint with both limit flags lit (configuration-error signal) |
| `pous/fb_stats.st` | `FB_STATS` | Running statistics min/max/mean/population-standard-deviation (Welford single-pass recurrence, no arrays, numerically stable); counted per sample, enable-gated, reset per shift/day for long runs |
| `pous/fb_avg_time.st` | `FB_AVG_TIME` | Tumbling-window time average: one full-window mean avg per window_s + current-window running mean avg_run + ready; a true sliding window needs an array (gotcha 7), use FB_LAG for continuous smoothing |
| `pous/fb_integ.st` | `FB_INTEG` | General integrator `out := out + k*u*dt_s`: priority reset > preset > hold, output clamp + at-limit flags q_hi/q_lo (for the flow-totalizing form see FB_TOTALIZER) |
| `pous/fb_deriv.st` | `FB_DERIV` | Filtered derivative du/dt (units/s): u first PT1 (t_filt_s=0 auto 3*dt_s) then differenced, same core as FB_ALARM_ROC -- outputs the value only, no alarm |
| `pous/fb_deadtime8.st` | `FB_DEADTIME8` | Dead time (dead time / transport delay): 8 scalar taps + phase linear interpolation, resolution t_delay/8; t_delay<=0 pass-through; in series with FB_LAG it is an FOPDT simulation plant |
| `pous/fb_mid3.st` | `FB_MID3` | Median of three / 2oo3 voting: 3 good take the median, 2 good take the average, 1 good pass through, all bad hold + q_fail; deviation monitoring between good channels q_dev |
| `pous/fb_pv_hold.st` | `FB_PV_HOLD` | Bad-value hold/substitute: tracks and remembers good values, on bad first holds for timeout_s to ride out chatter, on timeout holds/substitutes/follows per mode, q_bad/q_timeout indication |
| `pous/fb_pid.st` | `FB_PID` | Incremental (velocity-form) PID: conditional integration + back-calculation anti-windup, bumpless auto/manual; adds ff feedforward, track output tracking, sp_rate setpoint ramp, dev deviation output (all with defaults, backward compatible; typical case: a flow PID driving a variable-speed pump, output limits are the drive min/max frequency) |
| `pous/fb_ratio.st` | `FB_RATIO` | Ratio station: sp = clamp(ratio)*wild_flow + bias, feeds the sp of a downstream FB_PID |
| `pous/fb_cascade.st` | `FB_CASCADE` | Cascade coupling glue (cascade SP coupling): in CAS the master output 0..100% maps to the slave SP span; when not CAS the slave uses its local SP and the master tracks its equivalent percentage via track, bumpless transfer |
| `pous/fb_pid_pos.st` | `FB_PID_POS` | Positional PID (complements velocity-form FB_PID): observable p/i/d terms, SP weighting b, D on PV + PT1 filtering, back-calculation anti-windup, bumpless manual/track |
| `pous/fb_pid_3step.st` | `FB_PID_3STEP` | Step (3-step) PID: open/close pulse control of a motorized actuator, internal positional PID + deadband / minimum pulse width, position estimate pos_est from travel time when there is no position feedback |
| `pous/fb_manstation.st` | `FB_MANSTATION` | Manual/bias station: auto = auto_in+bias (rate-limited + clamped), manual sets directly; balance back-calculates the bias correction on the manual->auto transition edge (bumpless), tracking indication |
| `pous/fb_gain_sched3.st` | `FB_GAIN_SCHED3` | Three-zone gain scheduling: output a kp/ti/td parameter set by hysteresis zone of the scheduling variable (optional +/-hyst linear transition at boundaries), output feeds the parameter inputs of FB_PID / FB_PID_POS directly |
| `pous/fb_rampsoak8.st` | `FB_RAMPSOAK8` | 8-segment program setpoint (ramp-soak): per-segment ramp + soak, run/hold/advance/reset + optional cycling, scalar-expanded segment parameters (gotcha 7), latched on segment entry |
| `pous/fb_select_hl.st` | `FB_SELECT_HL` | High/low selector (override control, >/< selection), a_selected indicates the selected channel |
| `pous/fb_split_range.st` | `FB_SPLIT_RANGE` | Split-range output: u 0..100 split between two valves, segment A [0,split] 0->100, segment B [split,100] 0->100 or reversed (reverse_b), clamped and continuous throughout |
| `pous/fb_pwm.st` | `FB_PWM` | Time-proportioning output (time-proportioning): 0..100% -> on/off duty cycle over a fixed period, with minimum on/off-time clamping, clears the phase when disabled |
| `pous/fb_alarm_hl.st` | `FB_ALARM_HL` | Four-level H/L/HH/LL alarm, deadband + delay + simplified ISA-18.2 acknowledge latch, with the raw limit flags |
| `pous/fb_alarm_dev.st` | `FB_ALARM_DEV` | Deviation alarm (pv-sp over/under deviation, deviation alarm), deadband + delay + acknowledge latch (same pattern as FB_ALARM_HL) |
| `pous/fb_alarm_roc.st` | `FB_ALARM_ROC` | Rate-of-change alarm: the derivative is denoised by an internal PT1 (3*dt_s), |roc| over limit trips after a delay + latch, the roc output is usable for trend |
| `pous/fb_debounce.st` | `FB_DEBOUNCE` | DI debounce: u continuously held for t_on_s sets / t_off_s resets (independent bidirectional confirmation) |
| `pous/fb_motor.st` | `FB_MOTOR` | Motor start/stop wrapper: remote-gated start, stop priority, feedback-timeout-mismatch fault latch, status word 0/1/2/3 |
| `pous/fb_valve.st` | `FB_VALVE` | On/off valve wrapper: ZSO/ZSC feedback, travel timeout and dual-limit fault, fail-safe-close, status word 0/1/2/3. Reset cannot clear an active dual-limit or external fault; after the cause clears, reset accepts a held open request. |
| `pous/fb_motor_rev.st` | `FB_MOTOR_REV` | Bidirectional motor: forward/reverse/stop (stop priority), a reversal forces a full-stop interlock for reversal_delay_s, feedback-timeout / dual-feedback fault latch, status word 0..4 |
| `pous/fb_mov.st` | `FB_MOV` | Motor-operated valve (open/stop/close + ZSO/ZSC dual limits): auto-stop at position, mid-stop allowed, travel timeout and dual-limit fault latch, status word 0..4 |
| `pous/fb_dose.st` | `FB_DOSE` | Batch dosing: coarse/fine two stages + early in-flight cutoff of the fine valve, flow-integral metering, done + overshoot after settling; abort stops the valves and retains the quantity, reset begins the next batch |
| `pous/fb_runtime.st` | `FB_RUNTIME` | Equipment run-hours + start-count statistics (rising-edge count), reset clears, the instance may go in VAR RETAIN, feeds FB_DUTY2 for hour balancing |
| `pous/fb_duty2.st` | `FB_DUTY2` | Dual-pump duty/standby: select duty by rotation each start or by hour balancing, the already-running pump takes over bumplessly, on a duty fault the standby takes over immediately, on a dual fault both stop; pure edge memory, no timing |
| `pous/fb_interlock8.st` | `FB_INTERLOCK8` | 8-channel interlock summary + first-out recording (DCS first-out): resets only when all conditions are clear and reset is given, enable1..8 can bypass |
| `pous/fb_totalizer.st` | `FB_TOTALIZER` | Flow totalizing m3/h -> m3 (`total := total + flow*dt_s/3600`), reset clears, the instance may go in VAR RETAIN |
| `pous/fb_hyst.st` | `FB_HYST` | Bidirectional hysteresis switch: decides high-side-on or low-side-on automatically from the relative position of on_sp/off_sp (e.g. current-controlled feeding) |
| `pous/fb_hilo_fill.st` | `FB_HILO_FILL` | High-close low-open supply control (tank make-up / silo refill), holds in between |
| `pous/fb_pulser.st` | `FB_PULSER` | Periodic pulse generator (air-cannon / air-hammer purge): a pulse_len_s pulse every period_s, clears the phase when disabled |
| `pous/fb_alt2.st` | `FB_ALT2` | Dual-valve timed rotation + two external permissives (iron remover + packing scale): losing either permissive closes both valves |
| `pous/demo_main.st` | `demo_main` (PROGRAM) | Mini carbonation-tower loop demo: span -> PID -> alarm -> air cannon -> make-up + ratio -> PID(feedforward/track) -> split range, dual-pump DUTY2+RUNTIME, INTERLOCK8 first-out + steam cascade (CASCADE + two PIDs), FOPDT plant simulation (DEADTIME8+PT1), MID3 voting, DERIV/LIMIT/INTEG/STATS/AVG_TIME + the 0.3.0 extension chain (PV_HOLD->RAMPSOAK8->GAIN_SCHED3->PID_POS->MANSTATION, PID_3STEP, MOTOR_REV/MOV/DOSE device simulation, FLOW_COMP) |

## Usage notes

- **How to consume**: copy the `fb_*.st` files you need into your project's
  `pous/`, declare instances in your PROGRAM and call them with named parameters;
  scheduling is decided by the project `tasks.toml` (do not write a CONFIGURATION in
  the POU files).
- **`dt_s` convention**: every FB with time-based behavior takes a REAL sample
  period (seconds). Pass `tasks.toml`'s `interval_ms / 1000.0` (default 100 ms ->
  `0.1`). If you change the task period, remember to change this constant in step,
  otherwise every delay/pulse/total runs on the wrong time base.
- **Retention**: place `FB_TOTALIZER` / `FB_RUNTIME` instances under
  `VAR RETAIN ... END_VAR` to have them retained by IA2's retain.json snapshot. Note
  that IA2 restores via i32 access, so the retention precision of a REAL accumulated
  value is limited by this (count/setpoint types are fine).
- **Alarm acknowledge**: the `ack` of `FB_ALARM_HL` / `FB_ALARM_DEV` / `FB_ALARM_ROC`
  is level-active; the output semantics are `alarm = trip condition OR (latched AND
  NOT ack)` -- while the condition exists the alarm stays on, and after the condition
  clears it is held until acknowledged. `FB_INTERLOCK8` is the same but uses `reset`:
  it resets only when all conditions are clear and reset is given, and the first-out
  number is cleared on reset.
- **FB_PID upgrade (feedforward / tracking / SP ramp) is backward compatible**: the
  added inputs `ff`, `track` + `track_value` and `sp_rate` all have defaults, and at
  their defaults behavior is scan-for-scan identical to the old version, so existing
  calls need no changes. Mode priority is track > manual > auto; `ff` is added before
  the clamp, and the per-scan back-calculation `acc := out - ff` keeps clamping,
  ff increase/decrease and mode switching all bumpless; the `dev` output is the
  working deviation after the SP ramp (pv - sp_int).
- **First-scan alignment**: `FB_LAG` / `FB_LEADLAG` / `FB_RATE_LIMIT` automatically
  do `out := u` on the first scan (avoiding the startup transient of climbing from 0);
  the SP ramp of `FB_RAMP` / `FB_PID` does the same (RAMP's own output aligns via
  track); `FB_DERIV` (filter-state alignment, out cleared) and `FB_DEADTIME8` (the tap
  chain filled with u) do the same.
- **demo_main.st instantiates the sibling FBs**: the demonstration PROGRAM declares
  instances of the blocks defined in the neighbouring `fb_*.st` files rather than
  carrying its own copies, so it is checked together with them -- `cs check pous/*.st`
  resolves the FB types across all the files in one pass. To reuse the demo in a real
  project, copy `demo_main.st` along with the `fb_*.st` files it instantiates.

### Validation commands (as-delivered observed results)

```text
$ target/release/cs check library/process-control/pous/*.st
✓ 45 files clean        # exit code 0

# cross-file + codegen full validation (temporary project of all 45 pous files:
# 44 FBs + demo_main, which instantiates 28 of them -- validates the whole library
# + demo in one pass)
$ target/release/cs project check /tmp/libcheck-p2
✓ project libcheck_p2 compiles cleanly
```

## Dialect gotchas hit (vendored ironplc / cs)

1. **`cs check` resolves types across every file passed in one invocation; a file
   checked on its own sees only its own declarations.** When a PROGRAM instantiates an
   FB declared in another file, checking that file alone reports `P2008 Cannot
   determine kind of type identifier` + `P4012 invocation is not a variable in scope`;
   passing the interdependent set together (`cs check pous/*.st`) resolves the
   references and the check passes. A project-level compile (`cs project check <dir>`,
   offline, no server needed) resolves across the whole project the same way and also
   runs codegen. This is why the demo is checked with `cs check pous/*.st` rather than
   on its own.
2. **`dt` / `DT` is a reserved word** (the DATE_AND_TIME type); using it as a variable
   name is an immediate P0002 syntax error. This library uniformly uses `dt_s`,
   `td_s`, `ti_s`.
3. **Duplicate FUNCTION_BLOCK declarations are tolerated under a project compile**
   (observed: the same FB, same name and body, appearing in two files, passes
   `cs project check` with no duplicate-declaration error). The library keeps a single
   declaration per FB; the tolerance is only worth knowing if you vendor a block a
   project already defines elsewhere, since the compiler then won't flag the clash.
4. **`MAX()` / `ABS()` / `SQRT()` are observed usable** (both the static check and the
   project-compile codegen pass; SQRT is actually used by `FB_SQRT_FLOW`). For
   portability this library still implements MAX/ABS with inline `IF` (e.g. the
   divide-by-zero floor in `FB_SCALE`), not relying on the built-in function table.
5. **The following are all verified usable** (probed one by one before writing the
   library): non-ASCII UTF-8 in `(* ... *)` comments and characters like the arrow and
   greater-or-equal sign, scientific-notation
   literals (`1.0E-6`), negative-literal initial values, early `RETURN` inside an FB
   body, `VAR_INPUT` defaults (`kp : REAL := 1.0;`, including default-TRUE ones such as
   `enable1 : BOOL := TRUE`), multi-line named-parameter calls, expressions directly as
   arguments (`c4 := fault_a AND fault_b`, `ff := gas_flow * 0.01`), one instance's
   output directly as another call's argument (`permissive1 := mot.out_run`),
   `CASE ... OF 1, 2: ... ELSE ... END_CASE`,
   `WHILE ... DO ... END_WHILE` (both static check and project codegen pass, probe-verified;
   `FB_DEADTIME8` uses it as a bounded catch-up shift loop, <=8 iterations/scan to keep
   scan determinism), identifiers like
   `enable/reset/auto/direct/total/state/out/q` not colliding with reserved words, and
   a project with multiple PROGRAMs and multiple tasks.toml `[[programs]]` entries bound
   to one task. Counter-example: an IF/ELSIF branch body cannot contain only a comment
   with no statement (an empty statement list is a P0002 syntax error).
6. **General dialect rules** (inherited from the IA2 skill docs, followed by this
   library): use `AND/OR/NOT` for booleans, not `&&/||/!`; end every statement with `;`
   (including the last one before `END_IF`); do not write CONFIGURATION/TASK in POU
   files (synthesized from tasks.toml); prefer manual `dt_s` accumulation for timing
   over TON/TP instances (testable, portable -- this library's zero standard-FB
   dependency stems from this).
7. **An ARRAY in FUNCTION_BLOCK scope cannot be codegen'd (and `cs check` does not catch
   it).** `ARRAY[1..8] OF REAL` in an FB's VAR / VAR_INPUT **passes the static check**
   (including variable-index reads/writes), but the project compile reports
   `P9999 Capability is not implemented` at the codegen stage
   (`compile_array.rs#L156`: `array_vars` only registers PROGRAM-scope variables).
   PROGRAM-scope arrays are fully usable (element reads/writes, variable indices, and
   `:= [..]` literal initialization all pass codegen); passing a whole array as an
   argument to an FB is naturally also unusable. This is why `FB_CHAR` uses scalar
   point pairs x1..x8/y1..y8 instead of an array. Lesson: **passing `cs check` is not
   passing codegen** -- always probe new syntax all the way through with
   `cs project check`.
8. **An FB instance's output member cannot be the leftmost operand of an IF/ELSIF
   condition (and `cs check` does not catch it).** `IF inst.q THEN`, `IF NOT inst.q
   THEN`, `IF (inst.q) THEN`, `IF inst.r > 0.5 THEN` all pass the static check, but the
   project-compile codegen reports P9999 (`compile_expr.rs#L36`: the condition type is
   taken from the leftmost operand's resolved_type, and a structured variable lacks that
   annotation). `IF b AND inst.q THEN` (the member not leftmost), the right-hand value
   `x := inst.q`, the right-hand expression `x := inst.q AND b`, and the call argument
   `good := NOT inst.q` are all usable. The idiom (used throughout the demo): copy the
   FB output into a local variable first, then use the local in the condition. Observed
   on 0.3.0.
