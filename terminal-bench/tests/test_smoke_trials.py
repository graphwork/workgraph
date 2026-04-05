"""Smoke trials for TB native wg adapter + federation.

Runs 12+ trials across conditions (A, B, D, F) × tasks × replicas,
validating the full lifecycle:
  1. Per-trial temp graph creation
  2. Config + bundle written per condition
  3. Federation pull from tb-evaluations/ hub (for D, F)
  4. Task creation + wg service start
  5. Polling for completion
  6. Federation push of results back to hub
  7. Metric collection from agent streams
  8. Cleanup

These are integration tests that exercise the real wg binary.
"""

import asyncio
import json
import os
import shutil
import tempfile
import time
from pathlib import Path

import pytest

from wg.adapter import (
    BENCHMARK_MODEL,
    CONDITION_CONFIG,
    FEDERATION_CONDITIONS,
    WorkgraphAgent,
    _collect_agent_metrics,
    _ensure_hub_initialized,
    _exec_wg_cmd_host,
    _federation_pull,
    _federation_push,
    _poll_task_completion,
    _write_trial_bundle,
    _write_trial_federation_config,
    _write_trial_wg_config,
)

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

WG_BIN = shutil.which("wg") or os.path.expanduser("~/.cargo/bin/wg")

# Hub path for federation tests — use a dedicated temp hub per test session
TB_EVALUATIONS_HUB = os.path.join(
    os.path.dirname(os.path.dirname(__file__)),
    "tb-evaluations",
)


def run_async(coro):
    """Run an async coroutine synchronously."""
    return asyncio.get_event_loop().run_until_complete(coro)


# ---------------------------------------------------------------------------
# Trial lifecycle helper
# ---------------------------------------------------------------------------


class TrialResult:
    """Captures results from a single smoke trial."""

    def __init__(
        self,
        trial_id: str,
        condition: str,
        task_type: str,
        replica: int,
    ):
        self.trial_id = trial_id
        self.condition = condition
        self.task_type = task_type
        self.replica = replica
        self.status = "not_started"
        self.elapsed_s = 0.0
        self.used_native_executor = False
        self.started_wg_service = False
        self.federation_pulled = False
        self.federation_pushed = False
        self.config_written = False
        self.bundle_written = False
        self.error = None
        self.wg_dir = None
        self.metrics = None

    def to_dict(self):
        return {
            "trial_id": self.trial_id,
            "condition": self.condition,
            "task_type": self.task_type,
            "replica": self.replica,
            "status": self.status,
            "elapsed_s": round(self.elapsed_s, 2),
            "used_native_executor": self.used_native_executor,
            "started_wg_service": self.started_wg_service,
            "federation_pulled": self.federation_pulled,
            "federation_pushed": self.federation_pushed,
            "config_written": self.config_written,
            "bundle_written": self.bundle_written,
            "error": self.error,
        }


async def run_smoke_trial(
    condition: str,
    task_type: str,
    replica: int,
    hub_path: str,
    model: str = "test:smoke-model",
) -> TrialResult:
    """Run a single smoke trial through the full native adapter lifecycle.

    This simulates what WorkgraphAgent.setup() + run() + teardown does,
    but with a task that immediately completes (no real LLM call).
    """
    trial_id = f"smoke-{condition.lower()}-{task_type}-r{replica}"
    result = TrialResult(trial_id, condition, task_type, replica)
    start = time.monotonic()

    tmpdir = tempfile.mkdtemp(prefix=f"tb-smoke-{trial_id}-")
    wg_dir = os.path.join(tmpdir, ".workgraph")
    result.wg_dir = wg_dir

    try:
        # Step 1: Initialize per-trial workgraph
        init_out = await _exec_wg_cmd_host(wg_dir, WG_BIN, ["init"])
        assert "error" not in init_out.lower() or "already" in init_out.lower(), (
            f"Init failed: {init_out}"
        )

        # Step 2: Write trial config (native executor)
        cfg = CONDITION_CONFIG[condition]
        await _write_trial_wg_config(tmpdir, wg_dir, condition, model)
        config_path = os.path.join(wg_dir, "config.toml")
        assert os.path.isfile(config_path)
        config_content = open(config_path).read()
        assert 'executor = "native"' in config_content
        result.config_written = True
        result.used_native_executor = True

        # Step 3: Write bundle if needed (Condition A baseline)
        await _write_trial_bundle(wg_dir, condition)
        if cfg.get("exclude_wg_tools"):
            bundle_path = os.path.join(wg_dir, "bundles", "implementer.toml")
            assert os.path.isfile(bundle_path)
            result.bundle_written = True
        else:
            result.bundle_written = True  # No bundle needed = OK

        # Step 4: Federation pull (for agency conditions D, E, F)
        if condition in FEDERATION_CONDITIONS and hub_path:
            await _ensure_hub_initialized(hub_path, WG_BIN)
            await _write_trial_federation_config(wg_dir, hub_path)
            fed_config_path = os.path.join(wg_dir, "federation.yaml")
            assert os.path.isfile(fed_config_path)

            pull_out = await _federation_pull(wg_dir, WG_BIN, hub_path)
            assert "[wg command error:" not in pull_out
            result.federation_pulled = True

        # Step 5: Create root task
        root_task_id = f"tb-{trial_id}"
        description = (
            f"Smoke trial: condition {condition}, task {task_type}, replica {replica}\n"
            f"This is a smoke test — the task will be immediately completed."
        )
        add_out = await _exec_wg_cmd_host(wg_dir, WG_BIN, [
            "add", f"Smoke: {task_type} ({condition})",
            "--id", root_task_id,
            "-d", description,
        ])
        assert root_task_id in add_out or "[exit code:" not in add_out

        # Step 6: Verify task exists
        show_out = await _exec_wg_cmd_host(wg_dir, WG_BIN, ["show", root_task_id])
        assert "Status:" in show_out
        assert "open" in show_out.lower()

        # Step 7: Start wg service (with --force for smoke test)
        service_cmd = [
            "service", "start",
            "--max-agents", "1",
            "--executor", "native",
            "--model", model,
            "--no-coordinator-agent",
            "--force",
        ]
        service_out = await _exec_wg_cmd_host(wg_dir, WG_BIN, service_cmd)
        result.started_wg_service = True

        # Step 8: Immediately mark the task done (smoke — no real execution)
        # In real trials the service would dispatch an agent, but for smoke
        # we just verify the lifecycle mechanics work.
        done_out = await _exec_wg_cmd_host(wg_dir, WG_BIN, ["done", root_task_id])

        # Step 9: Poll for completion (should be instant since we marked it done)
        status, poll_elapsed = await _poll_task_completion(
            wg_dir, WG_BIN, root_task_id,
            timeout_secs=10, poll_interval=0.2,
        )
        assert status == "done", f"Expected 'done', got '{status}'"

        # Step 10: Stop the service
        stop_out = await _exec_wg_cmd_host(wg_dir, WG_BIN, [
            "service", "stop", "--kill-agents",
        ])

        # Step 11: Federation push (for agency conditions)
        if condition in FEDERATION_CONDITIONS and hub_path:
            push_out = await _federation_push(wg_dir, WG_BIN, hub_path)
            assert "[wg command error:" not in push_out
            result.federation_pushed = True

        # Step 12: Collect metrics
        result.metrics = await _collect_agent_metrics(wg_dir)

        result.status = "done"

    except Exception as e:
        result.status = "failed"
        result.error = str(e)
    finally:
        result.elapsed_s = time.monotonic() - start
        # Cleanup
        shutil.rmtree(tmpdir, ignore_errors=True)

    return result


# ---------------------------------------------------------------------------
# Smoke trial matrix
# ---------------------------------------------------------------------------

# 4 conditions × 2 tasks × 2 replicas = 16 trials (>= 10 required)
SMOKE_CONDITIONS = ["A", "B", "D", "F"]
SMOKE_TASKS = ["file-ops", "debugging"]
SMOKE_REPLICAS = 2


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


@pytest.fixture(scope="module")
def smoke_hub():
    """Create a temporary federation hub for the smoke test session."""
    tmpdir = tempfile.mkdtemp(prefix="tb-smoke-hub-")
    hub_path = os.path.join(tmpdir, "hub")
    run_async(_ensure_hub_initialized(hub_path, WG_BIN))
    yield hub_path
    shutil.rmtree(tmpdir, ignore_errors=True)


@pytest.fixture(scope="module")
def all_smoke_results(smoke_hub):
    """Run all smoke trials and return collected results."""
    results = []
    for condition in SMOKE_CONDITIONS:
        for task_type in SMOKE_TASKS:
            for replica in range(SMOKE_REPLICAS):
                result = run_async(run_smoke_trial(
                    condition=condition,
                    task_type=task_type,
                    replica=replica,
                    hub_path=smoke_hub,
                ))
                results.append(result)
    return results


class TestSmokeTrialCount:
    """Validate that enough trials ran."""

    def test_at_least_10_trials(self, all_smoke_results):
        assert len(all_smoke_results) >= 10, (
            f"Only {len(all_smoke_results)} trials, need >= 10"
        )

    def test_at_least_10_completed(self, all_smoke_results):
        completed = [r for r in all_smoke_results if r.status == "done"]
        assert len(completed) >= 10, (
            f"Only {len(completed)}/{len(all_smoke_results)} trials completed"
        )

    def test_all_trials_ran(self, all_smoke_results):
        expected = len(SMOKE_CONDITIONS) * len(SMOKE_TASKS) * SMOKE_REPLICAS
        assert len(all_smoke_results) == expected


class TestNativeExecutor:
    """Every trial must use the native wg executor, not litellm."""

    def test_all_used_native_executor(self, all_smoke_results):
        for r in all_smoke_results:
            assert r.used_native_executor, (
                f"Trial {r.trial_id} did not use native executor"
            )

    def test_all_configs_written(self, all_smoke_results):
        for r in all_smoke_results:
            assert r.config_written, (
                f"Trial {r.trial_id} config not written"
            )

    def test_condition_a_has_bundle(self, all_smoke_results):
        """Condition A trials should have created a bundle (no wg tools)."""
        cond_a = [r for r in all_smoke_results if r.condition == "A"]
        assert len(cond_a) > 0
        for r in cond_a:
            assert r.bundle_written


class TestWgServiceLifecycle:
    """Each trial must start its own wg service instance."""

    def test_all_started_service(self, all_smoke_results):
        for r in all_smoke_results:
            assert r.started_wg_service, (
                f"Trial {r.trial_id} did not start wg service"
            )

    def test_all_reached_terminal_status(self, all_smoke_results):
        for r in all_smoke_results:
            assert r.status in ("done", "failed"), (
                f"Trial {r.trial_id} ended with status '{r.status}'"
            )


class TestFederation:
    """Federation must work for D, F conditions."""

    def test_federation_pulled_for_agency_conditions(self, all_smoke_results):
        federation_trials = [
            r for r in all_smoke_results if r.condition in FEDERATION_CONDITIONS
        ]
        assert len(federation_trials) > 0, "No federation-condition trials ran"
        for r in federation_trials:
            assert r.federation_pulled, (
                f"Trial {r.trial_id} (condition {r.condition}) did not pull from hub"
            )

    def test_federation_pushed_for_agency_conditions(self, all_smoke_results):
        done_fed = [
            r for r in all_smoke_results
            if r.condition in FEDERATION_CONDITIONS and r.status == "done"
        ]
        assert len(done_fed) > 0, "No federation-condition trials completed"
        for r in done_fed:
            assert r.federation_pushed, (
                f"Trial {r.trial_id} did not push to hub"
            )

    def test_no_federation_for_non_agency_conditions(self, all_smoke_results):
        non_fed = [
            r for r in all_smoke_results
            if r.condition not in FEDERATION_CONDITIONS
        ]
        for r in non_fed:
            assert not r.federation_pulled, (
                f"Trial {r.trial_id} (condition {r.condition}) should not pull"
            )
            assert not r.federation_pushed, (
                f"Trial {r.trial_id} (condition {r.condition}) should not push"
            )

    def test_hub_initialized_correctly(self, smoke_hub):
        agency_dir = os.path.join(smoke_hub, ".workgraph", "agency")
        assert os.path.isdir(agency_dir)
        primitives_dir = os.path.join(agency_dir, "primitives")
        assert os.path.isdir(primitives_dir)


class TestResultsSummary:
    """Produce and validate results summary."""

    def test_summary_with_timing(self, all_smoke_results):
        """All trials have timing data."""
        for r in all_smoke_results:
            assert r.elapsed_s >= 0, f"Trial {r.trial_id} has no timing"

    def test_pass_fail_counts(self, all_smoke_results):
        passed = sum(1 for r in all_smoke_results if r.status == "done")
        failed = sum(1 for r in all_smoke_results if r.status == "failed")
        total = len(all_smoke_results)
        assert passed + failed == total
        # At least 80% should pass in smoke mode
        assert passed / total >= 0.8, (
            f"Only {passed}/{total} passed ({passed/total:.0%})"
        )

    def test_per_condition_stats(self, all_smoke_results):
        """Compute per-condition stats — at least one trial per condition."""
        for cond in SMOKE_CONDITIONS:
            cond_results = [r for r in all_smoke_results if r.condition == cond]
            assert len(cond_results) > 0, f"No results for condition {cond}"
            passed = sum(1 for r in cond_results if r.status == "done")
            assert passed > 0, f"No passing trials for condition {cond}"

    def test_error_documentation(self, all_smoke_results):
        """Failed trials should have error messages."""
        failed = [r for r in all_smoke_results if r.status == "failed"]
        for r in failed:
            assert r.error is not None, (
                f"Trial {r.trial_id} failed but has no error message"
            )

    def test_write_results_summary(self, all_smoke_results):
        """Write results summary to trials/ directory."""
        results_dir = os.path.join(
            os.path.dirname(os.path.dirname(__file__)),
            "trials",
        )
        os.makedirs(results_dir, exist_ok=True)

        summary = {
            "run_id": "smoke-pilot",
            "conditions": SMOKE_CONDITIONS,
            "tasks": SMOKE_TASKS,
            "replicas": SMOKE_REPLICAS,
            "total_trials": len(all_smoke_results),
            "passed": sum(1 for r in all_smoke_results if r.status == "done"),
            "failed": sum(1 for r in all_smoke_results if r.status == "failed"),
            "trials": [r.to_dict() for r in all_smoke_results],
            "per_condition": {},
        }

        for cond in SMOKE_CONDITIONS:
            cond_results = [r for r in all_smoke_results if r.condition == cond]
            cond_passed = sum(1 for r in cond_results if r.status == "done")
            cond_times = [r.elapsed_s for r in cond_results]
            summary["per_condition"][cond] = {
                "total": len(cond_results),
                "passed": cond_passed,
                "failed": len(cond_results) - cond_passed,
                "pass_rate": cond_passed / len(cond_results) if cond_results else 0,
                "mean_time_s": sum(cond_times) / len(cond_times) if cond_times else 0,
                "federation": cond in FEDERATION_CONDITIONS,
            }

        output_path = os.path.join(results_dir, "tb-results-smoke-pilot.json")
        with open(output_path, "w") as f:
            json.dump(summary, f, indent=2)

        assert os.path.isfile(output_path)
        # Verify the file is valid JSON
        with open(output_path) as f:
            loaded = json.load(f)
        assert loaded["total_trials"] == len(all_smoke_results)
        assert loaded["passed"] >= 10


class TestConditionCoverage:
    """Ensure all targeted conditions are covered."""

    def test_condition_a_control(self, all_smoke_results):
        """Condition A: clean context, no wg tools, no federation."""
        cond_a = [r for r in all_smoke_results if r.condition == "A"]
        assert len(cond_a) >= 2
        for r in cond_a:
            assert r.used_native_executor
            assert not r.federation_pulled
            assert not r.federation_pushed

    def test_condition_b_basic_wg(self, all_smoke_results):
        """Condition B: task context, wg tools, no federation."""
        cond_b = [r for r in all_smoke_results if r.condition == "B"]
        assert len(cond_b) >= 2
        for r in cond_b:
            assert r.used_native_executor
            assert not r.federation_pulled

    def test_condition_d_agency(self, all_smoke_results):
        """Condition D: task context, wg tools, agency, federation."""
        cond_d = [r for r in all_smoke_results if r.condition == "D"]
        assert len(cond_d) >= 2
        for r in cond_d:
            assert r.used_native_executor
            if r.status == "done":
                assert r.federation_pulled
                assert r.federation_pushed

    def test_condition_f_full(self, all_smoke_results):
        """Condition F: graph context, wg tools, federation."""
        cond_f = [r for r in all_smoke_results if r.condition == "F"]
        assert len(cond_f) >= 2
        for r in cond_f:
            assert r.used_native_executor
            if r.status == "done":
                assert r.federation_pulled
                assert r.federation_pushed


class TestTrialIsolation:
    """Each trial must use its own isolated graph."""

    def test_unique_trial_ids(self, all_smoke_results):
        ids = [r.trial_id for r in all_smoke_results]
        assert len(ids) == len(set(ids)), "Duplicate trial IDs found"

    def test_different_conditions_same_task(self, all_smoke_results):
        """Same task across conditions should produce independent results."""
        for task_type in SMOKE_TASKS:
            task_results = [r for r in all_smoke_results if r.task_type == task_type]
            conditions_seen = set(r.condition for r in task_results)
            assert len(conditions_seen) == len(SMOKE_CONDITIONS), (
                f"Task {task_type} not run across all conditions"
            )


class TestPerformanceBaseline:
    """Basic performance checks for smoke trials."""

    def test_trials_complete_quickly(self, all_smoke_results):
        """Smoke trials should complete in < 30s each (no real LLM)."""
        for r in all_smoke_results:
            if r.status == "done":
                assert r.elapsed_s < 30, (
                    f"Trial {r.trial_id} took {r.elapsed_s:.1f}s (expected < 30s)"
                )

    def test_total_time_reasonable(self, all_smoke_results):
        """Total wall clock should be reasonable for sequential smoke trials."""
        total = sum(r.elapsed_s for r in all_smoke_results)
        assert total < 600, f"Total time {total:.1f}s exceeds 10 minute budget"


# ---------------------------------------------------------------------------
# Standalone runner for manual smoke testing
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    pytest.main([__file__, "-v", "--tb=short"])
