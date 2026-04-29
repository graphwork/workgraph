#!/usr/bin/env bash
# Scenario: codex_chat_pty_passes_bypass_approvals
#
# Pins fix-codex-chat: the codex chat agent in the wg TUI MUST be
# launched with `--dangerously-bypass-approvals-and-sandbox`. Without
# this flag codex prompts the user to approve every shell command
# (including `wg status`, `wg add`), so the chat agent cannot inspect
# the graph or call `wg` tools — the user reported "it cant even see
# wg".
#
# The user's authorization is implicit: opening `wg tui` from their
# own terminal session is the same gesture as `claude
# --dangerously-skip-permissions` in claude-handler/PTY paths. Both
# paths share this bypass posture.
#
# This scenario installs a fake `codex` shim that captures its argv to
# a file, drives `wg tui` long enough for the auto-PTY to spawn the
# shim once, and asserts the captured argv contains the bypass flag.
# A pure source-grep would fire on innocent renames; argv capture
# verifies the actual launch behavior.
#
# No real codex CLI required (the shim never calls it). No LLM.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

if ! command -v tmux >/dev/null 2>&1; then
    loud_skip "MISSING TMUX" "tmux not on PATH; cannot drive a PTY for the TUI"
fi

scratch=$(make_scratch)
session="wgsmoke-codex-bypass-$$"
kill_tmux_session() {
    tmux kill-session -t "$session" 2>/dev/null || true
}
add_cleanup_hook kill_tmux_session

# Fake codex binary: capture argv and exit. The TUI's PTY pane will
# spawn this in lieu of real codex. We don't need the binary to do
# anything useful — just record what wg asked it to run.
shim_dir="$scratch/bin"
mkdir -p "$shim_dir"
argv_log="$scratch/codex.argv"
cat > "$shim_dir/codex" <<EOF
#!/usr/bin/env bash
# Append all argv (one per line, NUL-terminated for safety) plus a
# delimiter so multiple invocations are distinguishable.
{
    echo "----invocation----"
    for a in "\$@"; do
        printf '%s\n' "\$a"
    done
} >> "$argv_log"
# Hold the process open briefly so the TUI sees a live child; a quick
# exit can race the dump cycle and fail to register as "spawned".
sleep 30
EOF
chmod +x "$shim_dir/codex"
export PATH="$shim_dir:$PATH"

cd "$scratch"

# Init with codex as the executor + a codex-flavored model so the TUI's
# auto-PTY picks the codex spawn branch.
if ! wg init -m codex:gpt-5 --no-agency >init.log 2>&1; then
    loud_fail "wg init failed: $(tail -10 init.log)"
fi

# Start the dispatcher so `wg tui` has live state to render.
start_wg_daemon "$scratch" --max-agents 1
graph_dir="$WG_SMOKE_DAEMON_DIR"

# Create a chat with --executor codex so the per-chat override drives
# the auto-PTY into the codex branch (not whatever the global default
# might cascade to).
if ! wg chat create --executor codex --model codex:gpt-5 \
        --name codexbypass >create.log 2>&1; then
    loud_fail "wg chat create failed: $(tail -10 create.log)"
fi

# Drive wg tui long enough for the auto-PTY to fire once.
tmux new-session -d -s "$session" -x 200 -y 60 "wg tui"

# Wait for the shim to record at least one invocation.
deadline=$(( $(date +%s) + 30 ))
while (( $(date +%s) < deadline )); do
    if [[ -s "$argv_log" ]]; then
        break
    fi
    sleep 0.5
done

if [[ ! -s "$argv_log" ]]; then
    loud_fail "wg tui never spawned codex shim within 30s. dump:\n$(wg --json tui-dump 2>&1 | head -40)"
fi

# Hard assertion: bypass flag MUST be present in the captured argv.
if ! grep -q -- '--dangerously-bypass-approvals-and-sandbox' "$argv_log"; then
    loud_fail "codex chat PTY launched WITHOUT --dangerously-bypass-approvals-and-sandbox. Captured argv:\n$(cat "$argv_log")"
fi

echo "PASS: codex chat PTY launched with --dangerously-bypass-approvals-and-sandbox. Captured argv:"
cat "$argv_log"
exit 0
