use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use walkdir::WalkDir;

use workgraph::agency::{LocalStore, StoreCounts};

/// Directories to skip during recursive scan.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "__pycache__",
    "dist",
    "build",
    ".tox",
    ".cache",
];

/// A discovered agency store with its path and entity counts.
#[derive(Debug, Serialize)]
struct DiscoveredStore {
    path: String,
    bare: bool,
    counts: StoreCounts,
}

/// Scan a directory tree for agency stores.
///
/// Looks for directories matching:
///   - `<dir>/.workgraph/agency/roles/`  (project store)
///   - `<dir>/agency/roles/`             (bare store — only if no `.workgraph` parent)
///
/// Returns the list of discovered store root paths (the `agency/` dir).
fn find_agency_stores(root: &Path, max_depth: usize) -> Vec<(PathBuf, bool)> {
    let mut stores: Vec<(PathBuf, bool)> = Vec::new();
    // Track agency dirs we've already found as project stores
    // so we don't double-count them as bare stores.
    let mut seen: HashSet<PathBuf> = HashSet::new();

    let walker = WalkDir::new(root)
        .max_depth(max_depth)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if !entry.file_type().is_dir() {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            !SKIP_DIRS.contains(&name.as_ref())
        });

    for entry in walker.filter_map(|e| e.ok()) {
        if !entry.file_type().is_dir() {
            continue;
        }

        let dir = entry.path();
        let dir_name = entry.file_name().to_string_lossy();

        // Check for project store: .workgraph/agency/ exists and is a valid store
        if dir_name == ".workgraph" {
            let agency_dir = dir.join("agency");
            if LocalStore::new(&agency_dir).is_valid() {
                if let Ok(canonical) = agency_dir.canonicalize() {
                    seen.insert(canonical);
                }
                stores.push((agency_dir, false));
            }
            continue;
        }

        // Check for bare store: agency/ exists and is a valid store, but parent is NOT .workgraph
        if dir_name == "agency" && LocalStore::new(dir).is_valid() {
            // Skip if parent is .workgraph (already handled above)
            if dir.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy()) == Some(".workgraph".into()) {
                continue;
            }
            if let Ok(canonical) = dir.canonicalize() {
                if seen.contains(&canonical) {
                    continue;
                }
                seen.insert(canonical);
            }
            stores.push((dir.to_path_buf(), true));
        }
    }

    stores
}

pub fn run(root: &Path, json: bool, max_depth: usize) -> Result<()> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    let stores = find_agency_stores(&root, max_depth);

    if stores.is_empty() {
        if json {
            println!("{{\"stores\":[],\"totals\":{{\"roles\":0,\"motivations\":0,\"agents\":0,\"evaluations\":0}}}}");
        } else {
            println!("No agency stores found under {}", root.display());
        }
        return Ok(());
    }

    let mut discovered: Vec<DiscoveredStore> = Vec::new();
    let mut total = StoreCounts::default();

    for (store_path, bare) in &stores {
        let local = LocalStore::new(store_path);
        let counts = local.entity_counts();
        total.roles += counts.roles;
        total.tradeoffs += counts.tradeoffs;
        total.agents += counts.agents;
        total.evaluations += counts.evaluations;

        // Make path relative to root for display, or use absolute as fallback
        let display_path = store_path
            .strip_prefix(&root)
            .unwrap_or(store_path);

        discovered.push(DiscoveredStore {
            path: display_path.display().to_string(),
            bare: *bare,
            counts,
        });
    }

    if json {
        #[derive(Serialize)]
        struct Output {
            stores: Vec<DiscoveredStore>,
            totals: StoreCounts,
        }
        let output = Output {
            stores: discovered,
            totals: total,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Found {} agency store{}:\n", stores.len(), if stores.len() == 1 { "" } else { "s" });

        for store in &discovered {
            println!("  {}", store.path);
            println!(
                "    Roles: {}  Motivations: {}  Agents: {}  Evaluations: {}",
                store.counts.roles,
                store.counts.tradeoffs,
                store.counts.agents,
                store.counts.evaluations,
            );
            if store.bare {
                println!("    (bare store)");
            }
            println!();
        }

        if discovered.len() > 1 {
            println!(
                "Totals: {} roles, {} motivations, {} agents, {} evaluations",
                total.roles, total.tradeoffs, total.agents, total.evaluations,
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_project_store(dir: &Path) {
        let agency = dir.join(".workgraph").join("agency");
        std::fs::create_dir_all(agency.join("cache/roles")).unwrap();
        std::fs::create_dir_all(agency.join("primitives/tradeoffs")).unwrap();
        std::fs::create_dir_all(agency.join("cache/agents")).unwrap();
        std::fs::create_dir_all(agency.join("evaluations")).unwrap();
    }

    fn create_bare_store(dir: &Path) {
        let agency = dir.join("agency");
        std::fs::create_dir_all(agency.join("cache/roles")).unwrap();
        std::fs::create_dir_all(agency.join("primitives/tradeoffs")).unwrap();
        std::fs::create_dir_all(agency.join("cache/agents")).unwrap();
    }

    fn write_dummy_role(roles_dir: &Path, id: &str) {
        std::fs::write(
            roles_dir.join(format!("{}.yaml", id)),
            format!(
                "id: \"{}\"\nname: test\ncomponent_ids: []\noutcome_id: test\ndescription: test\nperformance: null\nlineage: null\n",
                id
            ),
        )
        .unwrap();
    }

    #[test]
    fn scan_finds_project_store() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("myproject");
        create_project_store(&project);
        write_dummy_role(
            &project.join(".workgraph").join("agency").join("cache/roles"),
            "abc123",
        );

        let stores = find_agency_stores(tmp.path(), 10);
        assert_eq!(stores.len(), 1);
        assert!(!stores[0].1); // not bare

        let local = LocalStore::new(&stores[0].0);
        let counts = local.entity_counts();
        assert_eq!(counts.roles, 1);
    }

    #[test]
    fn scan_finds_bare_store() {
        let tmp = TempDir::new().unwrap();
        let bare = tmp.path().join("shared");
        create_bare_store(&bare);
        write_dummy_role(&bare.join("agency").join("cache/roles"), "def456");

        let stores = find_agency_stores(tmp.path(), 10);
        assert_eq!(stores.len(), 1);
        assert!(stores[0].1); // bare
    }

    #[test]
    fn scan_finds_multiple_stores() {
        let tmp = TempDir::new().unwrap();

        let proj_a = tmp.path().join("alpha");
        create_project_store(&proj_a);
        write_dummy_role(
            &proj_a.join(".workgraph").join("agency").join("cache/roles"),
            "role1",
        );

        let proj_b = tmp.path().join("beta");
        create_project_store(&proj_b);

        let bare = tmp.path().join("shared");
        create_bare_store(&bare);
        write_dummy_role(&bare.join("agency").join("cache/roles"), "role2");
        write_dummy_role(&bare.join("agency").join("cache/roles"), "role3");

        let stores = find_agency_stores(tmp.path(), 10);
        assert_eq!(stores.len(), 3);
    }

    #[test]
    fn scan_skips_git_and_node_modules() {
        let tmp = TempDir::new().unwrap();

        // agency store hidden inside .git — should be skipped
        let git_store = tmp.path().join(".git").join("subdir");
        create_bare_store(&git_store);

        // agency store inside node_modules — should be skipped
        let nm_store = tmp.path().join("node_modules").join("pkg");
        create_bare_store(&nm_store);

        // Legitimate store
        let real = tmp.path().join("real");
        create_project_store(&real);

        let stores = find_agency_stores(tmp.path(), 10);
        assert_eq!(stores.len(), 1);
    }

    #[test]
    fn scan_respects_max_depth() {
        let tmp = TempDir::new().unwrap();

        // Store at depth 3: root / a / b / project / .workgraph/agency/roles
        let deep = tmp.path().join("a").join("b").join("project");
        create_project_store(&deep);

        // max_depth=2 should NOT find it (root=0, a=1, b=2, project=3)
        let stores = find_agency_stores(tmp.path(), 2);
        assert_eq!(stores.len(), 0);

        // max_depth=6 should find it
        let stores = find_agency_stores(tmp.path(), 6);
        assert_eq!(stores.len(), 1);
    }

    #[test]
    fn scan_empty_directory() {
        let tmp = TempDir::new().unwrap();
        let stores = find_agency_stores(tmp.path(), 10);
        assert_eq!(stores.len(), 0);
    }

    #[test]
    fn scan_empty_store_counts() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("empty");
        create_project_store(&project);

        let stores = find_agency_stores(tmp.path(), 10);
        assert_eq!(stores.len(), 1);

        let local = LocalStore::new(&stores[0].0);
        let counts = local.entity_counts();
        assert_eq!(counts.roles, 0);
        assert_eq!(counts.tradeoffs, 0);
        assert_eq!(counts.agents, 0);
        assert_eq!(counts.evaluations, 0);
    }
}
