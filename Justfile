# IA2 local task runner. The regression script defines the quality gate.

default:
    @just --list

# Complete offline quality gate. Prepare dependencies separately.
ci:
    python3 scripts/offline-regression.py

fmt:
    python3 scripts/offline-regression.py --suite fmt

clippy:
    python3 scripts/offline-regression.py --suite clippy

test:
    python3 scripts/offline-regression.py --suite rust

web:
    python3 scripts/offline-regression.py --suite web

# Apply rustfmt and Clippy fixes explicitly.
fix:
    cargo fmt --all
    cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged
