#!/usr/bin/env bash
# prepare-leaderboard.sh — Organize trial data into Terminal-Bench leaderboard submission format
#
# This script copies result.json and config.json files from our experiment runs
# into the HuggingFace leaderboard submission directory structure.
#
# Usage:
#   bash terminal-bench/prepare-leaderboard.sh [--dry-run]
#
# Prerequisites:
#   - Experiment data in terminal-bench/results/rerun-condition-{a,b}/ and full-condition-c/
#   - metadata.yaml files already in leaderboard-submission/
#
# NOTE: The TB leaderboard requires minimum 5 trials per task. Our current data
# has 3 trials. Run 2 more trials per condition before submitting.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RESULTS_DIR="$SCRIPT_DIR/results"
SUBMISSION_DIR="$SCRIPT_DIR/leaderboard-submission"

DRY_RUN=false
[[ "${1:-}" == "--dry-run" ]] && DRY_RUN=true

# ── Condition A ─────────────────────────────────────────────────────────────
COND_A_DIR="$SUBMISSION_DIR/workgraph-condition-a__minimax-m2.7"
COND_A_SOURCES=(
    "$RESULTS_DIR/rerun-condition-a/rerun-condition-a"
    "$RESULTS_DIR/rerun-condition-a/rerun-condition-a-completion"
)

# ── Condition B ─────────────────────────────────────────────────────────────
COND_B_DIR="$SUBMISSION_DIR/workgraph-condition-b__minimax-m2.7"
COND_B_SOURCES=(
    "$RESULTS_DIR/rerun-condition-b/rerun-condition-b"
    "$RESULTS_DIR/rerun-condition-b/rerun-condition-b-cont1"
    "$RESULTS_DIR/rerun-condition-b/rerun-condition-b-cont2"
)

# ── Condition C ─────────────────────────────────────────────────────────────
COND_C_DIR="$SUBMISSION_DIR/workgraph-condition-c__minimax-m2.7"
COND_C_SOURCES=(
    "$RESULTS_DIR/full-condition-c/full-condition-c"
    "$RESULTS_DIR/full-condition-c/full-condition-c-retry1"
    "$RESULTS_DIR/full-condition-c/full-condition-c-retry2"
)

copy_trials() {
    local dest_dir="$1"
    shift
    local sources=("$@")
    local count=0

    for src_dir in "${sources[@]}"; do
        if [[ ! -d "$src_dir" ]]; then
            echo "  SKIP (not found): $src_dir"
            continue
        fi

        # Each job directory gets its own subfolder
        local job_name
        job_name="$(basename "$src_dir")"
        local job_dest="$dest_dir/$job_name"

        # Copy the job-level result.json and config.json if they exist
        if $DRY_RUN; then
            echo "  DRY RUN: mkdir -p $job_dest"
        else
            mkdir -p "$job_dest"
        fi

        for f in result.json config.json; do
            if [[ -f "$src_dir/$f" ]]; then
                if $DRY_RUN; then
                    echo "  DRY RUN: cp $src_dir/$f -> $job_dest/$f"
                else
                    cp "$src_dir/$f" "$job_dest/$f"
                fi
            fi
        done

        # Copy each trial directory (task__hash/)
        for trial_dir in "$src_dir"/*/; do
            [[ -d "$trial_dir" ]] || continue
            local trial_name
            trial_name="$(basename "$trial_dir")"

            # Only copy if it has a result.json (skip non-trial dirs)
            if [[ -f "$trial_dir/result.json" ]]; then
                local trial_dest="$job_dest/$trial_name"
                if $DRY_RUN; then
                    echo "  DRY RUN: mkdir -p $trial_dest"
                    echo "  DRY RUN: cp $trial_dir/result.json -> $trial_dest/"
                else
                    mkdir -p "$trial_dest"
                    cp "$trial_dir/result.json" "$trial_dest/"
                fi

                # Copy config.json from trial if present
                if [[ -f "$trial_dir/config.json" ]]; then
                    if $DRY_RUN; then
                        echo "  DRY RUN: cp $trial_dir/config.json -> $trial_dest/"
                    else
                        cp "$trial_dir/config.json" "$trial_dest/"
                    fi
                fi

                count=$((count + 1))
            fi
        done
    done

    echo "  Copied $count trial results"
}

echo "=== Preparing Condition A ==="
copy_trials "$COND_A_DIR" "${COND_A_SOURCES[@]}"

echo ""
echo "=== Preparing Condition B ==="
copy_trials "$COND_B_DIR" "${COND_B_SOURCES[@]}"

echo ""
echo "=== Preparing Condition C ==="
copy_trials "$COND_C_DIR" "${COND_C_SOURCES[@]}"

echo ""
echo "=== Summary ==="
for cond_dir in "$SUBMISSION_DIR"/workgraph-condition-*/; do
    local_count=$(find "$cond_dir" -name "result.json" -not -path "*/metadata.yaml" | wc -l)
    echo "  $(basename "$cond_dir"): $local_count result.json files"
done

echo ""
echo "NOTE: Terminal-Bench leaderboard requires minimum 5 trials per task."
echo "Current data has 3 trials per task. Run 2 additional trials before submitting."
echo ""
echo "To submit, fork https://huggingface.co/datasets/harborframework/terminal-bench-2-leaderboard"
echo "and copy each condition directory into submissions/terminal-bench/2.0/"
