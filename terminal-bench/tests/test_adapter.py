"""Tests for the Terminal Bench native wg executor adapter.

Covers:
  - CONDITION_CONFIG validity
  - _exec_wg_cmd_host real CLI integration
  - _write_trial_wg_config config file generation
  - _write_trial_bundle bundle file generation (Condition A filtering)
  - _collect_agent_metrics stream.jsonl parsing
  - _poll_task_completion status detection
  - WorkgraphAgent initialization and condition aliases
  - End-to-end wg lifecycle: init → add → show → verify gates → cleanup
"""

import asyncio
import json
import os
import shutil
import tempfile
from pathlib import Path

import pytest

from wg.adapter import (
    BENCHMARK_MODEL,
    CONDITION_CONFIG,
    FEDERATION_CONDITIONS,
    ConditionAAgent,
    ConditionBAgent,
    ConditionCAgent,
    ConditionDAgent,
    ConditionEAgent,
    ConditionFAgent,
    WorkgraphAgent,
    _collect_agent_metrics,
    _ensure_hub_initialized,
    _exec_wg_cmd_host,
    _federation_pull,
    _federation_push,
    _normalize_model,
    _poll_task_completion,
    _write_trial_bundle,
    _write_trial_federation_config,
    _write_trial_wg_config,
)

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

WG_BIN = shutil.which("wg") or os.path.expanduser("~/.cargo/bin/wg")


def run_async(coro):
    """Run an async coroutine synchronously."""
    return asyncio.get_event_loop().run_until_complete(coro)


@pytest.fixture
def wg_trial_dir():
    """Create a temp dir with an initialized .workgraph for integration tests."""
    tmpdir = tempfile.mkdtemp(prefix="tb-test-")
    wg_dir = os.path.join(tmpdir, ".workgraph")
    # Initialize wg in the temp dir
    result = run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["init"]))
    assert "Initialized" in result or "workgraph" in result.lower(), (
        f"wg init failed: {result}"
    )
    yield tmpdir, wg_dir
    shutil.rmtree(tmpdir, ignore_errors=True)


# ---------------------------------------------------------------------------
# CONDITION_CONFIG tests
# ---------------------------------------------------------------------------


class TestConditionConfig:
    """Validate CONDITION_CONFIG structure and completeness."""

    def test_all_conditions_present(self):
        assert set(CONDITION_CONFIG.keys()) >= {"A", "B", "C", "D", "E", "F"}
        assert "G" in CONDITION_CONFIG
        assert "G-smart" in CONDITION_CONFIG

    def test_required_keys(self):
        required = {"exec_mode", "context_scope", "agency", "exclude_wg_tools", "max_agents"}
        for cond, cfg in CONDITION_CONFIG.items():
            assert set(cfg.keys()) >= required, f"Condition {cond} missing keys"

    def test_condition_a_is_control(self):
        """Condition A must exclude wg tools and use clean context."""
        cfg = CONDITION_CONFIG["A"]
        assert cfg["exclude_wg_tools"] is True
        assert cfg["context_scope"] == "clean"
        assert cfg["agency"] is None

    def test_condition_b_has_wg_tools(self):
        """Condition B is full tools, task context."""
        cfg = CONDITION_CONFIG["B"]
        assert cfg["exclude_wg_tools"] is False
        assert cfg["context_scope"] == "task"

    def test_agency_conditions(self):
        """Conditions D and E have agency identities; A/B/C/F do not."""
        for cond in ("A", "B", "C", "F"):
            assert CONDITION_CONFIG[cond]["agency"] is None, f"Condition {cond} should not have agency"
        for cond in ("D", "E"):
            agency = CONDITION_CONFIG[cond]["agency"]
            assert agency is not None, f"Condition {cond} should have agency"
            assert len(agency) == 2, "Agency should be (role, tradeoff) tuple"

    def test_max_agents_positive(self):
        for cond, cfg in CONDITION_CONFIG.items():
            assert cfg["max_agents"] >= 1, f"Condition {cond} max_agents must be >= 1"

    def test_all_use_full_exec_mode(self):
        """All conditions currently use full exec_mode."""
        for cond, cfg in CONDITION_CONFIG.items():
            assert cfg["exec_mode"] == "full"


# ---------------------------------------------------------------------------
# _exec_wg_cmd_host tests
# ---------------------------------------------------------------------------


class TestExecWgCmdHost:
    """Test the host-side wg CLI helper."""

    def test_successful_command(self, wg_trial_dir):
        _, wg_dir = wg_trial_dir
        result = run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["list"]))
        # Should not contain error indicators
        assert "[wg command error:" not in result
        assert "[wg command timed out" not in result

    def test_invalid_subcommand_returns_exit_code(self, wg_trial_dir):
        _, wg_dir = wg_trial_dir
        result = run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["nonexistent-cmd"]))
        assert "[exit code:" in result or "error" in result.lower()

    def test_returns_stdout(self, wg_trial_dir):
        tmpdir, wg_dir = wg_trial_dir
        # Add a task and then list
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
            "add", "CLI test task", "--id", "cli-test-1"
        ]))
        result = run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["list"]))
        assert "cli-test-1" in result

    def test_nonexistent_binary(self):
        """Non-existent binary returns error message."""
        result = run_async(_exec_wg_cmd_host("/tmp", "/nonexistent/wg", ["list"]))
        assert "[wg command error:" in result


# ---------------------------------------------------------------------------
# _write_trial_wg_config tests
# ---------------------------------------------------------------------------


class TestWriteTrialWgConfig:
    """Test config.toml generation per condition."""

    def test_config_a_written(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            wg_dir = os.path.join(tmpdir, ".workgraph")
            os.makedirs(wg_dir)
            run_async(_write_trial_wg_config(tmpdir, wg_dir, "A", "test-model"))
            config_path = os.path.join(wg_dir, "config.toml")
            assert os.path.isfile(config_path)
            content = open(config_path).read()
            assert 'executor = "native"' in content
            assert 'model = "test-model"' in content
            assert 'context_scope = "clean"' in content
            assert "max_agents = 1" in content

    def test_config_b_task_context(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            wg_dir = os.path.join(tmpdir, ".workgraph")
            os.makedirs(wg_dir)
            run_async(_write_trial_wg_config(tmpdir, wg_dir, "B", "some-model"))
            content = open(os.path.join(wg_dir, "config.toml")).read()
            assert 'context_scope = "task"' in content

    def test_config_e_graph_context(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            wg_dir = os.path.join(tmpdir, ".workgraph")
            os.makedirs(wg_dir)
            run_async(_write_trial_wg_config(tmpdir, wg_dir, "E", "m"))
            content = open(os.path.join(wg_dir, "config.toml")).read()
            assert 'context_scope = "graph"' in content

    def test_all_conditions_produce_valid_toml(self):
        """Every condition should produce a parseable config."""
        for cond in CONDITION_CONFIG:
            with tempfile.TemporaryDirectory() as tmpdir:
                wg_dir = os.path.join(tmpdir, ".workgraph")
                os.makedirs(wg_dir)
                run_async(_write_trial_wg_config(tmpdir, wg_dir, cond, "model"))
                content = open(os.path.join(wg_dir, "config.toml")).read()
                assert "[coordinator]" in content
                assert "[agent]" in content


# ---------------------------------------------------------------------------
# _write_trial_bundle tests
# ---------------------------------------------------------------------------


class TestWriteTrialBundle:
    """Test custom bundle generation."""

    def test_condition_a_creates_bundle(self):
        """Condition A should create an implementer bundle without wg tools."""
        with tempfile.TemporaryDirectory() as tmpdir:
            wg_dir = os.path.join(tmpdir, ".workgraph")
            os.makedirs(wg_dir)
            run_async(_write_trial_bundle(wg_dir, "A"))
            bundle_path = os.path.join(wg_dir, "bundles", "implementer.toml")
            assert os.path.isfile(bundle_path)
            content = open(bundle_path).read()
            assert "bash" in content
            assert "read_file" in content
            # Should NOT include wg tools
            assert "wg_" not in content

    def test_condition_b_no_bundle(self):
        """Condition B should not create a custom bundle."""
        with tempfile.TemporaryDirectory() as tmpdir:
            wg_dir = os.path.join(tmpdir, ".workgraph")
            os.makedirs(wg_dir)
            run_async(_write_trial_bundle(wg_dir, "B"))
            bundles_dir = os.path.join(wg_dir, "bundles")
            assert not os.path.exists(bundles_dir)

    def test_no_bundle_for_treatment_conditions(self):
        """Conditions C–F should not create custom bundles."""
        for cond in ("C", "D", "E", "F"):
            with tempfile.TemporaryDirectory() as tmpdir:
                wg_dir = os.path.join(tmpdir, ".workgraph")
                os.makedirs(wg_dir)
                run_async(_write_trial_bundle(wg_dir, cond))
                bundles_dir = os.path.join(wg_dir, "bundles")
                assert not os.path.exists(bundles_dir), (
                    f"Condition {cond} should not create bundles dir"
                )


# ---------------------------------------------------------------------------
# _collect_agent_metrics tests
# ---------------------------------------------------------------------------


class TestCollectAgentMetrics:
    """Test stream.jsonl metric collection."""

    def test_empty_agents_dir(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            metrics = run_async(_collect_agent_metrics(tmpdir))
            assert metrics["total_input_tokens"] == 0
            assert metrics["total_output_tokens"] == 0
            assert metrics["total_cost_usd"] == 0.0
            assert metrics["total_turns"] == 0
            assert metrics["tool_calls"] == []

    def test_nonexistent_dir(self):
        metrics = run_async(_collect_agent_metrics("/nonexistent/dir"))
        assert metrics["total_input_tokens"] == 0

    def test_single_agent_stream(self):
        """Parse a stream.jsonl with turn and result events."""
        with tempfile.TemporaryDirectory() as tmpdir:
            agents_dir = os.path.join(tmpdir, "agents")
            agent_dir = os.path.join(agents_dir, "agent-001")
            os.makedirs(agent_dir)

            events = [
                {
                    "type": "turn",
                    "usage": {"input_tokens": 100, "output_tokens": 50},
                    "tools_used": ["bash", "read_file"],
                },
                {
                    "type": "turn",
                    "usage": {"input_tokens": 200, "output_tokens": 80},
                    "tools_used": ["edit_file"],
                },
                {
                    "type": "result",
                    "usage": {"cost_usd": 0.005},
                },
            ]
            stream_path = os.path.join(agent_dir, "stream.jsonl")
            with open(stream_path, "w") as f:
                for ev in events:
                    f.write(json.dumps(ev) + "\n")

            metrics = run_async(_collect_agent_metrics(tmpdir))
            assert metrics["total_input_tokens"] == 300
            assert metrics["total_output_tokens"] == 130
            assert metrics["total_cost_usd"] == pytest.approx(0.005)
            assert metrics["total_turns"] == 2
            assert metrics["tool_calls"] == ["bash", "read_file", "edit_file"]

    def test_multiple_agents(self):
        """Metrics from multiple agents should aggregate."""
        with tempfile.TemporaryDirectory() as tmpdir:
            agents_dir = os.path.join(tmpdir, "agents")
            for agent_id in ("agent-a", "agent-b"):
                agent_dir = os.path.join(agents_dir, agent_id)
                os.makedirs(agent_dir)
                events = [
                    {
                        "type": "turn",
                        "usage": {"input_tokens": 50, "output_tokens": 25},
                        "tools_used": ["bash"],
                    },
                    {
                        "type": "result",
                        "usage": {"cost_usd": 0.001},
                    },
                ]
                with open(os.path.join(agent_dir, "stream.jsonl"), "w") as f:
                    for ev in events:
                        f.write(json.dumps(ev) + "\n")

            metrics = run_async(_collect_agent_metrics(tmpdir))
            assert metrics["total_input_tokens"] == 100  # 50 * 2
            assert metrics["total_output_tokens"] == 50   # 25 * 2
            assert metrics["total_cost_usd"] == pytest.approx(0.002)
            assert metrics["total_turns"] == 2

    def test_malformed_jsonl_skipped(self):
        """Malformed lines in stream.jsonl should be skipped, not crash."""
        with tempfile.TemporaryDirectory() as tmpdir:
            agents_dir = os.path.join(tmpdir, "agents")
            agent_dir = os.path.join(agents_dir, "agent-bad")
            os.makedirs(agent_dir)
            with open(os.path.join(agent_dir, "stream.jsonl"), "w") as f:
                f.write("not valid json\n")
                f.write(json.dumps({"type": "turn", "usage": {"input_tokens": 10, "output_tokens": 5}, "tools_used": []}) + "\n")
                f.write("{broken\n")

            metrics = run_async(_collect_agent_metrics(tmpdir))
            assert metrics["total_input_tokens"] == 10
            assert metrics["total_turns"] == 1

    def test_turn_without_usage(self):
        """Turn events without usage should still count as turns."""
        with tempfile.TemporaryDirectory() as tmpdir:
            agents_dir = os.path.join(tmpdir, "agents")
            agent_dir = os.path.join(agents_dir, "agent-x")
            os.makedirs(agent_dir)
            events = [
                {"type": "turn", "tools_used": ["bash"]},
            ]
            with open(os.path.join(agent_dir, "stream.jsonl"), "w") as f:
                for ev in events:
                    f.write(json.dumps(ev) + "\n")

            metrics = run_async(_collect_agent_metrics(tmpdir))
            assert metrics["total_turns"] == 1
            assert metrics["total_input_tokens"] == 0

    def test_result_without_cost(self):
        """Result events without cost_usd should not crash."""
        with tempfile.TemporaryDirectory() as tmpdir:
            agents_dir = os.path.join(tmpdir, "agents")
            agent_dir = os.path.join(agents_dir, "agent-y")
            os.makedirs(agent_dir)
            events = [
                {"type": "result", "usage": {}},
                {"type": "result"},
            ]
            with open(os.path.join(agent_dir, "stream.jsonl"), "w") as f:
                for ev in events:
                    f.write(json.dumps(ev) + "\n")

            metrics = run_async(_collect_agent_metrics(tmpdir))
            assert metrics["total_cost_usd"] == 0.0


# ---------------------------------------------------------------------------
# _poll_task_completion tests
# ---------------------------------------------------------------------------


class TestPollTaskCompletion:
    """Test polling for task terminal status."""

    def test_detects_done_status(self, wg_trial_dir):
        _, wg_dir = wg_trial_dir
        # Create a task
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
            "add", "Poll test", "--id", "poll-test-1",
        ]))
        # Mark it done manually
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["done", "poll-test-1"]))

        status, elapsed = run_async(_poll_task_completion(
            wg_dir, WG_BIN, "poll-test-1",
            timeout_secs=10, poll_interval=0.1,
        ))
        assert status == "done"
        assert elapsed < 10

    def test_detects_failed_status(self, wg_trial_dir):
        _, wg_dir = wg_trial_dir
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
            "add", "Fail test", "--id", "poll-fail-1",
        ]))
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
            "fail", "poll-fail-1", "--reason", "intentional test failure",
        ]))

        status, elapsed = run_async(_poll_task_completion(
            wg_dir, WG_BIN, "poll-fail-1",
            timeout_secs=10, poll_interval=0.1,
        ))
        assert status == "failed"

    def test_timeout_for_open_task(self, wg_trial_dir):
        _, wg_dir = wg_trial_dir
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
            "add", "Never finishes", "--id", "poll-timeout-1",
        ]))

        status, elapsed = run_async(_poll_task_completion(
            wg_dir, WG_BIN, "poll-timeout-1",
            timeout_secs=1, poll_interval=0.2,
        ))
        assert status == "timeout"
        assert elapsed >= 1.0


# ---------------------------------------------------------------------------
# WorkgraphAgent init tests
# ---------------------------------------------------------------------------


class TestWorkgraphAgentInit:
    """Test WorkgraphAgent initialization."""

    def test_default_condition_is_b(self):
        agent = WorkgraphAgent(condition="B")
        assert agent.condition == "B"

    def test_condition_normalization(self):
        agent = WorkgraphAgent(condition="a")
        assert agent.condition == "A"

    def test_name_is_workgraph(self):
        assert WorkgraphAgent.name() == "workgraph"

    def test_version_present(self):
        agent = WorkgraphAgent()
        assert agent.version() is not None
        assert "." in agent.version()

    def test_find_wg_binary(self):
        agent = WorkgraphAgent()
        assert os.path.isfile(agent._wg_binary_host_path) or shutil.which("wg")

    def test_custom_wg_binary(self):
        agent = WorkgraphAgent(wg_binary_host_path="/custom/path/wg")
        assert agent._wg_binary_host_path == "/custom/path/wg"

    def test_default_timeout(self):
        agent = WorkgraphAgent()
        assert agent.timeout == 1800  # 30 minutes

    def test_custom_timeout(self):
        agent = WorkgraphAgent(timeout=600)
        assert agent.timeout == 600

    def test_custom_poll_interval(self):
        agent = WorkgraphAgent(poll_interval=5.0)
        assert agent.poll_interval == 5.0


class TestConditionAgents:
    """Test condition-specific agent aliases."""

    def test_condition_a_agent(self):
        agent = ConditionAAgent()
        assert agent.condition == "A"
        assert agent.name() == "workgraph-condition-a"

    def test_condition_b_agent(self):
        agent = ConditionBAgent()
        assert agent.condition == "B"
        assert agent.name() == "workgraph-condition-b"

    def test_condition_c_agent(self):
        agent = ConditionCAgent()
        assert agent.condition == "C"
        assert agent.name() == "workgraph-condition-c"

    def test_condition_d_agent(self):
        agent = ConditionDAgent()
        assert agent.condition == "D"
        assert agent.name() == "workgraph-condition-d"

    def test_condition_e_agent(self):
        agent = ConditionEAgent()
        assert agent.condition == "E"
        assert agent.name() == "workgraph-condition-e"

    def test_condition_f_agent(self):
        agent = ConditionFAgent()
        assert agent.condition == "F"
        assert agent.name() == "workgraph-condition-f"

    def test_all_agents_use_benchmark_model(self):
        """All condition agents must use the same BENCHMARK_MODEL for reproducibility."""
        for AgentCls in (ConditionAAgent, ConditionBAgent, ConditionCAgent,
                         ConditionDAgent, ConditionEAgent, ConditionFAgent):
            agent = AgentCls()
            assert agent.model_name == BENCHMARK_MODEL, (
                f"{AgentCls.__name__} should use BENCHMARK_MODEL"
            )

    def test_condition_agents_accept_harbor_model(self):
        """Condition agents use Harbor's model_name when provided."""
        for AgentCls in (ConditionAAgent, ConditionBAgent, ConditionFAgent):
            agent = AgentCls(model_name="openrouter/custom/model-1.0")
            assert agent.model_name == "openrouter/custom/model-1.0", (
                f"{AgentCls.__name__} should accept Harbor's model_name"
            )


class TestModelNormalization:
    """Test _normalize_model conversion between Harbor and wg formats."""

    def test_wg_format_passthrough(self):
        assert _normalize_model("openrouter:minimax/minimax-m2.7") == "openrouter:minimax/minimax-m2.7"

    def test_harbor_format_conversion(self):
        assert _normalize_model("openrouter/minimax/minimax-m2.7") == "openrouter:minimax/minimax-m2.7"

    def test_openai_format_conversion(self):
        assert _normalize_model("openai/gpt-4o") == "openai:gpt-4o"

    def test_unknown_provider_passthrough(self):
        assert _normalize_model("custom-model-v1") == "custom-model-v1"

    def test_bare_model_passthrough(self):
        assert _normalize_model("claude-sonnet-4-6") == "claude-sonnet-4-6"

    def test_benchmark_model_is_valid(self):
        """BENCHMARK_MODEL must be in wg format with known provider."""
        assert ":" in BENCHMARK_MODEL
        provider = BENCHMARK_MODEL.split(":")[0]
        assert provider in ("openrouter", "openai", "anthropic")


# ---------------------------------------------------------------------------
# Integration: wg lifecycle (init → add → show → done → verify)
# ---------------------------------------------------------------------------


class TestWgLifecycleIntegration:
    """End-to-end tests exercising wg CLI through the adapter's helper."""

    def test_init_creates_graph(self, wg_trial_dir):
        tmpdir, wg_dir = wg_trial_dir
        assert os.path.isfile(os.path.join(wg_dir, "graph.jsonl"))
        assert os.path.isfile(os.path.join(wg_dir, "config.toml"))

    def test_add_and_show(self, wg_trial_dir):
        _, wg_dir = wg_trial_dir
        result = run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
            "add", "Integration test task", "--id", "integ-1",
            "-d", "Test description",
        ]))
        assert "integ-1" in result

        show = run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["show", "integ-1"]))
        assert "Integration test task" in show
        assert "Status:" in show
        assert "open" in show.lower()

    def test_add_with_verify_gate(self, wg_trial_dir):
        """Tasks created with --verify have verify command attached."""
        _, wg_dir = wg_trial_dir
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
            "add", "Verify gate test", "--id", "verify-gate-1",
            "--verify", "echo ok",
        ]))

        show = run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["show", "verify-gate-1"]))
        assert "echo ok" in show or "Verify" in show

    def test_dependency_resolution(self, wg_trial_dir):
        """Tasks with --after create dependency edges."""
        _, wg_dir = wg_trial_dir
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
            "add", "First task", "--id", "dep-first",
        ]))
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
            "add", "Second task", "--id", "dep-second",
            "--after", "dep-first",
        ]))

        show = run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["show", "dep-second"]))
        assert "dep-first" in show

    def test_task_done_transition(self, wg_trial_dir):
        _, wg_dir = wg_trial_dir
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
            "add", "Done transition", "--id", "done-1",
        ]))
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["done", "done-1"]))

        show = run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["show", "done-1"]))
        assert "done" in show.lower()

    def test_list_shows_tasks(self, wg_trial_dir):
        _, wg_dir = wg_trial_dir
        for i in range(3):
            run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
                "add", f"List task {i}", "--id", f"list-{i}",
            ]))

        result = run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["list"]))
        for i in range(3):
            assert f"list-{i}" in result

    def test_graph_state_cleanup(self, wg_trial_dir):
        """After tasks are done, graph state is consistent."""
        _, wg_dir = wg_trial_dir
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
            "add", "Cleanup test", "--id", "cleanup-1",
        ]))
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["done", "cleanup-1"]))

        # Graph file should exist and be non-empty
        graph_path = os.path.join(wg_dir, "graph.jsonl")
        assert os.path.isfile(graph_path)
        with open(graph_path) as f:
            lines = [l for l in f if l.strip()]
        assert len(lines) >= 1  # At least the task entry


# ---------------------------------------------------------------------------
# Integration: trial config + wg init combo
# ---------------------------------------------------------------------------


class TestTrialSetupIntegration:
    """Test that writing trial config + initializing wg produces a valid state."""

    def test_config_then_add_task(self, wg_trial_dir):
        """Write a trial config, then add a task — simulates adapter.setup() + run()."""
        tmpdir, wg_dir = wg_trial_dir

        # Write config like the adapter does
        run_async(_write_trial_wg_config(tmpdir, wg_dir, "B", "test-model"))

        # Verify config was written
        config_path = os.path.join(wg_dir, "config.toml")
        assert os.path.isfile(config_path)

        # Add a root task like the adapter.run() does
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
            "add", "TB trial root task", "--id", "tb-root-123",
            "-d", "Create the project structure under /tmp/...",
        ]))

        # Verify task exists
        show = run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["show", "tb-root-123"]))
        assert "tb-root-123" in show
        assert "open" in show.lower()

    def test_condition_a_bundle_with_task(self, wg_trial_dir):
        """Condition A: bundle filtering + task creation."""
        tmpdir, wg_dir = wg_trial_dir

        run_async(_write_trial_wg_config(tmpdir, wg_dir, "A", "test-model"))
        run_async(_write_trial_bundle(wg_dir, "A"))

        # Bundle should exist
        bundle_path = os.path.join(wg_dir, "bundles", "implementer.toml")
        assert os.path.isfile(bundle_path)

        # Task creation should still work
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
            "add", "Condition A task", "--id", "cond-a-1",
        ]))
        show = run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["show", "cond-a-1"]))
        assert "cond-a-1" in show

    def test_no_orphaned_state_after_cleanup(self, wg_trial_dir):
        """Simulate full trial lifecycle: create, done, verify no orphans."""
        tmpdir, wg_dir = wg_trial_dir

        # Create tasks
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
            "add", "Root", "--id", "trial-root",
        ]))
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
            "add", "Sub 1", "--id", "trial-sub-1", "--after", "trial-root",
        ]))
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, [
            "add", "Sub 2", "--id", "trial-sub-2", "--after", "trial-root",
        ]))

        # Complete all
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["done", "trial-root"]))
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["done", "trial-sub-1"]))
        run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["done", "trial-sub-2"]))

        # List should show all tasks as done
        listing = run_async(_exec_wg_cmd_host(wg_dir, WG_BIN, ["list"]))
        # No task should be "open" or "in-progress" (all internal/agency tasks aside)
        for line in listing.strip().split("\n"):
            if "trial-root" in line or "trial-sub-1" in line or "trial-sub-2" in line:
                assert "[ ]" not in line or "done" in line.lower() or "[x]" in line.lower()

        # Graph file integrity
        graph_path = os.path.join(wg_dir, "graph.jsonl")
        with open(graph_path) as f:
            for line in f:
                line = line.strip()
                if line:
                    json.loads(line)  # Should be valid JSON


# ---------------------------------------------------------------------------
# BENCHMARK_MODEL test
# ---------------------------------------------------------------------------


class TestBenchmarkModel:
    """Ensure the benchmark model constant is sensible."""

    def test_model_is_set(self):
        assert BENCHMARK_MODEL is not None
        assert len(BENCHMARK_MODEL) > 0

    def test_model_format(self):
        """Model should be in provider:model format."""
        assert ":" in BENCHMARK_MODEL
        provider, model = BENCHMARK_MODEL.split(":", 1)
        assert len(provider) > 0
        assert len(model) > 0


# ---------------------------------------------------------------------------
# Federation tests
# ---------------------------------------------------------------------------


class TestFederationConditions:
    """Validate FEDERATION_CONDITIONS set."""

    def test_federation_conditions_are_agency_conditions(self):
        """Only conditions with agency or F should federate."""
        assert FEDERATION_CONDITIONS == {"D", "E", "F"}

    def test_non_federation_conditions(self):
        """A, B, C should NOT be in FEDERATION_CONDITIONS."""
        for cond in ("A", "B", "C"):
            assert cond not in FEDERATION_CONDITIONS


class TestEnsureHubInitialized:
    """Test hub auto-initialization."""

    def test_initializes_new_hub(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            hub_path = os.path.join(tmpdir, "hub")
            run_async(_ensure_hub_initialized(hub_path, WG_BIN))
            # Agency dir should exist
            agency_dir = os.path.join(hub_path, ".workgraph", "agency")
            assert os.path.isdir(agency_dir)

    def test_skips_existing_hub(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            hub_path = os.path.join(tmpdir, "hub")
            # First init
            run_async(_ensure_hub_initialized(hub_path, WG_BIN))
            # Create a marker to detect re-init
            marker = os.path.join(hub_path, ".workgraph", "agency", "marker")
            with open(marker, "w") as f:
                f.write("exists")
            # Second call should skip init (marker should still exist)
            run_async(_ensure_hub_initialized(hub_path, WG_BIN))
            assert os.path.isfile(marker)


class TestWriteTrialFederationConfig:
    """Test federation.yaml generation."""

    def test_writes_federation_yaml(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            wg_dir = os.path.join(tmpdir, ".workgraph")
            os.makedirs(wg_dir)
            hub_path = "/tmp/fake-hub"
            run_async(_write_trial_federation_config(wg_dir, hub_path))
            fed_path = os.path.join(wg_dir, "federation.yaml")
            assert os.path.isfile(fed_path)

            import yaml
            with open(fed_path) as f:
                config = yaml.safe_load(f)
            assert "remotes" in config
            assert "hub" in config["remotes"]
            assert config["remotes"]["hub"]["path"] == "/tmp/fake-hub/.workgraph/agency"


class TestFederationPullPush:
    """Integration tests for federation pull/push."""

    def test_pull_from_hub(self):
        """Pull primitives from an initialized hub into a trial graph."""
        with tempfile.TemporaryDirectory() as tmpdir:
            # Set up hub
            hub_path = os.path.join(tmpdir, "hub")
            run_async(_ensure_hub_initialized(hub_path, WG_BIN))

            # Set up trial
            trial_dir = os.path.join(tmpdir, "trial")
            trial_wg = os.path.join(trial_dir, ".workgraph")
            run_async(_exec_wg_cmd_host(trial_wg, WG_BIN, ["init"]))

            # Pull from hub
            result = run_async(_federation_pull(trial_wg, WG_BIN, hub_path))
            # Should not error
            assert "[exit code:" not in result or "exit code: 0" in result or "Pulled" in result or "pulled" in result.lower() or result.strip() == "(no output)"

            # Trial should now have agency data
            trial_agency = os.path.join(trial_wg, "agency")
            assert os.path.isdir(trial_agency)

    def test_push_to_hub(self):
        """Push data from a trial graph to the hub."""
        with tempfile.TemporaryDirectory() as tmpdir:
            # Set up hub
            hub_path = os.path.join(tmpdir, "hub")
            run_async(_ensure_hub_initialized(hub_path, WG_BIN))

            # Set up trial with agency
            trial_dir = os.path.join(tmpdir, "trial")
            trial_wg = os.path.join(trial_dir, ".workgraph")
            run_async(_exec_wg_cmd_host(trial_wg, WG_BIN, ["init"]))
            run_async(_exec_wg_cmd_host(trial_wg, WG_BIN, ["agency", "init"]))

            # Push to hub
            result = run_async(_federation_push(trial_wg, WG_BIN, hub_path))
            # Should not error
            assert "[wg command error:" not in result

    def test_roundtrip_pull_push(self):
        """Pull from hub, do work in trial, push back."""
        with tempfile.TemporaryDirectory() as tmpdir:
            # Set up hub
            hub_path = os.path.join(tmpdir, "hub")
            run_async(_ensure_hub_initialized(hub_path, WG_BIN))

            # Set up trial
            trial_dir = os.path.join(tmpdir, "trial")
            trial_wg = os.path.join(trial_dir, ".workgraph")
            run_async(_exec_wg_cmd_host(trial_wg, WG_BIN, ["init"]))

            # Pull primitives from hub
            pull_result = run_async(_federation_pull(trial_wg, WG_BIN, hub_path))
            assert "[wg command error:" not in pull_result

            # Create and complete a task in the trial
            run_async(_exec_wg_cmd_host(trial_wg, WG_BIN, [
                "add", "Test task", "--id", "fed-test-1",
            ]))
            run_async(_exec_wg_cmd_host(trial_wg, WG_BIN, ["done", "fed-test-1"]))

            # Push results back to hub
            push_result = run_async(_federation_push(trial_wg, WG_BIN, hub_path))
            assert "[wg command error:" not in push_result


class TestWorkgraphAgentFederationParams:
    """Test federation parameters on WorkgraphAgent."""

    def test_default_no_federation(self):
        agent = WorkgraphAgent()
        assert agent.federation_hub is None
        assert agent.pull_primitives is True
        assert agent.push_evaluations is True
        assert agent.evolve_after_n == 0

    def test_custom_federation_hub(self):
        agent = WorkgraphAgent(federation_hub="/tmp/hub")
        assert agent.federation_hub == "/tmp/hub"

    def test_disable_pull(self):
        agent = WorkgraphAgent(federation_hub="/tmp/hub", pull_primitives=False)
        assert agent.pull_primitives is False

    def test_disable_push(self):
        agent = WorkgraphAgent(federation_hub="/tmp/hub", push_evaluations=False)
        assert agent.push_evaluations is False

    def test_evolve_after_n(self):
        agent = WorkgraphAgent(federation_hub="/tmp/hub", evolve_after_n=5)
        assert agent.evolve_after_n == 5


class TestFederationHubStructure:
    """Test the tb-evaluations hub directory structure."""

    HUB_PATH = os.path.join(
        os.path.dirname(os.path.dirname(__file__)),
        "tb-evaluations",
    )

    def test_hub_directory_exists(self):
        assert os.path.isdir(self.HUB_PATH), f"Hub not found at {self.HUB_PATH}"

    def test_hub_has_workgraph(self):
        wg_dir = os.path.join(self.HUB_PATH, ".workgraph")
        assert os.path.isdir(wg_dir)

    def test_hub_has_agency(self):
        agency_dir = os.path.join(self.HUB_PATH, ".workgraph", "agency")
        assert os.path.isdir(agency_dir)

    def test_hub_has_config(self):
        config_path = os.path.join(self.HUB_PATH, ".workgraph", "config.toml")
        assert os.path.isfile(config_path)
        content = open(config_path).read()
        assert "max_agents = 0" in content

    def test_hub_has_gitignore(self):
        gitignore = os.path.join(self.HUB_PATH, ".gitignore")
        assert os.path.isfile(gitignore)


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
