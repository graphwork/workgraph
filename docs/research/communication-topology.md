# Communication Topology: Multi-Bot, Multi-User, Scoped Routing

Design document for how workgraph reaches the right human, through the right channel, at the right scope.

---

## Table of Contents

1. [Actor Model](#1-actor-model)
2. [Scoping Model](#2-scoping-model)
3. [Configuration Schema](#3-configuration-schema)
4. [Inheritance and Override Rules](#4-inheritance-and-override-rules)
5. [Simple Default Case](#5-simple-default-case)
6. [Migration from Current notify.toml](#6-migration-from-current-notifytoml)
7. [Examples](#7-examples)

---

## 1. Actor Model

### Decision: Actors are named human reachability profiles

An **actor** is a named entity representing a human (or group of humans) who can be reached via one or more notification channels. Actors are the answer to "who should be notified?" — separate from "how do I reach them?" (channels) and "about what?" (routing rules).

```
Actor = name + reachability[]
Reachability = channel_type + target (platform-specific address)
```

### Why actors, not just channels

The current system (`notify.toml`) binds event types directly to channel types: "send urgent events to telegram." This works for one person but breaks down when:

- Two people need different notifications from the same graph
- One person wants Telegram for project A but Slack for project B
- A subgraph should notify a different person than the rest

The actor layer decouples **who** from **how**. A task says "notify alice" and Alice's actor profile declares she's reachable via Telegram bot X at chat Y. If Alice later switches to Slack, only her profile changes — no task or routing config updates needed.

### Actor definition

```toml
[actors.alice]
name = "Alice"

  # Reachability: ordered by preference
  [[actors.alice.channels]]
  type = "telegram"
  bot_token = "123:ABC..."      # optional — defaults to global [telegram] config
  chat_id = "11111111"

  [[actors.alice.channels]]
  type = "email"
  to = "alice@company.com"

[actors.bob]
name = "Bob"
  [[actors.bob.channels]]
  type = "slack"
  channel = "#bob-notifications"

  [[actors.bob.channels]]
  type = "sms"
  to = "+15559876543"
```

An actor's `channels` list is ordered by preference. The notification router tries them in order, falling through on failure (same as the current escalation chain).

### Relationship to the agency system

The agency system already has the concept of human agents — agents with executor type `"matrix"`, `"email"`, or `"shell"` (see `agency/types.rs:326-331`, `is_human_executor()`). Each `Agent` struct has an optional `contact` field.

Actors bridge these two systems:

- **Agency agents** describe what a human *does* (their role, skills, tradeoffs)
- **Actors** describe how a human is *reached* (their notification channels)
- An agency agent with `executor = "matrix"` can reference an actor: `contact = "actor:alice"`
- When the coordinator needs human input for a task assigned to a human agent, it resolves the agent's `contact` to an actor, then uses the actor's channels

This means the agency system does **not** need to know about Telegram, Slack, etc. It only knows "this agent is a human reachable via actor X."

### Roles: owner, reviewer, oncall

Actors can be assigned **roles** within a project or subgraph scope:

| Role | Receives | Purpose |
|------|----------|---------|
| `owner` | All failures, completions, approvals | The person responsible for the project/subgraph |
| `reviewer` | Approval requests, quality gates | Reviews work before it proceeds |
| `oncall` | Urgent escalations only | On-call rotation for critical issues |

Roles are not a separate concept — they are labels applied when binding an actor to a scope (see §2). A single actor can have multiple roles, and a single role can have multiple actors (e.g., two reviewers).

### Bots as first-class entities

Bots (Telegram bots, Slack apps, Discord bots) are **not** first-class entities in the actor model. They are channel configuration — an implementation detail of how an actor is reached. A bot token lives in the actor's channel config or in the global channel defaults.

Rationale: making bots first-class would create a `Bot → Actor → Channel` indirection layer that adds complexity without solving a real problem. The question "which bot do I use?" is answered by the channel config. The question "who do I notify?" is answered by the actor. These are naturally separate.

---

## 2. Scoping Model

### Decision: Four-level scope hierarchy with downward inheritance

Notification targets are resolved through a four-level hierarchy:

```
Global  →  Project  →  Subgraph  →  Task
```

Each level can specify actors and routing rules. Unspecified values inherit from the parent level.

### How scopes bind

| Scope | Where defined | What it controls |
|-------|--------------|-----------------|
| **Global** | `~/.config/workgraph/notify.toml` | Default actors, channels, and routing for all projects |
| **Project** | `.workgraph/notify.toml` | Per-project overrides — different actors, channels, or routing rules |
| **Subgraph** | Task tags or explicit `notify` field on root task of a subgraph | Weakly connected components that should notify a different actor |
| **Task** | `notify` field on individual task | Single-task override (rare, for special cases like "notify the CEO when deploy completes") |

### Subgraph scoping mechanism

Subgraphs don't have a first-class config file. Instead, they use **tag-based routing** combined with **explicit assignment on root tasks**.

A subgraph root task can declare:

```bash
wg add "Auth subsystem overhaul" \
  --tag "notify:alice:owner" \
  --tag "notify:bob:reviewer"
```

All descendant tasks (tasks reachable via `--after` edges) **inherit** their ancestor's notification bindings unless they override them. This uses the existing task graph ancestry — no new data structure needed.

When the notification router resolves who to notify for a task:

1. Check the task itself for `notify:*` tags
2. Walk up the dependency graph (reverse `--after` edges) looking for the nearest ancestor with `notify:*` tags
3. Fall back to project-level actors
4. Fall back to global actors

### Tag format

```
notify:{actor_name}:{role}
```

Examples:
- `notify:alice:owner` — Alice is the owner for this scope
- `notify:bob:reviewer` — Bob reviews this scope
- `notify:oncall-team:oncall` — The on-call team handles escalations

### Why tags, not a separate routing config per subgraph

- Tags already exist and are propagated through the graph
- No new config files or data structures needed
- Tags are visible in `wg show`, `wg viz`, `wg list --tags`
- Tags compose naturally — a task can have multiple `notify:` tags
- Tags can be set at task creation time or modified later with `wg tag`

---

## 3. Configuration Schema

### Extended notify.toml

The new schema extends the current `notify.toml` with actor definitions and scoped routing. The existing fields (`[routing]`, `[escalation]`, `[telegram]`, etc.) continue to work unchanged.

```toml
# ~/.config/workgraph/notify.toml
# (or .workgraph/notify.toml for project-level)

# ─── Channel defaults ───────────────────────────────────────────────
# These provide default credentials for channel types.
# Actors can reference these or override with their own credentials.

[telegram]
bot_token = "123456:ABC-DEF..."
chat_id = "12345678"

[slack]
bot_token = "xoxb-..."
app_token = "xapp-..."

[email]
smtp_host = "smtp.gmail.com"
smtp_port = 587
username = "notifications@company.com"
password = "app-password"
from = "notifications@company.com"

[webhook]
url = "https://internal.company.com/wg-events"
secret = "hmac-secret"

# ─── Actors ──────────────────────────────────────────────────────────

[actors.alice]
name = "Alice Chen"

  [[actors.alice.channels]]
  type = "telegram"
  chat_id = "11111111"          # uses global bot_token by default

  [[actors.alice.channels]]
  type = "email"
  to = "alice@company.com"

[actors.bob]
name = "Bob Martinez"

  [[actors.bob.channels]]
  type = "slack"
  channel = "#bob-dm"

  [[actors.bob.channels]]
  type = "sms"
  to = "+15559876543"

[actors.oncall]
name = "On-Call Engineer"

  [[actors.oncall.channels]]
  type = "webhook"
  url = "https://pagerduty.com/wg-integration"

# ─── Routing ─────────────────────────────────────────────────────────
# Routes events to actors by role. Replaces direct event→channel mapping.

[routing]
# Legacy format (still supported): event_type = [channel_names]
# default = ["telegram"]

# New format: event_type = [actor:role specifications]
owner = ["alice"]               # Default project owner
reviewer = ["bob"]              # Default reviewer
oncall = ["oncall"]             # Default on-call

# Event→role mapping: which roles get notified for which events
[routing.events]
task_failed = ["owner", "oncall"]
task_blocked = ["owner"]
task_ready = []                 # No notification by default
approval = ["reviewer", "owner"]
urgent = ["oncall", "owner"]    # oncall first, then owner

# ─── Escalation ──────────────────────────────────────────────────────

[escalation]
approval_timeout = 1800         # 30 min before trying next channel/actor
urgent_timeout = 3600           # 1h before trying next channel/actor
```

### Schema summary

| Section | Purpose | New? |
|---------|---------|------|
| `[telegram]`, `[slack]`, etc. | Channel credentials (defaults) | No — existing |
| `[actors.*]` | Named human reachability profiles | **Yes** |
| `[actors.*.channels]` | Per-actor channel list (ordered) | **Yes** |
| `[routing]` | Default actor role bindings | Extended |
| `[routing.events]` | Event → role mapping | **Yes** |
| `[escalation]` | Timeout configuration | No — existing |

### Rust types

```rust
/// An actor: a named human reachability profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub name: String,
    pub channels: Vec<ActorChannel>,
}

/// A channel binding within an actor profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorChannel {
    /// Channel type: "telegram", "slack", "email", "sms", "webhook", etc.
    #[serde(rename = "type")]
    pub channel_type: String,
    /// Channel-specific config (chat_id, to, channel, url, etc.)
    /// Merged with global channel defaults at resolution time.
    #[serde(flatten)]
    pub config: HashMap<String, toml::Value>,
}

/// Event-to-role routing configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventRoutingConfig {
    #[serde(default)]
    pub task_failed: Vec<String>,    // role names
    #[serde(default)]
    pub task_blocked: Vec<String>,
    #[serde(default)]
    pub task_ready: Vec<String>,
    #[serde(default)]
    pub approval: Vec<String>,
    #[serde(default)]
    pub urgent: Vec<String>,
}
```

---

## 4. Inheritance and Override Rules

### Resolution algorithm

When the system needs to notify about a task event, it resolves the notification target through this algorithm:

```
fn resolve_notification_targets(task, event_type, project) -> Vec<(Actor, Channel)>:
    # 1. Determine which roles should be notified for this event type
    roles = resolve_event_roles(event_type, project)
    # e.g., task_failed → ["owner", "oncall"]

    # 2. For each role, find the actor at the most specific scope
    targets = []
    for role in roles:
        actor = resolve_actor_for_role(task, role, project)
        if actor:
            targets.push(actor)

    return targets

fn resolve_actor_for_role(task, role, project) -> Option<Actor>:
    # Check task-level tags
    if tag = task.tags.find("notify:*:{role}"):
        return lookup_actor(tag.actor_name)

    # Walk up dependency graph
    for ancestor in task.ancestors():
        if tag = ancestor.tags.find("notify:*:{role}"):
            return lookup_actor(tag.actor_name)

    # Check project-level routing config
    if actor_name = project.notify_config.routing.get(role):
        return lookup_actor(actor_name)

    # Check global routing config
    if actor_name = global.notify_config.routing.get(role):
        return lookup_actor(actor_name)

    return None

fn resolve_event_roles(event_type, project) -> Vec<String>:
    # Check project event config, fall back to global
    if roles = project.notify_config.routing.events.get(event_type):
        return roles
    if roles = global.notify_config.routing.events.get(event_type):
        return roles
    # Built-in defaults
    match event_type:
        TaskFailed => ["owner"]
        Approval => ["reviewer", "owner"]
        Urgent => ["oncall", "owner"]
        _ => []
```

### Override semantics

- **Replace, don't merge.** If a project defines `[routing.events]`, it replaces the global event routing entirely. If a task tag says `notify:carol:owner`, Carol replaces Alice as owner for that scope. No merging of actor lists.
- **Scope specificity wins.** Task tags override subgraph tags override project config override global config.
- **Actors are global.** Actor definitions are always resolved from the global `~/.config/workgraph/notify.toml`. Project-level configs can add project-specific actors, but they don't override global actor definitions with the same name — names must be unique.
- **Channel defaults merge.** An actor's channel config merges with global channel defaults. If Alice's Telegram channel specifies only `chat_id`, the `bot_token` comes from the global `[telegram]` section.

### Interaction with telegram-global-routing.md

The poll-leader model from `telegram-global-routing.md` remains unchanged. It operates at the transport layer — "how do multiple repos share a single Telegram bot?" The actor model operates at the routing layer — "which human gets this message?"

The two layers compose:

1. **Routing layer** (this design): Resolves `(task, event) → (actor, channel_config)`
2. **Transport layer** (telegram-global-routing.md): Handles `(channel_config) → actual API call`, including poll-leader election, message-map tracking, and inbound routing

When the routing layer says "send to Alice via Telegram," the transport layer uses the shared poll-leader to send the message and registers it in `telegram-routing.json` for reply tracking. The `actor_name` is included in the message_map entry so inbound replies can be attributed to the correct actor.

---

## 5. Simple Default Case

### Single user, single bot: minimal config

The simplest possible config is what exists today — no actors at all:

```toml
# ~/.config/workgraph/notify.toml (minimal)

[routing]
default = ["telegram"]

[telegram]
bot_token = "123456:ABC-DEF..."
chat_id = "12345678"
```

This works because of the **implicit default actor**:

- When no actors are defined, the system creates an implicit actor from the channel defaults
- The `[telegram]` section's `chat_id` becomes the target
- The `[routing].default` channels are used for all events
- All notifications go to that single chat

**Zero-config progression:**

| Stage | Config needed | What works |
|-------|--------------|-----------|
| 1. No notify.toml | Nothing | No notifications (silent) |
| 2. Bot token + chat_id | 4 lines | All failures/approvals go to one Telegram chat |
| 3. Add routing rules | +5 lines | Different event types go to different channels |
| 4. Add actors | +10 lines per actor | Multi-user routing |
| 5. Add scope tags | `--tag notify:...` | Per-subgraph routing |

Each stage is strictly additive. You never need to restructure existing config to reach the next stage.

### Graceful degradation

If the actor system is configured but resolution fails (actor not found, all channels fail), the system falls back:

1. Try the resolved actor's channels in order
2. If all actor channels fail, try the `[routing].default` channels
3. If no default, try the `[routing].owner` actor's channels
4. If nothing works, log the failure and continue (never crash)

---

## 6. Migration from Current notify.toml

### Backward compatibility: full

The current `notify.toml` format is a valid subset of the new format. No migration is required.

| Current field | Still works? | What happens |
|--------------|-------------|-------------|
| `[routing].default = ["telegram"]` | Yes | Used as fallback when no actor matches |
| `[routing].urgent = ["telegram", "sms"]` | Yes | Interpreted as channel escalation chain (legacy mode) |
| `[routing].approval = ["telegram"]` | Yes | Same |
| `[escalation]` | Yes | Unchanged |
| `[telegram]`, `[email]`, etc. | Yes | Used as channel defaults |

### Detection of legacy vs. new mode

The system detects which mode to use:

```rust
fn is_actor_mode(config: &NotifyConfig) -> bool {
    !config.actors.is_empty()
}
```

- **No `[actors]` section:** Legacy mode. `[routing].default/urgent/approval` are treated as channel type lists, exactly as today.
- **`[actors]` section present:** Actor mode. `[routing]` is interpreted as role → actor mappings, `[routing.events]` maps events to roles.

### Migration steps (optional, for users who want multi-user)

1. **Define actors** for each human:
   ```toml
   [actors.me]
   name = "Me"
     [[actors.me.channels]]
     type = "telegram"
     chat_id = "12345678"    # Same chat_id that was in [telegram]
   ```

2. **Update routing** to reference actors instead of channels:
   ```toml
   [routing]
   owner = ["me"]

   [routing.events]
   task_failed = ["owner"]
   approval = ["owner"]
   urgent = ["owner"]
   ```

3. **Channel defaults stay.** The `[telegram]` section with `bot_token` stays as-is — actors reference it implicitly.

### Code changes required

| File | Change |
|------|--------|
| `src/notify/config.rs` | Add `actors: HashMap<String, Actor>` field, `events: EventRoutingConfig` field. Add `is_actor_mode()`. |
| `src/notify/mod.rs` | Add `ActorRouter` that wraps `NotificationRouter` and resolves actors → channels. |
| `src/notify/dispatch.rs` | `format_event()` already accepts optional `repo_name`. Extend to accept optional `actor_name` for message attribution. |
| `src/commands/service/mod.rs` | In `try_dispatch_notifications()`, resolve actors from task tags before dispatching. |
| `src/graph.rs` | Add helper to walk ancestors for `notify:` tags. |

---

## 7. Examples

### Example 1: Single user, single bot (minimal)

**Scenario:** One developer, personal project, just wants Telegram failure alerts.

**Config:** `~/.config/workgraph/notify.toml`

```toml
[routing]
default = ["telegram"]

[telegram]
bot_token = "123456:ABC-DEF..."
chat_id = "12345678"
```

**Behavior:**
- All task failures and approval requests → Telegram chat 12345678
- Task ready/blocked → no notification (default)
- Inbound messages from Telegram → routed to last-active project (per telegram-global-routing.md)

**Graph commands:** No special tags needed. Everything just works.

---

### Example 2: Team with roles

**Scenario:** Three-person team. Alice is the project lead, Bob reviews code, Carol is on-call this week.

**Config:** `~/.config/workgraph/notify.toml`

```toml
[telegram]
bot_token = "123456:ABC-DEF..."

[email]
smtp_host = "smtp.company.com"
smtp_port = 587
username = "wg-notifications@company.com"
password = "app-password"
from = "wg-notifications@company.com"

[actors.alice]
name = "Alice Chen"
  [[actors.alice.channels]]
  type = "telegram"
  chat_id = "11111111"
  [[actors.alice.channels]]
  type = "email"
  to = "alice@company.com"

[actors.bob]
name = "Bob Martinez"
  [[actors.bob.channels]]
  type = "telegram"
  chat_id = "22222222"

[actors.carol]
name = "Carol Wu"
  [[actors.carol.channels]]
  type = "telegram"
  chat_id = "33333333"
  [[actors.carol.channels]]
  type = "sms"
  to = "+15559876543"

[routing]
owner = ["alice"]
reviewer = ["bob"]
oncall = ["carol"]

[routing.events]
task_failed = ["owner", "oncall"]
task_blocked = ["owner"]
approval = ["reviewer", "owner"]
urgent = ["oncall", "owner"]

[escalation]
approval_timeout = 1800
urgent_timeout = 900
```

**Behavior:**
- Task fails → Alice gets Telegram msg, Carol gets Telegram msg
- Task needs approval → Bob gets Telegram msg. If no response in 30min, Alice gets notified too.
- Urgent event → Carol gets Telegram. If no response in 15min, try Carol's SMS. Then Alice gets Telegram.
- Task blocked → Alice gets Telegram
- Reply routing: each person's reply is routed back based on the message_map (per telegram-global-routing.md)

**Graph commands:** No special tags needed — the global roles apply.

---

### Example 3: Multi-project with different channels

**Scenario:** Alice manages two projects. Project Alpha uses Telegram. Project Beta uses Slack (because the beta team lives in Slack).

**Global config:** `~/.config/workgraph/notify.toml`

```toml
[telegram]
bot_token = "123456:ABC-DEF..."

[slack]
bot_token = "xoxb-..."
app_token = "xapp-..."

[actors.alice]
name = "Alice Chen"
  [[actors.alice.channels]]
  type = "telegram"
  chat_id = "11111111"
  [[actors.alice.channels]]
  type = "slack"
  channel = "#alice-dm"

[routing]
owner = ["alice"]

[routing.events]
task_failed = ["owner"]
approval = ["owner"]
urgent = ["owner"]
```

**Project Alpha config:** `.workgraph/notify.toml` (in project-alpha repo)

```toml
# Uses global config — telegram is Alice's first channel preference
# No overrides needed
```

**Project Beta config:** `.workgraph/notify.toml` (in project-beta repo)

```toml
# Override Alice's channel preference for this project
[actors.alice]
name = "Alice Chen"
  [[actors.alice.channels]]
  type = "slack"
  channel = "#beta-notifications"
  [[actors.alice.channels]]
  type = "telegram"
  chat_id = "11111111"

[routing]
owner = ["alice"]

[routing.events]
task_failed = ["owner"]
approval = ["owner"]
```

**Behavior:**
- Project Alpha failure → Alice gets Telegram (global default, Telegram is her first channel)
- Project Beta failure → Alice gets Slack #beta-notifications (project override puts Slack first)
- If Slack is down for Beta → falls through to Telegram

---

### Example 4: Scoped routing within a graph (subgraph-level)

**Scenario:** A large project has an auth subsystem (managed by Dave) and a frontend (managed by Eve). The project lead (Alice) gets everything, but Dave and Eve only get notifications for their respective areas.

**Config:** `~/.config/workgraph/notify.toml`

```toml
[telegram]
bot_token = "123456:ABC-DEF..."

[actors.alice]
name = "Alice Chen"
  [[actors.alice.channels]]
  type = "telegram"
  chat_id = "11111111"

[actors.dave]
name = "Dave Kim"
  [[actors.dave.channels]]
  type = "telegram"
  chat_id = "44444444"

[actors.eve]
name = "Eve Johnson"
  [[actors.eve.channels]]
  type = "telegram"
  chat_id = "55555555"

[routing]
owner = ["alice"]

[routing.events]
task_failed = ["owner"]
approval = ["owner"]
```

**Graph setup:**

```bash
# Create the auth subsystem root — Dave is the subgraph owner
wg add "Auth subsystem overhaul" \
  --tag "notify:dave:owner" \
  --tag "notify:alice:reviewer"

# Auth subtasks inherit Dave as owner
wg add "Implement OAuth2 provider" --after auth-subsystem-overhaul
wg add "Add token refresh logic" --after implement-oauth2-provider
wg add "Write auth integration tests" --after add-token-refresh-logic

# Create the frontend root — Eve is the subgraph owner
wg add "Frontend redesign" \
  --tag "notify:eve:owner" \
  --tag "notify:alice:reviewer"

# Frontend subtasks inherit Eve as owner
wg add "New login page" --after frontend-redesign
wg add "Dashboard components" --after frontend-redesign
```

**Behavior:**
- "Add token refresh logic" fails → Dave gets Telegram (inherited `notify:dave:owner` from ancestor "Auth subsystem overhaul")
- "Dashboard components" needs approval → Alice gets Telegram (inherited `notify:alice:reviewer` from ancestor "Frontend redesign")
- "Auth subsystem overhaul" itself fails → Dave gets Telegram (direct tag), Alice gets Telegram (reviewer)
- A top-level task with no `notify:` ancestors → Alice gets Telegram (project-level owner fallback)

---

### Example 5: Different channel types for different scopes

**Scenario:** A project uses Telegram for day-to-day notifications, a webhook for CI/CD integration, and PagerDuty (via webhook) for production incidents.

**Config:** `.workgraph/notify.toml`

```toml
[actors.team]
name = "Dev Team"
  [[actors.team.channels]]
  type = "telegram"
  chat_id = "-100123456"        # Group chat

[actors.ci]
name = "CI Pipeline"
  [[actors.ci.channels]]
  type = "webhook"
  url = "https://ci.company.com/wg-webhook"
  secret = "ci-secret"

[actors.pagerduty]
name = "PagerDuty"
  [[actors.pagerduty.channels]]
  type = "webhook"
  url = "https://events.pagerduty.com/v2/enqueue"
  secret = "pd-routing-key"

[routing]
owner = ["team"]
oncall = ["pagerduty"]

[routing.events]
task_failed = ["owner"]
task_ready = []
approval = ["owner"]
urgent = ["oncall", "owner"]

# All events also go to CI
[routing.events.always]
# Future extension: broadcast to these actors for all events
# always = ["ci"]
```

**Graph setup for production tasks:**

```bash
wg add "Deploy to production" \
  --tag "notify:pagerduty:oncall" \
  --verify "curl -f https://api.prod.company.com/health"
```

**Behavior:**
- Regular task failure → team Telegram group chat
- Production deploy failure → PagerDuty webhook fires (creates incident), then team Telegram
- Task approval → team Telegram group chat

---

## Design Decisions Summary

| Question | Decision |
|----------|----------|
| What is the actor model? | Named human reachability profiles in `[actors.*]` with ordered channel lists |
| How are actors scoped? | 4-level hierarchy: global → project → subgraph → task |
| How does a task inherit its target? | Walk up dependency graph for `notify:*:*` tags, fall back to project config, fall back to global |
| How does a human declare reachability? | `[[actors.name.channels]]` sections listing channel type + target |
| What's the simplest model? | `[routing].default = ["telegram"]` + `[telegram]` section — no actors needed |
| How does it extend to multi-user? | Add `[actors.*]` sections and `[routing]` role bindings — additive, no restructuring |
| How does it interact with notify.toml? | Fully backward compatible. Actor mode activates when `[actors]` section exists |
| Should bots be first-class? | No — bots are channel config, not actors. Bot tokens live in channel defaults or actor channel overrides |
| How do roles work? | Labels (`owner`, `reviewer`, `oncall`) bound to actors at routing config or via task tags |
| How does this interact with the agency system? | Agency agents use `contact = "actor:name"` to reference their notification actor |

---

## Appendix: Interaction with Existing Systems

### notify.toml config.rs changes

The `NotifyConfig` struct adds:

```rust
pub struct NotifyConfig {
    pub routing: RoutingConfig,          // existing
    pub escalation: EscalationConfig,    // existing
    pub channels: HashMap<String, toml::Value>,  // existing (channel defaults)
    pub actors: HashMap<String, Actor>,  // NEW
}
```

The `RoutingConfig` is extended:

```rust
pub struct RoutingConfig {
    // Legacy fields (still work)
    pub default: Vec<String>,
    pub urgent: Vec<String>,
    pub approval: Vec<String>,
    pub digest: Vec<String>,

    // New: role → actor bindings
    pub owner: Vec<String>,
    pub reviewer: Vec<String>,
    pub oncall: Vec<String>,

    // New: event → role mapping
    pub events: Option<EventRoutingConfig>,
}
```

### NotificationRouter changes

The router gains an `ActorResolver`:

```rust
pub struct ActorResolver {
    global_actors: HashMap<String, Actor>,
    project_actors: HashMap<String, Actor>,
    role_bindings: HashMap<String, Vec<String>>,  // role → actor names
    event_roles: EventRoutingConfig,
}

impl ActorResolver {
    /// Resolve which actors should be notified for a given task event.
    pub fn resolve(
        &self,
        task: &Task,
        event_type: EventType,
        graph: &Graph,
    ) -> Vec<ResolvedTarget> {
        // 1. Map event_type → roles
        // 2. For each role, walk task → ancestors → project → global
        // 3. For each actor, resolve channel config (merge with defaults)
        // 4. Return (actor, channel_config) pairs
    }
}
```

### Graph ancestry traversal

New helper on `Graph`:

```rust
impl Graph {
    /// Walk up the dependency graph from a task, yielding ancestors
    /// in breadth-first order.
    pub fn ancestors(&self, task_id: &str) -> impl Iterator<Item = &Task> {
        // Reverse-traverse `after` edges
    }

    /// Find the first `notify:` tag matching a role in the task's ancestry.
    pub fn resolve_notify_tag(&self, task_id: &str, role: &str) -> Option<String> {
        // Check task, then BFS up ancestors
    }
}
```

### telegram-routing.json extension

The message_map entries gain an `actor` field for reply attribution:

```json
{
  "4401": {
    "project_dir": "/home/user/project-alpha/.workgraph",
    "task_id": "fix-auth-bug",
    "event_type": "task_failed",
    "actor": "alice",
    "timestamp": "2026-03-11T14:30:00Z"
  }
}
```

This lets the inbound routing layer know which actor the message was sent to, enabling per-actor reply handling when multiple actors share the same Telegram bot.
