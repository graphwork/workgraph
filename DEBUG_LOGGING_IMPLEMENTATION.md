# Debug Logging Implementation for Prompt Capture

## Overview

Added debug logging functionality to capture complete prompt text sent to spawned agents, controlled by the `WG_DEBUG_PROMPTS` environment variable.

## Implementation Details

### Location 1: `src/service/executor.rs:build_prompt()`

- **Function**: `build_prompt(vars: &TemplateVars, scope: ContextScope, ctx: &ScopeContext) -> String`
- **Location**: Lines ~853-873
- **Purpose**: Captures the complete assembled prompt that gets sent to agents

**Debug Output Includes**:
- Task ID
- Context scope (clean, task, graph, full)
- Model name
- Prompt length in characters  
- Complete prompt content

### Location 2: `src/commands/spawn/execution.rs:spawn_agent_inner()`

- **Function**: `spawn_agent_inner(dir: &Path, task_id: &str, executor_name: &str, model: Option<&str>) -> Result<SpawnResult>`
- **Location**: Lines ~327-347
- **Purpose**: Captures metadata about the agent spawning process

**Debug Output Includes**:
- Task ID
- Executor type (claude, amplifier, native)
- Model name
- Context scope
- Execution mode
- Agent identity (if assigned)

## Usage

To enable debug logging, set the environment variable:

```bash
export WG_DEBUG_PROMPTS=1
# or
WG_DEBUG_PROMPTS=1 wg spawn <task-id> --executor claude
```

## Output Location

Debug logs are written to: `/tmp/wg_debug_prompts.log`

## Log Format

```
=== WG DEBUG: Spawning Agent ===
Task ID: test-task
Executor: claude
Model: claude-sonnet-4-20250514
Context Scope: Task
Execution Mode: full
Agent Identity: Default (no specific agent assigned)
=== End of Spawn Metadata ===

=== WG DEBUG: Assembled Prompt for Task test-task ===
Scope: Task
Model: claude-sonnet-4-20250514
Prompt length: 12345 characters
Prompt content:
[Full prompt content here...]
=== End of Prompt ===
```

## Verification

1. Create a test task: `wg add "Test task" --verify "echo done"`
2. Spawn with debug enabled: `WG_DEBUG_PROMPTS=1 wg spawn test-task --executor claude`
3. Check log file: `cat /tmp/wg_debug_prompts.log`

The log should contain both spawn metadata and the complete prompt text.