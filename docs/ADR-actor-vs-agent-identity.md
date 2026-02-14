# ADR: Unifying Actor and Agent Identity

**Status:** Accepted
**Date:** 2026-02-13
**Decision:** Collapse Actor into Agent — one unified identity model

## Context

Workgraph originally had two overlapping identity systems:

1. **Actor** (graph.rs) — a node in the work graph representing a human or AI that can perform work. Had capabilities, rate, capacity, trust levels, heartbeats, and a Matrix binding. Stored in `graph.jsonl` as `kind: "actor"`.

2. **Agent** (agency.rs) — a content-addressed pairing of a Role (what it does) and a Motivation (why/how it acts). Had performance tracking, lineage, and prompt rendering. Stored as YAML in `.workgraph/agency/agents/`.

The two systems overlapped in confusing ways:
- Both claimed to represent "an agent"
- Tasks had two identity pointers: `assigned` (actor) and `agent` (agency)
- Skills existed in both as `Actor.capabilities` (flat tags) and `Role.skills` (rich definitions)
- `Actor.role` (free-text job title) collided with the agency `Role` struct
- Three separate "agent registries" existed: actor nodes, agency agents, and service process entries

See the original analysis below for the full overlap mapping.

## Decision

**Collapse Actor into Agent.** The Actor struct and all `wg actor` commands have been removed. The Agent struct in the agency system now carries the operational fields that Actor previously owned:

| Old Actor field | New home |
|---|---|
| `capabilities` | `Agent.capabilities` |
| `rate` | `Agent.rate` |
| `capacity` | `Agent.capacity` |
| `trust_level` | `Agent.trust_level` |
| `matrix_user_id` | `Agent.contact` (generalized) |
| `actor_type` (Human/Agent) | `Agent.executor` — human executors (matrix, email, shell) vs AI executors (claude) |
| `role` (free-text) | Removed — the agency Role struct serves this purpose |
| `context_limit` | Removed — model-specific, not agent-specific |
| `response_times` | Removed — not used in practice |
| `last_seen` | Handled by service registry heartbeats |

### What an Agent is now

An Agent is a **unified identity** that can represent a human or an AI:

```
Agent = Role (what) + Motivation (why) + Operational fields (how)
```

- **AI agents** require a role and motivation (which drive prompt injection)
- **Human agents** can optionally have a role and motivation, but primarily need `--contact` and a human executor (`matrix`, `email`, `shell`)
- All agents can have capabilities (for task matching), rate (for cost forecasting), capacity (for workload planning), and a trust level (for permission gating)

### CLI changes

| Before | After |
|---|---|
| `wg actor add erik --role engineer -c rust` | `wg agent create "Erik" --executor matrix --contact "@erik:server" --capabilities rust` |
| `wg actor add claude --role agent -c coding` | `wg agent create "Claude Coder" --role <hash> --motivation <hash> --capabilities coding` |
| `wg actor list` | `wg agent list` |
| `wg claim <task> --actor erik` | `wg claim <task> --actor erik` (log field, not identity system) |
| `wg next --actor claude` | `wg next --actor claude` (session identifier) |

### ID generation

- **AI agents**: `SHA-256(role_id + motivation_id)` — same content-addressed scheme as before
- **Human agents** (no role/motivation): `SHA-256(name + executor)` — deterministic from name and executor

## Rationale

The original ADR recommended keeping Actor and Agent separate ("clearly separate their concerns"). After implementation experience, this turned out to be wrong:

1. **Users never understood the distinction.** "Actor" was jargon that didn't map to any mental model users already had. Everyone called them "agents."

2. **The operational fields belong on the agent.** Rate, capacity, and capabilities are properties of whoever is doing the work — that's the agent. Splitting them across two systems created friction without benefit.

3. **Human vs AI is an executor distinction, not a type distinction.** The `actor_type: Human | Agent` field was really saying "how does this entity receive work?" That's what the executor field already answers.

4. **One fewer concept to learn.** New users now learn: tasks, agents, roles, motivations. Not: tasks, actors, agents, roles, motivations.

## Consequences

- **Backward compatibility**: The parser silently skips `kind: "actor"` nodes in `graph.jsonl` for projects that haven't re-initialized
- **`wg actor` commands**: Removed entirely
- **`LogEntry.actor` field**: Retained — this is metadata about who performed an action, not part of the identity system
- **`wg agent run --actor <id>`**: Retained — this is a session identifier for the autonomous loop, not an Actor reference
- **Forecasting/workload commands**: Now operate on Agent data (capabilities, rate, capacity) instead of Actor data

## Migration

No explicit migration is needed. Old `kind: "actor"` nodes are silently ignored during parsing. Users should recreate their identities as agents:

```bash
# Old
wg actor add erik --name "Erik" --role engineer -c rust -c python

# New
wg agent create "Erik" --executor matrix --contact "@erik:server" --capabilities rust,python
```

---

## Original Analysis (preserved for historical reference)

The following sections document the state of both systems before the merge, preserved for context.

### System 1: Actor (removed)

An Actor was a node in the work graph stored in `graph.jsonl` alongside tasks and resources, representing a human or AI that can perform work. It tracked operational identity (capabilities, rate, capacity), availability (heartbeat, response times), trust levels, and human integration (Matrix binding).

### System 2: Agent (agency.rs)

An Agent in the agency system is a persistent, named pairing of a Role (what the agent does) and a Motivation (why and under what constraints). This system remains and now carries the unified identity.

### Where they overlapped

- Both claimed to represent "an agent"
- Tasks had `assigned` (actor ID) and `agent` (agent content-hash) — two identity pointers
- `Actor.capabilities` and `Role.skills` both tracked "what this agent can do"
- `Actor.role` (free-text) collided with the agency `Role` struct
- Three registries: actor nodes, agency agents, service process entries

### Resolution

All operational fields merged into Agent. Actor system removed. Single identity model.
