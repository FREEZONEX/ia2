#!/usr/bin/env python3
"""Runs the slice cases (harness.py) and the remaining P0 cases (cases_p0.py).
  python3 -I lab/run_cases.py --bin <ia2-runtime> --out runs [--cases A1,A2,...]"""
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import harness  # noqa: E402
import cases_p0  # noqa: E402
import cases_p1  # noqa: E402

harness.CASES.update(cases_p0.CASES)
harness.CASES.update(cases_p1.CASES)
harness.main()
