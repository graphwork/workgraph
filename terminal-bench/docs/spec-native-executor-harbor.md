# Spec: Replace LiteLLM Agent Loop with wg Native Executor in Harbor Adapter

## Problem

The Harbor adapter (`terminal-bench/wg/adapter.py`) currently runs a **Python LiteLLM agent loop** inside `_run_docker_agent_loop()`. This is a hand-rolled tool-calling loop (lines 543-754) that:
- Calls `litellm.acompletion()` for LLM inference
- Defines its own 3-tool set (bash, write_file, read_file)
- Routes tool executions through `environment.exec()` into Docker containers

This is **not** the wg executor. The whole point of the experiment is to test the wg system — the Rust native executor (`wg native-exec`) with its full tool set, context assembly, and coordinator logic. The LiteLLM loop is a completely different agent with different tools, prompts, and behavior.

## Goal

Replace `_run_docker_agent_loop()` with a `_run_native_executor()` path that:
1. Uses `wg` on the **host** to manage the task graph and coordinate execution
2. The wg native executor runs commands **inside the Docker container** via `environment.exec()`
3. All conditions (A-F) use the same wg executor, differentiated only by config (context_scope, exec_mode, bundles, agency)

## Current Architecture (What Exists)

### Host-native path (standalone runners, NOT Harbor)
Already works correctly in `run_scale_experiment.py` and pilot scripts:
```
Runner creates temp dir → wg init → writes config.toml → wg add "task" --verify "cmd"
→ wg service start → coordinator spawns native-exec agent
→ native-exec calls OpenRouter directly (Rust HTTP client, no LiteLLM)
→ Agent runs bash/file tools on HOST
→ wg service polls for completion → collects metrics from stream.jsonl
```

### Harbor/Docker path (current, broken)
```
Harbor → ConditionXAgent.setup() → creates wg graph dir on HOST ✓
Harbor → ConditionXAgent.run() → _run_docker_agent_loop()
  → Python LiteLLM loop (NOT wg) ✗
  → 3 tools only (bash, write_file, read_file) ✗
  → Tool calls routed through environment.exec() into Docker ✓
  → No wg context assembly, no wg tools, no coordinator ✗
```

### What we need
```
Harbor → ConditionXAgent.setup() → creates wg graph dir on HOST ✓ (already works)
Harbor → ConditionXAgent.run() → _run_native_executor()
  → wg add "task" --verify "cmd" on HOST
  → wg service start on HOST (spawns native-exec agent)
  → native-exec agent needs to route bash commands INTO Docker container
  → _poll_task_completion() waits for done/failed (already exists, line 322)
  → _collect_agent_metrics() reads stream.jsonl (already exists, line 361)
```

## The Hard Part: Bridging wg native-exec ↔ Docker

The wg native executor runs on the **host**. It spawns bash commands expecting a local environment. But TB tasks run inside **Docker containers** managed by Harbor.

**Options:**

### Option A: Mount + chroot approach
- Mount the Docker container's filesystem on the host
- Configure wg native-exec to chroot or use `docker exec` as its shell
- Fragile, requires Docker socket access

### Option B: Proxy shell script
- Create a shell wrapper that translates local bash calls into `docker exec <container> bash -c "..."` calls
- Set `WG_SHELL` or equivalent env var pointing to this wrapper
- Native executor thinks it's running locally but commands go to Docker
- **Problem**: wg native-exec doesn't support custom shell backends — it uses `tokio::process::Command` directly

### Option C: Use `environment.exec()` from Python, but drive it from wg service
- This is the hybrid approach: wg coordinates on the host, but when the native executor needs to run a command, it calls back to Python which routes through `environment.exec()`
- **Problem**: native-exec is a Rust binary, can't call Python callbacks

### Option D: Don't use Docker isolation for the agent — only for verification
- Run the wg native executor on the **host** directly (same as the standalone runners)
- Use Harbor's Docker container **only for the task environment setup and verification**
- In `setup()`: use `environment.exec()` to extract the task workspace from Docker to a host temp dir
- Agent works on host files
- In verification: use `environment.exec()` to run the verifier inside Docker
- **This is the most pragmatic approach** — it's closest to what the standalone runners already do

### Option E: Install wg inside the Docker container
- Copy the wg binary into the container during `setup()`
- Run `wg init`, `wg add`, `wg service start` **inside** the container via `environment.exec()`
- The native executor runs entirely inside Docker — commands are local to the container
- Need to pass `OPENROUTER_API_KEY` into the container
- **This is the cleanest approach** — everything runs in one place, matching how a real user would use wg

## Recommended Approach: Option E (wg inside Docker)

### Implementation steps

#### 1. Modify `setup()` to install wg in the container

```python
async def setup(self, environment: BaseEnvironment) -> None:
    # Copy host wg binary into the container
    wg_bin = self._wg_binary_host_path
    # Harbor environments support file upload or we can use docker cp
    await environment.exec(command="mkdir -p /usr/local/bin")
    # Upload wg binary — need to check Harbor's file upload API
    # Alternative: mount host binary path as a Docker volume
    
    # Initialize workgraph inside the container
    await environment.exec(command="wg init")
    
    # Write config.toml based on condition
    config_content = self._build_config_toml()
    await environment.exec(command=f"cat > .workgraph/config.toml << 'EOF'\n{config_content}\nEOF")
    
    # Write custom bundles if needed (Condition A: no wg tools)
    if CONDITION_CONFIG[self.condition].get("exclude_wg_tools"):
        bundle_content = self._build_condition_a_bundle()
        await environment.exec(command=f"mkdir -p .workgraph/bundles && cat > .workgraph/bundles/implementer.toml << 'EOF'\n{bundle_content}\nEOF")
```

#### 2. Modify `run()` to use wg service inside the container

```python
async def run(self, instruction: str, environment: BaseEnvironment, context: AgentContext) -> None:
    # Add the task with verification
    verify_cmd = context.task.verify_command  # or however Harbor exposes this
    await environment.exec(
        command=f'wg add "TB task" --verify "{verify_cmd}"',
        env={"OPENROUTER_API_KEY": os.environ["OPENROUTER_API_KEY"]},
    )
    
    # Start the service — this spawns the native executor agent
    # The agent runs INSIDE the container, commands are local
    await environment.exec(
        command=f'wg service start --model "{self._model}"',
        env={"OPENROUTER_API_KEY": os.environ["OPENROUTER_API_KEY"]},
        timeout_sec=self.timeout,
    )
    
    # wg service start blocks until all tasks complete (or use polling)
    # Collect metrics from .workgraph/agents/*/stream.jsonl inside container
```

#### 3. Collect metrics from inside the container

```python
    # Read stream.jsonl from inside container
    result = await environment.exec(command="cat .workgraph/agents/*/stream.jsonl")
    # Parse metrics same as _collect_agent_metrics() but from exec output
```

#### 4. Handle the wg binary delivery problem

The wg binary is compiled for the host (x86_64 Linux). The Docker containers are also Linux x86_64, so the binary should work. Options for getting it in:
- **Docker volume mount**: Mount `~/.cargo/bin/wg` as a read-only volume. Requires `--mounts-json` flag in `harbor run`.
- **`environment.upload()`**: If Harbor supports file upload to the container.
- **`docker cp`**: Copy binary in during setup. Need container ID.
- **Build inside container**: `cargo install` inside container — too slow, bad idea.

**Best option**: Use `--mounts-json` to mount the wg binary:
```bash
harbor run ... --mounts-json '["/home/bot/.cargo/bin/wg:/usr/local/bin/wg:ro"]'
```

Or handle it in `setup()`:
```python
# Check if wg is already available in container
result = await environment.exec(command="which wg")
if result.return_code != 0:
    # Upload via Harbor's upload mechanism or docker cp
    await environment.upload(local_path=self._wg_binary_host_path, remote_path="/usr/local/bin/wg")
    await environment.exec(command="chmod +x /usr/local/bin/wg")
```

### Files to modify

1. **`terminal-bench/wg/adapter.py`**:
   - Remove or deprecate `_run_docker_agent_loop()` (lines 543-754) and `AGENT_TOOLS` (lines 484-540)
   - Add `_run_native_executor()` that uses `environment.exec()` to run wg commands inside Docker
   - Modify `WorkgraphAgent.run()` to call `_run_native_executor()` instead of `_run_docker_agent_loop()`
   - Modify `WorkgraphAgent.setup()` to install wg binary and initialize graph inside the container (not on host)
   - Keep `_collect_agent_metrics()` but adapt to read from container via `environment.exec()`

2. **`terminal-bench/reproduce.sh`**:
   - Add `--mounts-json` for wg binary mount (if using volume mount approach)
   - Already updated: model name fix, conditions A-F

### What NOT to change

- `CONDITION_CONFIG` — stays the same, just drives config.toml generation
- `_write_trial_wg_config()` — reuse the config generation logic, just write it inside container instead of host
- `_write_trial_bundle()` — same, write inside container
- Condition-specific agent classes (ConditionAAgent through ConditionFAgent) — no changes needed
- `WG_QUICK_GUIDE`, `CONDITION_F_MEMORY` — these get injected by wg's context assembly, not by Python

### Key consideration: How wg service start works

`wg service start` is the coordinator. It:
1. Reads `.workgraph/config.toml` for executor type, model, context scope
2. Finds ready tasks
3. Spawns `wg native-exec` for each ready task
4. Native-exec reads the task description, assembles context based on scope, calls the LLM
5. Agent uses tools (bash, read_file, write_file, edit_file, glob, grep + wg tools based on condition)
6. When agent finishes, coordinator runs `--verify` command
7. Task transitions to done/failed

This all needs to happen **inside** the Docker container. The `OPENROUTER_API_KEY` must be available inside the container for the Rust HTTP client to reach OpenRouter.

### Verification plan

1. Single-task smoke test: `harbor run` with ConditionAAgent on `fix-git`, verify wg executor is used (check for `.workgraph/agents/*/stream.jsonl` inside container)
2. Verify Condition F: same test, check that graph context and wg tools are available to the agent
3. Compare results with existing pilot data to sanity-check behavior parity
