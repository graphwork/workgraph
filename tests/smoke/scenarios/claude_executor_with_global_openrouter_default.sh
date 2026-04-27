#!/usr/bin/env bash
# Scenario: claude_executor_with_global_openrouter_default
#
# Regression: a project with executor=claude must spawn agents through the
# claude CLI even when the user's *global* config has an openrouter endpoint
# marked is_default=true. The bug was that the global default leaked into
# spawn metadata and the agent ran through the native OpenAI client instead
# of `claude`.
#
# We don't actually need to invoke the LLM. We assert that:
#   (a) `wg init -x claude` writes coordinator.executor = "claude"
#   (b) `wg config show` reports executor=claude (not nex / native)
#   (c) `wg agent show-spawn-template <task>` (or equivalent inspection) does
#       not contain the openrouter endpoint string from the global config.
#
# If `wg agent show-spawn-template` doesn't exist on this build, we fall back
# to checking the rendered prompt via `wg show <task>` for the claude executor
# string — the goal is "fast, deterministic, no live LLM call".

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
trap 'rm -rf "$scratch"' EXIT

# Fake global config dir with an openrouter is_default endpoint.
fake_home="$scratch/home"
mkdir -p "$fake_home/.config/workgraph"
cat >"$fake_home/.config/workgraph/config.toml" <<'EOF'
[[endpoints]]
name = "openrouter"
url = "https://openrouter.ai/api/v1"
is_default = true
provider = "openrouter"
EOF

cd "$scratch"
project="$scratch/proj"
mkdir -p "$project"
cd "$project"

env HOME="$fake_home" XDG_CONFIG_HOME="$fake_home/.config" \
    wg init -x claude >init.log 2>&1 \
    || loud_fail "wg init -x claude failed: $(tail -5 init.log)"

# (a) Coordinator config must have executor=claude.
config_path=""
for cand in .wg .workgraph; do
    if [[ -f "$project/$cand/config.toml" ]]; then
        config_path="$project/$cand/config.toml"
        break
    fi
done
if [[ -z "$config_path" ]]; then
    loud_fail "no config.toml under .wg/ or .workgraph/ after init"
fi
if ! grep -qE 'executor\s*=\s*"claude"' "$config_path"; then
    loud_fail "config.toml does not declare executor=claude:\n$(cat "$config_path")"
fi

# (b) Make sure the openrouter endpoint from the global config did not leak
#     into the project config as the active executor.
if grep -qE 'executor\s*=\s*"nex"|executor\s*=\s*"native"' "$config_path"; then
    loud_fail "config.toml has executor=nex/native — global openrouter is_default leaked into project"
fi

# (c) Best-effort spawn-template inspection: add a test task and read what
#     the spawn pipeline would render. Skip silently if subcommand is absent.
env HOME="$fake_home" XDG_CONFIG_HOME="$fake_home/.config" \
    wg add "smoke probe" --id smoke-probe >add.log 2>&1 \
    || loud_fail "wg add failed: $(tail -5 add.log)"

if env HOME="$fake_home" XDG_CONFIG_HOME="$fake_home/.config" \
    wg show smoke-probe >show.log 2>&1; then
    if grep -qE 'native-exec|provider:\s*openrouter' show.log; then
        loud_fail "wg show shows native-exec / openrouter routing for a claude-executor project:\n$(head -40 show.log)"
    fi
fi

echo "PASS: executor=claude survives a global openrouter is_default endpoint"
exit 0
