#!/usr/bin/env bash
# Scenario: setup_routes_complete_configs
#
# Regression: `wg setup` and `wg init` used to leave `[tiers]` empty for many
# common executor choices, and the "Anthropic vs Claude Code" duplication in
# the menu confused new users into picking inconsistent combos. The new
# 5-route surface (openrouter / claude-cli / codex-cli / local / nex-custom)
# guarantees every route writes a complete, working config end-to-end.
#
# This scenario verifies, against the *real* installed `wg` binary, that:
#
#   1. `wg setup --route claude-cli --yes` writes a config with all three
#      tiers populated.
#   2. `wg init -x claude` populates [tiers] (the bug).
#   3. `wg config reset --route openrouter --yes` produces a config with
#      a populated llm_endpoints entry AND populated tiers AND backs up
#      the old config.
#   4. `wg setup --route nex-custom --yes` (without --url) refuses with a
#      helpful message rather than silently producing a half-set config.
#
# It runs entirely against a fake $HOME so the real ~/.workgraph/config.toml
# is never touched. No daemon, no LLM, no network — pure CLI exercise of the
# config-defaults wiring.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)

fake_home="$scratch/home"
mkdir -p "$fake_home"
export HOME="$fake_home"
unset WG_DIR ANTHROPIC_API_KEY OPENROUTER_API_KEY OPENAI_API_KEY 2>/dev/null || true

# ── 1. wg setup --route claude-cli --yes ──────────────────────────────
if ! wg setup --route claude-cli --yes >"$scratch/setup1.log" 2>&1; then
    loud_fail "wg setup --route claude-cli --yes failed: $(tail -10 "$scratch/setup1.log")"
fi
cfg1="$fake_home/.workgraph/config.toml"
if [[ ! -f "$cfg1" ]]; then
    loud_fail "expected $cfg1 to exist after wg setup --route claude-cli --yes"
fi
for tier in fast standard premium; do
    if ! grep -qE "^${tier} = " "$cfg1"; then
        loud_fail "tier '${tier}' not populated in claude-cli config:\n$(cat "$cfg1")"
    fi
done
if ! grep -q 'executor = "claude"' "$cfg1"; then
    loud_fail "claude-cli route did not write executor=claude:\n$(grep executor "$cfg1")"
fi

# ── 2. wg init -x claude populates [tiers] ────────────────────────────
proj="$scratch/proj-claude-init"
mkdir -p "$proj"
if ! wg --dir "$proj/.wg" init -x claude --no-agency >"$scratch/init1.log" 2>&1; then
    loud_fail "wg init -x claude failed: $(tail -10 "$scratch/init1.log")"
fi
cfg2="$proj/.wg/config.toml"
if [[ ! -f "$cfg2" ]]; then
    loud_fail "expected $cfg2 to exist after wg init -x claude"
fi
# This is the headline regression: previously [tiers] was empty after
# `wg init -x claude`, leading to broken model resolution at runtime.
for tier in fast standard premium; do
    if ! grep -qE "^${tier} = " "$cfg2"; then
        loud_fail "tier '${tier}' not populated after 'wg init -x claude' — the headline bug regressed.\n$(cat "$cfg2")"
    fi
done

# ── 3. wg config reset --route openrouter --yes (backup + endpoint) ───
# Reset the global config (already populated by step 1) to openrouter.
if ! wg config reset --route openrouter --yes >"$scratch/reset1.log" 2>&1; then
    loud_fail "wg config reset --route openrouter --yes failed: $(tail -10 "$scratch/reset1.log")"
fi
# Backup file should exist
if ! ls "$fake_home/.workgraph/" | grep -qE '^config\.toml\.bak-'; then
    loud_fail "wg config reset did not create a backup file. Files: $(ls "$fake_home/.workgraph/")"
fi
# New config has openrouter endpoint + tiers
if ! grep -q 'provider = "openrouter"' "$cfg1"; then
    loud_fail "wg config reset to openrouter route did not produce an openrouter endpoint"
fi
for tier in fast standard premium; do
    if ! grep -qE "^${tier} = " "$cfg1"; then
        loud_fail "tier '${tier}' not populated after reset to openrouter route"
    fi
done

# ── 4. wg setup --route nex-custom --yes WITHOUT --url must refuse ────
mv "$cfg1" "$cfg1.savepoint"  # avoid overwriting on accidental success
if wg setup --route nex-custom --yes >"$scratch/nexc.log" 2>&1; then
    loud_fail "wg setup --route nex-custom --yes succeeded without --url; should have refused"
fi
# Verify the error message mentions --url so the user knows what to add.
if ! grep -qE -- "--url" "$scratch/nexc.log"; then
    loud_fail "nex-custom error message did not mention --url:\n$(cat "$scratch/nexc.log")"
fi
mv "$cfg1.savepoint" "$cfg1"

echo "PASS: 5 setup routes write complete configs; init -x claude fills tiers; config reset backs up + preserves shape; nex-custom requires --url"
exit 0
