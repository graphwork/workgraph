//! Migration commands. Currently supports the chat-rename migration:
//! rewrites legacy `.coordinator-N` task ids to `.chat-N`, fixes up
//! after-edges, renames `coordinator-loop` tags to `chat-loop`, and
//! rewrites `Coordinator: <name>` / `Coordinator N` titles.

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

use workgraph::chat_id::{
    CHAT_LOOP_TAG, CHAT_PREFIX, LEGACY_COORDINATOR_LOOP_TAG, LEGACY_COORDINATOR_PREFIX,
};
use workgraph::graph::LogEntry;
use workgraph::parser::modify_graph;

use super::graph_path;

/// Result of a chat-rename migration.
#[derive(Debug, Default, Clone)]
pub struct ChatRenameMigrationResult {
    /// Old `.coordinator-N` ids that were rewritten to `.chat-N`.
    pub renamed_ids: Vec<(String, String)>,
    /// Number of `after`-edges that were rewritten on dependent tasks.
    pub rewritten_edges: usize,
    /// Number of tags renamed from `coordinator-loop` to `chat-loop`.
    pub renamed_tags: usize,
    /// Number of titles rewritten from `Coordinator: …` / `Coordinator N` to the new form.
    pub renamed_titles: usize,
}

impl ChatRenameMigrationResult {
    pub fn is_empty(&self) -> bool {
        self.renamed_ids.is_empty()
            && self.rewritten_edges == 0
            && self.renamed_tags == 0
            && self.renamed_titles == 0
    }
}

fn maybe_new_title(title: &str) -> Option<String> {
    if let Some(rest) = title.strip_prefix("Coordinator: ") {
        return Some(format!("Chat: {}", rest));
    }
    if let Some(rest) = title.strip_prefix("Coordinator ")
        && !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return Some(format!("Chat {}", rest));
    }
    None
}

/// Rewrite legacy chat-agent task ids and tags to the new canonical form.
///
/// Runs in-place on `<dir>/graph.jsonl`. Idempotent — running twice on a
/// migrated graph is a no-op.
pub fn run_chat_rename(dir: &Path, dry_run: bool, json: bool) -> Result<()> {
    let graph_path = graph_path(dir);

    let mut result = ChatRenameMigrationResult::default();
    let now = chrono::Utc::now().to_rfc3339();

    if dry_run {
        let graph = workgraph::parser::load_graph(&graph_path)?;
        for task in graph.tasks() {
            if task.id.starts_with(LEGACY_COORDINATOR_PREFIX) {
                let suffix = &task.id[LEGACY_COORDINATOR_PREFIX.len()..];
                let new_id = format!("{}{}", CHAT_PREFIX, suffix);
                result.renamed_ids.push((task.id.clone(), new_id));
            }
            if task.tags.iter().any(|t| t == LEGACY_COORDINATOR_LOOP_TAG) {
                result.renamed_tags += 1;
            }
            if maybe_new_title(&task.title).is_some() {
                result.renamed_titles += 1;
            }
            for after in &task.after {
                if after.starts_with(LEGACY_COORDINATOR_PREFIX) {
                    result.rewritten_edges += 1;
                }
            }
        }
    } else {
        modify_graph(&graph_path, |graph| {
            // Phase 1: build the id remap.
            let id_remap: HashMap<String, String> = graph
                .tasks()
                .filter_map(|t| {
                    t.id.strip_prefix(LEGACY_COORDINATOR_PREFIX)
                        .map(|suffix| (t.id.clone(), format!("{}{}", CHAT_PREFIX, suffix)))
                })
                .collect();
            for (old, new) in &id_remap {
                result.renamed_ids.push((old.clone(), new.clone()));
            }

            // Phase 2: collect all current task ids (keys to iterate).
            let all_ids: Vec<String> = graph.tasks().map(|t| t.id.clone()).collect();

            // Phase 3: rewrite each task's fields in place — at this point
            // the HashMap key still equals the task.id (no re-keying yet),
            // so get_task_mut works with the OLD id.
            for old_key in &all_ids {
                if let Some(t) = graph.get_task_mut(old_key) {
                    // Rewrite after-edges for this task.
                    let mut local_edges = 0usize;
                    for after in t.after.iter_mut() {
                        if let Some(new_id) = id_remap.get(after) {
                            *after = new_id.clone();
                            local_edges += 1;
                        }
                    }
                    if local_edges > 0 {
                        result.rewritten_edges += local_edges;
                    }

                    // Rewrite legacy tags.
                    let mut renamed_tag_in_task = false;
                    for tag in t.tags.iter_mut() {
                        if tag == LEGACY_COORDINATOR_LOOP_TAG {
                            *tag = CHAT_LOOP_TAG.to_string();
                            renamed_tag_in_task = true;
                        }
                    }
                    if renamed_tag_in_task {
                        result.renamed_tags += 1;
                    }

                    // Rewrite legacy titles.
                    if let Some(new_title) = maybe_new_title(&t.title) {
                        t.title = new_title;
                        result.renamed_titles += 1;
                    }

                    // Rewrite this task's own id if it's a legacy coordinator id.
                    if let Some(new_id) = id_remap.get(&t.id) {
                        let old_id = t.id.clone();
                        t.id = new_id.clone();
                        t.log.push(LogEntry {
                            timestamp: now.clone(),
                            actor: Some("migration".to_string()),
                            user: Some(workgraph::current_user()),
                            message: format!(
                                "wg migrate chat-rename: renamed task id {} -> {}",
                                old_id, new_id
                            ),
                        });
                    }
                }
            }

            // Phase 4: re-key the HashMap so lookups by the NEW id work.
            // We pull each renamed task out by its old key and re-add it,
            // which inserts at the new key (add_node uses node.id()).
            for (old_id, _new_id) in &id_remap {
                if let Some(node) = graph.take_node(old_id) {
                    graph.add_node(node);
                }
            }

            true
        })?;
    }

    if json {
        let payload = serde_json::json!({
            "renamed_ids": result.renamed_ids,
            "rewritten_edges": result.rewritten_edges,
            "renamed_tags": result.renamed_tags,
            "renamed_titles": result.renamed_titles,
            "dry_run": dry_run,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if result.is_empty() {
        println!("No legacy coordinator data found — graph is already on the new schema.");
    } else {
        if dry_run {
            println!("Dry run — no changes written:");
        } else {
            println!("Migration complete:");
        }
        println!("  task ids renamed: {}", result.renamed_ids.len());
        for (old, new) in &result.renamed_ids {
            println!("    {} -> {}", old, new);
        }
        println!("  after-edges rewritten: {}", result.rewritten_edges);
        println!(
            "  tags renamed (coordinator-loop -> chat-loop): {}",
            result.renamed_tags
        );
        println!("  titles renamed: {}", result.renamed_titles);
    }
    Ok(())
}

/// Result of a retire-compact-archive migration.
#[derive(Debug, Default, Clone)]
pub struct RetireCompactArchiveResult {
    /// Task ids that were marked Abandoned.
    pub abandoned_ids: Vec<String>,
    /// Number of `after` edges that were stripped from other tasks because
    /// they pointed at retired `.compact-N` / `.archive-N` ids.
    pub stripped_edges: usize,
}

impl RetireCompactArchiveResult {
    pub fn is_empty(&self) -> bool {
        self.abandoned_ids.is_empty() && self.stripped_edges == 0
    }
}

/// Mark every `.compact-N` and `.archive-N` task as Abandoned and strip
/// after-edges referencing those ids from other tasks. Idempotent — running
/// twice on a migrated graph is a no-op.
pub fn run_retire_compact_archive(dir: &Path, dry_run: bool, json: bool) -> Result<()> {
    let graph_path = graph_path(dir);
    let now = chrono::Utc::now().to_rfc3339();
    let mut result = RetireCompactArchiveResult::default();

    if dry_run {
        let graph = workgraph::parser::load_graph(&graph_path)?;
        for task in graph.tasks() {
            if (task.id.starts_with(".compact-") || task.id.starts_with(".archive-"))
                && task.status != workgraph::graph::Status::Abandoned
            {
                result.abandoned_ids.push(task.id.clone());
            }
        }
        for task in graph.tasks() {
            for dep in &task.after {
                if dep.starts_with(".compact-") || dep.starts_with(".archive-") {
                    result.stripped_edges += 1;
                }
            }
        }
    } else {
        workgraph::parser::modify_graph(&graph_path, |graph| {
            let all_ids: Vec<String> = graph.tasks().map(|t| t.id.clone()).collect();
            for tid in &all_ids {
                let is_target = tid.starts_with(".compact-") || tid.starts_with(".archive-");
                let already_abandoned = graph
                    .get_task(tid)
                    .map(|t| t.status == workgraph::graph::Status::Abandoned)
                    .unwrap_or(false);
                if is_target && !already_abandoned
                    && let Some(t) = graph.get_task_mut(tid)
                {
                    t.status = workgraph::graph::Status::Abandoned;
                    t.completed_at.get_or_insert_with(|| now.clone());
                    t.cycle_config = None;
                    t.log.push(LogEntry {
                        timestamp: now.clone(),
                        actor: Some("migration".to_string()),
                        user: Some(workgraph::current_user()),
                        message:
                            "wg migrate retire-compact-archive: retired .compact-N/.archive-N \
                             cycle scaffolding"
                                .to_string(),
                    });
                    result.abandoned_ids.push(tid.clone());
                }
            }
            // Strip after-edges pointing at retired ids.
            for tid in &all_ids {
                if let Some(t) = graph.get_task_mut(tid) {
                    let before = t.after.len();
                    t.after.retain(|dep| {
                        !(dep.starts_with(".compact-") || dep.starts_with(".archive-"))
                    });
                    let removed = before - t.after.len();
                    if removed > 0 {
                        result.stripped_edges += removed;
                    }
                }
            }
            true
        })?;
    }

    if json {
        let payload = serde_json::json!({
            "abandoned_ids": result.abandoned_ids,
            "stripped_edges": result.stripped_edges,
            "dry_run": dry_run,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if result.is_empty() {
        println!("No legacy .compact-N or .archive-N tasks found — graph is already migrated.");
    } else {
        if dry_run {
            println!("Dry run — no changes written:");
        } else {
            println!("Migration complete:");
        }
        println!("  tasks abandoned: {}", result.abandoned_ids.len());
        for id in &result.abandoned_ids {
            println!("    {}", id);
        }
        println!("  after-edges stripped: {}", result.stripped_edges);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Status, Task, WorkGraph};

    fn write_graph(dir: &Path, tasks: Vec<Task>) {
        let workgraph_dir = dir.join(".workgraph");
        std::fs::create_dir_all(&workgraph_dir).unwrap();
        let graph_path = workgraph_dir.join("graph.jsonl");
        let mut graph = WorkGraph::new();
        for t in tasks {
            graph.add_node(workgraph::graph::Node::Task(t));
        }
        workgraph::parser::save_graph(&graph, &graph_path).unwrap();
    }

    #[test]
    fn migrates_legacy_coordinator_id_to_chat_prefix() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let coord = Task {
            id: ".coordinator-3".to_string(),
            title: "Coordinator: alice".to_string(),
            status: Status::InProgress,
            tags: vec!["coordinator-loop".to_string()],
            ..Default::default()
        };
        let dependent = Task {
            id: "feature-x".to_string(),
            title: "Feature X".to_string(),
            status: Status::Open,
            after: vec![".coordinator-3".to_string()],
            ..Default::default()
        };
        write_graph(dir, vec![coord, dependent]);

        run_chat_rename(&dir.join(".workgraph"), false, true).unwrap();

        let graph =
            workgraph::parser::load_graph(&dir.join(".workgraph").join("graph.jsonl")).unwrap();

        // .chat-3 exists with renamed title and tag
        let migrated = graph.get_task(".chat-3").expect("chat-3 should exist");
        assert_eq!(migrated.title, "Chat: alice");
        assert!(migrated.tags.iter().any(|t| t == "chat-loop"));
        assert!(!migrated.tags.iter().any(|t| t == "coordinator-loop"));

        // Old key is gone
        assert!(graph.get_task(".coordinator-3").is_none());

        // Dependent task's after-edge was rewritten
        let dep = graph.get_task("feature-x").expect("dependent must exist");
        assert!(dep.after.iter().any(|a| a == ".chat-3"));
        assert!(!dep.after.iter().any(|a| a == ".coordinator-3"));
    }

    #[test]
    fn migration_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let coord = Task {
            id: ".coordinator-0".to_string(),
            title: "Coordinator 0".to_string(),
            status: Status::InProgress,
            tags: vec!["coordinator-loop".to_string()],
            ..Default::default()
        };
        write_graph(dir, vec![coord]);

        run_chat_rename(&dir.join(".workgraph"), false, true).unwrap();
        run_chat_rename(&dir.join(".workgraph"), false, true).unwrap();

        let graph =
            workgraph::parser::load_graph(&dir.join(".workgraph").join("graph.jsonl")).unwrap();
        assert!(graph.get_task(".chat-0").is_some());
        assert!(graph.get_task(".coordinator-0").is_none());
    }

    #[test]
    fn retire_compact_archive_abandons_legacy_tasks() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let chat = Task {
            id: ".chat-0".to_string(),
            title: "Chat 0".to_string(),
            status: Status::InProgress,
            ..Default::default()
        };
        let compact = Task {
            id: ".compact-0".to_string(),
            title: "Compact 0".to_string(),
            status: Status::Open,
            ..Default::default()
        };
        let archive = Task {
            id: ".archive-0".to_string(),
            title: "Archive 0".to_string(),
            status: Status::Open,
            ..Default::default()
        };
        let blocked = Task {
            id: "real-task".to_string(),
            title: "Real task".to_string(),
            status: Status::Open,
            after: vec![".compact-0".to_string(), "real-prereq".to_string()],
            ..Default::default()
        };
        write_graph(dir, vec![chat, compact, archive, blocked]);

        run_retire_compact_archive(&dir.join(".workgraph"), false, true).unwrap();

        let graph =
            workgraph::parser::load_graph(&dir.join(".workgraph").join("graph.jsonl")).unwrap();
        assert_eq!(
            graph.get_task(".compact-0").unwrap().status,
            Status::Abandoned
        );
        assert_eq!(
            graph.get_task(".archive-0").unwrap().status,
            Status::Abandoned
        );
        assert_eq!(graph.get_task(".chat-0").unwrap().status, Status::InProgress);
        let real = graph.get_task("real-task").unwrap();
        assert_eq!(real.after, vec!["real-prereq".to_string()]);
    }

    #[test]
    fn retire_compact_archive_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let compact = Task {
            id: ".compact-0".to_string(),
            title: "Compact 0".to_string(),
            status: Status::Open,
            ..Default::default()
        };
        write_graph(dir, vec![compact]);

        run_retire_compact_archive(&dir.join(".workgraph"), false, true).unwrap();
        run_retire_compact_archive(&dir.join(".workgraph"), false, true).unwrap();

        let graph =
            workgraph::parser::load_graph(&dir.join(".workgraph").join("graph.jsonl")).unwrap();
        assert_eq!(
            graph.get_task(".compact-0").unwrap().status,
            Status::Abandoned
        );
    }

    #[test]
    fn dry_run_does_not_modify() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let coord = Task {
            id: ".coordinator-1".to_string(),
            title: "Coordinator 1".to_string(),
            status: Status::InProgress,
            tags: vec!["coordinator-loop".to_string()],
            ..Default::default()
        };
        write_graph(dir, vec![coord]);

        run_chat_rename(&dir.join(".workgraph"), true, true).unwrap();

        let graph =
            workgraph::parser::load_graph(&dir.join(".workgraph").join("graph.jsonl")).unwrap();
        // Legacy id still present, no chat- yet
        assert!(graph.get_task(".coordinator-1").is_some());
        assert!(graph.get_task(".chat-1").is_none());
    }
}

// ---------------------------------------------------------------------------
// `wg migrate config` — rewrite stale config.toml files to canonical form.
// ---------------------------------------------------------------------------

/// What scopes `wg migrate config` should rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigMigrateTarget {
    Global,
    Local,
    All,
}

/// Per-file summary of what `wg migrate config` changed (or would change).
#[derive(Debug, Default, Clone)]
pub struct ConfigMigrateResult {
    /// Path of the file that was inspected.
    pub path: std::path::PathBuf,
    /// Whether the file existed at all.
    pub existed: bool,
    /// Top-level keys removed because they are deprecated/no-op.
    pub removed_keys: Vec<String>,
    /// Keys renamed (legacy → canonical).
    pub renamed_keys: Vec<(String, String)>,
    /// Keys whose values were rewritten (e.g. stale model strings).
    pub rewritten_values: Vec<(String, String, String)>, // (key, old, new)
    /// Path of the backup that was written (None on dry-run / no changes).
    pub backup_path: Option<std::path::PathBuf>,
    /// Whether the file was actually written (false on dry-run / no-op).
    pub wrote: bool,
}

impl ConfigMigrateResult {
    pub fn is_noop(&self) -> bool {
        self.removed_keys.is_empty()
            && self.renamed_keys.is_empty()
            && self.rewritten_values.is_empty()
    }
}

/// Top-level entry point: dispatch to global / local / both based on target.
pub fn run_config_migrate(
    workgraph_dir: &Path,
    target: ConfigMigrateTarget,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let global_path = workgraph::config::Config::global_config_path()?;
    let local_path = workgraph_dir.join("config.toml");

    let mut results = Vec::new();
    match target {
        ConfigMigrateTarget::Global => {
            results.push(migrate_one(&global_path, dry_run)?);
        }
        ConfigMigrateTarget::Local => {
            results.push(migrate_one(&local_path, dry_run)?);
        }
        ConfigMigrateTarget::All => {
            results.push(migrate_one(&global_path, dry_run)?);
            results.push(migrate_one(&local_path, dry_run)?);
        }
    }

    if json {
        let payload: Vec<serde_json::Value> = results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "path": r.path.display().to_string(),
                    "existed": r.existed,
                    "removed_keys": r.removed_keys,
                    "renamed_keys": r.renamed_keys,
                    "rewritten_values": r.rewritten_values,
                    "wrote": r.wrote,
                    "backup_path": r.backup_path.as_ref().map(|p| p.display().to_string()),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        for r in &results {
            print_one(r, dry_run);
        }
    }
    Ok(())
}

fn print_one(r: &ConfigMigrateResult, dry_run: bool) {
    if !r.existed {
        println!(
            "{}: file does not exist — nothing to migrate",
            r.path.display()
        );
        return;
    }
    if r.is_noop() {
        println!("{}: already canonical — no changes", r.path.display());
        return;
    }
    let prefix = if dry_run { "[dry-run] " } else { "" };
    println!("{}{}:", prefix, r.path.display());
    for k in &r.removed_keys {
        println!("  - removed deprecated key: {}", k);
    }
    for (old, new) in &r.renamed_keys {
        println!("  - renamed: {} → {}", old, new);
    }
    for (k, old, new) in &r.rewritten_values {
        println!("  - {}: {:?} → {:?}", k, old, new);
    }
    if r.wrote {
        if let Some(bk) = &r.backup_path {
            println!("  ✓ wrote (backup: {})", bk.display());
        } else {
            println!("  ✓ wrote");
        }
    } else if dry_run {
        println!("  (dry-run — file not modified; rerun without --dry-run to apply)");
    }
}

/// Read one config file, compute the canonical form, and (unless dry-run)
/// write it back with a `.pre-migrate.<timestamp>` backup.
///
/// Exposed `pub(crate)` so `wg config lint` can reuse the predicates in
/// dry-run mode without touching the file. When `dry_run = true` the
/// returned `ConfigMigrateResult` describes what *would* change.
pub(crate) fn migrate_one(path: &Path, dry_run: bool) -> Result<ConfigMigrateResult> {
    let mut result = ConfigMigrateResult {
        path: path.to_path_buf(),
        ..Default::default()
    };
    if !path.exists() {
        return Ok(result);
    }
    result.existed = true;

    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {}", path.display(), e))?;

    let mut doc: toml::Value = match toml::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            anyhow::bail!(
                "{} is not valid TOML: {}\nFix syntax errors before migrating.",
                path.display(),
                e
            );
        }
    };

    let mut removed: Vec<String> = Vec::new();
    let mut renamed: Vec<(String, String)> = Vec::new();
    let mut rewritten: Vec<(String, String, String)> = Vec::new();

    // 1. Drop deprecated/no-op keys.
    drop_deprecated(&mut doc, &mut removed);

    // 2. Rename legacy field names (chat_agent → coordinator_agent, etc).
    rename_legacy_fields(&mut doc, &mut renamed);

    // 3. Fix known stale model strings (claude-sonnet-4 → -4-6, etc).
    fix_stale_model_strings(&mut doc, &mut rewritten);

    result.removed_keys = removed;
    result.renamed_keys = renamed;
    result.rewritten_values = rewritten;

    if result.is_noop() || dry_run {
        return Ok(result);
    }

    // Write backup + new file.
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let backup = path.with_extension(format!("toml.pre-migrate.{}", now));
    std::fs::copy(path, &backup).map_err(|e| {
        anyhow::anyhow!("failed to back up {} → {}: {}", path.display(), backup.display(), e)
    })?;
    result.backup_path = Some(backup);

    let new_body = toml::to_string_pretty(&doc)
        .map_err(|e| anyhow::anyhow!("failed to serialize migrated config: {}", e))?;
    std::fs::write(path, new_body)
        .map_err(|e| anyhow::anyhow!("failed to write {}: {}", path.display(), e))?;
    result.wrote = true;

    Ok(result)
}

/// Top-level [section].key pairs that the migration removes outright.
/// These are deprecated/no-op as of the audit and are never written by
/// the canonical defaults or by `wg config init`.
const DEPRECATED_KEYS: &[(&str, &str)] = &[
    // Handler is now derived from model spec's provider prefix.
    ("agent", "executor"),
    ("dispatcher", "executor"),
    ("coordinator", "executor"),
    // Compactor (.compact-N) cycle was retired.
    ("dispatcher", "compactor_interval"),
    ("dispatcher", "compactor_ops_threshold"),
    ("dispatcher", "compaction_token_threshold"),
    ("dispatcher", "compaction_threshold_ratio"),
    ("coordinator", "compactor_interval"),
    ("coordinator", "compactor_ops_threshold"),
    ("coordinator", "compaction_token_threshold"),
    ("coordinator", "compaction_threshold_ratio"),
    // Verify-shadow-task auto-spawn was replaced by .evaluate-* + wg rescue.
    ("dispatcher", "verify_autospawn_enabled"),
    ("coordinator", "verify_autospawn_enabled"),
    // Legacy verify_mode predates the ## Validation pattern.
    ("dispatcher", "verify_mode"),
    ("coordinator", "verify_mode"),
    // Old FLIP threshold knob — replaced by per-agent eval thresholds.
    ("agency", "flip_verification_threshold"),
];

fn drop_deprecated(doc: &mut toml::Value, removed: &mut Vec<String>) {
    let table = match doc.as_table_mut() {
        Some(t) => t,
        None => return,
    };
    for (section, key) in DEPRECATED_KEYS {
        if let Some(toml::Value::Table(sec)) = table.get_mut(*section)
            && sec.remove(*key).is_some()
        {
            removed.push(format!("{}.{}", section, key));
        }
        // Also drop empty sections we just emptied.
        if let Some(toml::Value::Table(sec)) = table.get(*section)
            && sec.is_empty()
        {
            table.remove(*section);
            removed.push(format!("{} (empty section)", section));
        }
    }
}

fn rename_legacy_fields(doc: &mut toml::Value, renamed: &mut Vec<(String, String)>) {
    let table = match doc.as_table_mut() {
        Some(t) => t,
        None => return,
    };
    // Rename top-level [coordinator] section to [dispatcher] when no
    // [dispatcher] section already exists. If both exist, leave them
    // alone — the user has manually split them and we don't want to
    // silently merge.
    if table.contains_key("coordinator") && !table.contains_key("dispatcher") {
        if let Some(v) = table.remove("coordinator") {
            table.insert("dispatcher".to_string(), v);
            renamed.push(("[coordinator]".to_string(), "[dispatcher]".to_string()));
        }
    }

    // Within [dispatcher], rename chat_agent → coordinator_agent + max_chats → max_coordinators.
    if let Some(toml::Value::Table(disp)) = table.get_mut("dispatcher") {
        for (old, new) in &[
            ("chat_agent", "coordinator_agent"),
            ("max_chats", "max_coordinators"),
        ] {
            if disp.contains_key(*old) && !disp.contains_key(*new) {
                if let Some(v) = disp.remove(*old) {
                    disp.insert(new.to_string(), v);
                    renamed.push((
                        format!("dispatcher.{}", old),
                        format!("dispatcher.{}", new),
                    ));
                }
            }
        }
    }
}

/// Stale model string rewrites: maps `<old>` → `<new>` substrings inside
/// any string field anywhere in the config. Conservative — only matches
/// exact full strings, not arbitrary substrings, to avoid surprising
/// rewrites of unrelated values.
const STALE_MODEL_REWRITES: &[(&str, &str)] = &[
    (
        "openrouter:anthropic/claude-sonnet-4",
        "openrouter:anthropic/claude-sonnet-4-6",
    ),
    (
        "openrouter:anthropic/claude-haiku-4",
        "openrouter:anthropic/claude-haiku-4-5",
    ),
    (
        "openrouter:anthropic/claude-opus-4",
        "openrouter:anthropic/claude-opus-4-7",
    ),
    (
        "anthropic/claude-sonnet-4",
        "anthropic/claude-sonnet-4-6",
    ),
    (
        "anthropic/claude-haiku-4",
        "anthropic/claude-haiku-4-5",
    ),
    (
        "anthropic/claude-opus-4",
        "anthropic/claude-opus-4-7",
    ),
    // Codex / OpenAI model rewrites (2026-04-28):
    // o1-pro deprecated 2026-10-23; gpt-5.4 is the new balanced default.
    ("codex:o1-pro", "codex:gpt-5.4"),
    // Old tier names predating the gpt-5.4 generation.
    ("codex:gpt-5-mini", "codex:gpt-5.4-mini"),
    ("codex:gpt-5", "codex:gpt-5.4"),
    // gpt-5-codex sunsets 2026-07-23; gpt-5.4 is the direct replacement.
    ("codex:gpt-5-codex", "codex:gpt-5.4"),
    // gpt-5.4-pro superseded by gpt-5.5 (newer, cheaper at $5/$30 vs $30/$180).
    ("codex:gpt-5.4-pro", "codex:gpt-5.5"),
];

fn fix_stale_model_strings(
    doc: &mut toml::Value,
    rewritten: &mut Vec<(String, String, String)>,
) {
    walk_strings(doc, "", &mut |path, s| {
        for (old, new) in STALE_MODEL_REWRITES {
            // Match exact full string only (not substring) so e.g.
            // `claude-sonnet-4` doesn't fire when the value is already
            // `claude-sonnet-4-6`. The `-4` suffix is a prefix of `-4-6`,
            // so a naive substring match would loop.
            if s == *old {
                let new_str = (*new).to_string();
                rewritten.push((path.to_string(), s.clone(), new_str.clone()));
                return Some(new_str);
            }
        }
        None
    });
}

/// Walk every string value in a TOML doc, calling `f(path, &value)`.
/// If `f` returns `Some(new)`, replace the value with `new`. The path
/// uses dotted notation: `"agent.model"`, `"tiers.standard"`, etc.
fn walk_strings(
    val: &mut toml::Value,
    path: &str,
    f: &mut dyn FnMut(&str, &String) -> Option<String>,
) {
    match val {
        toml::Value::String(s) => {
            if let Some(new) = f(path, s) {
                *s = new;
            }
        }
        toml::Value::Array(arr) => {
            for (i, child) in arr.iter_mut().enumerate() {
                let child_path = format!("{}[{}]", path, i);
                walk_strings(child, &child_path, f);
            }
        }
        toml::Value::Table(tbl) => {
            for (k, child) in tbl.iter_mut() {
                let child_path = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", path, k)
                };
                walk_strings(child, &child_path, f);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod config_migrate_tests {
    use super::*;
    use tempfile::TempDir;

    fn write_config(dir: &Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn strips_deprecated_agent_executor() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            r#"
[agent]
executor = "claude"
model = "claude:opus"
"#,
        );
        let r = migrate_one(&path, false).unwrap();
        assert!(
            r.removed_keys.iter().any(|k| k == "agent.executor"),
            "should remove agent.executor; got {:?}",
            r.removed_keys,
        );
        let migrated = std::fs::read_to_string(&path).unwrap();
        assert!(
            !migrated.contains("executor"),
            "migrated config should not contain executor; got:\n{}",
            migrated,
        );
        assert!(
            migrated.contains("model = \"claude:opus\""),
            "migrated config should keep model; got:\n{}",
            migrated,
        );
    }

    #[test]
    fn fixes_stale_openrouter_sonnet_model() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            r#"
[agent]
model = "openrouter:anthropic/claude-sonnet-4"
"#,
        );
        let r = migrate_one(&path, false).unwrap();
        assert!(
            r.rewritten_values
                .iter()
                .any(|(_, _, new)| new == "openrouter:anthropic/claude-sonnet-4-6"),
            "should rewrite stale sonnet-4 to sonnet-4-6; got {:?}",
            r.rewritten_values,
        );
        let migrated = std::fs::read_to_string(&path).unwrap();
        assert!(migrated.contains("openrouter:anthropic/claude-sonnet-4-6"));
        assert!(!migrated.contains("\"openrouter:anthropic/claude-sonnet-4\""));
    }

    #[test]
    fn renames_chat_agent_to_coordinator_agent() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            r#"
[dispatcher]
chat_agent = true
max_chats = 4
"#,
        );
        let r = migrate_one(&path, false).unwrap();
        assert!(
            r.renamed_keys
                .iter()
                .any(|(_, new)| new == "dispatcher.coordinator_agent"),
            "should rename chat_agent → coordinator_agent; got {:?}",
            r.renamed_keys,
        );
        let migrated = std::fs::read_to_string(&path).unwrap();
        assert!(migrated.contains("coordinator_agent"));
        assert!(migrated.contains("max_coordinators"));
        assert!(!migrated.contains("chat_agent"));
        assert!(!migrated.contains("max_chats"));
    }

    #[test]
    fn dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            r#"
[agent]
executor = "claude"
"#,
        );
        let original = std::fs::read_to_string(&path).unwrap();
        let r = migrate_one(&path, true).unwrap();
        assert!(!r.removed_keys.is_empty());
        assert!(!r.wrote);
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(original, after, "dry-run must not touch the file");
    }

    #[test]
    fn idempotent_on_canonical_config() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            r#"
[agent]
model = "claude:opus"

[tiers]
fast = "claude:haiku"
standard = "claude:sonnet"
premium = "claude:opus"
"#,
        );
        let r = migrate_one(&path, false).unwrap();
        assert!(r.is_noop(), "canonical config should be a no-op; got {:?}", r);
    }

    #[test]
    fn fixes_stale_codex_o1_pro_to_gpt54() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            r#"
[agent]
model = "codex:o1-pro"

[tiers]
fast = "codex:gpt-5-mini"
standard = "codex:gpt-5"
premium = "codex:o1-pro"
"#,
        );
        let r = migrate_one(&path, false).unwrap();
        assert!(
            r.rewritten_values
                .iter()
                .any(|(_, old, new)| old == "codex:o1-pro" && new == "codex:gpt-5.4"),
            "should rewrite codex:o1-pro to codex:gpt-5.4; got {:?}",
            r.rewritten_values,
        );
        let migrated = std::fs::read_to_string(&path).unwrap();
        assert!(migrated.contains("codex:gpt-5.4"), "migrated should contain codex:gpt-5.4");
        assert!(!migrated.contains("\"codex:o1-pro\""), "migrated should not contain codex:o1-pro");
        assert!(!migrated.contains("\"codex:gpt-5-mini\""), "migrated should not contain codex:gpt-5-mini");
        assert!(!migrated.contains("\"codex:gpt-5\""), "migrated should not contain bare codex:gpt-5");
    }

    #[test]
    fn fixes_stale_codex_gpt5_codex_to_gpt54() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            r#"
[tiers]
standard = "codex:gpt-5-codex"
premium = "codex:gpt-5.4-pro"
"#,
        );
        let r = migrate_one(&path, false).unwrap();
        assert!(
            r.rewritten_values
                .iter()
                .any(|(_, old, new)| old == "codex:gpt-5-codex" && new == "codex:gpt-5.4"),
            "should rewrite codex:gpt-5-codex to codex:gpt-5.4; got {:?}",
            r.rewritten_values,
        );
        assert!(
            r.rewritten_values
                .iter()
                .any(|(_, old, new)| old == "codex:gpt-5.4-pro" && new == "codex:gpt-5.5"),
            "should rewrite codex:gpt-5.4-pro to codex:gpt-5.5; got {:?}",
            r.rewritten_values,
        );
    }

    #[test]
    fn renames_legacy_coordinator_section_to_dispatcher() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            r#"
[coordinator]
max_agents = 4
"#,
        );
        let r = migrate_one(&path, false).unwrap();
        assert!(
            r.renamed_keys
                .iter()
                .any(|(old, new)| old == "[coordinator]" && new == "[dispatcher]"),
            "should rename [coordinator] → [dispatcher]; got {:?}",
            r.renamed_keys,
        );
        let migrated = std::fs::read_to_string(&path).unwrap();
        assert!(migrated.contains("[dispatcher]"));
        assert!(!migrated.contains("[coordinator]"));
    }
}
