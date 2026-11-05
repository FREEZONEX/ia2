PROGRAM main
VAR
    counter : DINT := 0;
    paired : DINT := 0;
    pair_invalid : BOOL := FALSE;
END_VAR
counter := counter + 1;
paired := counter * 2;
pair_invalid := paired <> counter * 2;
END_PROGRAM
