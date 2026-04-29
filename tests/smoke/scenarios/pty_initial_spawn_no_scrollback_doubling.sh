#!/usr/bin/env bash
# Scenario: pty_initial_spawn_no_scrollback_doubling
#
# Guards the fix for the TUI initial-render PTY scrollback duplication bug
# (fix-pty-scrollback). The earlier resize-time fix (fix-tui-pty / commit
# e0f5d029b) hid SIGWINCH echo rows from scroll navigation, but the user
# still saw "the chat scrollback loops a bit and then settles" on first
# render of a chat tab — without resizing the terminal at all.
#
# Root cause: PtyPane was spawned at hardcoded 24x80, then the first
# frame's resize() to the actual chat-message area dimensions fired
# SIGWINCH. The vendor CLI (claude / codex / wg nex) honored SIGWINCH by
# clear-screen + reprint, pushing wrap-at-80-cols copies of recent content
# into vt100 scrollback alongside the unwrap-at-actual-cols reprint. The
# old dedup hid only the K hot-end echo rows; the wrap-mismatched older
# copies survived and the user saw "duplicates" when scrolling up.
#
# Fix: defer the actual PtyPane::spawn from maybe_auto_enable_chat_pty
# into the chat-tab render path, where msg_area dimensions are known.
# The child process opens its PTY at the real area size from the start,
# so no SIGWINCH echo is ever produced.
#
# This scenario re-runs the unit tests that pin the fix:
#
#   tui::pty_pane::tests::initial_spawn_at_default_then_resize_doubles_long_lines_in_scrollback
#     — demonstrates the bug at the vt100 parser level (lines that
#       wrap at 80 but fit in 120 appear twice in rendered scrollback
#       after a spawn-at-24x80 + resize-to-30x120).
#
#   tui::pty_pane::tests::spawn_at_correct_size_does_not_double_long_lines_in_scrollback
#     — demonstrates the post-fix invariant: spawning at the actual
#       render dimensions from the start produces each line exactly once.
#
#   tui::pty_pane::tests::pty_pane_dims_reports_spawn_size
#     — pins the PtyPane::dims() accessor used by the deferred-spawn
#       tests below.
#
#   tui::viz_viewer::state::chat_pty_deferred_spawn_tests::consume_with_no_pending_returns_false
#   tui::viz_viewer::state::chat_pty_deferred_spawn_tests::consume_with_zero_dims_keeps_pending
#   tui::viz_viewer::state::chat_pty_deferred_spawn_tests::consume_with_real_dims_spawns_at_those_dims
#     — pin the deferred-spawn semantics that drive the fix at the
#       chat-tab spawn-site level: the spawn config is captured up
#       front, kept across no-op zero-dim render frames, and executed
#       exactly once at the first frame that knows the real area.
#
# Exit 77 (SKIP) is NOT used — these tests run against the vt100 parser
# directly and a stub `/bin/sh -c sleep` for the dims check; no live
# endpoint or terminal required.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

run_test() {
    local filter="$1"
    echo "running $filter ..."
    if ! cargo test --bin wg "$filter" 2>&1; then
        echo "FAIL: $filter"
        exit 1
    fi
}

run_test "tui::pty_pane::tests::initial_spawn_at_default_then_resize_doubles_long_lines_in_scrollback"
run_test "tui::pty_pane::tests::spawn_at_correct_size_does_not_double_long_lines_in_scrollback"
run_test "tui::pty_pane::tests::pty_pane_dims_reports_spawn_size"
run_test "tui::viz_viewer::state::chat_pty_deferred_spawn_tests::consume_with_no_pending_returns_false"
run_test "tui::viz_viewer::state::chat_pty_deferred_spawn_tests::consume_with_zero_dims_keeps_pending"
run_test "tui::viz_viewer::state::chat_pty_deferred_spawn_tests::consume_with_real_dims_spawns_at_those_dims"

echo ""
echo "PASS: pty_initial_spawn_no_scrollback_doubling — chat-tab PTY spawns at the real area dims, no SIGWINCH echo on first frame"
exit 0
