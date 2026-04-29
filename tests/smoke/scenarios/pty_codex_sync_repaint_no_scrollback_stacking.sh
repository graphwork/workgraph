#!/usr/bin/env bash
# Scenario: pty_codex_sync_repaint_no_scrollback_stacking
#
# Pins fix-codex-chat-3: the user-reported regression in `wg tui`'s
# codex chat tab where (1) "real-time animations don't render" and
# (2) "scrolling up is repeating the animation text."
#
# Both symptoms are PTY emulation gaps:
#
#   (1) The event loop's idle-poll branch only set `needs_redraw=true`
#       on `has_timed_ui_elements()` ∨ `is_refresh_due()`, neither of
#       which observed PTY byte arrivals. Codex's interactive TUI emits
#       animation frames at 10–20 Hz, but the wg TUI redrew at the
#       global 1 Hz refresh interval, so the user saw a stalled
#       spinner. The fix wires `chat_pty_has_new_bytes()` (a
#       per-pane bytes_processed watermark) into has_timed_ui_elements
#       so any byte arrival on any embedded chat PTY triggers a redraw.
#
#   (2) Codex emits each animation frame as a `\x1b[?2026h` ... full
#       repaint with `\r\n`-separated rows ... `\x1b[?2026l` block
#       (DEC mode 2026, synchronized output). The repaint's trailing
#       newline scrolls one row off the top per frame, and after enough
#       frames the user sees stacked spinner copies in scrollback. vt100
#       0.16 doesn't implement BSU/ESU itself, so the fix intercepts
#       the markers in the PTY reader thread (`manage_sync_mode_scrollback`)
#       and trims any scrollback growth produced inside a sync block.
#
# This scenario re-runs the unit tests that pin both halves of the fix.
# No live LLM endpoint or terminal required — the byte streams are
# synthesized to match codex's actual repaint pattern, captured in the
# fix-codex-agent task's /tmp/codex-tui-long.bin.

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

# Bug 2: scrollback stacking from sync-mode repaints.
run_test "tui::pty_pane::tests::raw_sync_mode_repaint_scrolls_into_scrollback_without_intervention"
run_test "tui::pty_pane::tests::sync_mode_block_trim_removes_scrolled_rows"
run_test "tui::pty_pane::tests::five_sync_mode_frames_each_trim_independently"
run_test "tui::pty_pane::tests::no_sync_markers_means_no_scrollback_trim"
run_test "tui::pty_pane::tests::chunk_contains_sync_markers_detection"
run_test "tui::pty_pane::tests::snapshot_skip_recent_scrollback_drops_correct_rows"
run_test "tui::pty_pane::tests::pty_pane_codex_sync_repaint_does_not_stack_scrollback_rows"

# Bug 1: animation frames stalling because the idle poll branch never
# redrew on PTY byte arrivals.
run_test "tui::viz_viewer::state::chat_pty_redraw_trigger_tests::no_panes_means_no_fresh_bytes"
run_test "tui::viz_viewer::state::chat_pty_redraw_trigger_tests::fresh_pane_with_output_reports_new_bytes"
run_test "tui::viz_viewer::state::chat_pty_redraw_trigger_tests::watermark_update_clears_fresh_bytes_signal"
run_test "tui::viz_viewer::state::chat_pty_redraw_trigger_tests::watermark_update_prunes_dropped_panes"
run_test "tui::viz_viewer::state::chat_pty_redraw_trigger_tests::has_timed_ui_elements_observes_fresh_pty_bytes"

echo ""
echo "PASS: pty_codex_sync_repaint_no_scrollback_stacking — animations render between keypresses AND DEC mode 2026 sync repaints don't stack frames in scrollback"
exit 0
