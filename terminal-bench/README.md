# Terminal Bench Condition A Harness

This directory contains the Python adapter implementing Harbor's agent protocol for Terminal Bench evaluation. It provides a **bare agent** (Condition A) configuration to serve as the control group.

## Condition A Characteristics

| Aspect | Value |
|--------|-------|
| **Purpose** | Control group - what everyone has |
| **Tools** | bash, read_file, write_file, edit_file, glob, grep |
| **wg tools** | ❌ NONE |
| **Graph awareness** | ❌ NONE |
| **Journal/Resume** | ❌ DISABLED |
| **Task decomposition** | ❌ NONE |
| **System prompt** | Minimal (tool descriptions + task instruction) |

## Usage

### With Harbor Framework

```bash
# Install the adapter package
cd /home/erik/workgraph/terminal-bench
pip install -e .

# Run via Harbor
harbor run \
  --agent-import-path wg.adapter:WorkgraphAgent \
  -m minimax/minimax-m2.7 \
  --task-ids task-42 \
  -k 1
```

### Direct Python API

```python
from wg.adapter import Agent, run_task, TaskResult

# Simple one-liner
result = run_task(
    task_instruction="Fix the bug in module X",
    model="minimax/minimax-m2.7",
    max_turns=100,
)

print(f"Success: {result.success}")
print(f"Turns: {result.turns}")
print(f"Output: {result.output}")
```

### Using the Agent Class Directly

```python
from wg.adapter import Agent

agent = Agent(
    model="minimax/minimax-m2.7",
    max_turns=100,
    timeout_seconds=1800,
)

result = agent.run(
    task_instruction="Your task here",
    working_dir="/path/to/workspace",
)
```

## Architecture

The adapter:
1. Creates a temporary workgraph directory
2. Writes a Condition A bundle (`condition-a.toml`) with bash + file tools only
3. Writes a minimal system prompt with tool descriptions
4. Calls `wg native-exec` with `--exec-mode condition-a --no-resume`
5. Captures output from NDJSON logs
6. Returns standardized `TaskResult`

## Bundle Definition

The Condition A bundle is defined as:

```toml
name = "condition-a"
description = "Terminal Bench Condition A: Bare agent control group. No wg tools, no graph awareness."
tools = ["bash", "read_file", "write_file", "edit_file", "glob", "grep"]
context_scope = "clean"
```

## Comparison with Condition B

| Feature | Condition A | Condition B |
|---------|------------|------------|
| Tools | bash + file | bash + file + wg tools |
| Graph awareness | ❌ | ✅ |
| Journal/Resume | ❌ | ✅ |
| Task decomposition | ❌ | ✅ |
| External memory | ❌ | ✅ |
| Coordinator spawning | ❌ | ✅ |

## Docker Image Pre-caching

Docker Hub rate-limits anonymous pulls (~100 pulls per 6 hours). A full TB
run (89 tasks x 3 trials = 267 container starts) exceeds this, causing
`RuntimeError: toomanyrequests` failures.

**Before running a full experiment**, pre-pull all images:

```bash
# Check which images are missing
bash terminal-bench/pre-pull-images.sh --check

# Pull all missing images (sequential, safe for rate limits)
bash terminal-bench/pre-pull-images.sh

# Pull with parallelism (faster but watch rate limits)
bash terminal-bench/pre-pull-images.sh --parallel 4
```

Once pulled, images stay in Docker's local cache and won't be pulled again.
Harbor's `docker compose up` uses `pull_policy: missing` for prebuilt images,
so cached images are used directly.

## Files

- `wg/adapter.py` - Main adapter implementation
- `wg/__init__.py` - Package init
- `pre-pull-images.sh` - Pre-cache Docker images to avoid rate limiting
- `setup-docker.sh` - Docker + Harbor setup script
- `tb-harness.sh` - Native executor harness for all conditions
- `pyproject.toml` - Python package config
- `README.md` - This file
