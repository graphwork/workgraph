#!/usr/bin/env bash
# reproduce.sh — Reproduce the Terminal-Bench experiment from the workgraph paper
#
# This script runs all three experimental conditions (A, B, C) against the full
# Terminal-Bench 2.0 task suite. Each condition uses the same model (Minimax M2.7
# via OpenRouter) with the same parameters; only the agent scaffolding differs.
#
# Prerequisites:
#   - OPENROUTER_API_KEY set in environment
#   - Python 3.10+ with harbor framework installed: pip install harbor-bench
#   - Docker daemon running (TB tasks run in containers)
#   - Pre-pulled Docker images: bash terminal-bench/pre-pull-images.sh
#   - Workgraph adapter installed: pip install -e terminal-bench/
#
# Usage:
#   bash terminal-bench/reproduce.sh [--trials N] [--condition A|B|C|all] [--output-dir DIR]
#
# Cost estimate: ~$20 total for 3 conditions × 89 tasks × 3 trials with Minimax M2.7
# Time estimate: ~15 hours per condition with --n-concurrent 4

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# ── Defaults ────────────────────────────────────────────────────────────────
TRIALS=3
CONDITION="all"
OUTPUT_DIR="$SCRIPT_DIR/results/reproduction"
MODEL="minimax/minimax-m2.7"
MAX_TURNS=50
TIMEOUT=1800
CONCURRENT=4

# ── Parse args ──────────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --trials)      TRIALS="$2"; shift 2 ;;
        --condition)   CONDITION="$2"; shift 2 ;;
        --output-dir)  OUTPUT_DIR="$2"; shift 2 ;;
        --model)       MODEL="$2"; shift 2 ;;
        --concurrent)  CONCURRENT="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [options]"
            echo ""
            echo "Options:"
            echo "  --trials N         Number of trials per task (default: 3, leaderboard: 5)"
            echo "  --condition X      A, B, C, or all (default: all)"
            echo "  --output-dir DIR   Output directory (default: results/reproduction)"
            echo "  --model MODEL      Model name (default: minimax/minimax-m2.7)"
            echo "  --concurrent N     Concurrent tasks (default: 4)"
            echo ""
            echo "Conditions:"
            echo "  A  Bare agent (control): bash + file tools only"
            echo "  B  Stigmergic workgraph: A + wg tools + graph context"
            echo "  C  Enhanced planning: B + skill injection + snapshots"
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# ── Validate ────────────────────────────────────────────────────────────────
if [[ -z "${OPENROUTER_API_KEY:-}" ]]; then
    echo "ERROR: OPENROUTER_API_KEY not set"
    exit 1
fi

if ! command -v harbor &>/dev/null; then
    echo "ERROR: harbor CLI not found. Install: pip install harbor-bench"
    exit 1
fi

if ! docker info &>/dev/null 2>&1; then
    echo "ERROR: Docker daemon not running"
    exit 1
fi

mkdir -p "$OUTPUT_DIR"

# ── Run conditions ──────────────────────────────────────────────────────────

run_condition() {
    local cond="$1"
    local agent_class
    local cond_dir="$OUTPUT_DIR/condition-$cond"

    case "$cond" in
        A) agent_class="wg.adapter:ConditionAAgent" ;;
        B) agent_class="wg.adapter:ConditionBAgent" ;;
        C) agent_class="wg.adapter:ConditionCAgent" ;;
        *) echo "Unknown condition: $cond"; return 1 ;;
    esac

    echo ""
    echo "════════════════════════════════════════════════════════════════"
    echo "  Running Condition $cond: $agent_class"
    echo "  Model: $MODEL | Trials: $TRIALS | Concurrent: $CONCURRENT"
    echo "  Output: $cond_dir"
    echo "════════════════════════════════════════════════════════════════"
    echo ""

    mkdir -p "$cond_dir"

    harbor run \
        --agent-import-path "$agent_class" \
        -m "$MODEL" \
        -d terminal-bench@2.0 \
        -k "$TRIALS" \
        --n-concurrent "$CONCURRENT" \
        --no-delete \
        --max-turns "$MAX_TURNS" \
        --timeout "$TIMEOUT" \
        --trials-dir "$cond_dir" \
        2>&1 | tee "$cond_dir/run.log"

    echo "Condition $cond complete. Results in $cond_dir"
}

if [[ "$CONDITION" == "all" ]]; then
    for c in A B C; do
        run_condition "$c"
    done
else
    run_condition "$CONDITION"
fi

echo ""
echo "════════════════════════════════════════════════════════════════"
echo "  All conditions complete."
echo "  Results in: $OUTPUT_DIR"
echo ""
echo "  To analyze:"
echo "    python3 terminal-bench/results/analyze.py"
echo ""
echo "  To prepare leaderboard submission:"
echo "    bash terminal-bench/prepare-leaderboard.sh"
echo "════════════════════════════════════════════════════════════════"
