PROGRAM main
  VAR
    trial_request : BOOL;
    requests_consumed : DINT;
    trial_starts : DINT;
    drive_command : WORD;
    target_position : DINT;
  END_VAR

  IF trial_request THEN
    requests_consumed := requests_consumed + 1;
    trial_request := FALSE;
  END_IF;
  (* The simulated request is consumed without starting motion. *)
  trial_starts := 0;
  drive_command := 0;
  target_position := 0;
END_PROGRAM
