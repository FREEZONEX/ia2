"""Launcher: python3 -I bridge_run.py <bridge args>  (-I ignores the environment; this adds only its own directory)."""
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from bridge.main import main  # noqa: E402

if __name__ == '__main__':
    sys.exit(main())
