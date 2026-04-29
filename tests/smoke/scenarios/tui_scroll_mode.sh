#!/usr/bin/env bash
# Smoke scenario: tui-scroll-mode-toggle (implement-tui-scroll)
#
# Validates the Ctrl+] scroll mode implementation via cargo test.
# No live LLM endpoint required — all assertions are unit tests.
#
# Exit 0  = PASS
# Exit 77 = SKIP (cargo not available)
# Exit 1  = FAIL

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../../.." && pwd)"

# Find the actual repo root (this script lives under tests/smoke/scenarios/).
# Walk up until we find Cargo.toml.
REPO_ROOT="$(cd "$SCRIPT_DIR" && git rev-parse --show-toplevel 2>/dev/null || echo "")"
if [[ -z "$REPO_ROOT" ]]; then
    echo "SKIP: could not locate git repo root" >&2
    exit 77
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo "SKIP: cargo not found" >&2
    exit 77
fi

cd "$REPO_ROOT"

echo "=== Running scroll_mode unit tests ==="
cargo test scroll_mode --quiet 2>&1

echo "=== All scroll_mode tests passed ==="
