//! Migrate loops_to edges to structural cycles (Phase 3).
//!
//! This command previously migrated loops_to edges to cycle_config.
//! The loops_to field has been removed from Task, so migration is no longer needed.

use anyhow::Result;
use std::path::Path;

pub fn run(dir: &Path, _dry_run: bool) -> Result<()> {
    let path = super::graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    println!("No loops_to edges to migrate (loops_to has been removed).");
    Ok(())
}
