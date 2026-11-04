(*
  FB_VALVE — on/off valve (two-position valve) control, with travel timeout and dual-limit fault monitoring.

  Purpose: standard wrapper for a pneumatic/electric on/off valve, with ZSO/ZSC
  dual-limit feedback.
  Example: the feed valve must be fully open before the downstream may start; a travel
  timeout flags a stuck valve.

  Behavior:
    1) Fault monitoring:
       - zso and zsc TRUE at once -> limit-switch fault, immediate fault;
       - the commanded position and the limit feedback disagree (open valve without zso /
         close valve without zsc) for travel_timeout_s (manual dt_s accumulation) ->
         travel-timeout fault;
       - external fault fault_in -> fault. All latched; reset requires fault_in
         cleared and the dual-limit contradiction absent.
    2) Command latch: cmd_open sets out_open when remote and fault-free;
       cmd_close or fault resets it (fail-safe-close on fault, close takes priority).
    3) Status word status: 0 closed / 1 traveling / 2 open / 3 faulted.

  Inputs:
    cmd_open         BOOL  open command
    cmd_close        BOOL  close command (priority)
    remote           BOOL  remote permissive (gates open only)
    zso              BOOL  open limit (Zero Switch Open)
    zsc              BOOL  close limit (Zero Switch Closed)
    fault_in         BOOL  external fault input
    reset            BOOL  fault reset
    travel_timeout_s REAL  travel timeout s
    dt_s             REAL  sample period s
  Outputs:
    out_open BOOL  open output (TRUE open / FALSE close)
    fault    BOOL  fault (latched)
    status   INT   0 closed / 1 traveling / 2 open / 3 faulted
*)
FUNCTION_BLOCK FB_VALVE
  VAR_INPUT
    cmd_open         : BOOL;
    cmd_close        : BOOL;
    remote           : BOOL := TRUE;
    zso              : BOOL;
    zsc              : BOOL;
    fault_in         : BOOL;
    reset            : BOOL;
    travel_timeout_s : REAL := 30.0;
    dt_s             : REAL := 0.1;
  END_VAR
  VAR_OUTPUT
    out_open : BOOL;
    fault    : BOOL;
    status   : INT;
  END_VAR
  VAR
    travel_t : REAL; (* not-in-position accumulated time s *)
    in_pos   : BOOL;
  END_VAR

  (* 1) Fault monitoring *)
  IF zso AND zsc THEN
    fault := TRUE; (* both limits active at once: limit-switch fault *)
  END_IF;

  in_pos := (out_open AND zso) OR (NOT out_open AND zsc);
  IF in_pos THEN
    travel_t := 0.0;
  ELSE
    travel_t := travel_t + dt_s;
  END_IF;
  IF travel_timeout_s > 0.0 AND travel_t >= travel_timeout_s THEN
    fault := TRUE; (* travel timeout: stuck valve / lost limit *)
  END_IF;

  IF fault_in THEN
    fault := TRUE;
  END_IF;
  IF reset AND NOT fault_in AND NOT (zso AND zsc) THEN
    fault    := FALSE;
    travel_t := 0.0;
  END_IF;

  (* 2) Command latch (close/fault priority, fail-safe-close) *)
  IF cmd_open AND remote AND NOT fault THEN
    out_open := TRUE;
  END_IF;
  IF cmd_close OR fault THEN
    out_open := FALSE;
  END_IF;

  (* 3) Status word *)
  IF fault THEN
    status := 3;
  ELSIF out_open AND zso THEN
    status := 2;
  ELSIF NOT out_open AND zsc THEN
    status := 0;
  ELSE
    status := 1;
  END_IF;
END_FUNCTION_BLOCK
