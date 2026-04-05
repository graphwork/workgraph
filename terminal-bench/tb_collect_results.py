#!/usr/bin/env python3
"""
TB Results Collector — fan-in analysis for Terminal Bench trials.

Reads FLIP scores, evaluation scores, and verify results from completed
trial tasks. Produces a comparison report analyzing FLIP accuracy vs
external verifier results.

Usage:
    python tb_collect_results.py --manifest trials/manifest-pilot-01.json
    python tb_collect_results.py --manifest trials/manifest-pilot-01.json --wg-dir /path/to/.workgraph
"""

import argparse
import json
import os
import subprocess
import sys
from datetime import datetime, timezone


def run_wg(args: list[str], wg_dir: str | None = None) -> str:
    """Run a wg CLI command and return stdout."""
    cmd = ["wg"]
    if wg_dir:
        cmd.extend(["--dir", wg_dir])
    cmd.extend(args)
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    return result.stdout.strip()


def parse_task_status(wg_output: str) -> dict:
    """Parse wg show output into a structured dict."""
    info = {"status": "unknown", "logs": [], "verify_output": None}
    for line in wg_output.split("\n"):
        line = line.strip()
        if line.startswith("Status:"):
            info["status"] = line.split(":", 1)[1].strip()
        elif line.startswith("Log:"):
            # Logs follow this header
            continue
    return info


def find_evaluations(agency_dir: str, task_id: str) -> dict:
    """Find all evaluations for a task, separated by source."""
    result = {"flip": [], "llm": [], "other": []}
    if not os.path.isdir(agency_dir):
        return result

    # Evaluations may be named eval-<task_id>-<timestamp>.json
    # or sometimes just contain the task_id in the data
    for fname in sorted(os.listdir(agency_dir)):
        if not fname.endswith(".json"):
            continue
        fpath = os.path.join(agency_dir, fname)
        try:
            with open(fpath) as f:
                data = json.load(f)
        except (json.JSONDecodeError, OSError):
            continue

        if data.get("task_id") != task_id:
            continue

        source = data.get("source", "llm")
        if source == "flip":
            result["flip"].append(data)
        elif source == "llm":
            result["llm"].append(data)
        else:
            result["other"].append(data)

    return result


def collect_all_results(manifest: dict, wg_dir: str | None = None) -> list[dict]:
    """Collect results for all trial tasks in the manifest."""
    # Determine agency evaluations directory
    if wg_dir:
        agency_dir = os.path.join(wg_dir, "agency", "evaluations")
    else:
        agency_dir = os.path.join(".workgraph", "agency", "evaluations")

    results = []
    for task_id in manifest["trial_task_ids"]:
        # Parse task_id components: tb-<condition>-<task>-r<replica>
        parts = task_id.split("-")
        # Extract condition and task name
        condition = parts[1].upper() if len(parts) > 1 else "?"
        task_name = "-".join(parts[2:-1]) if len(parts) > 3 else "?"
        replica = parts[-1] if len(parts) > 1 else "?"

        # Get task status from wg
        wg_output = run_wg(["show", task_id], wg_dir)
        task_info = parse_task_status(wg_output)

        # Get evaluations
        evals = find_evaluations(agency_dir, task_id)

        # Extract scores
        flip_score = evals["flip"][-1]["score"] if evals["flip"] else None
        eval_score = evals["llm"][-1]["score"] if evals["llm"] else None
        eval_dims = evals["llm"][-1].get("dimensions") if evals["llm"] else None

        # Determine verify result from task status
        # done = verify passed (if --verify was set)
        # failed/abandoned = verify failed or agent couldn't complete
        verify_passed = None
        if task_info["status"] == "done":
            verify_passed = True
        elif task_info["status"] in ("failed", "abandoned"):
            verify_passed = False

        results.append({
            "task_id": task_id,
            "condition": condition,
            "task_name": task_name,
            "replica": replica,
            "status": task_info["status"],
            "verify_passed": verify_passed,
            "flip_score": flip_score,
            "eval_score": eval_score,
            "eval_dimensions": eval_dims,
            "flip_evaluations": len(evals["flip"]),
            "llm_evaluations": len(evals["llm"]),
        })

    return results


def compute_statistics(results: list[dict]) -> dict:
    """Compute per-condition and per-task statistics."""
    stats = {"by_condition": {}, "by_task": {}, "overall": {}}

    # Group by condition
    conditions = sorted(set(r["condition"] for r in results))
    for cond in conditions:
        cond_results = [r for r in results if r["condition"] == cond]
        stats["by_condition"][cond] = _compute_group_stats(cond_results)

    # Group by task
    tasks = sorted(set(r["task_name"] for r in results))
    for task in tasks:
        task_results = [r for r in results if r["task_name"] == task]
        stats["by_task"][task] = _compute_group_stats(task_results)

    # Overall
    stats["overall"] = _compute_group_stats(results)

    return stats


def _compute_group_stats(results: list[dict]) -> dict:
    """Compute statistics for a group of results."""
    total = len(results)
    completed = [r for r in results if r["status"] == "done"]
    failed = [r for r in results if r["status"] in ("failed", "abandoned")]
    in_progress = [r for r in results if r["status"] == "in-progress"]

    flip_scores = [r["flip_score"] for r in results if r["flip_score"] is not None]
    eval_scores = [r["eval_score"] for r in results if r["eval_score"] is not None]

    stats = {
        "total": total,
        "completed": len(completed),
        "failed": len(failed),
        "in_progress": len(in_progress),
        "pass_rate": len(completed) / total if total > 0 else 0.0,
        "mean_flip_score": sum(flip_scores) / len(flip_scores) if flip_scores else None,
        "mean_eval_score": sum(eval_scores) / len(eval_scores) if eval_scores else None,
    }

    # FLIP predictive analysis
    flip_verify_pairs = [
        (r["flip_score"], r["verify_passed"])
        for r in results
        if r["flip_score"] is not None and r["verify_passed"] is not None
    ]
    if flip_verify_pairs:
        pass_flips = [s for s, v in flip_verify_pairs if v]
        fail_flips = [s for s, v in flip_verify_pairs if not v]
        stats["flip_analysis"] = {
            "pairs": len(flip_verify_pairs),
            "mean_flip_when_pass": sum(pass_flips) / len(pass_flips) if pass_flips else None,
            "mean_flip_when_fail": sum(fail_flips) / len(fail_flips) if fail_flips else None,
        }

        # FLIP sensitivity/specificity at different thresholds
        for threshold in [0.5, 0.6, 0.7, 0.8]:
            tp = sum(1 for s, v in flip_verify_pairs if s >= threshold and v)
            fp = sum(1 for s, v in flip_verify_pairs if s >= threshold and not v)
            tn = sum(1 for s, v in flip_verify_pairs if s < threshold and not v)
            fn = sum(1 for s, v in flip_verify_pairs if s < threshold and v)
            sensitivity = tp / (tp + fn) if (tp + fn) > 0 else None
            specificity = tn / (tn + fp) if (tn + fp) > 0 else None
            stats[f"flip_threshold_{threshold}"] = {
                "sensitivity": sensitivity,
                "specificity": specificity,
                "tp": tp, "fp": fp, "tn": tn, "fn": fn,
            }

    return stats


def print_report(results: list[dict], stats: dict):
    """Print a human-readable report."""
    print(f"\n{'='*70}")
    print(f"Terminal Bench Trial Results")
    print(f"{'='*70}")

    # Per-condition summary
    print(f"\n--- Per-Condition Summary ---")
    print(f"{'Condition':<12} {'Total':<8} {'Pass':<8} {'Fail':<8} {'Rate':<8} {'FLIP':<8} {'Eval':<8}")
    print(f"{'-'*12} {'-'*8} {'-'*8} {'-'*8} {'-'*8} {'-'*8} {'-'*8}")
    for cond, cs in stats["by_condition"].items():
        flip_str = f"{cs['mean_flip_score']:.3f}" if cs['mean_flip_score'] is not None else "N/A"
        eval_str = f"{cs['mean_eval_score']:.3f}" if cs['mean_eval_score'] is not None else "N/A"
        print(f"{cond:<12} {cs['total']:<8} {cs['completed']:<8} {cs['failed']:<8} "
              f"{cs['pass_rate']:.1%}   {flip_str:<8} {eval_str:<8}")

    # Per-task summary
    print(f"\n--- Per-Task Summary ---")
    print(f"{'Task':<20} {'Total':<8} {'Pass':<8} {'Fail':<8} {'Rate':<8}")
    print(f"{'-'*20} {'-'*8} {'-'*8} {'-'*8} {'-'*8}")
    for task, ts in stats["by_task"].items():
        print(f"{task:<20} {ts['total']:<8} {ts['completed']:<8} {ts['failed']:<8} {ts['pass_rate']:.1%}")

    # Individual results
    print(f"\n--- Individual Results ---")
    print(f"{'Task ID':<35} {'Status':<15} {'Verify':<8} {'FLIP':<8} {'Eval':<8}")
    print(f"{'-'*35} {'-'*15} {'-'*8} {'-'*8} {'-'*8}")
    for r in results:
        verify_str = "PASS" if r["verify_passed"] else ("FAIL" if r["verify_passed"] is False else "?")
        flip_str = f"{r['flip_score']:.3f}" if r['flip_score'] is not None else "N/A"
        eval_str = f"{r['eval_score']:.3f}" if r['eval_score'] is not None else "N/A"
        print(f"{r['task_id']:<35} {r['status']:<15} {verify_str:<8} {flip_str:<8} {eval_str:<8}")

    # FLIP analysis
    overall = stats.get("overall", {})
    flip_analysis = overall.get("flip_analysis")
    if flip_analysis:
        print(f"\n--- FLIP Predictive Analysis ---")
        print(f"FLIP-verify pairs: {flip_analysis['pairs']}")
        if flip_analysis['mean_flip_when_pass'] is not None:
            print(f"Mean FLIP when verify PASS: {flip_analysis['mean_flip_when_pass']:.3f}")
        if flip_analysis['mean_flip_when_fail'] is not None:
            print(f"Mean FLIP when verify FAIL: {flip_analysis['mean_flip_when_fail']:.3f}")

        for threshold in [0.5, 0.6, 0.7, 0.8]:
            key = f"flip_threshold_{threshold}"
            if key in overall:
                t = overall[key]
                sens = f"{t['sensitivity']:.1%}" if t['sensitivity'] is not None else "N/A"
                spec = f"{t['specificity']:.1%}" if t['specificity'] is not None else "N/A"
                print(f"  Threshold {threshold}: sensitivity={sens}, specificity={spec} "
                      f"(TP={t['tp']} FP={t['fp']} TN={t['tn']} FN={t['fn']})")


def main():
    parser = argparse.ArgumentParser(
        description="Collect and analyze TB trial results"
    )
    parser.add_argument(
        "--manifest", required=True,
        help="Path to trial manifest JSON"
    )
    parser.add_argument(
        "--wg-dir", default=None,
        help="Path to .workgraph directory"
    )
    parser.add_argument(
        "--output", default=None,
        help="Output JSON path"
    )
    args = parser.parse_args()

    with open(args.manifest) as f:
        manifest = json.load(f)

    print(f"Collecting results for run: {manifest['run_id']}")
    print(f"  Trial tasks: {len(manifest['trial_task_ids'])}")

    results = collect_all_results(manifest, args.wg_dir)
    stats = compute_statistics(results)

    # Write JSON output
    output_path = args.output or os.path.join(
        os.path.dirname(args.manifest),
        f"tb-results-{manifest['run_id']}.json"
    )
    output_data = {
        "run_id": manifest["run_id"],
        "collected_at": datetime.now(timezone.utc).isoformat(),
        "conditions": manifest["conditions"],
        "tasks": manifest["tasks"],
        "replicas": manifest["replicas"],
        "model": manifest.get("model"),
        "results": results,
        "statistics": stats,
    }
    os.makedirs(os.path.dirname(output_path), exist_ok=True)
    with open(output_path, "w") as f:
        json.dump(output_data, f, indent=2)
    print(f"Results JSON written to: {output_path}")

    # Print human-readable report
    print_report(results, stats)


if __name__ == "__main__":
    main()
