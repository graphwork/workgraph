#!/usr/bin/env bash
# Scenario: codex_init_route_has_correct_defaults
#
# Regression (fix-codex-cli): `wg init --route codex-cli` wrote stale model
# strings (codex:o1-pro deprecated 2026-10-23; codex:gpt-5/gpt-5-mini didn't
# exist as versioned aliases) and omitted [models.evaluator] / [models.assigner],
# so agency meta-tasks (.evaluate-*, .flip-*, .assign-*) silently fell back to
# claude:haiku even on a codex-only project.
#
# Fix (2026-04-28): codex-cli route now writes:
#   [agent].model          = codex:gpt-5.4        (balanced/sonnet-equivalent)
#   [tiers].fast           = codex:gpt-5.4-mini   (haiku-equivalent)
#   [tiers].standard       = codex:gpt-5.4
#   [tiers].premium        = codex:gpt-5.5        (opus-equivalent, released 2026-04-23)
#   [models.evaluator].model = codex:gpt-5.4-mini
#   [models.assigner].model  = codex:gpt-5.4-mini
#
# This smoke pins the `wg init --dry-run` output and `wg config show` output
# against those assertions. No LLM endpoint is required — this is a pure
# config-generation test.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)

fake_home="$scratch/home"
mkdir -p "$fake_home/.config/workgraph"
: >"$fake_home/.config/workgraph/config.toml"

run_wg() {
    env -u WG_EXECUTOR_TYPE -u WG_MODEL -u WG_TIER -u WG_AGENT_ID -u WG_TASK_ID \
        HOME="$fake_home" XDG_CONFIG_HOME="$fake_home/.config" \
        wg "$@"
}

# ── Part 1: dry-run output ────────────────────────────────────────────────────

dry_out=$(run_wg init --route codex-cli --dry-run 2>&1) || \
    loud_fail "wg init --route codex-cli --dry-run failed: $dry_out"

# Agent/dispatcher model must be the balanced tier.
if ! grep -q 'model = "codex:gpt-5.4"' <<<"$dry_out"; then
    loud_fail "dry-run output missing 'model = \"codex:gpt-5.4\"'. Got:\n$dry_out"
fi

# Tiers must all be present with the correct versioned strings.
if ! grep -q 'fast = "codex:gpt-5.4-mini"' <<<"$dry_out"; then
    loud_fail "dry-run output missing fast tier codex:gpt-5.4-mini. Got:\n$dry_out"
fi
if ! grep -q 'standard = "codex:gpt-5.4"' <<<"$dry_out"; then
    loud_fail "dry-run output missing standard tier codex:gpt-5.4. Got:\n$dry_out"
fi
if ! grep -q 'premium = "codex:gpt-5.5"' <<<"$dry_out"; then
    loud_fail "dry-run output missing premium tier codex:gpt-5.5. Got:\n$dry_out"
fi

# Agency role-split models must be present (the regression: these were missing).
if ! grep -q 'codex:gpt-5.4-mini' <<<"$dry_out"; then
    loud_fail "dry-run output missing codex:gpt-5.4-mini for evaluator/assigner. Got:\n$dry_out"
fi

# No deprecated model strings.
if grep -q 'o1-pro' <<<"$dry_out"; then
    loud_fail "dry-run output contains deprecated codex:o1-pro. Got:\n$dry_out"
fi
if grep -qE 'gpt-5-mini|gpt-5"' <<<"$dry_out"; then
    loud_fail "dry-run output contains stale bare codex:gpt-5-mini or codex:gpt-5. Got:\n$dry_out"
fi
if grep -q 'claude:' <<<"$dry_out"; then
    loud_fail "dry-run output for codex-cli route contains 'claude:' (should be pure codex). Got:\n$dry_out"
fi

# ── Part 2: actual init + config show ─────────────────────────────────────────

project="$scratch/proj"
mkdir -p "$project"
cd "$project"

if ! run_wg init --route codex-cli --no-agency >init.log 2>&1; then
    loud_fail "wg init --route codex-cli failed: $(tail -10 init.log)"
fi

config_out=$(run_wg config --show 2>&1) || \
    loud_fail "wg config --show failed after init: $config_out"

# All four model strings must be codex-prefixed.
for model_str in "codex:gpt-5.4" "codex:gpt-5.4-mini" "codex:gpt-5.5"; do
    if ! grep -q "$model_str" <<<"$config_out"; then
        loud_fail "wg config show missing $model_str after codex-cli init. Got:\n$config_out"
    fi
done

# No claude: strings should appear from the codex route.
if grep -q 'claude:' <<<"$config_out"; then
    loud_fail "wg config show for codex-cli route contains 'claude:' (agency fallback regression). Got:\n$config_out"
fi

# ── Part 3: wg migrate config rewrites stale codex model strings ─────────────

# Build a project with the old config to test migration/lint.
old_project="$scratch/old_proj"
mkdir -p "$old_project"
cd "$old_project"

# Initialize with fresh codex init first to get the skeleton, then overwrite
# the model strings to simulate the pre-fix state.
if ! run_wg init --route codex-cli --no-agency >old_init.log 2>&1; then
    loud_fail "wg init for old config test failed: $(tail -10 old_init.log)"
fi

wg_dir=""
for cand in .wg .workgraph; do
    if [[ -d "$old_project/$cand" ]]; then
        wg_dir="$old_project/$cand"
        break
    fi
done
if [[ -z "$wg_dir" ]]; then
    loud_fail "no .wg/.workgraph after wg init for old config test"
fi

# Overwrite with stale model strings to simulate a pre-fix config.
cat >"$wg_dir/config.toml" <<'EOF'
[agent]
model = "codex:o1-pro"

[tiers]
fast = "codex:gpt-5-mini"
standard = "codex:gpt-5"
premium = "codex:o1-pro"
EOF

# `wg config lint --local` should flag the stale strings.
lint_out=$(run_wg --dir "$wg_dir" config lint --local 2>&1) || true
if ! grep -qiE 'o1-pro|stale|rewrite|issue|would' <<<"$lint_out"; then
    loud_fail "wg config lint --local did not flag stale codex:o1-pro. Output:\n$lint_out"
fi

echo "PASS: wg init --route codex-cli writes correct tiered codex defaults with agency role-split models"
exit 0
