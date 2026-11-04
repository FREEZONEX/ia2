PROGRAM main
VAR
    sp_flow : REAL;
    big : DINT;
    lamp : BOOL;
    ticks : UDINT := 0;
END_VAR
ticks := (ticks + 1) MOD 1000000000;
END_PROGRAM
