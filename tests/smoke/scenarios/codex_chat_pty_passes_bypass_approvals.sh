#!/usr/bin/env bash
# Scenario: codex_chat_pty_passes_bypass_approvals
#
# Pins fix-codex-chat (bypass flag) AND fix-codex-chat-2 (correct cwd
# + chat_dir add-dir) AND fix-pass-no (--no-alt-screen).
#
# fix-codex-chat: the codex chat agent in the wg TUI MUST be launched
# with `--dangerously-bypass-approvals-and-sandbox`. Without this flag
# codex prompts the user to approve every shell command (including
# `wg status`, `wg add`), so the chat agent cannot inspect the graph
# or call `wg` tools — the user reported "it cant even see wg".
#
# fix-codex-chat-2: the codex chat agent MUST be spawned with cwd =
# project root, NOT the per-chat scratch dir under `.wg/chat/chat-N/`.
# The first attempt at fix-codex-chat shipped with the spawn cwd still
# pointing at chat_dir, which made the codex banner report
# `directory: <project>/.wg/chat/chat-0` and the agent could not see
# project files or even launch `bash` (compounded by the half-empty
# chat_dir workspace confusing the LLM). The fix mirrors the claude
# chat path's `cwd = project_root` posture; per-chat scratch is still
# made writable via `--add-dir <chat_dir>`.
#
# The user's authorization is implicit: opening `wg tui` from their
# own terminal session is the same gesture as `claude
# --dangerously-skip-permissions` in claude-handler/PTY paths. Both
# paths share this bypass posture.
#
# fix-pass-no: codex defaults to alternate-screen TUI mode, which the
# wg PTY emulator handles poorly (lost scrollback, stacked animation
# frames, fragile cursor-overwrite). The codex CLI exposes
# `--no-alt-screen` for inline / line-streamed output — the same shape
# the claude chat path emits and the shape our emulator handles
# cleanly. The codex chat PTY MUST pass it.
#
# This scenario installs a fake `codex` shim that captures its argv +
# cwd to a file, drives `wg tui` long enough for the auto-PTY to spawn
# the shim once, and asserts:
#   1. argv contains `--dangerously-bypass-approvals-and-sandbox`
#   2. argv contains `--add-dir <chat_dir>` so per-chat scratch stays
#      writable from the project-root cwd
#   3. PWD at spawn time is the project root (the scratch dir), NOT
#      the per-chat scratch dir under `.wg/chat/chat-N/`
#   4. argv contains `--no-alt-screen` (fix-pass-no)
# A pure source-grep would fire on innocent renames; argv + PWD
# capture verifies the actual launch behavior.
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

# Fake codex binary: capture argv + PWD and exit. The TUI's PTY pane
# will spawn this in lieu of real codex. We don't need the binary to
# do anything useful — just record what wg asked it to run AND the
# directory wg ran it from.
shim_dir="$scratch/bin"
mkdir -p "$shim_dir"
argv_log="$scratch/codex.argv"
cwd_log="$scratch/codex.cwd"
cat > "$shim_dir/codex" <<EOF
#!/usr/bin/env bash
# Append all argv (one per line) plus a delimiter so multiple
# invocations are distinguishable. Also record the cwd at spawn time —
# fix-codex-chat-2 pins this to the project root, not the per-chat
# scratch dir under .wg/chat/chat-N/.
{
    echo "----invocation----"
    for a in "\$@"; do
        printf '%s\n' "\$a"
    done
} >> "$argv_log"
{
    echo "----invocation----"
    pwd
} >> "$cwd_log"
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

# Hard assertion 1 (fix-codex-chat): bypass flag MUST be in argv.
if ! grep -q -- '--dangerously-bypass-approvals-and-sandbox' "$argv_log"; then
    loud_fail "codex chat PTY launched WITHOUT --dangerously-bypass-approvals-and-sandbox. Captured argv:
$(cat "$argv_log")"
fi

# Hard assertion 2 (fix-codex-chat-2 / Bug 1): spawn cwd MUST be the
# project root. Resolve through `realpath` so /tmp vs /private/tmp on
# darwin or symlink hops don't false-fail.
expected_cwd="$(cd "$scratch" && pwd -P)"
captured_cwd="$(grep -v '^----invocation----$' "$cwd_log" | head -1)"
captured_cwd_resolved="$(cd "$captured_cwd" 2>/dev/null && pwd -P || echo "$captured_cwd")"
if [[ "$captured_cwd_resolved" != "$expected_cwd" ]]; then
    loud_fail "codex chat PTY spawned with WRONG cwd.
expected (project root): $expected_cwd
captured (spawn pwd):    $captured_cwd_resolved
Bug 1 of fix-codex-chat-2: previous behaviour spawned with cwd =
.wg/chat/chat-N/ which made the codex banner report 'directory:
<proj>/.wg/chat/chat-0' and broke shell command discovery."
fi

# Hard assertion 3 (fix-codex-chat-2 / chat_dir writability): when
# the spawn cwd is the project root, per-chat scratch (codex resume
# markers, session-id) lives under <project>/.wg/chat/chat-N/. We
# pass that via --add-dir so codex can still write there. Reject any
# spawn that omits the flag — it would silently break resume.
if ! grep -q -- '--add-dir' "$argv_log"; then
    loud_fail "codex chat PTY launched WITHOUT --add-dir <chat_dir>. Without it, codex cannot write the resume markers under .wg/chat/chat-N/. Captured argv:
$(cat "$argv_log")"
fi
# Verify the --add-dir argument actually points at chat_dir under
# .wg/chat/. A bare `--add-dir` flag without a chat-dir-shaped value
# would still be a regression.
add_dir_value="$(awk '/^--add-dir$/{getline; print; exit}' "$argv_log")"
case "$add_dir_value" in
    */.wg/chat/*) ;;
    *)
        loud_fail "codex chat PTY --add-dir does not point at a .wg/chat/ chat dir. Got: $add_dir_value"
        ;;
esac

# Hard assertion 4 (fix-pass-no): --no-alt-screen MUST be in argv so
# codex emits inline / line-streamed output instead of alternate-
# screen TUI mode. Without it, scrollback is lost, animation frames
# stack, and cursor-overwrite paths exercise our PTY emulator's
# weakest spots.
if ! grep -q -- '--no-alt-screen' "$argv_log"; then
    loud_fail "codex chat PTY launched WITHOUT --no-alt-screen. Without it codex defaults to alternate-screen mode, which the wg PTY emulator handles poorly (lost scrollback, stacked animation frames). Captured argv:
$(cat "$argv_log")"
fi

echo "PASS: codex chat PTY"
echo "  flag:    --dangerously-bypass-approvals-and-sandbox  ✓"
echo "  cwd:     $captured_cwd_resolved (project root)        ✓"
echo "  add-dir: $add_dir_value                               ✓"
echo "  flag:    --no-alt-screen                              ✓"
echo "Captured argv:"
cat "$argv_log"
exit 0
