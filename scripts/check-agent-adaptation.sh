#!/usr/bin/env bash
# Offline contract test for repository instructions and Agent Skills discovery.
# It uses only a temporary directory: no network, server, or hardware access.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SKILL_NAME="industrial-automation-skill"
SKILL_SRC="$REPO_ROOT/.claude/skills/$SKILL_NAME"
REPO_SKILL_LINK="$REPO_ROOT/.agents/skills/$SKILL_NAME"

fail() { printf 'agent adaptation check failed: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok: %s\n' "$*"; }

[ -s "$REPO_ROOT/AGENTS.md" ] || fail "AGENTS.md is missing or empty"
agents_bytes="$(wc -c < "$REPO_ROOT/AGENTS.md" | tr -d ' ')"
[ "$agents_bytes" -le 32768 ] || fail "AGENTS.md exceeds Codex's default 32 KiB project-instruction budget"
pass "AGENTS.md is present and within the default Codex size budget"

[ -L "$REPO_SKILL_LINK" ] || fail ".agents skill entry is not a symlink"
# An absolute target would resolve on the author's machine but dangle in
# every other clone — the committed link must stay relative.
link_target="$(readlink "$REPO_SKILL_LINK")"
case "$link_target" in
  /*) fail ".agents skill symlink target is absolute ($link_target); it must be relative to survive cloning" ;;
esac
[ -f "$REPO_SKILL_LINK/SKILL.md" ] || fail ".agents skill symlink does not resolve to SKILL.md"
canonical_path="$(cd "$SKILL_SRC" && pwd -P)"
repo_link_path="$(cd "$REPO_SKILL_LINK" && pwd -P)"
[ "$canonical_path" = "$repo_link_path" ] || fail ".agents skill does not resolve to the canonical skill"
pass "repository skill discovery resolves to one canonical copy"

grep -Fxq 'name: industrial-automation-skill' "$SKILL_SRC/SKILL.md" \
  || fail "SKILL.md name is missing or incorrect"
grep -Eq '^description: .+' "$SKILL_SRC/SKILL.md" \
  || fail "SKILL.md description is missing"
[ -f "$SKILL_SRC/agents/openai.yaml" ] || fail "agents/openai.yaml is missing"
grep -Fq 'default_prompt: "Use $industrial-automation-skill' "$SKILL_SRC/agents/openai.yaml" \
  || fail "Codex default prompt must explicitly invoke the skill"
[ -f "$SKILL_SRC/checklists/offline-readiness.md" ] || fail "offline readiness checklist is missing"
pass "skill metadata and offline handoff are present"

for windows_file in scripts/install-skill.ps1 scripts/build-windows.ps1 scripts/check-windows.ps1 scripts/package-windows.ps1 docs/windows.md crates/desktop/Cargo.toml; do
  [ -s "$REPO_ROOT/$windows_file" ] || fail "Windows onboarding file missing: $windows_file"
done
grep -Fq 'scripts/check-windows.ps1' "$REPO_ROOT/AGENTS.md" \
  || fail "AGENTS.md must name the native Windows quality gate"
grep -Fq 'scripts/install-skill.ps1' "$REPO_ROOT/README.md" \
  || fail "README must document the native Windows installer"
grep -Fq 'Windows' "$SKILL_SRC/checklists/first-contact.md" \
  || fail "first contact must explain Windows discovery"
pass "Windows installer, packaging, quality gate and onboarding are present (native checks run on Windows)"
grep -Fq 'IA2.exe' "$SKILL_SRC/SKILL.md" \
  || fail "Windows skill must document the native desktop entry"
grep -Fq '/api/desktop/shutdown' "$REPO_ROOT/docs/api.md" \
  || fail "Desktop lifecycle endpoint must be documented"

bash -n "$REPO_ROOT/scripts/install-skill.sh"
tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/ia2-agent-adaptation.XXXXXX")"
trap 'rm -rf "$tmp_root"' EXIT

PATH="/usr/bin:/bin" \
IA2_BIN_DIR="$tmp_root/bin" \
CLAUDE_DIR="$tmp_root/claude" \
AGENTS_DIR="$tmp_root/agents" \
bash "$REPO_ROOT/scripts/install-skill.sh" --skill-only >/dev/null

installed_skill="$tmp_root/claude/skills/$SKILL_NAME"
installed_link="$tmp_root/agents/skills/$SKILL_NAME"
[ -f "$installed_skill/SKILL.md" ] || fail "skill-only install did not copy SKILL.md"
[ -L "$installed_link" ] || fail "skill-only install did not create the Agent Skills symlink"
installed_path="$(cd "$installed_skill" && pwd -P)"
installed_link_path="$(cd "$installed_link" && pwd -P)"
[ "$installed_path" = "$installed_link_path" ] || fail "installed Agent Skills symlink resolves incorrectly"
diff -r "$SKILL_SRC" "$installed_skill" >/dev/null \
  || fail "installed skill tree differs from the repository source"

# Refusal 1: a destination that aliases the repository's canonical skill
# must be refused BEFORE anything is deleted. Exercise it against a
# throwaway repo copy so a guard regression can only destroy the copy.
guard_repo="$tmp_root/guard-repo"
mkdir -p "$guard_repo/scripts" "$guard_repo/.claude/skills"
cp "$REPO_ROOT/scripts/install-skill.sh" "$guard_repo/scripts/install-skill.sh"
cp -R "$SKILL_SRC" "$guard_repo/.claude/skills/$SKILL_NAME"
if PATH="/usr/bin:/bin" \
   IA2_BIN_DIR="$guard_repo/bin" \
   CLAUDE_DIR="$guard_repo/.claude" \
   AGENTS_DIR="$guard_repo/agents" \
   bash "$guard_repo/scripts/install-skill.sh" --skill-only >/dev/null 2>&1; then
  fail "installer accepted CLAUDE_DIR aliasing the repository skill source"
fi
[ -f "$guard_repo/.claude/skills/$SKILL_NAME/SKILL.md" ] \
  || fail "installer destroyed the skill source before refusing the aliased destination"

# Refusal 2: a non-symlink at the Agent Skills mirror path must be refused
# in preflight, before CLAUDE_DIR is touched at all.
conflict_root="$tmp_root/conflict"
mkdir -p "$conflict_root/agents/skills/$SKILL_NAME"
if PATH="/usr/bin:/bin" \
   IA2_BIN_DIR="$conflict_root/bin" \
   CLAUDE_DIR="$conflict_root/claude" \
   AGENTS_DIR="$conflict_root/agents" \
   bash "$REPO_ROOT/scripts/install-skill.sh" --skill-only >/dev/null 2>&1; then
  fail "installer accepted a non-symlink Agent Skills mirror"
fi
[ ! -e "$conflict_root/claude" ] \
  || fail "installer mutated CLAUDE_DIR before refusing the mirror conflict"

pass "skill-only installer works without Cargo, network, a server, or hardware — and refuses unsafe destinations before mutating"

printf 'IA2 agent adaptation checks passed.\n'
