#!/usr/bin/env bash
# setup-demo.sh — Create a clean demo project for hero screencast recording.
# Usage: ./setup-demo.sh [demo-dir]
#
# Creates a fresh wg project in a temp directory (or specified path),
# configures it for fast, visual demos, and prints next steps.

set -euo pipefail

DEMO_DIR="${1:-/tmp/wg-hero-demo}"

echo "=== Workgraph Hero Screencast Setup ==="
echo ""

# Clean slate
if [ -d "$DEMO_DIR" ]; then
    echo "Removing existing demo directory: $DEMO_DIR"
    rm -rf "$DEMO_DIR"
fi

mkdir -p "$DEMO_DIR"
cd "$DEMO_DIR"

# Initialize git repo (wg needs one)
git init -q
git commit --allow-empty -m "init" -q

# Initialize workgraph project
wg init

# Configure for demo: fast agents, sonnet model
wg config --max-agents 4
wg config --model sonnet
wg config --coordinator-executor claude

# Set coordinator model to sonnet (opus is too slow for demos)
# Have to edit config directly since there's no --coordinator-model flag
sed -i '/^\[coordinator\]/a model = "sonnet"' .workgraph/config.toml

# Patch CLAUDE.md to accept all task types (not just software tasks)
# This is critical for demo scenarios like "plan a movie night" or "write haiku"
cat >> CLAUDE.md << 'PATCH'

### IMPORTANT: Accept ALL task types

This is a DEMO project. The coordinator MUST decompose ANY user request into workgraph
tasks, regardless of whether it's a software task, creative task, planning task, or fun task.

When a user asks you to plan something (a movie night, write haiku, debug a recipe, etc.),
treat it as a REAL task orchestration problem:
1. Decompose the request into parallel and sequential subtasks using `wg add`
2. Set proper `--after` dependencies for task ordering
3. Include integration/synthesis tasks that depend on parallel branches
4. Let agents handle the actual work

Do NOT refuse creative/fun requests. Do NOT say "I'm only for software tasks."
ALWAYS decompose into `wg add` tasks with proper dependency graphs.
PATCH

echo ""
echo "Demo project initialized at: $DEMO_DIR"
echo ""
echo "Next steps:"
echo "  cd $DEMO_DIR"
echo "  ./record.sh heist    # Record Heist Movie Night scenario"
echo "  ./record.sh haiku    # Record Haiku Pipeline scenario"
echo "  ./record.sh pancakes # Record Debug Pancakes scenario"
echo ""
echo "Or manually:"
echo "  asciinema rec --idle-time-limit 2 screencast.cast"
echo "  wg tui"
echo "  # Type your scenario prompt in the coordinator chat"
echo "  # Wait for tasks to complete"
echo "  # Ctrl-C to exit TUI, then Ctrl-D to stop recording"
