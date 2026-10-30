#!/usr/bin/env python3
"""Runs the slice cases (harness.py), the remaining P0 cases (cases_p0.py) and the P1 cases (cases_p1.py, cases_p1_table.py).
  python3 -I lab/run_cases.py --bin <ia2-runtime> --out runs [--cases A1,A2,...]"""
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import harness  # noqa: E402
import cases_p0  # noqa: E402
import cases_p1  # noqa: E402
import cases_p1_table  # noqa: E402
import cases_d11  # noqa: E402

harness.CASES.update(cases_p0.CASES)
harness.CASES.update(cases_p1.CASES)
harness.CASES.update(cases_p1_table.CASES)
harness.CASES.update(cases_d11.CASES)
if __name__ == '__main__':
    harness.main()
