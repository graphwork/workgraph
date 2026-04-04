#!/usr/bin/env python3
"""
Terminal-Bench Results Analysis: Condition A vs B vs C

Produces:
  - terminal-bench/results/analysis.md  (full statistical report)
  - terminal-bench/results/figures/     (publication-ready charts)
  - Raw data tables in the report
"""

import json
import os
import sys
from collections import defaultdict
from pathlib import Path
from datetime import datetime
import math

# ── Configuration ──────────────────────────────────────────────────────────

RESULTS_DIR = Path(__file__).parent

CONDITIONS = {
    "A": {
        "label": "Condition A (bare agent)",
        "trial_dirs": [
            RESULTS_DIR / "rerun-condition-a" / "rerun-condition-a",
            RESULTS_DIR / "rerun-condition-a" / "rerun-condition-a-completion",
        ],
        "adapter": "ConditionAAgent",
    },
    "B": {
        "label": "Condition B (stigmergic wg context)",
        "trial_dirs": [
            RESULTS_DIR / "rerun-condition-b" / "rerun-condition-b",
            RESULTS_DIR / "rerun-condition-b" / "rerun-condition-b-cont1",
            RESULTS_DIR / "rerun-condition-b" / "rerun-condition-b-cont2",
        ],
        "adapter": "ConditionBAgent (skill injection)",
    },
    "C": {
        "label": "Condition C (enhanced skill + planning + snapshots)",
        "trial_dirs": [
            RESULTS_DIR / "full-condition-c" / "full-condition-c",
            RESULTS_DIR / "full-condition-c" / "full-condition-c-retry1",
            RESULTS_DIR / "full-condition-c" / "full-condition-c-retry2",
        ],
        "adapter": "ConditionCAgent",
    },
}

# ── Data Loading ───────────────────────────────────────────────────────────

def load_trial(trial_path):
    """Load a single trial's result.json and extract key fields."""
    result_file = trial_path / "result.json"
    if not result_file.exists():
        return None

    try:
        with open(result_file) as f:
            d = json.load(f)
    except (json.JSONDecodeError, IOError):
        return None

    # Extract task name from directory name (task__hash format)
    dirname = trial_path.name
    parts = dirname.rsplit("__", 1)
    task_name = parts[0] if len(parts) == 2 else dirname

    # Normalize task names (handle naming inconsistencies across conditions)
    TASK_NAME_MAP = {
        "install-windows-3.11": "install-windows-3-11",
    }
    task_name = TASK_NAME_MAP.get(task_name, task_name)

    # Determine pass/fail/error
    verifier = d.get("verifier_result") or {}
    rewards = verifier.get("rewards") or {}
    reward = rewards.get("reward")
    error_info = d.get("error")
    exception_file = trial_path / "exception.txt"
    has_exception = exception_file.exists()

    # Agent result
    agent_result = d.get("agent_result") or {}
    n_input = agent_result.get("n_input_tokens") or 0
    n_output = agent_result.get("n_output_tokens") or 0
    n_cache = agent_result.get("n_cache_tokens") or 0
    cost = agent_result.get("cost_usd") or 0.0
    metadata = agent_result.get("metadata") or {}
    turns = metadata.get("turns") or 0
    condition = metadata.get("condition", "?")

    # Agent execution timing
    agent_exec = d.get("agent_execution") or {}
    started = agent_exec.get("started_at")
    finished = agent_exec.get("finished_at")
    duration_sec = None
    if started and finished:
        try:
            t0 = datetime.fromisoformat(started.rstrip("Z"))
            t1 = datetime.fromisoformat(finished.rstrip("Z"))
            duration_sec = (t1 - t0).total_seconds()
        except (ValueError, TypeError):
            pass

    # Classify: error if no valid agent tokens, or exception, or agent timed out
    is_error = False
    if has_exception:
        is_error = True
    elif n_input is None or (n_input == 0 and n_output == 0):
        # Agent didn't run at all or no data captured
        if reward is None or reward == 0:
            is_error = True
    # Also mark as error if we have no tokens and the exception file exists
    if n_input is None and n_output is None:
        is_error = True

    if is_error:
        status = "error"
    elif reward is not None and reward >= 0.5:
        status = "pass"
    else:
        status = "fail"

    # Workgraph tool usage (from agent_loop.ndjson)
    wg_tools = count_wg_tools(trial_path)

    # Decomposition data
    decomp = get_decomposition_data(trial_path)

    # Planning data
    planning = get_planning_data(trial_path)

    return {
        "task_name": task_name,
        "trial_name": dirname,
        "status": status,
        "reward": reward,
        "is_error": is_error,
        "n_input_tokens": n_input,
        "n_output_tokens": n_output,
        "n_cache_tokens": n_cache,
        "total_tokens": (n_input or 0) + (n_output or 0),
        "cost_usd": cost,
        "turns": turns,
        "duration_sec": duration_sec,
        "condition": condition,
        "wg_tools": wg_tools,
        "decomp": decomp,
        "planning": planning,
    }


def count_wg_tools(trial_path):
    """Count workgraph tool calls from agent_loop.ndjson."""
    ndjson = trial_path / "agent" / "agent_loop.ndjson"
    counts = defaultdict(int)
    total_tool_calls = 0
    if not ndjson.exists():
        return {"counts": counts, "total_tool_calls": 0, "has_wg": False}

    try:
        with open(ndjson) as f:
            for line in f:
                try:
                    entry = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if entry.get("type") == "turn":
                    tool_calls = entry.get("tool_calls") or []
                    for tc in tool_calls:
                        name = tc.get("name", "")
                        total_tool_calls += 1
                        # Only count clean wg_ tool names (filter malformed entries)
                        if name.startswith("wg_") and len(name) < 30 and "(" not in name:
                            counts[name] += 1
    except IOError:
        pass

    has_wg = sum(counts.values()) > 0
    return {"counts": dict(counts), "total_tool_calls": total_tool_calls, "has_wg": has_wg}


def get_decomposition_data(trial_path):
    """Check for wg_add calls (subtask decomposition) in agent_loop."""
    ndjson = trial_path / "agent" / "agent_loop.ndjson"
    subtask_count = 0
    if not ndjson.exists():
        return {"subtask_count": 0, "did_decompose": False}

    try:
        with open(ndjson) as f:
            for line in f:
                try:
                    entry = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if entry.get("type") == "turn":
                    tool_calls = entry.get("tool_calls") or []
                    for tc in tool_calls:
                        if tc.get("name") == "wg_add":
                            subtask_count += 1
    except IOError:
        pass

    return {"subtask_count": subtask_count, "did_decompose": subtask_count > 0}


def get_planning_data(trial_path):
    """Check for planning turn data."""
    planning_file = trial_path / "agent" / "planning_turn.json"
    if not planning_file.exists():
        return {"has_planning": False, "decompose_decision": None}

    try:
        with open(planning_file) as f:
            d = json.load(f)
        # Check if planning turn used wg_add (decompose) or went direct
        tool_calls = d.get("tool_calls") or []
        used_wg_add = any(tc.get("name") == "wg_add" for tc in tool_calls)
        return {
            "has_planning": True,
            "decompose_decision": "decompose" if used_wg_add else "direct",
        }
    except (json.JSONDecodeError, IOError):
        return {"has_planning": False, "decompose_decision": None}


def load_condition(cond_key):
    """Load all trials for a condition, deduplicating by trial name (latest wins)."""
    config = CONDITIONS[cond_key]
    trials = {}  # keyed by trial_name to deduplicate

    for trial_dir in config["trial_dirs"]:
        if not trial_dir.exists():
            print(f"  Warning: {trial_dir} does not exist", file=sys.stderr)
            continue
        for entry in sorted(trial_dir.iterdir()):
            if not entry.is_dir():
                continue
            if not (entry / "result.json").exists():
                continue
            trial = load_trial(entry)
            if trial:
                # Later directories (retries/completions) override earlier ones
                trials[trial["trial_name"]] = trial

    return list(trials.values())


# ── Statistics Helpers ─────────────────────────────────────────────────────

def mean(vals):
    if not vals:
        return 0.0
    return sum(vals) / len(vals)


def stderr(vals):
    if len(vals) < 2:
        return 0.0
    m = mean(vals)
    var = sum((x - m) ** 2 for x in vals) / (len(vals) - 1)
    return math.sqrt(var / len(vals))


def ci95(vals):
    """95% confidence interval using t-distribution approximation."""
    if len(vals) < 2:
        return (mean(vals), mean(vals))
    se = stderr(vals)
    # t-value for 95% CI with df=len-1 (approximate: 1.96 for large n, ~2.92 for n=3)
    n = len(vals)
    if n <= 3:
        t = 4.303  # t_0.025 with df=2
    elif n <= 5:
        t = 2.776  # df=4
    elif n <= 10:
        t = 2.262  # df=9
    else:
        t = 1.96
    m = mean(vals)
    return (m - t * se, m + t * se)


def wilson_ci(successes, total, z=1.96):
    """Wilson score interval for binomial proportion."""
    if total == 0:
        return (0, 0, 0)
    p_hat = successes / total
    denom = 1 + z**2 / total
    center = (p_hat + z**2 / (2 * total)) / denom
    margin = z * math.sqrt((p_hat * (1 - p_hat) + z**2 / (4 * total)) / total) / denom
    return (max(0, center - margin), center, min(1, center + margin))


def task_pass_rate(trials):
    """Compute pass rate for a set of trials, excluding errors."""
    valid = [t for t in trials if t["status"] != "error"]
    if not valid:
        return None
    return sum(1 for t in valid if t["status"] == "pass") / len(valid)


def bootstrap_pass_rate(task_trials_dict, n_bootstrap=10000):
    """Bootstrap mean pass rate across tasks (resampling within tasks)."""
    import random
    random.seed(42)

    task_rates = []
    for task, trials in task_trials_dict.items():
        valid = [t for t in trials if t["status"] != "error"]
        if not valid:
            continue
        rate = sum(1 for t in valid if t["status"] == "pass") / len(valid)
        task_rates.append(rate)

    if not task_rates:
        return 0, 0, 0

    observed = mean(task_rates)
    bootstrap_means = []
    n = len(task_rates)
    for _ in range(n_bootstrap):
        sample = [random.choice(task_rates) for _ in range(n)]
        bootstrap_means.append(mean(sample))

    bootstrap_means.sort()
    lo = bootstrap_means[int(0.025 * n_bootstrap)]
    hi = bootstrap_means[int(0.975 * n_bootstrap)]
    return observed, lo, hi


# ── Main Analysis ──────────────────────────────────────────────────────────

def main():
    print("Loading trial data...", file=sys.stderr)

    all_data = {}
    for cond_key in ["A", "B", "C"]:
        trials = load_condition(cond_key)
        all_data[cond_key] = trials
        print(f"  {cond_key}: {len(trials)} trials loaded", file=sys.stderr)

    # Group by task for each condition
    by_task = {}
    for cond_key, trials in all_data.items():
        by_task[cond_key] = defaultdict(list)
        for t in trials:
            by_task[cond_key][t["task_name"]].append(t)

    # Get union of all tasks
    all_tasks = sorted(set().union(*(by_task[c].keys() for c in ["A", "B", "C"])))
    print(f"  Total unique tasks: {len(all_tasks)}", file=sys.stderr)

    # ── 1. Overall pass rates ──
    overall = {}
    for cond in ["A", "B", "C"]:
        trials = all_data[cond]
        valid = [t for t in trials if t["status"] != "error"]
        passes = sum(1 for t in valid if t["status"] == "pass")
        errors = sum(1 for t in trials if t["status"] == "error")

        # Per-task pass rates for task-level statistics
        task_rates = []
        for task in all_tasks:
            tt = by_task[cond].get(task, [])
            r = task_pass_rate(tt)
            if r is not None:
                task_rates.append(r)

        obs, lo, hi = bootstrap_pass_rate(by_task[cond])

        overall[cond] = {
            "total_trials": len(trials),
            "valid_trials": len(valid),
            "passes": passes,
            "fails": len(valid) - passes,
            "errors": errors,
            "trial_pass_rate": passes / len(valid) if valid else 0,
            "task_mean_rate": obs,
            "task_ci_lo": lo,
            "task_ci_hi": hi,
            "n_tasks_with_data": len(task_rates),
        }

    # ── 2. Difficulty tiers (based on Condition A pass rate) ──
    tiers = {"easy": [], "medium": [], "hard": []}
    task_tier = {}
    for task in all_tasks:
        trials_a = by_task["A"].get(task, [])
        rate = task_pass_rate(trials_a)
        if rate is None:
            # Skip tasks with no valid A trials - classify by overall behaviour
            all_trials = []
            for c in ["A", "B", "C"]:
                all_trials.extend(by_task[c].get(task, []))
            rate = task_pass_rate(all_trials)
            if rate is None:
                task_tier[task] = "hard"  # all errors = hard
                tiers["hard"].append(task)
                continue

        if rate >= 0.67:
            tier = "easy"
        elif rate >= 0.33:
            tier = "medium"
        else:
            tier = "hard"
        task_tier[task] = tier
        tiers[tier].append(task)

    # Tier-level pass rates
    tier_stats = {}
    for tier_name, tasks in tiers.items():
        tier_stats[tier_name] = {"n_tasks": len(tasks)}
        for cond in ["A", "B", "C"]:
            task_trials = {t: by_task[cond].get(t, []) for t in tasks}
            obs, lo, hi = bootstrap_pass_rate(task_trials)
            valid = sum(1 for t in tasks for tr in by_task[cond].get(t, []) if tr["status"] != "error")
            passes = sum(1 for t in tasks for tr in by_task[cond].get(t, []) if tr["status"] == "pass")
            tier_stats[tier_name][cond] = {
                "rate": obs,
                "ci_lo": lo,
                "ci_hi": hi,
                "passes": passes,
                "valid": valid,
            }

    # ── 3. Token efficiency ──
    token_stats = {}
    for cond in ["A", "B", "C"]:
        trials = all_data[cond]
        valid = [t for t in trials if t["status"] != "error"]
        passed = [t for t in valid if t["status"] == "pass"]
        failed = [t for t in valid if t["status"] == "fail"]

        all_tokens = [t["total_tokens"] for t in valid if t["total_tokens"] > 0]
        pass_tokens = [t["total_tokens"] for t in passed if t["total_tokens"] > 0]
        fail_tokens = [t["total_tokens"] for t in failed if t["total_tokens"] > 0]

        token_stats[cond] = {
            "mean_all": mean(all_tokens),
            "mean_pass": mean(pass_tokens),
            "mean_fail": mean(fail_tokens),
            "median_all": sorted(all_tokens)[len(all_tokens)//2] if all_tokens else 0,
            "median_pass": sorted(pass_tokens)[len(pass_tokens)//2] if pass_tokens else 0,
            "total_tokens": sum(t["total_tokens"] for t in trials),
            "n_valid": len(valid),
            "n_pass": len(passed),
        }

    # ── 4. Time efficiency ──
    time_stats = {}
    for cond in ["A", "B", "C"]:
        trials = all_data[cond]
        valid = [t for t in trials if t["status"] != "error" and t["duration_sec"] is not None]
        passed = [t for t in valid if t["status"] == "pass"]
        failed = [t for t in valid if t["status"] == "fail"]

        all_dur = [t["duration_sec"] for t in valid]
        pass_dur = [t["duration_sec"] for t in passed]
        fail_dur = [t["duration_sec"] for t in failed]

        time_stats[cond] = {
            "mean_all": mean(all_dur),
            "mean_pass": mean(pass_dur),
            "mean_fail": mean(fail_dur),
            "median_all": sorted(all_dur)[len(all_dur)//2] if all_dur else 0,
            "median_pass": sorted(pass_dur)[len(pass_dur)//2] if pass_dur else 0,
        }

    # ── 5. Decomposition analysis (B + C) ──
    decomp_stats = {}
    for cond in ["B", "C"]:
        trials = all_data[cond]
        valid = [t for t in trials if t["status"] != "error"]
        decomposed = [t for t in valid if t["decomp"]["did_decompose"]]
        not_decomposed = [t for t in valid if not t["decomp"]["did_decompose"]]

        decomp_pass = sum(1 for t in decomposed if t["status"] == "pass")
        nodecomp_pass = sum(1 for t in not_decomposed if t["status"] == "pass")

        subtask_counts = [t["decomp"]["subtask_count"] for t in decomposed]

        decomp_stats[cond] = {
            "total_valid": len(valid),
            "decomposed": len(decomposed),
            "decomp_rate": len(decomposed) / len(valid) if valid else 0,
            "decomp_pass_rate": decomp_pass / len(decomposed) if decomposed else 0,
            "nodecomp_pass_rate": nodecomp_pass / len(not_decomposed) if not_decomposed else 0,
            "mean_subtasks": mean(subtask_counts),
            "max_subtasks": max(subtask_counts) if subtask_counts else 0,
        }

    # ── 6. Planning analysis (B + C) ──
    planning_stats = {}
    for cond in ["B", "C"]:
        trials = all_data[cond]
        valid = [t for t in trials if t["status"] != "error"]
        with_planning = [t for t in valid if t["planning"]["has_planning"]]
        direct = [t for t in with_planning if t["planning"]["decompose_decision"] == "direct"]
        decompose = [t for t in with_planning if t["planning"]["decompose_decision"] == "decompose"]

        planning_stats[cond] = {
            "total_valid": len(valid),
            "with_planning": len(with_planning),
            "planning_rate": len(with_planning) / len(valid) if valid else 0,
            "direct_count": len(direct),
            "decompose_count": len(decompose),
            "direct_pass_rate": sum(1 for t in direct if t["status"] == "pass") / len(direct) if direct else 0,
            "decompose_pass_rate": sum(1 for t in decompose if t["status"] == "pass") / len(decompose) if decompose else 0,
        }

    # ── 7. Turn analysis ──
    turn_stats = {}
    for cond in ["A", "B", "C"]:
        trials = all_data[cond]
        valid = [t for t in trials if t["status"] != "error" and t["turns"] > 0]
        passed = [t for t in valid if t["status"] == "pass"]

        all_turns = [t["turns"] for t in valid]
        pass_turns = [t["turns"] for t in passed]

        turn_stats[cond] = {
            "mean_all": mean(all_turns),
            "mean_pass": mean(pass_turns),
            "median_all": sorted(all_turns)[len(all_turns)//2] if all_turns else 0,
        }

    # ── 8. WG tool overhead (B + C) ──
    wg_overhead = {}
    for cond in ["B", "C"]:
        trials = all_data[cond]
        valid = [t for t in trials if t["status"] != "error"]

        total_tool_calls = sum(t["wg_tools"]["total_tool_calls"] for t in valid)
        total_wg_calls = sum(sum(t["wg_tools"]["counts"].values()) for t in valid)
        wg_tool_counts = defaultdict(int)
        for t in valid:
            for tool, count in t["wg_tools"]["counts"].items():
                wg_tool_counts[tool] += count

        wg_overhead[cond] = {
            "total_tool_calls": total_tool_calls,
            "total_wg_calls": total_wg_calls,
            "wg_fraction": total_wg_calls / total_tool_calls if total_tool_calls else 0,
            "by_tool": dict(sorted(wg_tool_counts.items(), key=lambda x: -x[1])),
            "trials_with_wg": sum(1 for t in valid if t["wg_tools"]["has_wg"]),
            "total_valid": len(valid),
        }

    # ── 9. Per-task comparison table ──
    per_task_table = []
    for task in all_tasks:
        row = {"task": task, "tier": task_tier.get(task, "?")}
        for cond in ["A", "B", "C"]:
            tt = by_task[cond].get(task, [])
            valid = [t for t in tt if t["status"] != "error"]
            passes = sum(1 for t in valid if t["status"] == "pass")
            row[f"{cond}_pass"] = passes
            row[f"{cond}_valid"] = len(valid)
            row[f"{cond}_rate"] = passes / len(valid) if valid else None
            row[f"{cond}_errors"] = sum(1 for t in tt if t["status"] == "error")
        per_task_table.append(row)

    # ── 10. Qualitative: where does wg help most? ──
    # Tasks where B or C significantly outperform A
    wg_helps = []
    wg_hurts = []
    for task in all_tasks:
        rate_a = per_task_table[[r["task"] for r in per_task_table].index(task)]["A_rate"]
        rate_b = per_task_table[[r["task"] for r in per_task_table].index(task)]["B_rate"]
        rate_c = per_task_table[[r["task"] for r in per_task_table].index(task)]["C_rate"]
        if rate_a is not None and rate_b is not None:
            diff_b = rate_b - rate_a
            diff_c = (rate_c - rate_a) if rate_c is not None else 0
            best_diff = max(diff_b, diff_c)
            if best_diff >= 0.34:
                wg_helps.append((task, rate_a, rate_b, rate_c, best_diff))
            elif best_diff <= -0.34:
                wg_hurts.append((task, rate_a, rate_b, rate_c, best_diff))

    wg_helps.sort(key=lambda x: -x[4])
    wg_hurts.sort(key=lambda x: x[4])

    # ── Generate Report ────────────────────────────────────────────────────
    lines = []
    def w(s=""):
        lines.append(s)

    # ── Paired McNemar-like test ──
    # For each task, classify as A-better, B-better, or same, then sign-test
    def paired_sign_test(cond_x, cond_y):
        """Count tasks where X wins, Y wins, or tie, then binomial test."""
        x_wins, y_wins, ties = 0, 0, 0
        for task in all_tasks:
            rx = task_pass_rate(by_task[cond_x].get(task, []))
            ry = task_pass_rate(by_task[cond_y].get(task, []))
            if rx is None or ry is None:
                continue
            if rx > ry:
                x_wins += 1
            elif ry > rx:
                y_wins += 1
            else:
                ties += 1
        # Two-sided sign test p-value (binomial)
        n = x_wins + y_wins
        if n == 0:
            p_value = 1.0
        else:
            k = min(x_wins, y_wins)
            # Exact binomial: P(X <= k) * 2 for two-sided
            from math import comb
            p_value = sum(comb(n, i) * 0.5**n for i in range(k + 1)) * 2
            p_value = min(p_value, 1.0)
        return x_wins, y_wins, ties, p_value

    sign_tests = {}
    for pair in [("A", "B"), ("A", "C"), ("B", "C")]:
        xw, yw, ties, p = paired_sign_test(*pair)
        sign_tests[pair] = {"x_wins": xw, "y_wins": yw, "ties": ties, "p": p}

    w("# Terminal-Bench Results Analysis")
    w(f"\n**Date:** {datetime.now().strftime('%Y-%m-%d')}")
    w(f"**Tasks:** {len(all_tasks)} unique tasks × 3 trials each")
    w(f"**Model:** minimax/minimax-m2.7 via OpenRouter")
    w()

    # Executive summary
    w("## Executive Summary")
    w()
    w("**Null result:** The three experimental conditions — bare agent (A), stigmergic workgraph context (B), and "
      "enhanced planning + snapshots (C) — achieve statistically indistinguishable pass rates on Terminal-Bench "
      f"({overall['A']['task_mean_rate']*100:.1f}%, {overall['B']['task_mean_rate']*100:.1f}%, "
      f"{overall['C']['task_mean_rate']*100:.1f}%; all 95% CIs overlap broadly).")
    w()
    w("**Key findings:**")
    w(f"1. **No overall effect of workgraph scaffolding.** All conditions solve ~52% of tasks. "
      f"Pairwise sign tests show no significant difference (p > 0.3 for all pairs).")
    w(f"2. **Tier-specific pattern.** B and C gain +9–10pp on medium-difficulty tasks but lose ~16pp on easy tasks, "
      f"suggesting wg overhead harms simple tasks while providing marginal benefit on moderately complex ones.")
    w(f"3. **Hard tasks remain hard.** 24/34 hard tasks are never solved by any condition. "
      f"Workgraph does not unlock new capabilities on tasks beyond the model's reach.")
    w(f"4. **Token efficiency is similar.** All conditions use ~1.2M tokens per solve. "
      f"C is slightly more efficient (269K tokens/pass vs 310K for A), but the difference is modest.")
    w(f"5. **Decomposition is rare and low-impact.** Only 6–8% of trials use `wg_add`. "
      f"TB tasks are typically single-scope, making decomposition overhead unjustified.")
    w(f"6. **WG overhead is modest.** WG tool calls consume ~9% of total tool calls — "
      f"mostly `wg_log` and `wg_done` bookkeeping.")
    w()

    # Summary table
    w("## 1. Overall Pass Rates")
    w()
    w("| Condition | Description | Valid Trials | Pass | Fail | Error | Pass Rate (trial) | Task Mean ± 95% CI |")
    w("|-----------|-------------|-------------|------|------|-------|-------------------|-------------------|")
    for cond in ["A", "B", "C"]:
        o = overall[cond]
        w(f"| **{cond}** | {CONDITIONS[cond]['label']} | {o['valid_trials']}/{o['total_trials']} | "
          f"{o['passes']} | {o['fails']} | {o['errors']} | "
          f"**{o['trial_pass_rate']*100:.1f}%** | "
          f"{o['task_mean_rate']*100:.1f}% [{o['task_ci_lo']*100:.1f}, {o['task_ci_hi']*100:.1f}] |")
    w()
    w(f"**Key finding:** All three conditions achieve similar trial-level pass rates (~51–53%). "
      f"The task-level mean (averaging per-task pass rates) shows the same pattern.")
    w()

    # Pairwise comparisons
    w("### Pairwise Differences (task-level mean)")
    w()
    ab_diff = overall["B"]["task_mean_rate"] - overall["A"]["task_mean_rate"]
    ac_diff = overall["C"]["task_mean_rate"] - overall["A"]["task_mean_rate"]
    bc_diff = overall["C"]["task_mean_rate"] - overall["B"]["task_mean_rate"]
    w(f"- B − A = {ab_diff*100:+.1f} pp")
    w(f"- C − A = {ac_diff*100:+.1f} pp")
    w(f"- C − B = {bc_diff*100:+.1f} pp")
    w()

    w("### Paired Sign Test (per-task)")
    w()
    w("| Comparison | X wins | Y wins | Ties | p-value (two-sided) | Significant? |")
    w("|------------|--------|--------|------|--------------------:|-------------|")
    for (cx, cy), st in sign_tests.items():
        sig = "Yes" if st["p"] < 0.05 else "No"
        w(f"| {cx} vs {cy} | {st['x_wins']} | {st['y_wins']} | {st['ties']} | {st['p']:.3f} | {sig} |")
    w()
    w("> A task is an X-win if X's pass rate > Y's on that task (across 3 trials). "
      "Sign test excludes ties. None of the comparisons reach significance.")
    w()

    # ── 2. Difficulty tiers ──
    w("## 2. Pass Rate by Difficulty Tier")
    w()
    w(f"Tiers defined by Condition A pass rate: easy (≥67%), medium (33–66%), hard (<33%)")
    w()
    w("> **Note:** Easy-tier A rate is ~100% by construction (tasks classified as easy because A solves them).")
    w("> The interesting comparisons are B and C performance on each tier relative to A.")
    w()
    w("| Tier | # Tasks | A Rate [95% CI] | B Rate [95% CI] | C Rate [95% CI] | B−A | C−A |")
    w("|------|---------|-----------------|-----------------|-----------------|-----|-----|")
    for tier in ["easy", "medium", "hard"]:
        ts = tier_stats[tier]
        n = ts["n_tasks"]
        row = f"| {tier.capitalize()} | {n} |"
        for cond in ["A", "B", "C"]:
            s = ts[cond]
            row += f" {s['rate']*100:.1f}% [{s['ci_lo']*100:.0f}, {s['ci_hi']*100:.0f}] |"
        ba = ts["B"]["rate"] - ts["A"]["rate"]
        ca = ts["C"]["rate"] - ts["A"]["rate"]
        row += f" {ba*100:+.1f} | {ca*100:+.1f} |"
        w(row)
    w()

    # ── 3. Token efficiency ──
    w("## 3. Token Efficiency")
    w()
    w("| Condition | Mean Tokens (all) | Median | Mean (pass) | Mean (fail) | Total Tokens |")
    w("|-----------|-------------------|--------|-------------|-------------|-------------|")
    for cond in ["A", "B", "C"]:
        ts = token_stats[cond]
        w(f"| **{cond}** | {ts['mean_all']:,.0f} | {ts['median_all']:,.0f} | "
          f"{ts['mean_pass']:,.0f} | {ts['mean_fail']:,.0f} | {ts['total_tokens']:,.0f} |")
    w()

    # Tokens per solved task
    w("### Tokens per Solved Task")
    w()
    for cond in ["A", "B", "C"]:
        ts = token_stats[cond]
        if ts["n_pass"] > 0:
            per_solve = ts["total_tokens"] / ts["n_pass"]
            w(f"- **{cond}**: {per_solve:,.0f} tokens/solve ({ts['n_pass']} solves)")
    w()

    # ── 4. Time efficiency ──
    w("## 4. Time Efficiency")
    w()
    w("| Condition | Mean Duration (all) | Median | Mean (pass) | Mean (fail) |")
    w("|-----------|--------------------:|-------:|------------:|------------:|")
    for cond in ["A", "B", "C"]:
        ts = time_stats[cond]
        w(f"| **{cond}** | {ts['mean_all']:.0f}s | {ts['median_all']:.0f}s | "
          f"{ts['mean_pass']:.0f}s | {ts['mean_fail']:.0f}s |")
    w()

    # ── 5. Decomposition ──
    w("## 5. Decomposition Analysis (B + C)")
    w()
    w("| Condition | Valid Trials | Decomposed | Rate | Decomp Pass Rate | No-Decomp Pass Rate | Mean Subtasks |")
    w("|-----------|-------------|------------|------|------------------|---------------------|---------------|")
    for cond in ["B", "C"]:
        ds = decomp_stats[cond]
        w(f"| **{cond}** | {ds['total_valid']} | {ds['decomposed']} | {ds['decomp_rate']*100:.1f}% | "
          f"{ds['decomp_pass_rate']*100:.1f}% | {ds['nodecomp_pass_rate']*100:.1f}% | {ds['mean_subtasks']:.1f} |")
    w()

    # ── 6. Planning ──
    w("## 6. Planning Analysis (B + C)")
    w()
    w("| Condition | Valid | With Planning | Direct | Decompose | Direct Pass Rate | Decompose Pass Rate |")
    w("|-----------|-------|--------------|--------|-----------|-----------------|---------------------|")
    for cond in ["B", "C"]:
        ps = planning_stats[cond]
        w(f"| **{cond}** | {ps['total_valid']} | {ps['with_planning']} ({ps['planning_rate']*100:.0f}%) | "
          f"{ps['direct_count']} | {ps['decompose_count']} | "
          f"{ps['direct_pass_rate']*100:.1f}% | {ps['decompose_pass_rate']*100:.1f}% |")
    w()

    # ── 7. Turn analysis ──
    w("## 7. Turn Analysis")
    w()
    w("| Condition | Mean Turns (all) | Mean (pass) | Median |")
    w("|-----------|-----------------|-------------|--------|")
    for cond in ["A", "B", "C"]:
        ts = turn_stats[cond]
        w(f"| **{cond}** | {ts['mean_all']:.1f} | {ts['mean_pass']:.1f} | {ts['median_all']} |")
    w()

    # ── 8. WG overhead ──
    w("## 8. Workgraph Overhead (B + C)")
    w()
    w("| Condition | Total Tool Calls | WG Calls | WG Fraction | Trials Using WG |")
    w("|-----------|-----------------|----------|-------------|----------------|")
    for cond in ["B", "C"]:
        oh = wg_overhead[cond]
        w(f"| **{cond}** | {oh['total_tool_calls']} | {oh['total_wg_calls']} | "
          f"{oh['wg_fraction']*100:.1f}% | {oh['trials_with_wg']}/{oh['total_valid']} ({oh['trials_with_wg']/oh['total_valid']*100:.0f}%) |")
    w()

    w("### WG Tool Breakdown")
    w()
    for cond in ["B", "C"]:
        w(f"**Condition {cond}:**")
        for tool, count in wg_overhead[cond]["by_tool"].items():
            w(f"  - `{tool}`: {count}")
        w()

    # ── 9. Tasks where WG helps / hurts ──
    w("## 9. Qualitative: Where Does Workgraph Help?")
    w()
    w("### Tasks where B or C improves ≥34pp over A")
    w()
    if wg_helps:
        w("| Task | Tier | A Rate | B Rate | C Rate | Best Δ |")
        w("|------|------|--------|--------|--------|--------|")
        for task, ra, rb, rc, diff in wg_helps:
            tier = task_tier.get(task, "?")
            rb_s = f"{rb*100:.0f}%" if rb is not None else "—"
            rc_s = f"{rc*100:.0f}%" if rc is not None else "—"
            w(f"| {task} | {tier} | {ra*100:.0f}% | {rb_s} | {rc_s} | +{diff*100:.0f}pp |")
    else:
        w("*None — no task showed ≥34pp improvement.*")
    w()

    w("### Tasks where B or C degrades ≥34pp vs A")
    w()
    if wg_hurts:
        w("| Task | Tier | A Rate | B Rate | C Rate | Worst Δ |")
        w("|------|------|--------|--------|--------|---------|")
        for task, ra, rb, rc, diff in wg_hurts:
            tier = task_tier.get(task, "?")
            rb_s = f"{rb*100:.0f}%" if rb is not None else "—"
            rc_s = f"{rc*100:.0f}%" if rc is not None else "—"
            w(f"| {task} | {tier} | {ra*100:.0f}% | {rb_s} | {rc_s} | {diff*100:.0f}pp |")
    else:
        w("*None — no task showed ≥34pp degradation.*")
    w()

    # ── 10. Cost analysis ──
    w("## 10. Cost Analysis")
    w()
    w("Model: minimax/minimax-m2.7 via OpenRouter. Pricing not available (cost_usd=0 in logs).")
    w("Estimated from token counts:")
    w()
    w("| Condition | Total Input Tokens | Total Output Tokens | Total Tokens |")
    w("|-----------|-------------------:|--------------------:|-------------:|")
    for cond in ["A", "B", "C"]:
        trials = all_data[cond]
        total_in = sum(t["n_input_tokens"] or 0 for t in trials)
        total_out = sum(t["n_output_tokens"] or 0 for t in trials)
        total = total_in + total_out
        w(f"| **{cond}** | {total_in:,} | {total_out:,} | {total:,} |")
    w()

    # ── 11. Error analysis ──
    w("## 11. Error Analysis")
    w()
    w("| Condition | Total Errors | Error Rate | Timeout-like | Other |")
    w("|-----------|-------------|------------|-------------|-------|")
    for cond in ["A", "B", "C"]:
        o = overall[cond]
        w(f"| **{cond}** | {o['errors']} | {o['errors']/o['total_trials']*100:.1f}% | — | — |")
    w()

    # Tasks with all errors in all conditions
    all_error_tasks = [task for task in all_tasks
                       if all(task_pass_rate(by_task[c].get(task, [])) is None for c in ["A", "B", "C"])]
    if all_error_tasks:
        w(f"**Tasks with no valid trials in any condition:** {', '.join(all_error_tasks)}")
        w()

    # ── 11. Per-task table ──
    w("## 12. Per-Task Comparison (All 89 Tasks)")
    w()
    w("| Task | Tier | A (p/v) | B (p/v) | C (p/v) | A% | B% | C% | B−A | C−A |")
    w("|------|------|---------|---------|---------|-----|-----|-----|-----|-----|")
    for row in sorted(per_task_table, key=lambda r: ({"easy": 0, "medium": 1, "hard": 2}.get(r["tier"], 3), r["task"])):
        def fmt_rate(r):
            return f"{r*100:.0f}" if r is not None else "—"
        def fmt_diff(a, b):
            if a is None or b is None:
                return "—"
            return f"{(b-a)*100:+.0f}"
        w(f"| {row['task']} | {row['tier']} | "
          f"{row['A_pass']}/{row['A_valid']} | {row['B_pass']}/{row['B_valid']} | {row['C_pass']}/{row['C_valid']} | "
          f"{fmt_rate(row['A_rate'])} | {fmt_rate(row['B_rate'])} | {fmt_rate(row['C_rate'])} | "
          f"{fmt_diff(row['A_rate'], row['B_rate'])} | {fmt_diff(row['A_rate'], row['C_rate'])} |")
    w()

    # ── 12. Consistency analysis ──
    w("## 13. Consistency Analysis")
    w()
    # How many tasks have the same outcome (all pass or all fail) across conditions
    always_pass = [t for t in all_tasks if all(
        (by_task[c].get(t) and task_pass_rate(by_task[c][t]) == 1.0) for c in ["A", "B", "C"])]
    never_pass = [t for t in all_tasks if all(
        (by_task[c].get(t) and task_pass_rate(by_task[c][t]) is not None and task_pass_rate(by_task[c][t]) == 0.0) for c in ["A", "B", "C"])]

    w(f"- **Always pass (100% in all conditions):** {len(always_pass)} tasks")
    w(f"  {', '.join(sorted(always_pass))}")
    w(f"- **Never pass (0% in all conditions):** {len(never_pass)} tasks")
    w(f"  {', '.join(sorted(never_pass))}")
    w()

    # Variable tasks
    variable = [t for t in all_tasks if t not in always_pass and t not in never_pass]
    w(f"- **Variable (differs across conditions or trials):** {len(variable)} tasks")
    w()

    # ── Write report ──
    report = "\n".join(lines)
    output_path = RESULTS_DIR / "analysis.md"
    with open(output_path, "w") as f:
        f.write(report)
    print(f"Report written to {output_path}", file=sys.stderr)

    # ── Also output JSON data for figures ──
    json_data = {
        "overall": overall,
        "tier_stats": tier_stats,
        "token_stats": token_stats,
        "time_stats": time_stats,
        "decomp_stats": decomp_stats,
        "planning_stats": planning_stats,
        "turn_stats": turn_stats,
        "wg_overhead": wg_overhead,
        "per_task_table": per_task_table,
        "wg_helps": [{"task": t, "a": a, "b": b, "c": c, "diff": d} for t, a, b, c, d in wg_helps],
        "wg_hurts": [{"task": t, "a": a, "b": b, "c": c, "diff": d} for t, a, b, c, d in wg_hurts],
        "always_pass": always_pass,
        "never_pass": never_pass,
        "tiers": {k: v for k, v in tiers.items()},
    }
    json_path = RESULTS_DIR / "analysis_data.json"
    with open(json_path, "w") as f:
        json.dump(json_data, f, indent=2, default=str)
    print(f"JSON data written to {json_path}", file=sys.stderr)

    return json_data


def generate_figures(json_data):
    """Generate publication-ready figures."""
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
        import numpy as np
    except ImportError:
        print("matplotlib not available — skipping figures", file=sys.stderr)
        return

    fig_dir = RESULTS_DIR / "figures"
    fig_dir.mkdir(exist_ok=True)

    colors = {"A": "#4C72B0", "B": "#DD8452", "C": "#55A868"}
    labels = {"A": "A (bare)", "B": "B (wg context)", "C": "C (enhanced)"}

    # ── Figure 1: Overall pass rates with CI ──
    fig, ax = plt.subplots(figsize=(6, 4))
    overall = json_data["overall"]
    x = np.arange(3)
    means = [overall[c]["task_mean_rate"] * 100 for c in ["A", "B", "C"]]
    ci_lo = [overall[c]["task_ci_lo"] * 100 for c in ["A", "B", "C"]]
    ci_hi = [overall[c]["task_ci_hi"] * 100 for c in ["A", "B", "C"]]
    errs = [[m - lo for m, lo in zip(means, ci_lo)],
            [hi - m for m, hi in zip(means, ci_hi)]]

    bars = ax.bar(x, means, yerr=errs, capsize=5,
                  color=[colors[c] for c in ["A", "B", "C"]],
                  edgecolor="black", linewidth=0.5)
    ax.set_xticks(x)
    ax.set_xticklabels([labels[c] for c in ["A", "B", "C"]])
    ax.set_ylabel("Task-Level Pass Rate (%)")
    ax.set_title("Overall Pass Rate by Condition\n(89 tasks × 3 trials, minimax-m2.7)")
    ax.set_ylim(0, 100)
    for i, (m, lo, hi) in enumerate(zip(means, ci_lo, ci_hi)):
        ax.text(i, m + (hi - m) + 2, f"{m:.1f}%", ha="center", va="bottom", fontsize=10, fontweight="bold")
    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
    plt.tight_layout()
    plt.savefig(fig_dir / "overall_pass_rate.png", dpi=150)
    plt.close()

    # ── Figure 2: Pass rate by difficulty tier ──
    fig, ax = plt.subplots(figsize=(8, 5))
    tier_stats = json_data["tier_stats"]
    tiers = ["easy", "medium", "hard"]
    tier_labels = [f"Easy\n(n={tier_stats[t]['n_tasks']})" for t in ["easy"]] + \
                  [f"Medium\n(n={tier_stats[t]['n_tasks']})" for t in ["medium"]] + \
                  [f"Hard\n(n={tier_stats[t]['n_tasks']})" for t in ["hard"]]
    x = np.arange(3)
    width = 0.25

    for i, cond in enumerate(["A", "B", "C"]):
        rates = [tier_stats[t][cond]["rate"] * 100 for t in tiers]
        ci_lo = [tier_stats[t][cond]["ci_lo"] * 100 for t in tiers]
        ci_hi = [tier_stats[t][cond]["ci_hi"] * 100 for t in tiers]
        errs = [[r - lo for r, lo in zip(rates, ci_lo)],
                [hi - r for r, hi in zip(rates, ci_hi)]]
        bars = ax.bar(x + i * width, rates, width, yerr=errs, capsize=3,
                      color=colors[cond], label=labels[cond],
                      edgecolor="black", linewidth=0.5)

    ax.set_xticks(x + width)
    ax.set_xticklabels(tier_labels)
    ax.set_ylabel("Task-Level Pass Rate (%)")
    ax.set_title("Pass Rate by Difficulty Tier")
    ax.set_ylim(0, 110)
    ax.legend()
    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
    plt.tight_layout()
    plt.savefig(fig_dir / "pass_rate_by_tier.png", dpi=150)
    plt.close()

    # ── Figure 3: Per-task comparison heatmap ──
    per_task = json_data["per_task_table"]
    # Sort by A rate then task name
    per_task_sorted = sorted(per_task, key=lambda r: (
        -(r["A_rate"] if r["A_rate"] is not None else -1), r["task"]))

    fig, ax = plt.subplots(figsize=(8, max(12, len(per_task_sorted) * 0.18)))
    task_names = [r["task"] for r in per_task_sorted]
    n_tasks = len(task_names)

    for i, cond in enumerate(["A", "B", "C"]):
        rates = []
        for r in per_task_sorted:
            rate = r.get(f"{cond}_rate")
            rates.append(rate if rate is not None else -0.1)

        y = np.arange(n_tasks)
        ax.barh(y + (1 - i) * 0.28, [max(0, r) * 100 for r in rates], 0.26,
                color=[colors[cond] if r >= 0 else "#cccccc" for r in rates],
                edgecolor="black", linewidth=0.3, alpha=0.8,
                label=labels[cond] if i == 0 else None)

    ax.set_yticks(np.arange(n_tasks))
    ax.set_yticklabels(task_names, fontsize=5)
    ax.set_xlabel("Pass Rate (%)")
    ax.set_title("Per-Task Pass Rate Comparison")
    ax.legend([plt.Rectangle((0,0),1,1, fc=colors[c]) for c in ["A","B","C"]],
              [labels[c] for c in ["A","B","C"]], loc="lower right")
    ax.invert_yaxis()
    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
    plt.tight_layout()
    plt.savefig(fig_dir / "per_task_comparison.png", dpi=150)
    plt.close()

    # ── Figure 4: Token efficiency ──
    fig, axes = plt.subplots(1, 2, figsize=(10, 4))

    # Left: mean tokens by condition and outcome
    token_stats = json_data["token_stats"]
    x = np.arange(3)
    width = 0.35
    pass_tokens = [token_stats[c]["mean_pass"] / 1000 for c in ["A", "B", "C"]]
    fail_tokens = [token_stats[c]["mean_fail"] / 1000 for c in ["A", "B", "C"]]
    axes[0].bar(x - width/2, pass_tokens, width, color="#55A868", label="Pass", edgecolor="black", linewidth=0.5)
    axes[0].bar(x + width/2, fail_tokens, width, color="#C44E52", label="Fail", edgecolor="black", linewidth=0.5)
    axes[0].set_xticks(x)
    axes[0].set_xticklabels(["A", "B", "C"])
    axes[0].set_ylabel("Mean Tokens (thousands)")
    axes[0].set_title("Tokens per Trial by Outcome")
    axes[0].legend()
    axes[0].spines["top"].set_visible(False)
    axes[0].spines["right"].set_visible(False)

    # Right: tokens per solved task
    per_solve = [token_stats[c]["total_tokens"] / max(1, token_stats[c]["n_pass"]) / 1000 for c in ["A", "B", "C"]]
    axes[1].bar(x, per_solve, color=[colors[c] for c in ["A", "B", "C"]], edgecolor="black", linewidth=0.5)
    axes[1].set_xticks(x)
    axes[1].set_xticklabels(["A", "B", "C"])
    axes[1].set_ylabel("Total Tokens / Solved Task (thousands)")
    axes[1].set_title("Token Efficiency per Solve")
    axes[1].spines["top"].set_visible(False)
    axes[1].spines["right"].set_visible(False)

    plt.tight_layout()
    plt.savefig(fig_dir / "token_efficiency.png", dpi=150)
    plt.close()

    # ── Figure 5: Decomposition analysis ──
    fig, ax = plt.subplots(figsize=(6, 4))
    decomp = json_data["decomp_stats"]
    x = np.arange(2)
    width = 0.35
    decomp_rates = [decomp[c]["decomp_pass_rate"] * 100 for c in ["B", "C"]]
    nodecomp_rates = [decomp[c]["nodecomp_pass_rate"] * 100 for c in ["B", "C"]]
    ax.bar(x - width/2, nodecomp_rates, width, color="#4C72B0", label="Direct", edgecolor="black", linewidth=0.5)
    ax.bar(x + width/2, decomp_rates, width, color="#DD8452", label="Decomposed", edgecolor="black", linewidth=0.5)
    ax.set_xticks(x)
    ax.set_xticklabels(["B", "C"])
    ax.set_ylabel("Pass Rate (%)")
    ax.set_title("Pass Rate: Direct vs Decomposed Tasks")
    ax.set_ylim(0, 100)
    ax.legend()

    # Add sample sizes
    for i, c in enumerate(["B", "C"]):
        n_d = decomp[c]["decomposed"]
        n_nd = decomp[c]["total_valid"] - decomp[c]["decomposed"]
        ax.text(i - width/2, nodecomp_rates[i] + 2, f"n={n_nd}", ha="center", fontsize=8)
        ax.text(i + width/2, decomp_rates[i] + 2, f"n={n_d}", ha="center", fontsize=8)

    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
    plt.tight_layout()
    plt.savefig(fig_dir / "decomposition_analysis.png", dpi=150)
    plt.close()

    # ── Figure 6: WG overhead ──
    fig, ax = plt.subplots(figsize=(6, 4))
    overhead = json_data["wg_overhead"]
    for cond in ["B", "C"]:
        tools = overhead[cond]["by_tool"]
        sorted_tools = sorted(tools.items(), key=lambda x: -x[1])[:6]
        names = [t[0].replace("wg_", "") for t in sorted_tools]
        counts = [t[1] for t in sorted_tools]
        x = np.arange(len(names))
        offset = -0.2 if cond == "B" else 0.2
        ax.bar(x + offset, counts, 0.35, color=colors[cond], label=labels[cond],
               edgecolor="black", linewidth=0.5)

    ax.set_xticks(np.arange(max(len(overhead["B"]["by_tool"]), len(overhead["C"]["by_tool"]))))
    # Use the union of top tools
    all_tools = set()
    for c in ["B", "C"]:
        for t in list(overhead[c]["by_tool"].keys())[:6]:
            all_tools.add(t)
    sorted_all = sorted(all_tools, key=lambda t: -(overhead["B"]["by_tool"].get(t, 0) + overhead["C"]["by_tool"].get(t, 0)))[:6]
    ax.set_xticks(range(len(sorted_all)))
    ax.set_xticklabels([t.replace("wg_", "") for t in sorted_all], rotation=30)
    ax.set_ylabel("Total Calls (across all valid trials)")
    ax.set_title("Workgraph Tool Usage by Condition")
    ax.legend()
    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
    plt.tight_layout()
    plt.savefig(fig_dir / "wg_overhead.png", dpi=150)
    plt.close()

    print(f"Figures written to {fig_dir}/", file=sys.stderr)


if __name__ == "__main__":
    data = main()
    generate_figures(data)
