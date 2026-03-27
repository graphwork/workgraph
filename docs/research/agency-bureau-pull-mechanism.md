# Agency Bureau Pull Mechanism: Research & Design

**Task:** agency-pull-research
**Date:** 2026-03-27
**Status:** Complete

---

## 1. Current Agency Init Flow

The `wg init` command (`src/commands/init.rs:19`) orchestrates project initialization. Agency setup is delegated to `agency_init::run()` (`src/commands/agency_init.rs:11`) unless `--no-agency` is passed.

### Init sequence

1. **Create `.workgraph/` directory** and `graph.jsonl`
2. **Call `agency_init::run()`** which:
   a. **Seed hardcoded starters** via `agency::seed_starters()` — 8 role components + 4 outcomes + 4 tradeoffs defined as Rust literals in `src/agency/starters.rs`
   b. **Auto-import bundled CSV** via `try_csv_import()` (`agency_init.rs:252`):
      - Checks for `<project_root>/agency/starter.csv`
      - Skips if `import-manifest.yaml` already exists
      - Runs `agency_import::run()` which parses the CSV and writes YAML primitives
      - Writes a provenance manifest at `.workgraph/agency/import-manifest.yaml`
   c. **Create default agent** (Programmer + Careful)
   d. **Create 4 special agents** (Assigner, Evaluator, Evolver, Creator)
   e. **Enable auto_assign + auto_evaluate** in config, set haiku model defaults
   f. **Wire special agent hashes** into config
   g. **Register creator-pipeline function**
3. **Check skill/bundle status** and print hints

### Key observations

- The bundled CSV at `agency/starter.csv` is already tracked in git — it contains 29 rows (18 components, 6 outcomes, 11 tradeoffs) in 9-column Agency format.
- Import is content-hash deduplicated: re-importing the same CSV is a no-op.
- The manifest records source path, wg version, timestamp, counts, and SHA-256 of the CSV file.
- Hardcoded starters in `starters.rs` serve as a fallback if the CSV is missing.

---

## 2. Current CSV Format & Import Mechanism

### CSV format (9 columns — Agency format)

```
type,name,description,quality,domain_specificity,domain,origin_instance_id,parent_content_hash,scope
```

| Column | Type | Description |
|--------|------|-------------|
| `type` | enum | `role_component`, `desired_outcome`, `trade_off_config` |
| `name` | string | Human-readable name |
| `description` | string | Behavioral instruction (verb phrase, 50-150 chars) |
| `quality` | int 0-100 | Mapped to `avg_score` as 0.0-1.0 |
| `domain_specificity` | int/string | Stored as metadata |
| `domain` | comma-separated | Stored as `domain_tags` |
| `origin_instance_id` | UUID | Stored as metadata |
| `parent_content_hash` | hash | Stored in `lineage.parent_ids` |
| `scope` | string | `task`, `meta`, `meta:assigner`, etc. |

### Import mechanism (`src/commands/agency_import.rs`)

- Auto-detects format by column count (≥9 = Agency, else Legacy 7-column)
- Normalizes type names: accepts both old (`skill`/`outcome`/`tradeoff`) and new formats
- Content-hash deduplication: each primitive's ID is derived from its content, so reimporting is idempotent
- Writes YAML files to `.workgraph/agency/primitives/{components,outcomes,tradeoffs}/`
- Supports `--dry-run` and `--tag` for provenance
- Writes `import-manifest.yaml` with SHA-256 of CSV for change detection

### Import manifest (`import-manifest.yaml`)

```yaml
source: agency/starter.csv
version: "v0.1.0"
imported_at: "2026-03-27T00:00:00Z"
counts:
  role_components: 18
  desired_outcomes: 6
  trade_off_configs: 11
content_hash: "<sha256>"
```

---

## 3. Existing Pull Infrastructure

### `wg agency pull` already exists

The command at `src/commands/agency_pull.rs` implements **YAML-to-YAML federation transfer** between agency stores. It is fundamentally different from CSV import:

| Aspect | `wg agency pull` (current) | CSV import (`wg agency import`) |
|--------|---------------------------|--------------------------------|
| **Source** | Another `.workgraph/agency/` directory (local path or named remote) | A CSV file |
| **Format** | YAML entity files | 9-column CSV |
| **Resolution** | Named remotes in `federation.yaml`, or filesystem path | Direct file path |
| **Merge** | Performance data merge, evaluation dedup, force overwrite option | Content-hash dedup (idempotent write) |
| **Entities** | Roles, tradeoffs, agents, components, outcomes, evaluations | Components, outcomes, tradeoffs only |
| **Scope** | Cross-project federation | Bootstrapping from upstream primitive pool |

### Federation remotes (`federation.yaml`)

```yaml
remotes:
  upstream:
    path: "/path/to/other/.workgraph/agency"
    description: "Agency bureau upstream"
    last_sync: "2026-03-27T00:00:00Z"
```

The existing `wg agency pull <source>` resolves sources via:
1. Named remote lookup in `federation.yaml`
2. Filesystem path fallback (with `~/` expansion and `agency/` suffix probing)

### What's missing for "bureau pull"

The current `wg agency pull` only works with **local filesystem paths**. There is no HTTP/URL support. To pull from an upstream "bureau" (a curated primitive repository hosted remotely), we need:

1. **URL-based source resolution** — fetch a CSV or YAML store from an HTTP URL
2. **Change detection** — compare content hash before downloading
3. **Integration with `wg init`** — optionally pull from a configured upstream on project init

---

## 4. Pull Mechanism Evaluation

### Option A: Simple HTTP Fetch (CSV) ← **Recommended**

Fetch a CSV from a URL, then feed it through the existing `agency_import::run()` pipeline.

**Pros:**
- Lowest complexity — reuses 100% of existing import infrastructure
- `reqwest` is already a dependency (with `blocking` feature)
- Content-hash comparison via manifest enables efficient no-change detection
- The CSV is the canonical distribution format from Agency
- Works with any HTTP server (GitHub raw URLs, CDN, S3, custom registry)

**Cons:**
- No incremental sync — downloads entire CSV each time
- No entity-level version tracking (CSV is all-or-nothing)

**Implementation estimate:** ~150 lines of new code.

### Option B: Git Subtree

Track the Agency repo (or a subdirectory) as a git subtree.

**Pros:**
- Full version history, bisectable
- Git-native merge conflict resolution

**Cons:**
- Heavy — pulls entire repo history
- Requires git operations at init time (slow, may need auth)
- Complicates the repo structure
- Users who don't use Agency get dead weight

**Verdict:** Too heavy for distributing a single CSV.

### Option C: Git Submodule

Reference the Agency repo as a submodule.

**Pros:**
- Versioned reference to exact commit

**Cons:**
- Submodule UX is notoriously poor
- Requires git clone at init (same auth/speed issues as subtree)
- Users must `git submodule update --init` manually
- Breaks `cargo install` from crates.io

**Verdict:** Worse UX than subtree for the same benefit.

### Option D: Custom Registry with Version Pinning

A dedicated HTTP API serving versioned primitive pools.

**Pros:**
- Most flexible — version pinning, incremental updates, auth
- Could support per-primitive-type endpoints

**Cons:**
- Requires building and hosting a registry service
- Significant new infrastructure
- Overkill for current needs (one upstream, one CSV)

**Verdict:** Future consideration when multiple bureaus exist. Not justified now.

### Recommendation: Option A (HTTP Fetch) with future Option D escape hatch

The HTTP fetch approach is the right choice because:
1. The existing `agency_import::run()` handles all CSV parsing, content-hash dedup, and manifest writing
2. The `reqwest` dependency already exists with the `blocking` feature
3. The content-hash in the manifest enables "only re-import if changed" semantics
4. A GitHub raw URL (`https://raw.githubusercontent.com/...`) is sufficient as the default upstream
5. Custom URLs can override for private/enterprise deployments

---

## 5. CLI Interface Specification

### New subcommand: extend `wg agency import` with `--url`

Rather than creating a new command, extend the existing import command to accept URLs:

```bash
# Current (local file):
wg agency import agency/starter.csv

# New (remote URL):
wg agency import --url https://raw.githubusercontent.com/org/agency/main/starter.csv

# With configured default source:
wg agency import --upstream
```

### Alternative: add `--url` to existing `wg agency pull`

Since `wg agency pull` already handles the "get primitives from elsewhere" semantic, it could be extended:

```bash
# Current (local store path or named remote):
wg agency pull /path/to/store
wg agency pull upstream

# New (URL to CSV):
wg agency pull --csv-url https://raw.githubusercontent.com/org/agency/main/starter.csv
```

### **Recommended approach: extend `wg agency import`**

Rationale:
- `wg agency import` already handles CSV → YAML conversion and manifest writing
- `wg agency pull` handles YAML → YAML federation — different semantic
- Adding `--url` to import keeps the import pipeline cohesive
- The federation pull can stay focused on inter-project YAML transfer

### Full CLI specification

```
wg agency import [OPTIONS] [CSV_PATH]

Arguments:
  [CSV_PATH]    Local path to CSV file (existing behavior)

Options:
  --url <URL>       Fetch CSV from a remote URL instead of local file
  --upstream        Fetch from the configured upstream URL (config: agency.upstream_url)
  --dry-run         Show what would be imported without writing
  --tag <TAG>       Provenance tag (default: agency-import)
  --force           Re-import even if manifest hash matches (skip change detection)
  --check           Only check if upstream has changed (exit 0 = changed, exit 1 = same)
```

### Configuration (`config.toml`)

```toml
[agency]
upstream_url = "https://raw.githubusercontent.com/org/agency/main/starter.csv"
```

When `--upstream` is used without a URL, it reads from this config value. This lets `wg agency import --upstream` be a one-line command for routine updates.

---

## 6. Merge Semantics

### Current behavior (content-hash dedup)

The import pipeline already handles the primary merge concern: **content-hash deduplication**. Each primitive's ID is derived from its content (description + type-specific fields). If a primitive with the same content-hash already exists locally, the import overwrites the YAML file but the content is identical — effectively a no-op.

### Scenarios and behavior

| Scenario | Behavior | Rationale |
|----------|----------|-----------|
| **New primitive in upstream** | Written to local store | New content-hash → new file |
| **Unchanged primitive** | Overwritten (identical content) | Idempotent — no functional change |
| **Locally modified primitive** | Not affected | Different content-hash → different file |
| **Primitive removed upstream** | Remains locally | Import is additive, never deletes |
| **Local performance data** | Preserved | Import writes to `primitives/` dir; performance is accumulated in the YAML `performance` field but import only sets `avg_score` from `quality` column — existing evaluations are not touched |

### Content-hash guarantee

Because primitive IDs are content-derived hashes:
- Two identical descriptions always produce the same hash → same file path
- A modified description produces a different hash → different file, no collision
- Local changes to a primitive (e.g., via evaluation) change its YAML content but not its file name (the hash is from the definition, not the full file)

### Edge case: local primitive with same name but different content

If a user creates a component named "Code Review" with description "Reviews code for issues" and the upstream CSV has a "Code Review" with description "Reviews code for correctness and style", these produce **different content hashes** and coexist as separate files. The name collision is harmless — the system identifies primitives by hash, not name.

### Deletion semantics

Import is **strictly additive**. It never deletes local primitives. This is the correct default because:
- Local primitives may have accumulated performance data from real deployments
- Deletion could break existing agent compositions that reference the deleted primitive
- If upstream intentionally removes a primitive, the local copy becomes "locally maintained"

For users who want to align exactly with upstream (remove locals not in upstream), a future `--prune` flag could be added, but this is not needed for the initial implementation.

---

## 7. Changes to `wg init`

### Current init + agency behavior

`wg init` → `agency_init::run()` → `try_csv_import()` → imports from `agency/starter.csv` if present.

### Proposed enhancement

Add an optional `--upstream` / `--pull` flag to `wg init` that fetches the CSV from a remote URL before running the standard import:

```bash
# Current:
wg init                          # Uses bundled CSV if present

# New:
wg init --upstream               # Fetches from configured URL, then imports
wg init --upstream-url <URL>     # Fetches from specific URL, then imports
```

### Implementation in `try_csv_import()`

Extend the existing function to:
1. Check for a configured `agency.upstream_url` in global or project config
2. If present and no `--no-agency` flag: fetch the CSV to a temp file
3. Compare content-hash against the bundled CSV (if any) and the import manifest
4. If different: import the fetched CSV
5. If same: skip (already imported)

This is a minimal change: the existing `try_csv_import()` already handles the manifest check and import delegation. The only addition is an HTTP fetch step before the existing logic.

---

## 8. Files Requiring Modification

### For `wg agency import --url` support

| File | Change |
|------|--------|
| `src/cli.rs:2609-2621` | Add `--url`, `--upstream`, `--force`, `--check` options to `Import` variant |
| `src/commands/agency_import.rs` | Add `fetch_csv(url) -> Result<Vec<u8>>` function using `reqwest::blocking::get`; add manifest hash comparison for change detection; extend `run()` to accept URL source |
| `src/main.rs:1136-1140` | Pass new options through to `agency_import::run()` |
| `src/config.rs` (or `workgraph::config`) | Add `upstream_url: Option<String>` to `AgencyConfig` |

### For `wg init --upstream` support

| File | Change |
|------|--------|
| `src/cli.rs` | Add `--upstream` and `--upstream-url` flags to `Init` command |
| `src/commands/init.rs` | Pass upstream URL to `agency_init::run()` |
| `src/commands/agency_init.rs:252-283` | Extend `try_csv_import()` to optionally fetch from URL before local CSV fallback |

### No changes needed

| File | Reason |
|------|--------|
| `src/federation.rs` | YAML federation is a separate system; not involved in CSV import |
| `src/commands/agency_pull.rs` | YAML-to-YAML federation pull; not involved in CSV bureau pull |
| `agency/starter.csv` | Already bundled; no format changes needed |
| `.workgraph/agency/import-manifest.yaml` | Existing manifest format is sufficient (source field will show URL) |

---

## 9. Implementation Sketch

### Core: `fetch_csv()` in `agency_import.rs`

```rust
/// Fetch CSV content from a URL. Returns the raw bytes.
fn fetch_csv(url: &str) -> Result<Vec<u8>> {
    let response = reqwest::blocking::get(url)
        .with_context(|| format!("Failed to fetch '{}'", url))?;
    
    if !response.status().is_success() {
        anyhow::bail!("HTTP {} fetching '{}'", response.status(), url);
    }
    
    let bytes = response.bytes()
        .with_context(|| format!("Failed to read response from '{}'", url))?;
    
    Ok(bytes.to_vec())
}
```

### Change detection flow

```
1. Read existing import-manifest.yaml (if any)
2. Fetch CSV from URL → bytes
3. Compute SHA-256 of bytes
4. Compare with manifest.content_hash
5. If same → print "Already up to date" → exit
6. If different → write bytes to temp file → run existing import pipeline
```

### Estimated effort

- HTTP fetch + change detection: ~50 lines
- CLI flag wiring: ~30 lines  
- Config field addition: ~10 lines
- `wg init` integration: ~20 lines
- Tests: ~80 lines
- **Total: ~190 lines of new code**

---

## 10. Summary of Recommendations

1. **Pull mechanism:** HTTP fetch (Option A) — simplest viable, reuses existing import infrastructure, `reqwest` already available.

2. **CLI interface:** Extend `wg agency import` with `--url` and `--upstream` flags rather than creating a new command or modifying `wg agency pull`.

3. **Merge semantics:** Content-hash deduplication (already implemented). Import is additive, never deletes. Local performance data preserved.

4. **`wg init` integration:** Optional `--upstream` flag that fetches from configured URL before standard import. Bundled CSV remains as fallback.

5. **Default upstream:** A configurable URL in `config.toml` (`agency.upstream_url`), defaulting to a GitHub raw URL for the Agency project's starter CSV.

6. **Change detection:** SHA-256 comparison between fetched CSV and import manifest avoids redundant reimports.

7. **Keep `wg agency pull` for YAML federation.** The bureau pull (CSV from URL) and project federation (YAML between stores) are distinct operations with distinct semantics. Don't conflate them.
