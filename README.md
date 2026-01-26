# workgraph

A task graph for getting things done. Works for humans, works for AI agents, works for both at once.

## What is this?

You've got tasks. Some block others. Multiple people (or AIs) need to coordinate without stepping on each other. Workgraph handles that.

```bash
wg init
wg add "Design the API"
wg add "Build the backend" --blocked-by design-the-api
wg add "Write tests" --blocked-by build-the-backend

wg ready        # what can I work on?
wg claim design-the-api --actor erik
wg done design-the-api   # automatically unblocks the next task
```

Tasks flow through `open → in-progress → done`. Dependencies are respected. No one works on the same thing twice.

## Install

From source:

```bash
git clone https://github.com/graphwork/workgraph
cd workgraph
cargo install --path .
```

Or directly via cargo:

```bash
cargo install --git https://github.com/graphwork/workgraph
```

Verify it works:

```bash
wg --help
```

## Setup

### 1. Initialize in your project

```bash
cd your-project
wg init
```

This creates `.workgraph/` with your task graph.

### 2. Add some tasks

```bash
# Simple task
wg add "Set up CI pipeline"

# Task with a blocker
wg add "Deploy to staging" --blocked-by set-up-ci-pipeline

# Task with metadata
wg add "Implement auth" \
  --hours 8 \
  --skill rust \
  --skill security \
  --deliverable src/auth.rs
```

### 3. Register yourself (or your agent)

```bash
# Human
wg actor add erik --name "Erik" --role engineer -c rust -c python

# AI agent
wg actor add claude --name "Claude" --role agent -c coding -c testing -c docs
```

### 4. Start working

```bash
wg ready                         # see what's available
wg claim set-up-ci-pipeline --actor erik
# ... do the work ...
wg done set-up-ci-pipeline       # unblocks deploy-to-staging
```

## Using with AI Coding Assistants

Workgraph includes a skill definition that teaches AI assistants the full protocol. The skill lives at `.claude/skills/wg.md` in this repo.

### Claude Code

Copy the skill to your project:

```bash
mkdir -p .claude/skills
cp /path/to/workgraph/.claude/skills/wg.md .claude/skills/
```

Then add to your `CLAUDE.md`:

```markdown
Use workgraph for task management.
```

Claude Code will automatically load the skill and follow the protocol.

### OpenCode / Codex / Other Agents

These systems typically use `AGENTS.md` for instructions. Copy the skill content into your `AGENTS.md`:

```bash
cat /path/to/workgraph/.claude/skills/wg.md >> AGENTS.md
```

Or just add the core instruction:

```markdown
## Task Management

Use workgraph (`wg`) for task coordination:
- `wg ready` to see available tasks
- `wg claim <task> --actor <name>` before working
- `wg done <task>` when complete
- `wg fail <task> --reason "..."` if blocked

See `wg --help` for all commands.
```

### What the skill teaches

The full skill covers:
- When to use workgraph (multi-step projects, cross-session work, coordination)
- The claim/work/done protocol
- Progress logging and artifact tracking
- Planning with dependencies
- Analysis commands for project health
- Multi-agent coordination rules

It's designed to be self-contained - an agent can read it and immediately know how to participate.

## Agentic workflows

### Pattern 1: Human plans, agent executes

You define the work, agent does it:

```bash
# You: create the plan
wg add "Refactor auth module" --skill rust
wg add "Update tests" --blocked-by refactor-auth-module --skill testing
wg add "Update docs" --blocked-by refactor-auth-module --skill docs

# Agent: execute
wg agent --actor claude --max-tasks 10
```

The agent will work through ready tasks, respecting dependencies.

### Pattern 2: Agent plans and executes

Let the agent figure out what needs doing:

```markdown
# In CLAUDE.md or your prompt:

Break down this goal into tasks using workgraph:
1. Analyze what needs to be done
2. Create tasks with `wg add`, linking dependencies with --blocked-by
3. Work through them with `wg ready` / `wg claim` / `wg done`
4. If you discover more work, add it to the graph
```

### Pattern 3: Top-level coordinator

One agent manages the work, spawns sub-agents for execution:

```markdown
# Coordinator prompt:

You are a project coordinator. Your job:
1. Check `wg ready` for available work
2. For each ready task, spawn a sub-agent to handle it
3. Sub-agents should `wg claim`, do the work, then `wg done` or `wg fail`
4. Monitor progress with `wg list` and `wg analyze`
5. Replan if needed - add tasks, adjust dependencies
6. Continue until `wg ready` returns nothing and all tasks are done
```

### Pattern 4: Parallel agents

Multiple agents working simultaneously:

```bash
# Terminal 1
wg agent --actor claude-1

# Terminal 2
wg agent --actor claude-2

# Terminal 3
wg agent --actor claude-3
```

Each agent claims different tasks. The claim mechanism prevents conflicts.

### Pattern 5: Mixed human + AI

```bash
# Human claims the design work
wg claim design-api --actor erik

# Agent handles implementation once design is done
wg agent --actor claude
```

The agent waits for your work to complete before touching dependent tasks.

## The recommended flow

For most projects:

1. **Plan first**: Sketch out the major tasks and dependencies
   ```bash
   wg add "Goal task"
   wg add "Step 1"
   wg add "Step 2" --blocked-by step-1
   wg add "Step 3" --blocked-by step-2
   # ... etc
   ```

2. **Check the structure**:
   ```bash
   wg analyze        # health check
   wg critical-path  # what's the longest chain?
   wg bottlenecks    # what should we prioritize?
   ```

3. **Execute**: Either manually or with agents
   ```bash
   wg agent --actor claude --once  # one task at a time, review between
   # or
   wg agent --actor claude         # let it run
   ```

4. **Adapt**: As you learn more, update the graph
   ```bash
   wg add "New thing we discovered" --blocked-by whatever
   wg fail stuck-task --reason "Need to rethink this"
   wg retry stuck-task  # when ready to try again
   ```

5. **Ship**: When `wg ready` is empty and everything important is done, you're there.

## Key concepts

**Tasks** have a status (`open`, `in-progress`, `done`, `failed`, `abandoned`) and can block other tasks.

**Actors** are humans or AI agents. They claim tasks to work on them.

**The graph** is tasks connected by "blocked-by" relationships. A task is blocked until all its blockers are done.

**Context flow**: Tasks can declare inputs (what they need) and deliverables (what they produce). Use `wg context <task>` to see what's available.

**Trajectories**: For AI agents, `wg trajectory <task>` suggests the best order to claim related tasks, minimizing context switches.

## Analysis commands

```bash
wg ready           # what can be worked on now?
wg list            # all tasks
wg show <id>       # full task details
wg why-blocked <id> # trace the blocker chain
wg impact <id>     # what depends on this?
wg bottlenecks     # tasks blocking the most work
wg critical-path   # longest dependency chain
wg forecast        # when will we be done?
wg analyze         # comprehensive health report
```

## Storage

Everything lives in `.workgraph/graph.jsonl`. One JSON object per line. Human-readable, git-friendly, easy to hack on.

```jsonl
{"kind":"task","id":"design-api","title":"Design the API","status":"done"}
{"kind":"task","id":"build-backend","title":"Build the backend","status":"open","blocked_by":["design-api"]}
{"kind":"actor","id":"claude","name":"Claude","role":"agent","capabilities":["coding","testing"]}
```

Configuration is in `.workgraph/config.toml`:

```toml
[agent]
executor = "claude"
model = "opus-4-5"
interval = 10

[project]
name = "My Project"
```

## More docs

- [docs/COMMANDS.md](docs/COMMANDS.md) - Complete command reference
- [docs/AGENT-GUIDE.md](docs/AGENT-GUIDE.md) - Deep dive on agent operation

## License

MIT
