#!/usr/bin/env bash
# Smoke: wg html (no flags) emits TUI-parity viewer with clickable viz +
# edge-attributed connector spans + theme-toggle + side panel + valid JSON.
#
# Owners: wg-html-redesign (initial v1 click-target work) and wg-html-v2
# (TUI-parity rewrite — adds edge highlighting, theme support, panel.js asset).
set -euo pipefail

OUTDIR=$(mktemp -d)
trap 'rm -rf "$OUTDIR"' EXIT

# Default invocation: no flags. v2 default is "all tasks" (TUI parity).
wg html --out "$OUTDIR" 2>&1

INDEX="$OUTDIR/index.html"

[ -f "$INDEX" ] || { echo "FAIL: index.html not created"; exit 1; }

# Static assets must accompany the index for rsync deployments / file:// use.
[ -f "$OUTDIR/style.css" ] || { echo "FAIL: style.css missing"; exit 1; }
[ -f "$OUTDIR/panel.js"  ] || { echo "FAIL: panel.js missing";  exit 1; }

# ASCII viz element must be present (not SVG).
grep -q 'class="viz-pre"' "$INDEX" || { echo "FAIL: viz-pre element missing"; exit 1; }

# At least one clickable task-link span (the user's must-have: clickability).
grep -q 'class="task-link"' "$INDEX" || { echo "FAIL: no clickable task-link spans"; exit 1; }

# Side panel container (the detail-overlay anchor for clicks).
grep -q 'id="side-panel"' "$INDEX" || { echo "FAIL: side-panel missing"; exit 1; }

# Theme toggle button (auto + manual override per task spec).
grep -q 'id="theme-toggle"' "$INDEX" || { echo "FAIL: theme-toggle button missing"; exit 1; }

# Inline JSON globals consumed by panel.js.
grep -q 'window\.WG_TASKS'  "$INDEX" || { echo "FAIL: WG_TASKS JSON missing"; exit 1; }
grep -q 'window\.WG_EDGES'  "$INDEX" || { echo "FAIL: WG_EDGES JSON missing"; exit 1; }
grep -q 'window\.WG_CYCLES' "$INDEX" || { echo "FAIL: WG_CYCLES JSON missing"; exit 1; }

# panel.js script tag is wired (separate file, not inline blob).
grep -q 'src="panel.js"' "$INDEX" || { echo "FAIL: panel.js script tag missing"; exit 1; }

# Inline localStorage bootstrap (avoids a flash on dark/light load).
grep -q "localStorage.getItem('wg-html-theme')" "$INDEX" \
    || { echo "FAIL: theme bootstrap script missing"; exit 1; }

# ASCII content should match wg viz output — pick the first task id and
# verify it is wrapped in a task-link span.
FIRST_TASK=$(wg viz --all --no-tui --columns 120 2>/dev/null | head -1 | awk '{print $1}')
if [ -n "$FIRST_TASK" ]; then
    grep -q "data-task-id=\"$FIRST_TASK\"" "$INDEX" \
        || { echo "FAIL: task id '$FIRST_TASK' from viz not clickable in HTML"; exit 1; }
fi

# Validate the inline JSON blobs parse cleanly. Each blob is `window.X = {...};`.
python3 - "$INDEX" <<'PYEOF'
import sys, json
content = open(sys.argv[1]).read()
markers = ['window.WG_TASKS = ', 'window.WG_EDGES = ', 'window.WG_CYCLES = ']
for marker in markers:
    pos = content.find(marker)
    if pos < 0:
        print(f'FAIL: {marker.strip()} not found')
        sys.exit(1)
    pos += len(marker)
    decoder = json.JSONDecoder()
    try:
        obj, _ = decoder.raw_decode(content, pos)
    except json.JSONDecodeError as e:
        print(f'FAIL: {marker.strip()} JSON invalid: {e}')
        sys.exit(1)
    print(f'JSON valid for {marker.strip()}: {len(obj) if hasattr(obj, "__len__") else "?"} entries')
PYEOF
[ $? -eq 0 ] || exit 1

echo "PASS: wg html ASCII viz + clickability + theme + JSON smoke"
