(* Offline-only execution-budget fixture. No devices or physical outputs.
   Both flags default FALSE. The scenario explicitly arms and triggers the
   infinite loop only after proving finite execution and its warning alarm. *)
PROGRAM main
VAR
    arm_hang : BOOL;
    run_hang : BOOL;
    scans : DINT;
    total : DINT;
    idx : DINT;
    spin_count : DINT;
END_VAR

total := 0;
FOR idx := 1 TO 100 DO
    total := total + idx;
END_FOR;
scans := scans + 1;

IF arm_hang THEN
    WHILE run_hang DO
        spin_count := spin_count + 1;
    END_WHILE;
END_IF;
END_PROGRAM
