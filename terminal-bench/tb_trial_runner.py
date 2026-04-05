#!/usr/bin/env python3
"""
TB Trial Runner — creates workgraph tasks from Terminal Bench task definitions.

Converts TB calibration tasks into wg tasks that run through the full agency
pipeline: .assign → execute → .flip → .evaluate → .verify

Usage:
    # Create trial tasks (fanout)
    python tb_trial_runner.py create --config trial-config.json

    # Create with custom wg directory
    python tb_trial_runner.py create --config trial-config.json --wg-dir /path/to/.workgraph

    # List created trial tasks
    python tb_trial_runner.py status --config trial-config.json

    # Collect results (fan-in)
    python tb_trial_runner.py collect --config trial-config.json --output results.json
"""

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path


# ---------------------------------------------------------------------------
# TB Task definitions (host-side execution compatible)
# ---------------------------------------------------------------------------

# Each task has: id, title, instruction_file, verify_cmd, difficulty
# verify_cmd runs on the host to check task completion

TB_TASKS = {
    "file-ops": {
        "id": "file-ops",
        "title": "File Operations: create project structure",
        "instruction_file": "tasks/condition-a-calibration/01-file-ops-easy.txt",
        "verify_cmd": (
            "test -f /tmp/project/src/main.py && "
            "test -f /tmp/project/src/utils.py && "
            "test -f /tmp/project/src/tests/test_utils.py && "
            "test -f /tmp/project/data/config.json && "
            "test -f /tmp/project/README.md && "
            "test -f /tmp/project/.gitignore && "
            "python3 -c \"import json; json.load(open('/tmp/project/data/config.json'))\" && "
            "python3 -m pytest /tmp/project/src/tests/test_utils.py -v"
        ),
        "difficulty": "easy",
    },
    "text-processing": {
        "id": "text-processing",
        "title": "Text Processing: word frequency counter",
        "instruction_file": "tasks/condition-a-calibration/02-text-processing-easy.txt",
        "verify_cmd": (
            "test -f /tmp/wordfreq.py && "
            "echo 'the the the dog dog cat' | python3 /tmp/wordfreq.py | head -1 | grep -q 'the'"
        ),
        "difficulty": "easy",
    },
    "debugging": {
        "id": "debugging",
        "title": "Debugging: fix merge sort bugs",
        "instruction_file": "tasks/condition-a-calibration/03-debugging-medium.txt",
        "verify_cmd": (
            "test -f /tmp/buggy_sort.py && "
            "python3 /tmp/buggy_sort.py 2>&1 | grep -v FAIL | grep -c PASS | "
            "python3 -c \"import sys; n=int(sys.stdin.read().strip()); sys.exit(0 if n>=6 else 1)\""
        ),
        "difficulty": "medium",
    },
    "algorithm": {
        "id": "algorithm",
        "title": "Algorithm: key-value store with transactions",
        "instruction_file": "tasks/condition-a-calibration/06-algorithm-hard.txt",
        "verify_cmd": (
            "test -f /tmp/kvstore.py && test -f /tmp/kv_test.txt && "
            "python3 /tmp/kvstore.py < /tmp/kv_test.txt | head -1 | grep -q '10'"
        ),
        "difficulty": "hard",
    },
}


# ---------------------------------------------------------------------------
# Condition configurations
# ---------------------------------------------------------------------------

CONDITION_CONFIGS = {
    "A": {
        "description": "Minimal context, no decomposition guidance",
        "context_scope": "clean",
        "tags": ["tb-trial", "condition-A"],
    },
    "C": {
        "description": "Skill injection, planning phase",
        "context_scope": "task",
        "tags": ["tb-trial", "condition-C"],
    },
    "D": {
        "description": "Self-verify loops, agency identity",
        "context_scope": "task",
        "tags": ["tb-trial", "condition-D"],
    },
    "E": {
        "description": "Organization decomposition",
        "context_scope": "graph",
        "tags": ["tb-trial", "condition-E"],
    },
}


def load_config(config_path: str) -> dict:
    """Load trial configuration from JSON file."""
    with open(config_path) as f:
        return json.load(f)


def load_instruction(task_def: dict, tb_root: str) -> str:
    """Load task instruction text from file."""
    instruction_path = os.path.join(tb_root, task_def["instruction_file"])
    with open(instruction_path) as f:
        return f.read().strip()


def run_wg(args: list[str], wg_dir: str | None = None) -> str:
    """Run a wg CLI command and return stdout."""
    cmd = ["wg"]
    if wg_dir:
        cmd.extend(["--dir", wg_dir])
    cmd.extend(args)
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    if result.returncode != 0:
        print(f"  wg command failed: {' '.join(cmd)}", file=sys.stderr)
        print(f"  stderr: {result.stderr.strip()}", file=sys.stderr)
    return result.stdout.strip()


def create_trial_task(
    task_def: dict,
    condition: str,
    replica: int,
    instruction: str,
    model: str | None,
    wg_dir: str | None,
) -> str:
    """Create a single trial task in the workgraph. Returns the task ID."""
    cond_cfg = CONDITION_CONFIGS[condition]
    task_id = f"tb-{condition.lower()}-{task_def['id']}-r{replica}"
    title = f"TB-{condition}: {task_def['title']} (rep {replica})"

    # Build task description
    description = (
        f"## Terminal Bench Trial\n\n"
        f"**Condition:** {condition} — {cond_cfg['description']}\n"
        f"**Task:** {task_def['id']} ({task_def['difficulty']})\n"
        f"**Replica:** {replica}\n\n"
        f"## Instructions\n\n"
        f"{instruction}\n\n"
        f"## Validation\n\n"
        f"- [ ] Task instructions followed completely\n"
        f"- [ ] All verification steps in the instructions pass\n"
        f"- [ ] Output matches expected results\n"
    )

    # Build wg add command
    cmd = [
        "add", title,
        "--id", task_id,
        "-d", description,
        "--verify", task_def["verify_cmd"],
        "--no-place",  # skip placement — we want raw execution
    ]

    # Add context scope
    if cond_cfg.get("context_scope"):
        cmd.extend(["--context-scope", cond_cfg["context_scope"]])

    # Add model override if specified
    if model:
        cmd.extend(["--model", model])

    # Add tags
    for tag in cond_cfg["tags"]:
        cmd.extend(["-t", tag])
    cmd.extend(["-t", f"task-{task_def['id']}"])
    cmd.extend(["-t", f"rep-{replica}"])
    cmd.extend(["-t", f"difficulty-{task_def['difficulty']}"])

    output = run_wg(cmd, wg_dir)
    print(f"  Created: {task_id}")
    return task_id


def create_fanin_task(
    trial_task_ids: list[str],
    config: dict,
    wg_dir: str | None,
) -> str:
    """Create the results-collection fan-in task."""
    task_id = f"tb-collect-{config.get('run_id', 'results')}"
    title = f"Collect TB trial results: {config.get('run_id', 'all')}"

    conditions = config.get("conditions", ["A", "D"])
    tasks = config.get("tasks", ["file-ops", "debugging"])

    description = (
        f"## Results Collection (Fan-in)\n\n"
        f"Collect and analyze results from {len(trial_task_ids)} trial tasks.\n\n"
        f"**Conditions:** {', '.join(conditions)}\n"
        f"**Tasks:** {', '.join(tasks)}\n"
        f"**Replicas:** {config.get('replicas', 3)} per condition×task\n\n"
        f"## Instructions\n\n"
        f"1. Read evaluation data from `.workgraph/agency/evaluations/` for all trial tasks\n"
        f"2. For each trial task, extract:\n"
        f"   - FLIP score (source: 'flip')\n"
        f"   - Evaluation score (source: 'llm')\n"
        f"   - Verify result (task status + verify output)\n"
        f"3. Check task statuses via `wg show <task-id>` for each trial\n"
        f"4. Produce a comparison table in `terminal-bench/trials/tb-results-{config.get('run_id', 'latest')}.json`\n"
        f"5. Calculate:\n"
        f"   - Pass rate per condition (verify passed / total)\n"
        f"   - Mean FLIP score per condition\n"
        f"   - Mean eval score per condition\n"
        f"   - FLIP-verify correlation (does low FLIP predict verify failure?)\n\n"
        f"## Trial Task IDs\n\n"
    )
    for tid in trial_task_ids:
        description += f"- `{tid}`\n"

    description += (
        f"\n## Validation\n\n"
        f"- [ ] Results JSON file produced at terminal-bench/trials/\n"
        f"- [ ] All trial tasks accounted for in results\n"
        f"- [ ] Per-condition statistics calculated\n"
    )

    # Fan-in depends on all trial tasks
    cmd = [
        "add", title,
        "--id", task_id,
        "-d", description,
        "--no-place",
    ]

    # Add --after for each trial task
    if trial_task_ids:
        cmd.extend(["--after", ",".join(trial_task_ids)])

    cmd.extend(["-t", "tb-trial"])
    cmd.extend(["-t", "tb-fanin"])

    run_wg(cmd, wg_dir)
    print(f"  Created fan-in: {task_id}")
    return task_id


def cmd_create(args):
    """Create trial tasks from config."""
    config = load_config(args.config)
    tb_root = os.path.dirname(os.path.abspath(args.config))
    wg_dir = args.wg_dir

    conditions = config.get("conditions", ["A", "D"])
    task_ids_list = config.get("tasks", ["file-ops", "debugging"])
    replicas = config.get("replicas", 3)
    model = config.get("model")

    print(f"Creating TB trial tasks:")
    print(f"  Conditions: {conditions}")
    print(f"  Tasks: {task_ids_list}")
    print(f"  Replicas: {replicas}")
    print(f"  Model: {model or '(default)'}")
    print()

    all_trial_ids = []

    for condition in conditions:
        if condition not in CONDITION_CONFIGS:
            print(f"  WARNING: Unknown condition '{condition}', skipping")
            continue

        print(f"Condition {condition}: {CONDITION_CONFIGS[condition]['description']}")
        for task_name in task_ids_list:
            if task_name not in TB_TASKS:
                print(f"  WARNING: Unknown task '{task_name}', skipping")
                continue

            task_def = TB_TASKS[task_name]
            instruction = load_instruction(task_def, tb_root)

            for replica in range(replicas):
                task_id = create_trial_task(
                    task_def, condition, replica,
                    instruction, model, wg_dir,
                )
                all_trial_ids.append(task_id)
        print()

    # Create fan-in results collection task
    if all_trial_ids:
        print("Creating fan-in results collection task:")
        fanin_id = create_fanin_task(all_trial_ids, config, wg_dir)
        all_trial_ids.append(fanin_id)

    # Write manifest for tracking
    manifest = {
        "run_id": config.get("run_id", "default"),
        "conditions": conditions,
        "tasks": task_ids_list,
        "replicas": replicas,
        "model": model,
        "trial_task_ids": all_trial_ids[:-1],  # exclude fan-in
        "fanin_task_id": all_trial_ids[-1] if all_trial_ids else None,
    }
    manifest_path = os.path.join(tb_root, "trials", f"manifest-{config.get('run_id', 'default')}.json")
    os.makedirs(os.path.dirname(manifest_path), exist_ok=True)
    with open(manifest_path, "w") as f:
        json.dump(manifest, f, indent=2)
    print(f"\nManifest written to: {manifest_path}")
    print(f"Total tasks created: {len(all_trial_ids)}")
    print(f"\nTo run: wg service start --max-agents {min(len(all_trial_ids) - 1, 4)}")


def cmd_status(args):
    """Show status of trial tasks."""
    config = load_config(args.config)
    tb_root = os.path.dirname(os.path.abspath(args.config))
    run_id = config.get("run_id", "default")
    manifest_path = os.path.join(tb_root, "trials", f"manifest-{run_id}.json")

    if not os.path.exists(manifest_path):
        print(f"No manifest found at {manifest_path}")
        print("Run 'create' first.")
        sys.exit(1)

    with open(manifest_path) as f:
        manifest = json.load(f)

    print(f"Trial Run: {manifest['run_id']}")
    print(f"Conditions: {manifest['conditions']}")
    print(f"Tasks: {manifest['tasks']}")
    print(f"Replicas: {manifest['replicas']}")
    print()

    for task_id in manifest["trial_task_ids"]:
        output = run_wg(["show", task_id, "--json"], args.wg_dir)
        try:
            task = json.loads(output)
            status = task.get("status", "unknown")
            print(f"  {task_id}: {status}")
        except json.JSONDecodeError:
            # Fall back to text parsing
            for line in output.split("\n"):
                if "Status:" in line:
                    print(f"  {task_id}: {line.strip()}")
                    break
            else:
                print(f"  {task_id}: (could not parse status)")

    if manifest.get("fanin_task_id"):
        output = run_wg(["show", manifest["fanin_task_id"]], args.wg_dir)
        for line in output.split("\n"):
            if "Status:" in line:
                print(f"\n  Fan-in {manifest['fanin_task_id']}: {line.strip()}")
                break


def cmd_collect(args):
    """Collect results from completed trial tasks."""
    config = load_config(args.config)
    tb_root = os.path.dirname(os.path.abspath(args.config))
    run_id = config.get("run_id", "default")
    manifest_path = os.path.join(tb_root, "trials", f"manifest-{run_id}.json")
    wg_dir = args.wg_dir

    if not os.path.exists(manifest_path):
        print(f"No manifest found at {manifest_path}")
        sys.exit(1)

    with open(manifest_path) as f:
        manifest = json.load(f)

    # Determine the workgraph agency directory
    if wg_dir:
        agency_dir = os.path.join(wg_dir, "agency", "evaluations")
    else:
        agency_dir = os.path.join(".workgraph", "agency", "evaluations")

    results = []
    for task_id in manifest["trial_task_ids"]:
        result = collect_task_result(task_id, agency_dir, wg_dir)
        results.append(result)

    # Compute per-condition statistics
    stats = compute_statistics(results, manifest["conditions"])

    output = {
        "run_id": manifest["run_id"],
        "conditions": manifest["conditions"],
        "tasks": manifest["tasks"],
        "replicas": manifest["replicas"],
        "model": manifest.get("model"),
        "results": results,
        "statistics": stats,
    }

    output_path = args.output or os.path.join(
        tb_root, "trials", f"tb-results-{run_id}.json"
    )
    os.makedirs(os.path.dirname(output_path), exist_ok=True)
    with open(output_path, "w") as f:
        json.dump(output, f, indent=2)
    print(f"Results written to: {output_path}")

    # Print summary
    print(f"\n{'='*60}")
    print(f"TB Trial Results: {run_id}")
    print(f"{'='*60}")
    for cond, cond_stats in stats.items():
        print(f"\nCondition {cond}:")
        print(f"  Pass rate:       {cond_stats['pass_rate']:.1%} ({cond_stats['passed']}/{cond_stats['total']})")
        print(f"  Mean FLIP score: {cond_stats['mean_flip']:.3f}" if cond_stats['mean_flip'] is not None else "  Mean FLIP score: N/A")
        print(f"  Mean eval score: {cond_stats['mean_eval']:.3f}" if cond_stats['mean_eval'] is not None else "  Mean eval score: N/A")


def collect_task_result(task_id: str, agency_dir: str, wg_dir: str | None) -> dict:
    """Collect result data for a single trial task."""
    result = {
        "task_id": task_id,
        "status": "unknown",
        "flip_score": None,
        "eval_score": None,
        "verify_passed": None,
        "eval_dimensions": None,
    }

    # Get task status
    output = run_wg(["show", task_id], wg_dir)
    for line in output.split("\n"):
        line = line.strip()
        if line.startswith("Status:"):
            result["status"] = line.split(":", 1)[1].strip()
            break

    # Check verify result: task in 'done' status means verify passed (if --verify was set)
    if result["status"] == "done":
        result["verify_passed"] = True
    elif result["status"] in ("failed", "abandoned"):
        result["verify_passed"] = False

    # Look for FLIP evaluation
    flip_task_id = f".flip-{task_id}"
    flip_evals = find_evaluations(agency_dir, task_id, source="flip")
    if flip_evals:
        latest = flip_evals[-1]  # most recent
        result["flip_score"] = latest.get("score")

    # Look for LLM evaluation
    eval_task_id = f".evaluate-{task_id}"
    llm_evals = find_evaluations(agency_dir, task_id, source="llm")
    if llm_evals:
        latest = llm_evals[-1]
        result["eval_score"] = latest.get("score")
        result["eval_dimensions"] = latest.get("dimensions")

    return result


def find_evaluations(agency_dir: str, task_id: str, source: str = "llm") -> list[dict]:
    """Find evaluation files for a task with a given source."""
    evals = []
    if not os.path.isdir(agency_dir):
        return evals

    prefix = f"eval-{task_id}-"
    for fname in sorted(os.listdir(agency_dir)):
        if not fname.startswith(prefix):
            continue
        fpath = os.path.join(agency_dir, fname)
        try:
            with open(fpath) as f:
                data = json.load(f)
            if data.get("source") == source or (source == "llm" and data.get("source") not in ("flip",)):
                evals.append(data)
        except (json.JSONDecodeError, OSError):
            continue

    return evals


def compute_statistics(results: list[dict], conditions: list[str]) -> dict:
    """Compute per-condition statistics."""
    stats = {}
    for condition in conditions:
        cond_lower = condition.lower()
        cond_results = [
            r for r in results
            if f"-{cond_lower}-" in r["task_id"]
        ]
        total = len(cond_results)
        passed = sum(1 for r in cond_results if r.get("verify_passed") is True)

        flip_scores = [r["flip_score"] for r in cond_results if r["flip_score"] is not None]
        eval_scores = [r["eval_score"] for r in cond_results if r["eval_score"] is not None]

        stats[condition] = {
            "total": total,
            "passed": passed,
            "failed": total - passed,
            "pass_rate": passed / total if total > 0 else 0.0,
            "mean_flip": sum(flip_scores) / len(flip_scores) if flip_scores else None,
            "mean_eval": sum(eval_scores) / len(eval_scores) if eval_scores else None,
            "flip_scores": flip_scores,
            "eval_scores": eval_scores,
        }

        # FLIP-verify correlation: does low FLIP predict verify failure?
        flip_verify_pairs = [
            (r["flip_score"], r["verify_passed"])
            for r in cond_results
            if r["flip_score"] is not None and r["verify_passed"] is not None
        ]
        if flip_verify_pairs:
            # Simple analysis: mean FLIP for pass vs fail
            pass_flips = [s for s, v in flip_verify_pairs if v]
            fail_flips = [s for s, v in flip_verify_pairs if not v]
            stats[condition]["flip_pass_mean"] = (
                sum(pass_flips) / len(pass_flips) if pass_flips else None
            )
            stats[condition]["flip_fail_mean"] = (
                sum(fail_flips) / len(fail_flips) if fail_flips else None
            )

    return stats


def main():
    parser = argparse.ArgumentParser(
        description="TB Trial Runner — create workgraph tasks from Terminal Bench definitions"
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    # create
    create_parser = subparsers.add_parser("create", help="Create trial tasks")
    create_parser.add_argument(
        "--config", required=True,
        help="Path to trial config JSON"
    )
    create_parser.add_argument(
        "--wg-dir", default=None,
        help="Path to .workgraph directory"
    )

    # status
    status_parser = subparsers.add_parser("status", help="Show trial task status")
    status_parser.add_argument(
        "--config", required=True,
        help="Path to trial config JSON"
    )
    status_parser.add_argument(
        "--wg-dir", default=None,
        help="Path to .workgraph directory"
    )

    # collect
    collect_parser = subparsers.add_parser("collect", help="Collect trial results")
    collect_parser.add_argument(
        "--config", required=True,
        help="Path to trial config JSON"
    )
    collect_parser.add_argument(
        "--output", default=None,
        help="Output JSON path (default: trials/tb-results-<run_id>.json)"
    )
    collect_parser.add_argument(
        "--wg-dir", default=None,
        help="Path to .workgraph directory"
    )

    args = parser.parse_args()

    if args.command == "create":
        cmd_create(args)
    elif args.command == "status":
        cmd_status(args)
    elif args.command == "collect":
        cmd_collect(args)


if __name__ == "__main__":
    main()
