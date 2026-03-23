#!/usr/bin/env python3
"""Record hero v3 raw screencast: real service, real TUI, real interactions.

Uses the recording harness (record_harness.py) to capture a tmux session at 65x38.
Follows the scenario from .workgraph/artifacts/screencast-scenario-v3.md.

Phase 1: CLI intro — wg status, wg add, wg viz
Phase 2: TUI — launch, chat with coordinator, navigate, watch progression, inspect
"""

import json
import os
import random
import re
import subprocess
import sys
import time

random.seed(42)

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import importlib
record_harness = importlib.import_module("record-harness")
RecordingHarness = record_harness.RecordingHarness
_verify_cast = record_harness._verify_cast

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
CAST_FILE = os.path.join(SCRIPT_DIR, "recordings", "hero-v3-raw.cast")
DEMO_DIR = f"/tmp/wg-hero-v3-{os.getpid()}"

PROMPT = "Ship the search service"

CLAUDE_MD = """\
# Search Service Demo

When the user asks to "ship the search service", decompose into these tasks:

1. design-schema — Define the search index schema and API
2. build-indexer — Document indexer (after design-schema)
3. build-query-api — Query parsing + ranking (after design-schema)
4. setup-storage — Elasticsearch setup (after design-schema)
5. wire-endpoints — Connect to HTTP routes (after build-indexer, build-query-api, setup-storage)
6. add-auth — Auth middleware (after wire-endpoints)
7. run-load-tests — Load test benchmark (after add-auth)

Use exactly these task IDs. Create all 7 tasks using wg add with --after dependencies as specified.
Tasks 2-4 MUST be parallel (all depend only on design-schema). Keep your response brief.
Do NOT create any other tasks or subtasks.
"""

# Fallback data if real coordinator doesn't respond
TASKS_FALLBACK = [
    ("Design schema", "design-schema", None,
     "Define search index schema and REST API contract"),
    ("Build indexer", "build-indexer", "design-schema",
     "Document indexer with batch and real-time modes"),
    ("Build query API", "build-query-api", "design-schema",
     "Search query parsing, ranking, and pagination"),
    ("Setup storage", "setup-storage", "design-schema",
     "Configure Elasticsearch cluster and mappings"),
    ("Wire endpoints", "wire-endpoints",
     "build-indexer,build-query-api,setup-storage",
     "Connect indexer and query API to HTTP routes"),
    ("Add auth layer", "add-auth", "wire-endpoints",
     "API key auth middleware and rate limiting"),
    ("Run load tests", "run-load-tests", "add-auth",
     "Benchmark: 1k concurrent queries, p99 < 50ms"),
]

CHAT_RESPONSE_FALLBACK = (
    "I'll decompose this into a task graph:\n\n"
    "1. **design-schema** \u2014 API schema & contract\n"
    "2. **build-indexer** \u2192 document indexer\n"
    "3. **build-query-api** \u2192 search & ranking\n"
    "4. **setup-storage** \u2192 Elasticsearch config\n"
    "5. **wire-endpoints** \u2192 HTTP routes (after 2\u20134)\n"
    "6. **add-auth** \u2192 auth middleware\n"
    "7. **run-load-tests** \u2192 benchmarks\n\n"
    "Tasks 2\u20134 run in parallel. Creating now..."
)

# Task progression: (task_ids_to_claim, seconds_before_done)
PROGRESSION = [
    (["design-schema"], 4),
    (["build-indexer", "build-query-api", "setup-storage"], 8),
    (["wire-endpoints"], 4),
    (["add-auth"], 3),
    (["run-load-tests"], 4),
]

EXPECTED_TASK_IDS = {
    "design-schema", "build-indexer", "build-query-api",
    "setup-storage", "wire-endpoints", "add-auth", "run-load-tests",
}


def wg(*args):
    """Run wg command in the demo directory."""
    try:
        return subprocess.run(
            ["wg"] + list(args),
            capture_output=True, text=True,
            cwd=DEMO_DIR, timeout=30,
        )
    except subprocess.TimeoutExpired:
        return None


def setup_demo():
    """Initialize a fresh demo project."""
    if os.path.exists(DEMO_DIR):
        subprocess.run(["rm", "-rf", DEMO_DIR])
    os.makedirs(DEMO_DIR)

    subprocess.run(["git", "init", "-q"], cwd=DEMO_DIR, check=True)
    subprocess.run(
        ["git", "commit", "--allow-empty", "-m", "init", "-q"],
        cwd=DEMO_DIR, check=True,
    )

    wg("init")

    # Write CLAUDE.md for coordinator
    with open(os.path.join(DEMO_DIR, "CLAUDE.md"), "w") as f:
        f.write(CLAUDE_MD)

    print(f"Demo project at {DEMO_DIR}")


def reinit_for_tui(coordinator_enabled=True):
    """Reinitialize .workgraph for a clean TUI demo."""
    wg("service", "stop")
    time.sleep(1)

    wg_dir = os.path.join(DEMO_DIR, ".workgraph")
    if os.path.exists(wg_dir):
        subprocess.run(["rm", "-rf", wg_dir])

    wg("init")
    configure_service(coordinator_enabled=coordinator_enabled)


def configure_service(coordinator_enabled=True, max_agents=0):
    """Set config for the demo service."""
    wg("config", "--max-agents", str(max_agents))

    config_path = os.path.join(DEMO_DIR, ".workgraph", "config.toml")
    with open(config_path) as f:
        config = f.read()

    if not coordinator_enabled:
        config = config.replace(
            "coordinator_agent = true", "coordinator_agent = false"
        )

    # Hide system tasks for cleaner display
    if "show_system_tasks" in config:
        config = config.replace(
            "show_system_tasks = true", "show_system_tasks = false"
        )

    with open(config_path, "w") as f:
        f.write(config)


def start_service():
    """Start wg service in demo project."""
    wg("service", "start", "--force")
    time.sleep(3)

    r = wg("service", "status")
    if r and r.stdout:
        for line in r.stdout.strip().split("\n")[:2]:
            print(f"  {line}")


def check_tasks_created(timeout=120):
    """Wait for all 7 expected tasks to exist in the graph."""
    deadline = time.monotonic() + timeout
    last_report = 0

    while time.monotonic() < deadline:
        r = wg("list")
        if r and r.stdout:
            found = {tid for tid in EXPECTED_TASK_IDS if tid in r.stdout}
            now = time.monotonic()
            if found == EXPECTED_TASK_IDS:
                print(f"  All 7 tasks created!")
                return True
            if now - last_report > 15:
                print(f"  {len(found)}/7 tasks so far ({int(deadline - now)}s remaining)")
                last_report = now
        time.sleep(3)

    print(f"  TIMEOUT: coordinator did not create all tasks in {timeout}s")
    return False


def inject_fallback():
    """Simulated fallback: inject chat history + create tasks manually."""
    print("  FALLBACK: injecting simulated coordinator response")

    # Write chat history
    chat = [
        {
            "role": "user",
            "text": PROMPT,
            "timestamp": "2026-03-23T20:00:01+00:00",
            "edited": False,
        },
        {
            "role": "assistant",
            "text": CHAT_RESPONSE_FALLBACK,
            "timestamp": "2026-03-23T20:00:08+00:00",
            "edited": False,
        },
    ]
    chat_file = os.path.join(DEMO_DIR, ".workgraph", "chat-history.json")
    with open(chat_file, "w") as f:
        json.dump(chat, f)

    # Create tasks with small delays for incremental TUI refresh
    for title, tid, after, desc in TASKS_FALLBACK:
        cmd = ["add", title, "--id", tid, "-d", desc]
        if after:
            cmd.extend(["--after", after])
        wg(*cmd)
        time.sleep(0.3)

    print(f"  Injected chat + {len(TASKS_FALLBACK)} tasks")


def progress_tasks(h):
    """Drive task state transitions via wg claim/done."""
    for batch, hold in PROGRESSION:
        # Claim all tasks in batch (they go in-progress)
        for tid in batch:
            wg("claim", tid)
            print(f"    {tid}: in-progress")

        # Hold while capturing frames (viewer sees active tasks)
        h.sleep(hold)

        # Complete with staggered timing for visual effect
        for i, tid in enumerate(batch):
            if i > 0:
                h.sleep(1.5)
            wg("done", tid)
            print(f"    {tid}: done")
            h.sleep(0.5)

        # Brief pause between batches
        h.sleep(1.5)


# ── Recording phases ─────────────────────────────────────────

def phase_cli(h):
    """Phase 1: CLI intro — wg status, wg add, wg viz."""
    print("\n=== Phase 1: CLI Intro ===")

    h.wait_for("$", timeout=5)
    h.send_keys("C-l")
    h.sleep(1)

    # wg status
    h.type_naturally("wg status", wpm=55)
    h.send_keys("Enter")
    h.sleep(2.5)

    # wg add "Parse input"
    h.type_naturally('wg add "Parse input"', wpm=55)
    h.send_keys("Enter")
    h.sleep(1.5)

    # wg add "Validate" --after parse-input
    h.type_naturally('wg add "Validate" --after parse-input', wpm=55)
    h.send_keys("Enter")
    h.sleep(1.5)

    # wg viz
    h.type_naturally("wg viz", wpm=55)
    h.send_keys("Enter")
    h.sleep(3)

    snap = h.snapshot()
    has_viz = "parse-input" in snap.lower() or "validate" in snap.lower()
    print(f"  Graph visible: {has_viz}")


def phase_tui_launch(h):
    """Phase 2: Launch TUI."""
    print("\n=== Phase 2: Launch TUI ===")

    h.send_keys("C-l")
    h.sleep(0.5)

    h.type_naturally("wg tui --recording", wpm=55)
    h.send_keys("Enter")

    # Wait for TUI to render
    found = h.wait_for("Chat", timeout=15)
    h.sleep(3)

    snap = h.snapshot()
    tui_loaded = "Chat" in snap or "Graph" in snap or "LIVE" in snap
    print(f"  TUI loaded: {tui_loaded}")
    if not tui_loaded:
        print(f"  Snapshot preview:\n{snap[:200]}")
    return tui_loaded


def phase_chat(h, use_real_coordinator=True):
    """Phase 3: Type chat prompt, wait for coordinator response."""
    print("\n=== Phase 3: Chat ===")

    # Enter chat input mode
    h.send_keys("c")
    h.sleep(1.5)

    # Type the prompt naturally
    h.type_naturally(PROMPT, wpm=50)
    h.sleep(0.5)

    # Submit
    h.send_keys("Enter")
    print(f"  Submitted: '{PROMPT}'")

    if use_real_coordinator:
        # Wait for coordinator to create tasks
        print("  Waiting for coordinator response (up to 120s)...")
        coordinator_ok = check_tasks_created(timeout=120)

        if not coordinator_ok:
            print("  Coordinator failed — switching to fallback")
            inject_fallback()
            return False
        return True
    else:
        # Immediate fallback
        time.sleep(2)
        inject_fallback()
        return False


def phase_navigate(h):
    """Phase 4: Navigate through the task graph."""
    print("\n=== Phase 4: Navigate ===")

    # Exit chat mode to graph
    h.send_keys("Escape")
    h.sleep(1)

    # Navigate down through tasks
    for i in range(3):
        h.send_keys("Down")
        h.sleep(0.8)
    h.flush_frame()

    # Show Detail tab (1)
    h.send_keys("1")
    h.sleep(3)

    snap = h.snapshot()
    print(f"  Detail tab visible: {'Detail' in snap or 'Status' in snap or 'Depends' in snap}")

    # Move down to wire-endpoints area
    h.send_keys("Down")
    h.sleep(0.8)
    h.send_keys("Down")
    h.sleep(1.5)

    # Back to graph
    h.send_keys("Escape")
    h.sleep(0.5)


def phase_progression(h):
    """Phase 5: Watch tasks progress through states."""
    print("\n=== Phase 5: Task Progression ===")
    progress_tasks(h)

    snap = h.snapshot()
    has_done = "done" in snap.lower()
    print(f"  Tasks show done state: {has_done}")


def phase_inspect(h):
    """Phase 6: Inspect completed task results."""
    print("\n=== Phase 6: Inspect Results ===")

    # Navigate to last task (run-load-tests)
    h.send_keys("End")
    h.sleep(1)

    # Show Log tab (2)
    h.send_keys("2")
    h.sleep(3)

    # Navigate up to build-indexer
    for i in range(4):
        h.send_keys("Up")
        h.sleep(0.5)
    h.sleep(2)

    # Show Chat tab (0) — see the full conversation
    h.send_keys("0")
    h.sleep(3)

    snap = h.snapshot()
    has_chat = PROMPT.lower() in snap.lower() or "search" in snap.lower()
    print(f"  Chat conversation visible: {has_chat}")


def phase_exit(h):
    """Phase 7: Exit TUI."""
    print("\n=== Phase 7: Exit ===")

    h.send_keys("q")
    h.sleep(2)


# ── Main ─────────────────────────────────────────────────────

def record():
    """Main recording orchestrator."""

    # Phase 0: Setup
    print("\n=== Phase 0: Setup ===")
    setup_demo()

    # Determine if we can try real coordinator
    # The claude CLI uses OAuth credentials from ~/.claude/.credentials.json
    # It doesn't need ANTHROPIC_API_KEY
    creds_exist = os.path.exists(os.path.expanduser("~/.claude/.credentials.json"))
    use_real = creds_exist
    print(f"  Claude credentials: {'found' if creds_exist else 'not found'}")
    print(f"  Coordinator mode: {'real' if use_real else 'simulated fallback'}")

    try:
        shell_cmd = (
            f"cd {DEMO_DIR} && "
            f"export PS1='\\[\\033[1;32m\\]$ \\[\\033[0m\\]' && "
            f"exec bash --norc --noprofile"
        )

        with RecordingHarness(
            cast_file=CAST_FILE,
            cwd=DEMO_DIR,
            shell_command=shell_cmd,
            idle_time_limit=5.0,  # Higher for raw recording
        ) as h:

            # Phase 1: CLI intro
            phase_cli(h)

            # Reinitialize for clean TUI demo
            print("\n=== Reinitialize for TUI ===")
            if use_real:
                reinit_for_tui(coordinator_enabled=True)
            else:
                reinit_for_tui(coordinator_enabled=False)
            start_service()

            # Phase 2: Launch TUI
            tui_ok = phase_tui_launch(h)
            if not tui_ok:
                print("ERROR: TUI did not load. Aborting.")
                return False

            # Phase 3: Chat with coordinator
            coordinator_ok = phase_chat(h, use_real_coordinator=use_real)

            # Let TUI refresh and show tasks
            h.sleep(5)

            snap = h.snapshot()
            has_tasks = any(
                tid in snap for tid in ["design-schema", "build-indexer", "wire-endpoints"]
            )
            print(f"  Tasks visible in TUI: {has_tasks}")

            # Phase 4: Navigate
            phase_navigate(h)

            # Phase 5: Task progression
            phase_progression(h)

            # Phase 6: Inspect
            phase_inspect(h)

            # Phase 7: Exit
            phase_exit(h)

            duration = h.duration
            frames = h.frame_count

        # Verify the cast file
        print(f"\n=== Recording Summary ===")
        print(f"  File: {CAST_FILE}")
        print(f"  Duration: {duration:.1f}s")
        print(f"  Frames: {frames}")
        print(f"  Coordinator: {'real' if (use_real and coordinator_ok) else 'simulated'}")

        print(f"\n=== Verifying cast file ===")
        ok = _verify_cast(CAST_FILE)
        return ok

    finally:
        # Cleanup
        wg("service", "stop")
        print(f"\nDemo dir: {DEMO_DIR}")


if __name__ == "__main__":
    success = record()
    sys.exit(0 if success else 1)
