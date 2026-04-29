#!/usr/bin/env bash
# Scenario: pty_resize_dedup_no_scrollback_echo
#
# Guards the fix for the TUI PTY scrollback duplication bug (fix-tui-pty).
#
# Root cause: when a PTY child emits a SIGWINCH reflow (clear-screen + full
# repaint), rows that were previously in scrollback get pushed back in by the
# scroll_up path in vt100, creating duplicates at the "hot end" of the
# scrollback VecDeque.
#
# Fix: `PtyPane::resize()` snapshots the pre-resize scrollback count; after
# a 120 ms quiet window (RESIZE_DEDUP_WINDOW), `maybe_resolve_dedup()` computes
# K = post_count - pre_count and stores it in `scrollback_hidden`.  The
# scroll_up / scroll_down methods then skip the K most-recently-appended rows
# so the user never lands on the SIGWINCH echo.
#
# This scenario re-runs the two unit tests that cover the dedup logic:
#
#   tui::pty_pane::tests::sigwinch_reflow_duplicates_scrollback_and_dedup_hides_them
#     — verifies that the bug exists without dedup and is fixed with it.
#
#   tui::pty_pane::tests::scroll_up_skips_sigwinch_hidden_rows
#     — verifies the scroll offset arithmetic of the dedup guard.
#
# Running these as a smoke scenario ensures regressions in the PTY resize /
# scrollback code path are caught at `wg done` time.
#
# Exit 77 (SKIP) is NOT used — these tests do not require an external endpoint
# or live terminal; they run against the vt100 parser directly.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

# Re-run the dedup unit tests.  `cargo test --bin wg` exercises the main binary
# crate where `src/tui/pty_pane.rs` (and its #[cfg(test)] block) lives.
# Note: cargo test accepts exactly one filter arg; run the two tests separately.
echo "running pty_pane sigwinch dedup unit tests..."
if ! cargo test --bin wg \
        "tui::pty_pane::tests::sigwinch_reflow_duplicates_scrollback_and_dedup_hides_them" \
        2>&1; then
    echo "FAIL: sigwinch_reflow_duplicates test failed"
    exit 1
fi
if ! cargo test --bin wg \
        "tui::pty_pane::tests::scroll_up_skips_sigwinch_hidden_rows" \
        2>&1; then
    echo "FAIL: scroll_up_skips_sigwinch_hidden_rows test failed"
    exit 1
fi

echo ""
echo "PASS: pty_resize_dedup — scrollback echo rows are hidden after SIGWINCH reflow"
exit 0
