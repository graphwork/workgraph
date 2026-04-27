#!/usr/bin/env bash
# Scenario: priority_int_and_string_deserialize
#
# Regression: a graph.jsonl row with `"priority":10` (integer) caused the
# `Priority` deserializer to crash with `invalid type: integer 10, expected
# string or map`. Both integer and string forms must read cleanly.
#
# We materialise a synthetic graph.jsonl containing rows with both shapes and
# call `wg list` (a read-only command). It must succeed and show both rows.
# This is fast (no daemon, no LLM) and deterministic.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
trap 'rm -rf "$scratch"' EXIT
cd "$scratch"

# Init to discover the canonical graph dir. We don't care which executor —
# any will produce a `.wg` (or `.workgraph`) layout.
if ! wg init -x shell >init.log 2>&1; then
    loud_fail "wg init failed during smoke setup: $(tail -5 init.log)"
fi

graph_dir=""
for cand in .wg .workgraph; do
    if [[ -f "$scratch/$cand/graph.jsonl" ]]; then
        graph_dir="$scratch/$cand"
        break
    fi
done
if [[ -z "$graph_dir" ]]; then
    loud_fail "could not locate graph.jsonl under .wg/ or .workgraph/ after init"
fi

# Append synthetic rows with int / string / map priority shapes. Using the
# same `kind:task` schema as `wg add`.
cat >>"$graph_dir/graph.jsonl" <<'EOF'
{"kind":"task","id":"int-prio","title":"Integer priority","status":"open","priority":10,"created_at":"2026-04-26T00:00:00+00:00"}
{"kind":"task","id":"str-prio","title":"String priority","status":"open","priority":"high","created_at":"2026-04-26T00:00:00+00:00"}
{"kind":"task","id":"map-prio","title":"Map priority","status":"open","priority":{"name":"normal","value":50},"created_at":"2026-04-26T00:00:00+00:00"}
EOF

# `wg list` must succeed (no deserializer crash).
if ! wg list >list.log 2>&1; then
    loud_fail "wg list crashed reading graph.jsonl with mixed priority shapes:\n$(tail -10 list.log)"
fi

# Both rows must appear.
for id in int-prio str-prio; do
    if ! grep -q "$id" list.log; then
        loud_fail "wg list output missing row '$id':\n$(cat list.log)"
    fi
done

echo "PASS: graph.jsonl with int/string/map priority forms reads cleanly"
exit 0
