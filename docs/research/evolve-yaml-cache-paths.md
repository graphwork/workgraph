# Research: Evolve YAML Cache Write/Read Paths

**Task:** research-evolve-yaml  
**Date:** 2026-04-02  
**Reference:** [Bug report](/home/erik/workgraph/wg_evolve_yaml_bug_report.md)

## 1. Files Involved in Cache Read/Write

### Core storage layer
- `src/agency/store.rs` — Generic `save_yaml`, `load_yaml`, `load_all_yaml`, `find_by_prefix` helpers
- `src/agency/types.rs` — All entity type definitions (Role, Agent, RoleComponent, etc.)

### Write callers (production code, non-test)

| Caller file | Line(s) | What it writes | Context |
|---|---|---|---|
| `src/agency/store.rs` | 40–44 | `save_yaml` — core write path for all entities | Serializes via `serde_yaml::to_string`, then `fs::write` |
| `src/agency/store.rs` | 179 | `save_role` — delegates to `save_yaml` | Public API for role writes |
| `src/agency/eval.rs` | 78, 94, 106, 124, 147 | `save_agent`, `save_role`, `save_tradeoff`, `save_component`, `save_outcome` | `record_evaluation` — updates performance records after each task evaluation |
| `src/commands/evolve/operations.rs` | 189, 274, 324, 398, 560, 602, 640, 713, 772, 832, 885, 944 | `save_role`, `save_tradeoff`, `save_component`, `save_outcome`, `save_agent` | Evolution operations: create/modify roles, tradeoffs, components; random compose; config swaps |
| `src/commands/evolve/apply_synthesis.rs` | 234, 286, 324 | `save_role`, `save_component` | Fan-out synthesis: applies aggregated evolution operations |
| `src/commands/evolve/meta.rs` | 92, 177, 244, 286, 399, 491, 589 | `save_role`, `save_agent`, `save_component`, `save_tradeoff` | Meta-operations: swap_role, swap_tradeoff, compose_agent, bizarre_ideation |
| `src/commands/evolve/mod.rs` | 430 | `save_agent` | Auto-pairing: creates agents for new roles with best tradeoff |
| `src/agency/starters.rs` | 671, 682, 690, 702 | All entity types | `seed_starters` — initial agency population |
| `src/agency/run_mode.rs` | 372, 393, 415, 438 | `save_component`, `save_agent` | Run-mode learning: updates components and agents based on experiment results |
| `src/commands/role.rs` | 43, 261, 269 | `save_role` | CLI `wg role create`, `wg role edit` |
| `src/commands/agent_crud.rs` | 145 | `save_agent` | CLI `wg agent create` |
| `src/commands/agency_create.rs` | 472, 498, 528 | `save_component`, `save_outcome`, `save_tradeoff` | CLI `wg agency create component/outcome/tradeoff` |
| `src/commands/agency_init.rs` | 81, 149 | `save_agent` | CLI `wg agency init` default agent creation |
| `src/commands/agency_import.rs` | 214, 251, 296 | `save_component`, `save_outcome`, `save_tradeoff` | CSV import path |
| `src/commands/agency_migrate.rs` | 226, 260, 292, 325, 350, 393, 474 | All entity types | Migration from legacy formats |
| `src/federation.rs` | 863–1025 | All entity types via `AgencyStore` trait | Federation merge operations |
| `src/service/executor.rs` | 1190, 1200, 1223, 1684 | `save_role`, `save_tradeoff`, `save_agent` | Executor creating entities from LLM output |
| `src/commands/tradeoff.rs` | 43, 264, 272 | `save_tradeoff` | CLI `wg tradeoff create`, `wg tradeoff edit` |
| `src/commands/match_cmd.rs` | 185 | `save_agent` | `wg match` updates |
| `src/commands/next.rs` | 228 | `save_agent` | `wg next` agent updates |
| `src/commands/assign.rs` | (test only) | — | — |

### Read callers (production code, non-test)

| Caller file | Line(s) | What it reads | Context |
|---|---|---|---|
| `src/agency/store.rs` | 35–37 | `load_yaml` — core single-file read | `fs::read_to_string` + `serde_yaml::from_str` |
| `src/agency/store.rs` | 47–62 | `load_all_yaml` — directory scan read | Iterates `.yaml` files, calls `load_yaml` per file |
| `src/agency/store.rs` | 65–91 | `find_by_prefix` — prefix-match read | Loads all, filters by ID prefix |
| `src/commands/evolve/mod.rs` | 68 | `load_all_roles` | Evolve entry: loads roles for evolution cycle |
| `src/commands/evolve/mod.rs` | 69–70 | `load_all_tradeoffs` | Evolve entry: loads tradeoffs |
| `src/commands/evolve/apply_synthesis.rs` | 36–37 | `load_all_roles`, `load_all_tradeoffs` | Synthesis apply: loads current state |
| `src/commands/evolve/apply_synthesis.rs` | 75, 78 | `load_all_roles`, `load_all_tradeoffs` | Re-load after each operation |
| `src/agency/eval.rs` | 69, 84, 98, 115, 138 | `find_agent_by_prefix`, `find_role_by_prefix`, etc. | Record evaluation: loads entity by prefix |
| `src/commands/role.rs` | 57 | `load_all_roles` | CLI `wg role list` |
| `src/commands/agent_crud.rs` | 190 | `load_all_agents` | CLI `wg agent list` |
| `src/commands/tradeoff.rs` | 56 | `load_all_tradeoffs` | CLI `wg tradeoff list` |
| `src/commands/service/coordinator.rs` | 1012, 1440, 1965 | `load_all_agents_or_warn`, `load_all_roles` | Coordinator agent selection |
| `src/commands/service/mod.rs` | 1004, 2395, 3110, 4027, etc. | `load_all_agents_or_warn`, `load_all_roles` | Service startup and dispatch |
| `src/agency/run_mode.rs` | 215, 241 | `load_all_agents_or_warn`, `load_all_components` | Run-mode assignment |
| `src/agency/lineage.rs` | 12, 46, 80, 114 | `load_all_roles`, `load_all_components`, etc. | Lineage ancestry traversal |

## 2. Serialization Format for Timestamps

### Two distinct timestamp patterns in the codebase:

**Pattern A: `DateTime<Utc>` (chrono) — in `Lineage.created_at`**
- Type: `chrono::DateTime<Utc>` (`src/agency/types.rs:133`)
- Default: `Utc::now()` (`src/agency/types.rs:132`)
- Serialization: `serde_yaml` delegates to chrono's serde impl, producing RFC 3339 format: `"2026-04-01T15:30:45.123456789Z"`
- **This is the likely corruption source.** When `serde_yaml 0.9` serializes `DateTime<Utc>`, it produces a bare (unquoted) timestamp like `2026-04-01T15:30:45.123456789Z`. If a write is interrupted mid-stream, the timestamp can be split across lines.

**Pattern B: `String` timestamps — in `EvaluationRef.timestamp`, `DeploymentRef.timestamp`, `StalenessFlag.flagged_at`, `TaskAssignmentRecord.timestamp`**
- Type: plain `String` (e.g., `src/agency/types.rs:77`, `110`, `95`)
- Format: RFC 3339 strings, set by calling code (e.g., `chrono::Utc::now().to_rfc3339()`)
- These are always string-typed in YAML, typically quoted.

### serde_yaml 0.9 `DateTime<Utc>` behavior

`serde_yaml 0.9` serializes `DateTime<Utc>` as a YAML timestamp scalar. The YAML 1.1 spec (which `serde_yaml 0.9` follows) has a native timestamp type. The output looks like:

```yaml
created_at: 2026-04-01T15:30:45.123456789+00:00
```

This is **unquoted** and parsed as a YAML timestamp. If the serialized string is corrupted (truncated, split across lines), the YAML parser will fail with a structural error.

## 3. The Write Path — No Validation, No Atomicity

The central write function (`src/agency/store.rs:40–44`):

```rust
fn save_yaml<T: serde::Serialize>(val: &T, dir: &Path, id: &str) -> Result<PathBuf, AgencyError> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.yaml", id));
    fs::write(&path, serde_yaml::to_string(val)?)?;
    Ok(path)
}
```

**Problems:**
1. **No write validation:** The serialized YAML is never parsed back to verify correctness before/after writing.
2. **No atomic write:** `fs::write` truncates the target file and writes in place. If the process crashes or is killed mid-write, the file is left in a partially-written state.
3. **No fsync:** The data may still be in OS page cache when `fs::write` returns. A system crash could lose data.
4. **Concurrent writers:** Multiple agents can call `save_role` on the same role file simultaneously (via `record_evaluation` performance updates). There is no file locking on the agency store — only the graph JSONL has flock-based locking.

## 4. Existing Atomic Write Patterns in the Codebase

Several other subsystems already implement write-to-temp + rename:

| Module | Pattern | Lines |
|---|---|---|
| `src/parser.rs` | `modify_graph`: write to `.graph.jsonl.tmp`, fsync, rename | 203–236 |
| `src/messages.rs` | Message file rewrite: write to `.tmp`, rename | 325–338 |
| `src/messages.rs` | Cursor files: write to `.tmp`, rename | 376–394 |
| `src/chat.rs` | Chat rotation + cursor: write to `.tmp`, rename | 347–359, 640–656, 786–810 |
| `src/service/registry.rs` | Registry save: write to `.tmp`, fsync, rename | 178–207 |

The `tempfile` crate is already a dependency (used extensively in tests). The established pattern is:
1. Write to a `.tmp` suffixed file in the same directory
2. `fsync` the file (in critical paths)
3. `fs::rename` atomically

## 5. serde/YAML Library and Validation Hooks

**Library:** `serde_yaml = "0.9.34+deprecated"` (Cargo.toml line 33, Cargo.lock line 4490)

**Important note:** `serde_yaml 0.9` is **deprecated**. The maintainer (dtolnay) archived the repo. The successor is `serde_yml` (different crate). This means no further bug fixes for timestamp handling.

**Validation hooks available:**
- **serde `Deserialize` validation:** Custom `deserialize` impls or `#[serde(deserialize_with)]` can validate on read
- **serde `Serialize` validation:** Custom `serialize` impls can validate on write
- **No built-in serde_yaml validation hooks** — the library does not provide pre-write or post-write verification callbacks
- **Round-trip test:** The simplest validation is `serde_yaml::from_str(&serde_yaml::to_string(val)?)` — serialize then immediately re-parse to confirm validity

## 6. Recommended Fix Approach

### Fix 1: Atomic writes (HIGH PRIORITY)

Modify `save_yaml` in `src/agency/store.rs:40–44` to use write-to-temp + rename:

```rust
fn save_yaml<T: serde::Serialize>(val: &T, dir: &Path, id: &str) -> Result<PathBuf, AgencyError> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.yaml", id));
    let tmp_path = dir.join(format!(".{}.yaml.tmp", id));
    let content = serde_yaml::to_string(val)?;
    fs::write(&tmp_path, &content)?;
    fs::rename(&tmp_path, &path)?;
    Ok(path)
}
```

This matches the existing pattern in `src/service/registry.rs:178–207` and `src/parser.rs:203–236`. Consider adding `fsync` for extra safety (as `registry.rs` does).

**Files to modify:** `src/agency/store.rs:40–44`

### Fix 2: Write validation (MEDIUM PRIORITY)

Add a round-trip parse check after serialization:

```rust
fn save_yaml<T: serde::Serialize + serde::de::DeserializeOwned>(
    val: &T, dir: &Path, id: &str
) -> Result<PathBuf, AgencyError> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.yaml", id));
    let content = serde_yaml::to_string(val)?;
    // Validate: ensure the serialized YAML parses back successfully
    let _: T = serde_yaml::from_str(&content)?;
    let tmp_path = dir.join(format!(".{}.yaml.tmp", id));
    fs::write(&tmp_path, &content)?;
    fs::rename(&tmp_path, &path)?;
    Ok(path)
}
```

**Note:** This requires adding `DeserializeOwned` bound to `save_yaml`, which propagates to all callers. All entity types already implement both `Serialize` and `Deserialize`, so this is safe.

**Files to modify:** `src/agency/store.rs:40` (add type bound + validation)

### Fix 3: Graceful degradation on read (HIGH PRIORITY)

Modify `load_all_yaml` in `src/agency/store.rs:47–62` to skip corrupted files with warnings instead of failing the entire operation:

```rust
fn load_all_yaml<T: serde::de::DeserializeOwned + HasId>(
    dir: &Path,
) -> Result<Vec<T>, AgencyError> {
    let mut items = Vec::new();
    if !dir.exists() {
        return Ok(items);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
            match load_yaml::<T>(&path) {
                Ok(item) => items.push(item),
                Err(e) => {
                    eprintln!(
                        "Warning: skipping corrupt YAML file {}: {}",
                        path.display(), e
                    );
                }
            }
        }
    }
    items.sort_by(|a, b| a.entity_id().cmp(b.entity_id()));
    Ok(items)
}
```

**Files to modify:** `src/agency/store.rs:47–62`

**Trade-off:** This changes the error behavior — callers that currently expect `Err` on corrupt files will silently skip them. The `evolve/mod.rs:68` caller uses `.context("Failed to load roles")?` which would no longer trigger. The bug report explicitly recommends this behavior.

### Fix 4: Consider migrating from deprecated `serde_yaml` (LOW PRIORITY, LONG-TERM)

`serde_yaml 0.9.34` is deprecated. Options:
- **`serde_yml`** — community fork, maintains API compat
- **Stay on 0.9** — works, but no bug fixes
- This is a large-scope change (27 files import `serde_yaml`) and should be a separate task

### Priority Order

1. **Atomic writes** (Fix 1) — prevents corruption at the source
2. **Graceful degradation** (Fix 3) — makes the system resilient to existing/future corruption
3. **Write validation** (Fix 2) — defense in depth, catches serialization bugs
4. **serde_yaml migration** (Fix 4) — long-term maintenance

---

*All file:line references verified against current codebase as of 2026-04-02.*
