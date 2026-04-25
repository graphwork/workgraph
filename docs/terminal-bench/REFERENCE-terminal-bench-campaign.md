# Terminal Bench Campaign: Reference Document

## Purpose

This document is the knowledge base for the Terminal Bench validation campaign. Workgraph agents executing tasks in this campaign should read this for context. It captures research, architectural analysis, competitive intelligence, and strategic decisions made during planning.

---

## 1. The Thesis

**Memory makes computation universal.** (arXiv:2412.17794, Erik Garrison, Dec 2024)

- Transformers in a single forward pass are TC0 (bounded parallel)
- Chain-of-thought (feeding output back as input) makes them Turing-complete
- But attention is O(n^2) -- quadratic cost for memory
- External memory (the workgraph) provides the same universality at linear cost
- The graph is CRISPR for LLMs: a chronological log of computational history that bounded agents can read back

**Blog post**: http://thinks.lol/2025/01/memory-makes-computation-universal/
**Paper**: https://arxiv.org/abs/2412.17794

## 2. Terminal Bench Overview

**What**: 89 diverse terminal tasks in Docker containers. Software engineering, ML training, security, sysadmin, data science, video processing, crypto, research reproduction.

**How it works**: Each task = instruction + Docker image + outcome-based tests. Agent gets a terminal, works, tests check final container state.

**Scoring**: Resolution rate (% tasks solved). One attempt per task. Results with 95% confidence intervals.

**Difficulty distribution**:
- 48.6% tasks: < 1 hour for expert
- 47.3% tasks: 1-24 hours for expert
- 4.1% tasks: 1-7 days for expert

**Links**:
- Repository: https://github.com/laude-institute/terminal-bench
- Website: https://www.tbench.ai/
- Leaderboard (2.0): https://www.tbench.ai/leaderboard/terminal-bench/2.0
- Paper: https://arxiv.org/html/2601.11868v1

**Running it**:
```bash
uv tool install terminal-bench
# or: pip install terminal-bench

tb run \
    --agent <agent-name> \
    --model <provider/model> \
    --dataset-name terminal-bench-core \
    --dataset-version 0.1.1 \
    --n-concurrent 8
```

**Agent integration options**:
1. Container installation (deploy agent into Docker container)
2. Direct Python integration (full logging + API access)
3. MCP server (exposes tmux session)

**Prompt injection**: Agents receive the task instruction as their prompt. Custom system prompts are injected by the agent harness BEFORE the task instruction. This is where ForgeCode puts identity, planning rules, tool descriptions, and skills. This is where Workgraph puts the wg CLI instructions, graph context, and scope-based prompt assembly.

---

## 3. Competitive Intelligence: ForgeCode

**What**: Rust-based multi-agent coding system. Open source (Apache 2.0). 4,700 GitHub stars. Top of Terminal Bench 2.0 at 81.8%.

**Repository**: https://github.com/antinomyhq/forgecode
**Docs**: https://forgecode.dev/docs/
**Install**: `npx forgecode@latest`

### Architecture

Three agents:
- **Muse** -- planning agent (read-only, no code changes)
- **Forge** -- implementation agent (read-write)
- **Sage** -- research subagent (used internally by Muse and Forge)

Philosophy: "Analytical reasoning about *what to do* must not be conflated with *doing it*."

### How They Went From 25% to 81.8%

**Critical blog posts** (MUST READ for anyone working on this campaign):
- Part 1: https://forgecode.dev/blog/benchmarks-dont-matter/
- Part 2: https://forgecode.dev/blog/gpt-5-4-agent-improvements/

| Change | Impact | Notes |
|--------|--------|-------|
| Enforced `todo_write` planning | **+28 points** (38% → 66%) | Single biggest gain. Runtime mandates planning, not optional. |
| Non-interactive mode | ~+8 points | Rewrites system prompt to eliminate clarification requests. Forces decisive action. |
| Semantic entry-point discovery | ~+5 points | Lightweight analysis finds relevant files BEFORE exploration. |
| Subagent parallelization + progressive thinking | ~+5 points | Routine work (file reads, searches) delegated to low-budget subagents. High thinking for planning, low for execution, high for verification. |
| Schema field ordering | ~+1 point | `required` before `properties` in tool schemas. "GPT 5.4 anchors on what it sees first." |
| Schema flattening | ~+1 point | Single-level tool schemas eliminate nested ambiguity. |
| Explicit truncation signals | ~+0.5 points | Plain text "truncated 3823 more lines" instead of metadata. Models miss metadata. |
| Enforced verification | ~+1 point | Runtime mandates verification step. Model cannot skip it. |

**Key result**: With identical Gemini 3.1 Pro weights, ForgeCode scored 78.4% vs Google's own agent at 68.5%. **10-point delta from scaffold alone.**

### ForgeCode vs Workgraph: Architectural Comparison

| Dimension | ForgeCode | Workgraph |
|-----------|-----------|-----------|
| Planning | Enforced `todo_write` (flat checklist) | Task graph with dependencies, cycles, verification gates |
| Agent decomposition | 3 fixed agents (Muse/Forge/Sage) | Arbitrary agent types via agency system, composable from primitives |
| Memory | Conversational context preserved across agent switches | Externalized graph state survives agent lifetimes, context limits, crashes |
| Verification | Runtime-enforced checklist review | `--verify "command"` gates on tasks, automated evaluation pipeline |
| Subagent coordination | Sequential (Muse → Forge) with parallel routine work | Stigmergic (graph-mediated), arbitrary topology, cycles |
| Entry-point discovery | Semantic analysis phase | Can be a dedicated research subtask with artifacts |
| Context management | Thinking budget policies (high/low/high) | Bundle system (bare/light/full), scope-based prompt assembly, journal/resume |
| Resume/recovery | Context preserved in memory | Journal-based crash recovery with stale-state detection |
| Model support | 300+ models via OpenAI-compatible | Anthropic + OpenAI-compatible via native executor |
| Language | Rust | Rust |
| License | Apache 2.0 | (Erik's project) |

### What Workgraph Can Do That ForgeCode Cannot

1. **Persistent external memory**: ForgeCode's todo_write is in-context. Workgraph's task log survives context exhaustion.
2. **Dependency-driven execution**: ForgeCode's agents follow a fixed sequence. Workgraph's coordinator dispatches based on graph topology -- ready tasks spawn automatically.
3. **Cycles**: ForgeCode has no iteration concept. Workgraph supports structural cycles with convergence detection.
4. **Cross-agent artifact sharing**: ForgeCode's agents share conversational context. Workgraph agents share typed artifacts through the graph, discoverable by any future agent.
5. **Agent resume**: If a ForgeCode agent dies, it starts over. Workgraph agents resume from journal with stale-state detection.
6. **Verification gates**: ForgeCode's verification is a prompt. Workgraph's `--verify` runs an actual command and blocks downstream until it passes.
7. **Arbitrary decomposition depth**: ForgeCode has 3 agents. Workgraph can spawn N agents for N subtasks, each with their own subtasks.

---

## 4. Current State: Native Executor

### What Works

- Multi-provider routing (Anthropic + OpenAI-compatible)
- Tool-use loop with proper stop reason handling
- Journal/resume with stale-state detection
- Bundle system (bare/light/full tool filtering)
- In-process wg tools (microsecond latency)
- Structured NDJSON logging (stream.jsonl)
- 12 tools: bash, read_file, write_file, edit_file, list_files, glob, grep, wg_show, wg_list, wg_add, wg_done, wg_fail, wg_log, wg_artifact, wg_msg_send, wg_msg_read

### Critical Bugs to Fix Before Terminal Bench

| Bug | File | Line(s) | Impact | Fix |
|-----|------|---------|--------|-----|
| Silent JSON parse failure | `openai_client.rs` | ~514, ~1714 | `unwrap_or_default()` replaces malformed tool args with null. Agent loops forever with empty tool calls. | Return parse error to model so it can self-correct. |
| Hardcoded 200K context budget | `resume.rs` | ~61 | Compaction budget assumes 200K window. Models with smaller context windows never trigger compaction, hitting API 400. | Make context window configurable per provider/model. Look up from OpenRouter model metadata or config. |
| Tool call format extraction | `openai_client.rs` | `extract_tool_calls_from_text()` | May miss non-standard tool call formats from open models. Calls silently lost. | Test with actual model output. First check if OpenRouter returns native `tool_calls` (may already work). |
| No streaming in agent loop | `agent.rs` | 325 | Uses `.send()` not `.send_streaming()`. No real-time observability. | Switch to `send_streaming()` with text callback writing to stream.jsonl. |
| No heartbeat during tool execution | `agent.rs` | tool execution block | Coordinator can't detect hung agents during long bash commands. | Write Heartbeat event every 30s during tool execution. |
| No context pressure signaling | `agent.rs` | main loop | Agent doesn't know it's running out of context. | Estimate token usage per turn, inject warning at 80% capacity. |

### Key Source Files

| File | Purpose |
|------|---------|
| `src/executor/native/agent.rs` | Main agent loop (tool-use cycle) |
| `src/executor/native/openai_client.rs` | OpenAI-compatible API client (streaming, tool call parsing) |
| `src/executor/native/client.rs` | Anthropic Messages API client |
| `src/executor/native/provider.rs` | Provider trait + model routing |
| `src/executor/native/resume.rs` | Journal-based resume with stale-state detection |
| `src/executor/native/journal.rs` | Append-only JSONL conversation persistence |
| `src/executor/native/tools/mod.rs` | Tool registry and dispatch |
| `src/executor/native/tools/bash.rs` | Bash execution with timeout |
| `src/executor/native/tools/file.rs` | File I/O tools (read, write, edit, glob, grep) |
| `src/executor/native/tools/wg.rs` | In-process workgraph tools |
| `src/executor/native/bundle.rs` | Exec_mode → tool filtering |
| `src/stream_event.rs` | Unified NDJSON streaming format |
| `src/commands/native_exec.rs` | CLI entry point for `wg native-exec` |

---

## 5. Experiment Design

### Conditions

**Condition A: Bare Agent (Control)**
- Native executor, single session
- Tools: bash, read_file, write_file, edit_file, glob, grep
- No wg tools, no graph, no journal/resume
- No task decomposition, no external memory
- System prompt: minimal (tool descriptions + task instruction)
- This is what everyone else has

**Condition B: Agent + Workgraph (Treatment)**
- Native executor with full wg tool access
- Tools: everything in Condition A + wg_show, wg_list, wg_add, wg_done, wg_fail, wg_log, wg_artifact
- Journal/resume enabled (survives context exhaustion)
- System prompt: scope-based assembly (task context + graph awareness + wg CLI instructions)
- Agent can: decompose into subtasks, log progress, create verification gates, read artifacts from prior work
- Coordinator can spawn child agents for subtasks if needed
- This is the thesis

**Condition C: ForgeCode Baseline (Optional)**
- ForgeCode with same model
- Their scaffold, their prompts, their runtime
- Establishes current SOTA scaffold performance for direct comparison

### Models

Primary: **Minimax M2.7** via OpenRouter
- Cost-effective inference
- Good tool calling
- No local GPU needed

> **Note**: Calibration runs (documented in `terminal-bench/results/`) were performed with Qwen3-32B. The primary experiment model was subsequently changed to Minimax M2.7.

Calibration (historical): **Qwen3-32B** via OpenRouter
- Used for initial calibration runs
- 32K context (constrained -- forces the memory question)

Calibration: **Claude Haiku** via native executor (Anthropic API)
- Cheap ($0.25/MTok input)
- Weak enough to be a fair comparison to open models
- Known-good tool calling
- Native executor already works with Anthropic API

### Metrics

- **Primary**: Resolution rate (% tasks solved) per condition
- **By difficulty**: Easy / Medium / Hard breakdown
- **Token efficiency**: Total tokens consumed per solved task
- **Time efficiency**: Wall-clock time per solved task
- **Decomposition analysis** (Condition B only): How many subtasks created? How deep?
- **Resume analysis** (Condition B only): How many tasks hit context limits? How many recovered via journal?
- **Statistical**: 3 runs per condition, report mean ± stderr

### Expected Results

- **Easy tasks**: Both conditions similar. Single session suffices.
- **Medium tasks**: Workgraph helps. Decomposition, logging, artifact sharing improve reliability.
- **Hard tasks**: Workgraph's thesis. Bare agent hits context wall. Graph agent decomposes, checkpoints, resumes. This is where the gap opens.

---

## 6. Terminal Bench Integration Approach

### How Custom Prompts Work

Terminal Bench gives each agent:
1. The task instruction (from the benchmark)
2. Access to a Docker container with the environment

The agent harness wraps the task instruction with its own system prompt. This is where scaffold engineering happens. ForgeCode injects identity, planning rules, tool schemas, skills, and non-interactive mode directives.

For Workgraph:
- **Condition A system prompt**: Minimal. "You are a coding agent. Complete the following task. You have access to bash and file tools."
- **Condition B system prompt**: The scope-based prompt assembly from `src/service/executor.rs`. This includes the REQUIRED_WORKFLOW_SECTION (wg CLI commands, message handling, validation, completion gates), the GRAPH_PATTERNS_SECTION (cycle awareness, golden rule), and task-specific context.

### Runtime Architecture

The native executor (agent loop) runs on the **host machine**. It calls OpenRouter for completions and executes tools **inside** the Docker container. The `wg` binary is injected into the container for Condition B.

```
[Host machine (laptop / Hetzner)]
  │
  ├─ Harbor (orchestrates benchmark)
  │   └─ Docker container (per task)
  │       ├─ Task environment (pre-installed packages, files, tests)
  │       ├─ wg binary (injected at task start, Condition B only)
  │       └─ .workgraph/ (initialized at task start, Condition B only)
  │
  ├─ Native executor (agent loop, runs on host)
  │   ├─ Calls OpenRouter API for LLM completions
  │   └─ Executes tools inside container (bash, file ops, wg commands)
  │
  └─ Results collected by Harbor → submission directory
```

**Model inference**: Via API. No local GPU needed. Cost ~$10-30 for full 89-task x 5-trial run.

**Host requirements**: Docker, Harbor, `wg` binary. CPU/RAM are not the bottleneck -- API latency is.

### Installing Workgraph in Containers

The `wg` binary is a single statically-linked Rust binary (~15-20MB). Zero runtime dependencies -- no Python, no Node.js, no package manager needed inside the container.

**Three injection methods:**

```bash
# Method 1: docker cp (simplest, per-container)
docker cp target/release/wg <container_id>:/usr/local/bin/wg
docker exec <container_id> wg init

# Method 2: Volume mount (cleanest for development)
docker run -v $(pwd)/target/release/wg:/usr/local/bin/wg:ro ...

# Method 3: Build once, inject at adapter level
# The Harbor agent adapter copies wg into each container at task start
```

The adapter handles this automatically -- at task start for Condition B, it:
1. Copies `wg` binary into the container
2. Runs `wg init` to create `.workgraph/` directory
3. Creates the root task from the Terminal Bench instruction
4. Starts the agent loop with wg tools enabled

For Condition A, no injection needed -- just bash and file tools.

**Pre-building the binary:**
```bash
cd <workgraph-repo-root>
cargo build --release
# Binary at: target/release/wg
```

### Agent Adapter

A thin adapter that bridges Harbor's agent protocol to the native executor:

1. Receives a Terminal Bench task instruction from Harbor
2. Sets up tools pointing at the Docker container (bash via `docker exec`, file ops via mounted paths)
3. For Condition B: injects `wg` binary, initializes workgraph, creates root task
4. Runs the native executor agent loop with the appropriate system prompt
5. Captures outcome (did tests pass?) and reports to Harbor

```python
# Pseudocode for the adapter
def run_task(task_instruction, condition, model, container_id):
    if condition == "B":
        # Inject workgraph into container
        subprocess.run(["docker", "cp", "target/release/wg", f"{container_id}:/usr/local/bin/wg"])
        subprocess.run(["docker", "exec", container_id, "wg", "init"])
        subprocess.run(["docker", "exec", container_id, "wg", "add", task_instruction])
    
    run_native_exec(
        prompt=task_instruction if condition == "A" else build_prompt(task_instruction, scope="full"),
        exec_mode="full",
        model=model,  # e.g., "minimax/minimax-m2.7"
        tools=["bash", "read_file", "write_file", "edit_file", "glob", "grep"]
              + (["wg_show", "wg_list", "wg_add", "wg_done", "wg_fail", "wg_log", "wg_artifact"] if condition == "B" else []),
        journal=condition == "B",
        resume=condition == "B",
        container_id=container_id,  # Tools execute inside this container
    )
```

### Integration Options

The cleanest approach is likely the **direct Python integration** or **MCP server** that Terminal Bench/Harbor provides. The adapter above would implement Harbor's agent interface, dispatching to `wg native-exec` under the hood.

---

## 7. Terminal Bench 2.0: Submission Process & Local Development

### Leaderboard Submission (When Ready)

Terminal Bench 2.0 uses the **Harbor Framework** for evaluation and **HuggingFace PRs** for submission.

**Step 1: Run evaluation with Harbor**
```bash
# Install Harbor (requires Docker running)
# See: https://harborframework.com/docs/tutorials/running-terminal-bench

harbor run \
  -d terminal-bench/terminal-bench-2 \
  --agent-import-path your.agent:YourAgent \
  -m your-model \
  -k 5    # REQUIRED: minimum 5 trials per task for leaderboard
```

**Step 2: Package results**
```
submissions/terminal-bench/2.0/<agent>__<model>/
  metadata.yaml              # Agent name, org, model, repo link
  <job-folder>/
    config.json
    trial-1/result.json
    trial-2/result.json
    trial-3/result.json
    trial-4/result.json
    trial-5/result.json
```

**Step 3: Create metadata.yaml**
```yaml
agent_url: https://github.com/erikg/workgraph
agent_display_name: "Workgraph Native"
agent_org_display_name: "Erik Garrison"

models:
  - model_name: minimax-m2.7
    model_provider: minimax
    model_display_name: "Minimax M2.7"
    model_org_display_name: "Minimax"
```

**Step 4: Submit via HuggingFace PR**
1. Fork: `https://huggingface.co/datasets/harborframework/terminal-bench-2-leaderboard`
2. Add submission directory under `submissions/terminal-bench/2.0/`
3. Open Pull Request
4. Bot auto-validates (checks: timeout_multiplier==1.0, no resource overrides, min 5 trials, valid result files)
5. Maintainer reviews and merges → results appear on leaderboard automatically

**Constraints enforced by validation bot:**
- `timeout_multiplier` must equal `1.0` (no extra time)
- No agent timeout overrides (`override_timeout_sec`, `max_timeout_sec`)
- No verifier timeout overrides
- No resource overrides (`override_cpus`, `override_memory_mb`, `override_storage_mb`)
- All trial directories must have valid `result.json`
- **SECURITY**: Scrub API keys and proprietary prompts from logs -- submissions are public

**Contact**: alexgshaw64@gmail.com or Discord https://discord.gg/6xWPKhGDbA

### Local Development Workflow (Before Submission)

**There is NO limit on local runs.** You can run hundreds of times while developing. Only the final submission goes to the leaderboard. The `-k 5` minimum is only checked by the submission bot.

**Recommended development flow:**

```bash
# Phase 1: Get adapter working (minutes)
# Run ONE task, ONE trial
harbor run -d terminal-bench/terminal-bench-2 \
  --agent-import-path wg.adapter:WorkgraphAgent \
  -m minimax/minimax-m2.7 \
  --task-ids task-42 \
  -k 1

# Phase 2: Test diverse tasks (hours)
# Run 5-10 tasks spanning easy/medium/hard, 1 trial each
harbor run -d terminal-bench/terminal-bench-2 \
  --agent-import-path wg.adapter:WorkgraphAgent \
  -m minimax/minimax-m2.7 \
  --task-ids task-1,task-15,task-30,task-50,task-70 \
  -k 1

# Phase 3: Iterate on failures
# Re-run failing tasks, tune prompts, fix bugs
# This is where most development time goes

# Phase 4: Baseline run (hours, can run overnight)
# All 89 tasks, 1 trial — get rough numbers
harbor run -d terminal-bench/terminal-bench-2 \
  --agent-import-path wg.adapter:WorkgraphAgent \
  -m minimax/minimax-m2.7 \
  -k 1

# Phase 5: Submission-quality run
# All 89 tasks, 5 trials — statistical rigor
harbor run -d terminal-bench/terminal-bench-2 \
  --agent-import-path wg.adapter:WorkgraphAgent \
  -m minimax/minimax-m2.7 \
  -k 5
```

**What "-k 5" means**: Each of the 89 tasks runs 5 separate times. LLMs are stochastic — same task might pass on run 1 and fail on run 2. Five trials give mean pass rate with confidence intervals. This is for statistical rigor, not attempt limiting.

**Time estimates for local runs:**
- 1 task, 1 trial: ~5-30 minutes (depending on task complexity and model speed)
- 89 tasks, 1 trial: ~4-12 hours (with `--n-concurrent 4-8`)
- 89 tasks, 5 trials: ~20-60 hours (run overnight / over weekend)
- With smaller/slower models: proportionally longer
- With OpenRouter: faster but costs money
- With vLLM: fastest local option (continuous batching, tensor parallelism)

### Terminal Bench 1.0 vs 2.0

| Aspect | 1.0 (Legacy) | 2.0 (Current) |
|--------|-------------|----------------|
| Tasks | 80 | 89 |
| CLI tool | `tb run` / `uvx terminal-bench run` | `harbor run` |
| Submission | Email mikeam@cs.stanford.edu or alex@laude.org | HuggingFace PR |
| Validation | Manual | Automated bot |
| Min trials | Not enforced | 5 per task |
| Active leaderboard | Yes (but less activity) | Yes (primary, 123 entries) |

**We target 2.0** — it's the active leaderboard with the most entries and the automated submission process.

### What the Leaderboard Shows

Current top entries on Terminal Bench 2.0:

| Rank | Agent | Model | Accuracy |
|------|-------|-------|----------|
| 1 | ForgeCode | GPT-5.4 | 81.8% |
| 2 | ForgeCode | Claude Opus 4.6 | 81.8% |
| 3 | TongAgents | Gemini 3.1 Pro | 80.2% |
| ... | ... | ... | ... |
| 39 | Claude Code | Claude Opus 4.6 | 58.0% |
| Last | (worst) | -- | 3.1% |

**Our target**: Beat Claude Code's 58% with an open model. If Workgraph + Minimax M2.7 scores above 58%, we've shown that a model with the right memory architecture beats a frontier model with a basic scaffold. That alone is newsworthy.

**Dream target**: Approach or beat ForgeCode's 78-82% range. If we do that with Minimax M2.7, it's paradigm-shifting.

---

## 8. Timeline (6-Day Plan, ~13 Days to Deadline)

| Day | Focus | Deliverable |
|-----|-------|-------------|
| 1 | Smoke test + fix critical native executor bugs | Native executor completes 15-turn task with Minimax M2.7 |
| 2 | Streaming + Terminal Bench setup + adapter | Agent observable; TB runs single task |
| 3 | Build Condition A + Condition B harnesses | Both conditions run on 3-5 tasks |
| 4 | Full experiment run (89 tasks x 2 conditions) | Raw results |
| 5 | Second runs + analysis | Statistical results with confidence intervals |
| 6 | Write-up + publish | Blog post + README + leaderboard submission |

---

## 9. End-to-End Testing Requirements

Before running Terminal Bench, the native executor must pass these tests:

### Smoke Tests (Day 1)

1. **Claude via native executor**: Simple file create + read + edit cycle. Validates happy path.
2. **Minimax M2.7**: Same test. Find what breaks.
3. **API endpoint**: Same test via model API. Validates remote model access.

### Integration Tests (Day 1-2)

4. **Multi-turn tool use**: 15+ turns with file tools + bash. Agent must read, edit, compile, test.
5. **Journal/resume**: Start a task, kill the agent mid-way, resume from journal. Verify stale-state detection.
6. **Context exhaustion**: Give agent a task that requires > 32K tokens of context. Verify graceful handling (compaction or clean exit with `wg log`).
7. **wg tool integration**: Agent creates subtasks, logs progress, marks done. Graph state correct after.

### Host System Requirements

**Minimal requirements** (laptop or Hetzner node):
- Docker installed and running
- Harbor framework installed
- `wg` binary built (`cargo build --release` in workgraph repo)
- OpenRouter API key
- Internet connection (for OpenRouter API calls)
- No GPU needed. No local model inference. CPU/RAM not the bottleneck.

**Setup:**
```bash
# 1. Docker (REQUIRED)
sudo apt-get update && sudo apt-get install docker.io docker-compose-v2
sudo usermod -aG docker $USER
# Log out and back in, then verify:
docker run hello-world

# 2. Harbor Framework (REQUIRED for Terminal Bench 2.0)
pip install harbor-framework
# or: uv tool install harbor-framework

# 3. Build workgraph binary
cd <workgraph-repo-root>
cargo build --release
# Binary at: target/release/wg

# 4. OpenRouter API key
export OPENROUTER_API_KEY=<your-key>
# Verify:
curl -s https://openrouter.ai/api/v1/models \
  -H "Authorization: Bearer $OPENROUTER_API_KEY" | head -c 200
```

**Works identically on**: laptop, Hetzner dedicated, any Linux box with Docker + internet. A Hetzner node is preferable for long runs (no lid-close / sleep issues).

### Environment Configuration

```bash
# Primary model: Minimax M2.7
export OPENROUTER_API_KEY=<key>
# Model string for native executor: "minimax/minimax-m2.7"

# Calibration model: Claude Haiku via Anthropic (optional)
export ANTHROPIC_API_KEY=<key>
# Model string: "claude-haiku-4-latest"

# Workgraph native executor config
# In .workgraph/config.toml:
# [native_executor]
# provider = "openai"
# openai_base_url = "https://openrouter.ai/api/v1"
```

**Cost estimates**:
- Per task (est. 50K tokens): ~$0.02-0.04
- Full 89 tasks x 5 trials: ~$10-30 total

### Test Tags

Mark tests that require external endpoints:
```rust
#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "requires OPENROUTER_API_KEY"]
    fn test_minimax_m2_7_tool_calling() { ... }
    
    #[test]
    #[ignore = "requires ANTHROPIC_API_KEY"]
    fn test_anthropic_haiku() { ... }
    
    #[test]
    fn test_tool_registry_dispatch() { ... }  // Always runs, no external deps
}
```

Run selectively:
```bash
cargo test                                    # Unit tests only
cargo test -- --ignored                       # All external endpoint tests
cargo test -- --ignored test_openrouter       # OpenRouter tests only
```

---

## 10. Key Insights to Remember

1. **ForgeCode proved scaffold > model** (10-point delta, same weights). We're proving memory > scaffold.

2. **Enforced planning was ForgeCode's biggest win** (+28 points). Workgraph's task graph is enforced planning on steroids -- with dependencies, verification, and persistence.

3. **The hard tasks are where workgraph wins.** Easy tasks don't need external memory. Hard tasks exceed context limits. That's where the graph provides value.

4. **Context exhaustion is a feature, not a bug.** When Condition A agents hit context limits and fail, Condition B agents resume from journal. The failure IS the proof.

5. **The system prompt is the scaffold.** Terminal Bench tasks come as plain instructions. The system prompt wrapper is where all the agent engineering lives. Workgraph's scope-based prompt assembly IS the competitive advantage.

6. **Token cost doesn't matter if pass rate improves.** Even if Condition B uses more tokens per task (because of wg tool calls, graph context), higher pass rate is the primary metric.

7. **This system built itself.** 216K lines of Rust, 103K lines of docs, 1,057 commits, 75 days, 1 person. That's the ultimate demo. Terminal Bench makes it legible to others.

---

## 11. Strategic Context

### Why This Matters

This campaign is not just a benchmark run. It's a **constructive proof of a theoretical result**:

- **The paper** (arXiv:2412.17794): Universality requires (1) stable evolution of thought and (2) reliable access to history of thought. Memory makes computation universal.
- **The construction** (Workgraph): A stigmergic task graph that provides externalized memory to bounded LLM agents. The graph is CRISPR for LLMs -- a chronological log that agents read back to act on their full computational history.
- **The proof** (Terminal Bench): Show that agents with external memory (workgraph) outperform agents without it, same model, same tools. The delta IS the value of memory.

### Why This Is Different From ForgeCode's Approach

ForgeCode optimized the scaffold: enforced planning, progressive thinking, schema tricks, subagent parallelization. All within a single session. All within the context window.

Workgraph optimizes the **memory architecture**: persistent state that survives context exhaustion, dependency-driven execution, stigmergic coordination through shared graph state, crash recovery via journal/resume.

ForgeCode proved scaffold > model (10-point delta). We're proving memory > scaffold. If a $0 open model with the right memory architecture beats expensive frontier models with sophisticated scaffolds, that's a fundamental result about the nature of intelligence -- not just an engineering win.

### The Self-Bootstrapping Argument

Workgraph was built by workgraph. 216K lines of Rust, 103K lines of design docs, 1,057 commits, 75 days, 1 person. The system orchestrated the agents that wrote the system. This is the ultimate proof-of-concept, but it's self-referential. Terminal Bench makes it legible to the outside world by providing an independent, third-party evaluation.

### Deadline

~13 days from April 2, 2026. Target: published results + leaderboard submission by April 15, 2026.

---

## 12. Links Index

### Workgraph
- Paper: https://arxiv.org/abs/2412.17794
- Blog: http://thinks.lol/2025/01/memory-makes-computation-universal/
- Repository: https://github.com/erikg/workgraph

### Terminal Bench
- Repository: https://github.com/laude-institute/terminal-bench
- Website: https://www.tbench.ai/
- Leaderboard 2.0: https://www.tbench.ai/leaderboard/terminal-bench/2.0
- Leaderboard 1.0: https://www.tbench.ai/leaderboard/terminal-bench/1.0
- Paper: https://arxiv.org/html/2601.11868v1

### ForgeCode (Primary Competitor)
- Repository: https://github.com/antinomyhq/forgecode
- Docs: https://forgecode.dev/docs/
- Blog Part 1 (25% → 78.4%): https://forgecode.dev/blog/benchmarks-dont-matter/
- Blog Part 2 (78.4% → 81.8%): https://forgecode.dev/blog/gpt-5-4-agent-improvements/

### Claude Code (Reference Implementation)
- Claude Code scores 58% on Terminal Bench 2.0 (rank ~39)
- Architecture analyzed in separate comparative report (not included in this repo)

### Models
- Minimax M2.7 (primary experiment model)
- Qwen3-32B: https://huggingface.co/Qwen/Qwen3-32B (used for calibration)
- OpenRouter: https://openrouter.ai/

### Campaign Documents (This Repo)
- Roadmap (6-day plan): `docs/terminal-bench/ROADMAP-terminal-bench.md`
- Campaign briefing (this file): `docs/terminal-bench/REFERENCE-terminal-bench-campaign.md`
- Executor design requirements: `docs/terminal-bench/DESIGN-native-executor-improvements.md`
