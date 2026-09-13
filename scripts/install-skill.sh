#!/usr/bin/env bash
#
# install-skill.sh — set up IA2 for a coding agent (Claude Code, Codex, …).
#
# Builds the `cs` CLI + `ia2-server` + its `lsp-launcher` sidecar, installs
# them on your PATH, and drops
# the `industrial-automation-skill` into ~/.claude/skills/ (canonical copy,
# auto-loaded by Claude Code) plus a symlink at ~/.agents/skills/ — the
# Agent Skills standard path scanned by Codex, Kimi Code, Cursor, Gemini
# CLI and friends — so whichever agent you run can author / compile /
# run / debug / deploy IEC 61131-3 PLC programs through IA2.
#
# Just the SKILL (not the binaries)? The recommended route is the
# vercel-labs/skills installer — it copies the skill + its references:
#     npx skills add https://github.com/supcon-international/ia2/tree/main/.claude/skills/industrial-automation-skill
# This script is the one-shot "skill + cs + ia2-server, from a clone" path.
#
# This is the DEV-MACHINE installer. (For provisioning a Linux edge box, see
# infra/install.sh — different thing.)
# Native Windows: scripts/install-skill.ps1 installs IA2.exe and four backend/CLI .exe files,
# built web UI, library and copied user skills without admin or symlinks.
# See docs/windows.md; scripts/check-windows.ps1 is the native quality gate.
#
# Usage — run from a clone of the IA2 repo:
#     git clone --recursive https://github.com/supcon-international/ia2
#     cd ia2 && ./scripts/install-skill.sh
# Or install only the skill (no build, network, or hardware required):
#     ./scripts/install-skill.sh --skill-only
#
# Env knobs:
#     IA2_BIN_DIR   where to install cs + ia2-server   (default: ~/.local/bin)
#     CLAUDE_DIR    Claude config root for the skill    (default: ~/.claude)
#     AGENTS_DIR    Agent-Skills root for the mirror    (default: ~/.agents)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

BIN_DIR="${IA2_BIN_DIR:-$HOME/.local/bin}"
CLAUDE_DIR="${CLAUDE_DIR:-$HOME/.claude}"
AGENTS_DIR="${AGENTS_DIR:-$HOME/.agents}"
SKILL_NAME="industrial-automation-skill"
SKILL_SRC="$REPO_ROOT/.claude/skills/$SKILL_NAME"
SKILL_DST="$CLAUDE_DIR/skills/$SKILL_NAME"
AGENT_SKILL_DST="$AGENTS_DIR/skills/$SKILL_NAME"
LIB_SRC="$REPO_ROOT/library"
LIB_DST="$HOME/.local/share/ia2/library"
SKILL_ONLY="false"

# Scratch dir for the staged copy + swap installs below; removed on any
# exit so an interrupted copy never leaves a half-written tree behind.
stage_dir=""
trap 'if [ -n "$stage_dir" ]; then rm -rf "$stage_dir"; fi' EXIT

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m ✓\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31mERROR:\033[0m %s\n' "$*" >&2; exit 1; }
usage() {
  cat <<'EOF'
Usage: ./scripts/install-skill.sh [--skill-only]

  --skill-only  Install the IA2 agent skill and Codex discovery link only.
                Skips Cargo, submodules, binaries, and the FB library.
EOF
}

case "${1:-}" in
  "") ;;
  --skill-only) SKILL_ONLY="true" ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; die "unknown argument: $1" ;;
esac
[ "$#" -le 1 ] || { usage >&2; die "expected at most one argument"; }

# ---- preflight ----------------------------------------------------------
# Every refusal lives here, BEFORE anything is built, copied, or deleted,
# so a refusing run leaves the machine exactly as it found it.
[ -d "$SKILL_SRC" ] \
  || die "skill source missing at $SKILL_SRC — run this from inside the IA2 repo clone."

# Resolve a directory to its physical path; prints nothing if it does not
# exist. Portable — macOS ships bash 3.2 and no guaranteed realpath.
resolve_dir() { (cd "$1" 2>/dev/null && pwd -P) || true; }

# Aliasing guard: if the destination resolves to the same tree as the
# repository source (CLAUDE_DIR pointed back at the clone, or a symlinked
# parent such as ~/.claude/skills -> <repo>/.claude/skills), the replace
# below would destroy the canonical copy.
skill_src_real="$(resolve_dir "$SKILL_SRC")"
skill_dst_real="$(resolve_dir "$SKILL_DST")"
if [ -n "$skill_dst_real" ] && [ "$skill_dst_real" = "$skill_src_real" ]; then
  die "$SKILL_DST resolves to the repository skill source ($skill_src_real); refusing to overwrite it."
fi

# Mirror conflict: refuse up front, before binaries or skill files move.
if [ -e "$AGENT_SKILL_DST" ] && [ ! -L "$AGENT_SKILL_DST" ]; then
  die "$AGENT_SKILL_DST exists and is not a symlink; leaving it untouched."
fi

if [ "$SKILL_ONLY" = "false" ]; then
  command -v cargo >/dev/null 2>&1 \
    || die "Rust toolchain not found. Install it from https://rustup.rs and re-run."

  # Same aliasing guard for the FB-library copy done after the build.
  if [ -d "$LIB_SRC" ]; then
    lib_src_real="$(resolve_dir "$LIB_SRC")"
    lib_dst_real="$(resolve_dir "$LIB_DST")"
    if [ -n "$lib_dst_real" ] && [ "$lib_dst_real" = "$lib_src_real" ]; then
      die "$LIB_DST resolves to the repository library source ($lib_src_real); refusing to overwrite it."
    fi
  fi

  # The vendored ironplc compiler is a git submodule; the build needs it.
  # An un-checked-out submodule is an empty directory.
  if [ -z "$(ls -A "$REPO_ROOT/vendor/ironplc" 2>/dev/null)" ]; then
    say "fetching the vendored ironplc submodule"
    git -C "$REPO_ROOT" submodule update --init --recursive \
      || die "git submodule update failed (needed for vendor/ironplc)."
  fi

  # ---- build ------------------------------------------------------------
  say "building cs + ia2-server + lsp-launcher (release) — the first build can take a few minutes"
  cargo build --release -p ia2-cli -p server -p lsp-launcher

  # ---- install binaries -------------------------------------------------
  say "installing binaries → $BIN_DIR"
  mkdir -p "$BIN_DIR"
  install -m 0755 "$REPO_ROOT/target/release/cs"     "$BIN_DIR/cs"
  install -m 0755 "$REPO_ROOT/target/release/server" "$BIN_DIR/ia2-server"
  # ia2-server spawns this per editor WebSocket to run the Monaco LSP
  # bridge; it looks for the binary NEXT TO ITSELF, so it must be
  # installed alongside — without it every editor LSP connection dies
  # at spawn.
  install -m 0755 "$REPO_ROOT/target/release/lsp-launcher" "$BIN_DIR/lsp-launcher"
fi

# ---- install skill ------------------------------------------------------
say "installing skill → $SKILL_DST"
mkdir -p "$CLAUDE_DIR/skills"
# Staged copy + swap: the previous install is only deleted once the new
# copy has fully landed, so a failed or interrupted cp cannot destroy it.
stage_dir="$SKILL_DST.tmp.$$"
rm -rf "$stage_dir"
cp -R "$SKILL_SRC" "$stage_dir"
rm -rf "$SKILL_DST"
mv "$stage_dir" "$SKILL_DST"
stage_dir=""

# Mirror at the Agent Skills standard user path (~/.agents/skills) so
# Codex / Kimi Code / Cursor / Gemini CLI discover the same skill.
# Per-skill symlink to the canonical copy — one source of truth.
# (A non-symlink at this path was already refused in preflight.)
say "mirroring skill → $AGENTS_DIR/skills/$SKILL_NAME"
mkdir -p "$AGENTS_DIR/skills"
ln -sfn "$SKILL_DST" "$AGENT_SKILL_DST"

# ---- verify -------------------------------------------------------------
[ -f "$SKILL_DST/SKILL.md" ] || die "installed skill is missing SKILL.md."
[ -L "$AGENT_SKILL_DST" ] || die "Agent Skills mirror was not created."
[ -f "$AGENT_SKILL_DST/SKILL.md" ] || die "Agent Skills mirror does not resolve to SKILL.md."

if [ "$SKILL_ONLY" = "false" ]; then
  "$BIN_DIR/cs" --help >/dev/null 2>&1 || die "cs failed to run after install."
  ok "$("$BIN_DIR/cs" --version 2>/dev/null || echo 'cs') installed and runnable"
fi

# ---- FB-library registry ------------------------------------------------
# The server enables `cs library import` only when it can find a library
# dir. A server started from an arbitrary CWD can't see the repo's
# ./library, so vendor a copy to the installed-layout fallback path the
# server probes (~/.local/share/ia2/library).
if [ "$SKILL_ONLY" = "false" ]; then
  if [ -d "$LIB_SRC" ]; then
    mkdir -p "$(dirname "$LIB_DST")"
    # Same staged copy + swap as the skill install above.
    stage_dir="$LIB_DST.tmp.$$"
    rm -rf "$stage_dir"
    cp -R "$LIB_SRC" "$stage_dir"
    rm -rf "$LIB_DST"
    mv "$stage_dir" "$LIB_DST"
    stage_dir=""
    ok "FB library registry → $LIB_DST"
  fi
fi

# ---- next steps ---------------------------------------------------------
if [ "$SKILL_ONLY" = "true" ]; then
  cat <<EOF

$(ok "Done.")
  skill               → $SKILL_DST
  Agent Skills mirror → $AGENT_SKILL_DST

Restart Codex (or another coding agent) so it discovers the skill.
The IA2 binaries were not built or installed.
EOF
  exit 0
fi

on_path=""; case ":${PATH}:" in *":$BIN_DIR:"*) on_path="yes";; esac

cat <<EOF

$(ok "Done.")
  cs           → $BIN_DIR/cs
  ia2-server   → $BIN_DIR/ia2-server
  lsp-launcher → $BIN_DIR/lsp-launcher
  skill        → $SKILL_DST
  Agent Skills → $AGENT_SKILL_DST

Next:
EOF
[ -n "$on_path" ] || cat <<EOF
  1. Put the binaries on your PATH (this shell didn't have $BIN_DIR):
       echo 'export PATH="$BIN_DIR:\$PATH"' >> ~/.zshrc   # or ~/.bashrc; then reopen the shell
EOF
cat <<EOF
  $([ -n "$on_path" ] && echo 1 || echo 2). Start the IA2 server (headless API on :3001):
       ia2-server --bind 127.0.0.1:3001 &
  $([ -n "$on_path" ] && echo 2 || echo 3). Restart your coding agent so it discovers the skill, then just ask it
     to build a PLC program — it'll use the industrial-automation-skill + cs.

Optional — visual IDE + agent-takeover overlay (needs Node/pnpm):
    pnpm -C apps/web install && pnpm -C apps/web build
    ia2-server --bind 127.0.0.1:3001 --static-dir apps/web/dist
EOF
