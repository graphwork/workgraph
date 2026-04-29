#!/usr/bin/env bash
# Scenario: chat_archive_kills_tmux_session
#
# Verifies the chat-archive paths (CLI + IPC) tear down the canonical
# wg-chat-* tmux session so we don't accumulate orphan sessions across
# many chats. This is the design's "Archive/delete chat -> kill_
# underlying_session() explicitly" invariant from
# docs/design/chat-agent-persistence.md.
#
# Strategy:
#   1. wg init + wg chat create produces a chat task (.chat-N).
#   2. We MANUALLY start a tmux session named exactly the way the TUI
#      would have ("wg-chat-<project_tag>-chat-<N>") running a long
#      sleep — this models the "TUI was here, spawned the chat under
#      tmux, then the user opened a non-TUI shell and now wants to
#      archive".
#   3. wg chat archive <ref> succeeds and the tmux session is gone
#      within 2 seconds.
#
# Skips when tmux is not on PATH (the design's graceful-fallback path
# can't tear down what was never wrapped).

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg
if ! command -v tmux >/dev/null 2>&1; then
    loud_skip "TMUX MISSING" "tmux not installed; chat persistence path doesn't apply"
fi

scratch=$(make_scratch)
cd "$scratch"

# wg init succeeds in a fresh dir; --executor claude is fine even
# without API keys because we never actually run the agent.
if ! wg init -x claude >init.log 2>&1; then
    loud_fail "wg init failed: $(tail -5 init.log)"
fi

# Create chat-0. Service is not running so wg chat create takes the
# direct-graph path (no IPC).
if ! wg chat create --name "smoke-archive-tmux" >create.log 2>&1; then
    loud_fail "wg chat create failed: $(tail -10 create.log)"
fi

# Find the chat id we just created. Newest is highest numeric suffix.
chat_id=$(wg chat list 2>/dev/null \
    | grep -oE 'chat-[0-9]+' \
    | sort -uV \
    | tail -1 \
    | tr -d 'chat-' || true)
if [[ -z "$chat_id" ]]; then
    loud_fail "couldn't parse chat id from wg chat list output:
$(wg chat list 2>&1 | head -20)"
fi

project_tag="$(basename "$scratch")"
# Match the rust-side sanitization in chat_id::sanitize_session_segment
# (`.` and `:` -> `-`) — tmux session names cannot contain those chars,
# and the production code rewrites them on the way in.
sanitized_tag="${project_tag//./-}"
sanitized_tag="${sanitized_tag//:/-}"
session_name="wg-chat-${sanitized_tag}-chat-${chat_id}"

# Bring up the canonical tmux session running a long sleep. We don't
# need a real chat agent — only a process that proves "the session was
# here" so the post-archive check is meaningful.
if ! tmux new-session -d -s "$session_name" -- sh -c 'sleep 3600' 2>tmux.log; then
    loud_fail "couldn't start tmux session $session_name: $(cat tmux.log)"
fi
register_tmux_kill() {
    tmux kill-session -t "$session_name" 2>/dev/null || true
}
add_cleanup_hook register_tmux_kill

# Sanity: session is alive before archive.
if ! tmux has-session -t "$session_name" 2>/dev/null; then
    loud_fail "fixture session $session_name didn't survive new-session — tmux setup is broken"
fi

# Archive — this is the path under test.
if ! wg chat archive "$chat_id" >archive.log 2>&1; then
    loud_fail "wg chat archive $chat_id failed: $(tail -10 archive.log)"
fi

# Poll for up to 2s — kill-session is fire-and-forget but tmux's
# session-removal latency is ms-scale on Linux.
killed=0
for _ in $(seq 1 10); do
    if ! tmux has-session -t "$session_name" 2>/dev/null; then
        killed=1
        break
    fi
    sleep 0.2
done
if [[ "$killed" -ne 1 ]]; then
    loud_fail "tmux session $session_name still exists 2s after wg chat archive — \
chat-archive path failed to call kill_chat_tmux_session_for_id. \
Active sessions: $(tmux list-sessions 2>/dev/null | head -5)"
fi

echo "PASS: wg chat archive killed tmux session $session_name"
exit 0
