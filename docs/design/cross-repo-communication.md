# Cross-Repo Workgraph Communication

> Enable workgraph instances across repositories to dispatch tasks, share dependencies, observe state, and share trace functions.

## 1. Problem Statement

Each workgraph instance is isolated to a single repository. In practice, projects depend on each other — a grants project (`~/grants/agentic-orgs`) needs features built in the workgraph tool itself (`~/workgraph`). Today this requires manually switching directories, running separate services, and manually coordinating task completion across repos. We need first-class cross-repo communication.

### 1.1 What Already Exists

| Capability | Status | Location |
|------------|--------|----------|
| Agency federation (roles/motivations/agents) | **Complete** | `src/federation.rs`, `src/commands/agency_{scan,pull,push,remote,merge}.rs` |
| Federation config (`.workgraph/federation.yaml`) | **Complete** | Named remotes for agency stores |
| Unix socket IPC | **Complete** | `.workgraph/service/daemon.sock`, JSON-RPC protocol |
| Trace function extract/instantiate | **Complete** | `src/commands/trace_{extract,instantiate}.rs`, `.workgraph/functions/` |
| Global `--dir` flag | **Complete** | Override workgraph directory for any command |
| Content-addressed agency entities | **Complete** | SHA-256 hashing in `src/agency.rs` |
| Task IDs | **Slug-based** | NOT content-addressed; generated from first 3 words of title |
| Cross-repo task dispatch | **Missing** | No `--repo` flag, no remote task creation |
| Cross-repo dependencies | **Missing** | `blocked_by` only accepts local task IDs |
| Trace function cross-repo sharing | **Missing** | Extract/instantiate only work within a single repo |
| Service-to-service communication | **Missing** | Each daemon only manages its own graph |

### 1.2 Design Principles

1. **Build on existing infrastructure** — federation.yaml, Unix sockets, trace functions all exist. Extend, don't rebuild.
2. **Minimal protocol extension** — add new IPC request types rather than redesigning the protocol.
3. **Graceful degradation** — cross-repo features should work even if the remote service isn't running (fall back to direct file access).
4. **Namespace task references** — `repo:task-id` syntax for unambiguous cross-repo references.
5. **No global coordinator** — each service remains autonomous. Cross-repo is peer-to-peer via socket forwarding.

## 2. Federation Config Expansion

### 2.1 Current State

`.workgraph/federation.yaml` currently only stores agency remotes:

```yaml
remotes:
  upstream:
    path: /home/erik/shared-agency
    description: "Team shared agency store"
    last_sync: "2026-02-19T22:00:00Z"
```

### 2.2 Proposed Expansion

Expand federation.yaml to also register **peer workgraphs** (not just agency stores):

```yaml
# Agency remotes (existing, unchanged)
remotes:
  upstream:
    path: /home/erik/shared-agency
    description: "Team shared agency store"
    last_sync: "2026-02-19T22:00:00Z"

# Peer workgraph instances (NEW)
peers:
  workgraph:
    path: /home/erik/workgraph
    description: "The workgraph tool itself"
    # socket auto-discovered from <path>/.workgraph/service/state.json
  grants:
    path: /home/erik/grants/agentic-orgs
    description: "Agentic orgs grant project"
```

### 2.3 Peer Resolution

Given a peer reference string, resolution order:
1. Named peer in `federation.yaml` → look up `path`, derive `.workgraph/` directory
2. Absolute path (`~/workgraph` or `/home/erik/workgraph`) → find `.workgraph/` subdirectory
3. Relative path → resolve from CWD

Socket discovery for a peer:
1. Read `<peer_path>/.workgraph/service/state.json` → get `socket_path`
2. Check if service is alive (`is_process_alive(pid)`)
3. If alive → use IPC for real-time operations
4. If not alive → fall back to direct file access (load graph.jsonl)

### 2.4 Data Structures

```rust
/// A peer workgraph instance
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    pub path: String,
    pub description: Option<String>,
}

/// Extended federation config
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FederationConfig {
    /// Agency store remotes (existing)
    #[serde(default)]
    pub remotes: HashMap<String, RemoteConfig>,
    /// Peer workgraph instances (new)
    #[serde(default)]
    pub peers: HashMap<String, PeerConfig>,
}
```

### 2.5 CLI: `wg peer`

```
wg peer add <name> <path> [-d <description>]
wg peer remove <name>
wg peer list                          # Shows peers + service status
wg peer show <name>                   # Details, socket status, task counts
wg peer status                        # Quick health check of all peers
```

### 2.6 Implementation Notes

- Federation.yaml is already parsed in `src/commands/agency_remote.rs`. Extend the struct.
- Peer management commands mirror `wg agency remote` structure — can share YAML I/O code.
- `wg peer list` should indicate which peers have running services (check state.json + PID).

## 3. Cross-Repo Task Dispatch

### 3.1 CLI Syntax

Option A — `--repo` flag on `wg add`:
```bash
wg add --repo workgraph "Fix the trace system" -d "Description here"
wg add --repo ~/workgraph "Fix the trace system" -d "Description here"
```

Option B — colon-namespaced ID:
```bash
wg add "Fix the trace system" --into workgraph:
```

**Recommendation: Option A** (`--repo` flag). Rationale:
- Cleaner CLI — the `--repo` flag is a well-understood pattern (git, docker, etc.)
- Works with all existing `wg add` flags without parsing ambiguity
- The repo name resolves via peer config or absolute path

### 3.2 Dispatch Mechanism

When `wg add --repo <peer>` is invoked:

1. **Resolve peer** to a `.workgraph/` directory path (via federation.yaml or direct path)
2. **Check if peer service is running** (state.json + PID check)
3. **If running**: Send task creation via a new `AddTask` IPC request to the peer's socket
4. **If not running**: Directly modify the peer's `graph.jsonl` (acquire file lock, add task, save)
5. **If running, after add**: Send `GraphChanged` IPC to wake the peer coordinator
6. **Return**: Print the created task ID with peer prefix (`workgraph:fix-the-trace`)

### 3.3 New IPC Request Type

```rust
/// New IPC request for remote task creation
AddTask {
    title: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    blocked_by: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    deliverables: Vec<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    verify: Option<String>,
    /// Who requested this (for provenance)
    #[serde(default)]
    origin: Option<String>,
}
```

The daemon handler for `AddTask`:
1. Load graph
2. Generate task ID (using existing `generate_id()` logic)
3. Create task node with all provided fields
4. Set `origin` in a log entry: `"Remote add from <origin>"`
5. Save graph
6. Return `{"ok": true, "task_id": "<id>"}`

### 3.4 New IPC Request: QueryTask

To check on a task's status from another repo:

```rust
QueryTask {
    task_id: String,
}
```

Response includes: `task_id`, `title`, `status`, `assigned`, `started_at`, `completed_at`, `failure_reason`.

### 3.5 Implementation Plan

1. Add `AddTask` and `QueryTask` to `IpcRequest` enum in `src/commands/service.rs`
2. Add handlers in the daemon dispatch (near line 2321)
3. Add `--repo` flag to `wg add` in `src/main.rs` and `src/commands/add.rs`
4. Add peer resolution logic (reuse federation.yaml parsing, add `peers` section)
5. When `--repo` is set, dispatch via IPC or direct file access

## 4. Cross-Repo Dependencies

### 4.1 Namespaced Task References

Introduce `peer:task-id` syntax for cross-repo references:

```bash
# Local task blocked by a task in the "workgraph" peer
wg add "Use new trace" --blocked-by workgraph:implement-recursive-trace

# Also support absolute paths
wg add "Use new trace" --blocked-by ~/workgraph:implement-recursive-trace
```

### 4.2 Dependency Storage

In `graph.jsonl`, cross-repo blocked_by entries are stored with the full namespaced reference:

```json
{
  "kind": "task",
  "id": "use-new-trace",
  "blocked_by": ["workgraph:implement-recursive-trace"],
  ...
}
```

The colon delimiter is unambiguous because local task IDs are generated as slugs (lowercase alphanumeric + dashes, no colons).

### 4.3 Dependency Resolution

When the coordinator checks task readiness (in the coordinator tick):

**Current logic** (simplified):
```rust
for dep in &task.blocked_by {
    if let Some(dep_task) = graph.get_task(dep) {
        if dep_task.status != Status::Done { not_ready = true; }
    }
}
```

**New logic**:
```rust
for dep in &task.blocked_by {
    if let Some((peer_name, remote_task_id)) = parse_remote_ref(dep) {
        // Cross-repo dependency
        match resolve_remote_task_status(peer_name, remote_task_id, &federation_config) {
            Ok(Status::Done) => { /* satisfied */ }
            Ok(_) => { not_ready = true; }
            Err(_) => { not_ready = true; /* peer unreachable, stay blocked */ }
        }
    } else {
        // Local dependency (existing behavior)
        if let Some(dep_task) = graph.get_task(dep) {
            if dep_task.status != Status::Done { not_ready = true; }
        }
    }
}
```

### 4.4 Remote Status Resolution

`resolve_remote_task_status(peer, task_id, config)`:

1. Look up peer path from `config.peers` or parse as absolute path
2. Check if peer service is running
3. **If running**: Send `QueryTask { task_id }` via IPC → return status
4. **If not running**: Load peer's `graph.jsonl` directly → find task → return status
5. **If peer not found**: Return error (task stays blocked)

### 4.5 Polling Considerations

Cross-repo dependency checks happen during the coordinator tick (every `poll_interval` seconds). This means:
- Resolution is **polling-based**, not push-based
- A task blocked by a remote dep will be checked every tick
- The default poll_interval (60s) is acceptable for this use case
- For faster resolution, the completing repo could `notify_graph_changed` on the dependent repo (see §4.6)

### 4.6 Cross-Repo Notifications (Optional Enhancement)

When a task completes that is referenced as a dependency in another repo, the completing service could notify the dependent service:

```rust
// In the done/fail command handler:
// Check if any peer workgraphs have tasks blocked by this task
// If so, send GraphChanged to wake their coordinator
```

This requires the completing service to know about its dependents. Two approaches:
- **Push registration**: When adding a cross-repo dep, register a callback in the remote's config
- **Lazy scan**: On task completion, scan federation peers for references to `local:task-id`

**Recommendation**: Start with polling only (simpler, no state to maintain). Add push notifications as a future optimization if 60s latency is too slow.

### 4.7 Display and Querying

- `wg show <task>` renders cross-repo deps with peer name: `Blocked by: workgraph:implement-recursive-trace (done)`
- `wg why-blocked <task>` resolves remote deps and shows their current status
- `wg list` can filter by cross-repo dependency status

### 4.8 Implementation Plan

1. Add `parse_remote_ref(dep: &str) -> Option<(peer, task_id)>` utility function
2. Modify readiness check in coordinator tick (`src/commands/service.rs` ~line 970)
3. Add `resolve_remote_task_status()` using IPC or direct file access
4. Modify `wg add` to accept `peer:task-id` syntax in `--blocked-by`
5. Update `wg show`, `wg why-blocked` to display remote dep status
6. Load federation config in coordinator tick (cache with TTL)

## 5. Trace Function Portability

### 5.1 Current State

Trace functions are stored in `.workgraph/functions/<id>.yaml`. The extract command creates them from completed tasks; the instantiate command creates tasks from templates. Both operate within a single repo only.

### 5.2 Cross-Repo Extract

`wg trace extract` already supports `--output <path>`, which can write to any filesystem path:

```bash
# Extract in repo A, save to a path accessible by repo B
wg trace extract my-task --subgraph --output ~/shared/workflow.yaml
```

This already works. No changes needed for basic file-based sharing.

### 5.3 Cross-Repo Instantiate

`wg trace instantiate` currently loads functions from the local `.workgraph/functions/` directory. Extend it to accept a file path or a `peer:function-id` reference:

```bash
# Instantiate from a file path (works across repos)
wg trace instantiate --from ~/workgraph/.workgraph/functions/build-feature.yaml --input feature_name=auth

# Instantiate from a peer's function library
wg trace instantiate --from workgraph:build-feature --input feature_name=auth
```

### 5.4 New `--from` Flag for Instantiate

Add `--from <source>` to `wg trace instantiate`:

1. If source contains `:` → parse as `peer:function-id`, resolve peer, load function from peer's `.workgraph/functions/`
2. If source ends in `.yaml` → treat as a file path, load directly
3. Otherwise → existing behavior (search local `.workgraph/functions/`)

### 5.5 Function Registry in Federation

Optionally, `wg trace list --include-peers` could show functions available across all federated peers:

```
$ wg trace list --include-peers
Local functions:
  deploy-pipeline    (3 tasks, extracted from deploy-v2)

Peer functions:
  workgraph:build-feature  (5 tasks, extracted from add-auth)
  workgraph:review-cycle   (2 tasks, loop-based review)
```

### 5.6 Implementation Plan

1. Add `--from <source>` flag to `wg trace instantiate` command args
2. Implement source resolution (peer lookup, file path, or local search)
3. Add `wg trace list --include-peers` to discover functions across repos
4. Functions remain YAML files — no new storage format needed

## 6. Architecture Summary

```
┌─────────────────────────────────────────────────────────────────┐
│                        Repo A (.workgraph/)                     │
│                                                                 │
│  graph.jsonl                    service/daemon.sock              │
│  ├── task: use-new-trace        ├── Agents, Status, Spawn, ...  │
│  │   blocked_by:                ├── AddTask (NEW)               │
│  │     workgraph:impl-trace ────┤── QueryTask (NEW)             │
│  │                              │                               │
│  functions/                     federation.yaml                  │
│  └── deploy.yaml                ├── remotes: {upstream: ...}    │
│                                 └── peers:                      │
│                                       workgraph: ~/workgraph    │
│                                       grants: ~/grants/...      │
└─────────────────┬───────────────────────────────────────────────┘
                  │ IPC (QueryTask) or direct file read
                  ▼
┌─────────────────────────────────────────────────────────────────┐
│                        Repo B (~workgraph/.workgraph/)          │
│                                                                 │
│  graph.jsonl                    service/daemon.sock              │
│  ├── task: impl-trace (done) ───▶ responds to QueryTask         │
│  │                                                              │
│  functions/                                                     │
│  └── build-feature.yaml ────────▶ accessible via peer:func-id  │
└─────────────────────────────────────────────────────────────────┘
```

## 7. Implementation Phases

### Phase 1: Federation Config & Peer Management
- Extend `federation.yaml` with `peers` section
- Add `wg peer add|remove|list|show|status` commands
- Peer resolution utility (name → path → .workgraph dir)
- ~200 lines new code in `src/commands/peer.rs` + federation.yaml parsing

### Phase 2: Cross-Repo Task Dispatch
- Add `AddTask` and `QueryTask` IPC request types
- Add daemon handlers for both
- Add `--repo` flag to `wg add`
- Peer socket discovery and IPC client for remote services
- ~300 lines: IPC types, handlers, add.rs changes

### Phase 3: Cross-Repo Dependencies
- `parse_remote_ref()` utility for `peer:task-id` syntax
- Modify coordinator readiness check to resolve remote deps
- `resolve_remote_task_status()` via IPC or direct graph read
- Update `wg show`, `wg why-blocked` for remote deps
- ~250 lines: coordinator changes, display updates

### Phase 4: Trace Function Portability
- Add `--from <source>` to `wg trace instantiate`
- Source resolution (peer, file path, or local)
- `wg trace list --include-peers`
- ~150 lines: instantiate changes, peer function discovery

### Total Estimate: ~900 lines of new code

## 8. Testing Strategy

- **Unit tests**: `parse_remote_ref()`, peer resolution, `AddTask`/`QueryTask` serialization
- **Integration tests**: Two temp workgraph dirs, test cross-repo add, dependency resolution, trace instantiate
- **Service tests**: Start two daemons, verify IPC-based QueryTask and AddTask
- Test graceful degradation when peer service is not running (fall back to file access)

## 9. Security Considerations

- **Path traversal**: Peer paths must be canonicalized; reject paths outside user's home or explicitly allowed directories
- **Socket access**: Unix sockets are already 0600 (owner-only). Cross-repo IPC between repos owned by the same user is safe
- **Remote code execution**: `AddTask` creates data (tasks), not code. Exec commands are only set by local agents, never by remote dispatch
- **DoS**: Rate-limit AddTask requests per connection (e.g., 100/second) to prevent a misbehaving peer from flooding the graph

## 10. Future Extensions

- **Git-remote peers**: `wg peer add team git@github.com:org/project` — fetch graph.jsonl via git
- **HTTP/WebSocket bridge**: For peers on different machines (beyond Unix socket reach)
- **Cross-repo provenance**: Track which repo originated a task, build a multi-repo operation log
- **Dependency subscriptions**: Push-based notification when remote tasks complete (see §4.6)
- **Shared function registries**: A central repo of trace functions that any peer can instantiate from
