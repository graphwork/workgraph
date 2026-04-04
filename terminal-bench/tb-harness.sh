#!/usr/bin/env bash
# Terminal Bench Harness — drives native executor for Condition A, B, and C
#
# Usage:
#   ./tb-harness.sh --condition A --model "minimax/minimax-m2.7" --task "Create hello.txt with 'hello world', verify it contains the text"
#   ./tb-harness.sh --condition B --model "minimax/minimax-m2.7" --task "Create hello.txt with 'hello world', verify it contains the text"
#   ./tb-harness.sh --condition A --model "minimax/minimax-m2.7" --task-file tasks/task-42.txt
#   ./tb-harness.sh --condition A --task "..." --results-dir ./results/run-001
#
# Environment:
#   OPENROUTER_API_KEY — required for OpenRouter models
#   WG_BINARY — path to wg binary (default: searches PATH then target/release/wg)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Defaults ──────────────────────────────────────────────────────────────────
CONDITION="A"
MODEL="minimax/minimax-m2.7"
TASK_TEXT=""
TASK_FILE=""
TASK_ID=""
MAX_TURNS=50
TIMEOUT=1800
RESULTS_DIR=""
QUIET=false

# ── Parse args ────────────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --condition)  CONDITION="$2"; shift 2 ;;
        --model)      MODEL="$2"; shift 2 ;;
        --task)       TASK_TEXT="$2"; shift 2 ;;
        --task-file)  TASK_FILE="$2"; shift 2 ;;
        --task-id)    TASK_ID="$2"; shift 2 ;;
        --max-turns)  MAX_TURNS="$2"; shift 2 ;;
        --timeout)    TIMEOUT="$2"; shift 2 ;;
        --results-dir) RESULTS_DIR="$2"; shift 2 ;;
        --quiet)      QUIET=true; shift ;;
        -h|--help)
            echo "Usage: $0 --condition A|B|C --model MODEL --task INSTRUCTION [options]"
            echo ""
            echo "Options:"
            echo "  --condition A|B|C    A=bare, B=agent+wg, C=agent+wg+skill injection (default: A)"
            echo "  --model MODEL        OpenRouter model name (default: minimax/minimax-m2.7)"
            echo "  --task TEXT           Task instruction (inline)"
            echo "  --task-file FILE     Task instruction from file"
            echo "  --task-id ID         Task identifier (default: auto-generated)"
            echo "  --max-turns N        Max agent turns (default: 100)"
            echo "  --timeout SECS       Timeout in seconds (default: 1800)"
            echo "  --results-dir DIR    Where to store results (default: auto)"
            echo "  --quiet              Suppress progress output"
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# ── Validate inputs ──────────────────────────────────────────────────────────
if [[ -z "$TASK_TEXT" && -z "$TASK_FILE" ]]; then
    echo "ERROR: Must provide --task or --task-file"
    exit 1
fi

if [[ -n "$TASK_FILE" ]]; then
    TASK_TEXT="$(cat "$TASK_FILE")"
fi

if [[ -z "${OPENROUTER_API_KEY:-}" ]]; then
    echo "ERROR: OPENROUTER_API_KEY not set"
    exit 1
fi

# ── Find wg binary ───────────────────────────────────────────────────────────
WG_BIN="${WG_BINARY:-}"
if [[ -z "$WG_BIN" ]]; then
    if command -v wg &>/dev/null; then
        WG_BIN="$(command -v wg)"
    elif [[ -f "$REPO_ROOT/target/release/wg" ]]; then
        WG_BIN="$REPO_ROOT/target/release/wg"
    elif [[ -f "$REPO_ROOT/target/debug/wg" ]]; then
        WG_BIN="$REPO_ROOT/target/debug/wg"
    else
        echo "ERROR: wg binary not found. Run 'cargo build --release' first."
        exit 1
    fi
fi
[[ "$QUIET" == false ]] && echo "Using wg binary: $WG_BIN"

# ── Generate task ID and results dir ─────────────────────────────────────────
if [[ -z "$TASK_ID" ]]; then
    TASK_ID="tb-$(echo "$CONDITION" | tr '[:upper:]' '[:lower:]')-$(date +%s)-$$"
fi

if [[ -z "$RESULTS_DIR" ]]; then
    RESULTS_DIR="$SCRIPT_DIR/results/$(date +%Y%m%d-%H%M%S)-${CONDITION,,}-${TASK_ID}"
fi
mkdir -p "$RESULTS_DIR"

# ── Set up temp workgraph directory ──────────────────────────────────────────
WORK_DIR="$(mktemp -d /tmp/tb-harness-XXXXXX)"
WG_DIR="$WORK_DIR/.workgraph"
FAKE_HOME="$WORK_DIR/home"
mkdir -p "$FAKE_HOME"

# Initialize workgraph
HOME="$FAKE_HOME" "$WG_BIN" --dir "$WG_DIR" init 2>/dev/null

# Set up OpenRouter endpoint
HOME="$FAKE_HOME" "$WG_BIN" --dir "$WG_DIR" endpoint add openrouter \
    --provider openrouter \
    --url "https://openrouter.ai/api/v1" \
    --key-env OPENROUTER_API_KEY 2>/dev/null
HOME="$FAKE_HOME" "$WG_BIN" --dir "$WG_DIR" endpoint set-default openrouter 2>/dev/null

[[ "$QUIET" == false ]] && echo "Work directory: $WORK_DIR"
[[ "$QUIET" == false ]] && echo "Results directory: $RESULTS_DIR"

# ── Condition-specific setup ─────────────────────────────────────────────────
if [[ "$CONDITION" == "A" ]]; then
    # Condition A: bare agent, no wg tools
    # Create a custom bundle that has bash + file tools but NO wg tools
    BUNDLES_DIR="$WG_DIR/bundles"
    mkdir -p "$BUNDLES_DIR"
    cat > "$BUNDLES_DIR/condition-a.toml" <<'BUNDLE'
name = "condition-a"
description = "Terminal Bench Condition A: bare agent with bash + file tools, no wg tools"
tools = ["bash", "read_file", "write_file", "edit_file", "glob", "grep", "list_files"]
context_scope = "clean"
system_prompt_suffix = ""
BUNDLE
    EXEC_MODE="condition-a"
    RESUME_FLAG="--no-resume"

    # Build the Condition A system prompt
    cat > "$WORK_DIR/prompt.txt" <<PROMPT
You are a coding agent completing a task. You have access to bash and file tools.
Focus on completing the task efficiently and correctly.
Do not ask for clarification — proceed with your best judgment.
When you believe you have completed the task, provide a final summary of what you did.

## Task

${TASK_TEXT}
PROMPT

elif [[ "$CONDITION" == "B" ]]; then
    # Condition B: agent + workgraph, all tools
    EXEC_MODE="full"
    RESUME_FLAG=""

    # Create the root task in workgraph
    HOME="$FAKE_HOME" "$WG_BIN" --dir "$WG_DIR" add "$TASK_TEXT" --id "$TASK_ID" --context-scope task 2>/dev/null

    # Build the Condition B system prompt (trimmed for token efficiency)
    cat > "$WORK_DIR/prompt.txt" <<PROMPT
# Task Assignment

You are an AI agent working on a task. You have bash, file tools, and workgraph tools.

## Your Task ID: ${TASK_ID}

## Workgraph Tools
- wg_log("${TASK_ID}", "msg") — log progress (persists across context limits)
- wg_add("title") — create subtask for complex work
- wg_artifact("${TASK_ID}", "path") — record output files
- wg_done("${TASK_ID}") — mark task complete
- wg_fail("${TASK_ID}", "reason") — mark task failed

Use wg_log to checkpoint progress. Use wg_add to decompose complex tasks.
When finished, call wg_done.

## Task

${TASK_TEXT}
PROMPT

elif [[ "$CONDITION" == "C" ]]; then
    # Condition C: agent + workgraph + skill injection + planning phase
    # Same wg tools as B, but with a skill prompt that teaches decomposition heuristics
    EXEC_MODE="full"
    RESUME_FLAG=""

    # Create the root task in workgraph
    HOME="$FAKE_HOME" "$WG_BIN" --dir "$WG_DIR" add "$TASK_TEXT" --id "$TASK_ID" --context-scope task 2>/dev/null

    # Build the Condition C system prompt (skill injection + planning phase)
    cat > "$WORK_DIR/prompt.txt" <<PROMPT
# Task Assignment

You are an AI agent completing a Terminal Bench task.
Your root task ID is: **${TASK_ID}**

## Workgraph: Your External Memory

You have a workgraph — a persistent task graph that acts as external memory.
It survives even if your context fills up. Use it.

### Always do this
- \`wg_log("${TASK_ID}", "Starting: <plan>")\` before your first action
- \`wg_log("${TASK_ID}", "Done: <result>")\` after completing a step
- \`wg_done("${TASK_ID}")\` when the task is complete
- \`wg_fail("${TASK_ID}", "reason")\` if you cannot complete the task

### Decompose when needed
If the task has 3+ distinct phases or might exhaust your context:
- \`wg_add("Step 1: <title>")\` to create subtasks
- Solve each subtask, then \`wg_done\` each one
- Finally \`wg_done("${TASK_ID}")\`

If the task is simple (< 10 steps), skip decomposition and solve directly.

### Record outputs
- \`wg_artifact("${TASK_ID}", "/path/to/file")\` for files you create

## Planning Phase

Before writing code or running commands, analyze the task in ONE response:
1. What does the task require?
2. How many steps? Simple (< 10) or complex (10+)?
3. Plan: decompose or solve directly?
4. First action?

Then execute your plan.

## Tools
- bash, read_file, write_file, edit_file, glob, grep — for working in the environment
- wg_log, wg_add, wg_done, wg_fail, wg_show, wg_list, wg_artifact, wg_msg_send, wg_msg_read — for task coordination

Begin by analyzing the task below, then execute.

## Task

${TASK_TEXT}
PROMPT

else
    echo "ERROR: Condition must be A, B, or C"
    exit 1
fi

# ── Record metadata ──────────────────────────────────────────────────────────
cat > "$RESULTS_DIR/metadata.json" <<META
{
    "condition": "${CONDITION}",
    "model": "${MODEL}",
    "task_id": "${TASK_ID}",
    "task_instruction": $(echo "$TASK_TEXT" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'),
    "max_turns": ${MAX_TURNS},
    "timeout_seconds": ${TIMEOUT},
    "started_at": "$(date -Iseconds)",
    "wg_binary": "${WG_BIN}",
    "exec_mode": "${EXEC_MODE}"
}
META

[[ "$QUIET" == false ]] && echo ""
[[ "$QUIET" == false ]] && echo "═══════════════════════════════════════════════════════════════"
[[ "$QUIET" == false ]] && echo "  Terminal Bench Harness — Condition ${CONDITION}"
[[ "$QUIET" == false ]] && echo "  Model: ${MODEL}"
[[ "$QUIET" == false ]] && echo "  Task: ${TASK_TEXT:0:80}..."
[[ "$QUIET" == false ]] && echo "  Exec mode: ${EXEC_MODE}"
[[ "$QUIET" == false ]] && echo "═══════════════════════════════════════════════════════════════"
[[ "$QUIET" == false ]] && echo ""

# ── Run native executor ──────────────────────────────────────────────────────
START_TIME=$(date +%s)

set +e
HOME="$FAKE_HOME" \
OPENROUTER_API_KEY="$OPENROUTER_API_KEY" \
WG_LLM_PROVIDER="openai" \
"$WG_BIN" --dir "$WG_DIR" \
    native-exec \
    --prompt-file "$WORK_DIR/prompt.txt" \
    --exec-mode "$EXEC_MODE" \
    --task-id "$TASK_ID" \
    --model "$MODEL" \
    --provider openai \
    --endpoint-url "https://openrouter.ai/api/v1" \
    --api-key "$OPENROUTER_API_KEY" \
    --max-turns "$MAX_TURNS" \
    $RESUME_FLAG \
    > "$RESULTS_DIR/stdout.ndjson" \
    2> "$RESULTS_DIR/stderr.log"
EXIT_CODE=$?
set -e

END_TIME=$(date +%s)
DURATION=$((END_TIME - START_TIME))

# ── Collect results ──────────────────────────────────────────────────────────
# Count turns and tool calls from NDJSON output
TURNS=$(grep -c '"turn"' "$RESULTS_DIR/stdout.ndjson" 2>/dev/null || true)
TURNS="${TURNS:-0}"
TOOL_CALLS=$(grep -c '"tool_call"' "$RESULTS_DIR/stdout.ndjson" 2>/dev/null || true)
TOOL_CALLS="${TOOL_CALLS:-0}"
TOOL_RESULTS=$(grep -c '"tool_result"' "$RESULTS_DIR/stdout.ndjson" 2>/dev/null || true)
TOOL_RESULTS="${TOOL_RESULTS:-0}"

# Extract token usage from stderr (native-exec prints summary there)
INPUT_TOKENS=$(grep -oP '\d+(?=\+\d+ tokens)' "$RESULTS_DIR/stderr.log" 2>/dev/null | head -1 || true)
INPUT_TOKENS="${INPUT_TOKENS:-0}"
OUTPUT_TOKENS=$(grep -oP '(?<=\+)\d+(?= tokens)' "$RESULTS_DIR/stderr.log" 2>/dev/null | head -1 || true)
OUTPUT_TOKENS="${OUTPUT_TOKENS:-0}"

# Copy workgraph state if Condition B or C
if [[ "$CONDITION" == "B" || "$CONDITION" == "C" ]]; then
    cp -r "$WG_DIR" "$RESULTS_DIR/workgraph_state" 2>/dev/null || true
fi

# Copy the prompt for reference
cp "$WORK_DIR/prompt.txt" "$RESULTS_DIR/prompt.txt"

# Write results summary
cat > "$RESULTS_DIR/result.json" <<RESULT
{
    "condition": "${CONDITION}",
    "model": "${MODEL}",
    "task_id": "${TASK_ID}",
    "exit_code": ${EXIT_CODE},
    "duration_seconds": ${DURATION},
    "turns": ${TURNS},
    "tool_calls": ${TOOL_CALLS},
    "tool_results": ${TOOL_RESULTS},
    "tokens": {
        "input": ${INPUT_TOKENS:-0},
        "output": ${OUTPUT_TOKENS:-0}
    },
    "completed_at": "$(date -Iseconds)"
}
RESULT

# ── Report ───────────────────────────────────────────────────────────────────
echo ""
echo "═══════════════════════════════════════════════════════════════"
echo "  Results Summary"
echo "═══════════════════════════════════════════════════════════════"
echo "  Condition:    ${CONDITION}"
echo "  Model:        ${MODEL}"
echo "  Exit code:    ${EXIT_CODE}"
echo "  Duration:     ${DURATION}s"
echo "  Turns:        ${TURNS}"
echo "  Tool calls:   ${TOOL_CALLS}"
echo "  Tool results: ${TOOL_RESULTS}"
echo "  Tokens:       ${INPUT_TOKENS:-?}+${OUTPUT_TOKENS:-?}"
echo "  Results at:   ${RESULTS_DIR}"
echo "═══════════════════════════════════════════════════════════════"

# ── Cleanup ──────────────────────────────────────────────────────────────────
rm -rf "$WORK_DIR"

exit $EXIT_CODE
