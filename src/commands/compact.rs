//! `wg compact` — manually trigger compaction to produce context.md.

use anyhow::Result;
use std::path::Path;

use workgraph::service::compactor;

pub fn run(dir: &Path, json: bool) -> Result<()> {
    let state = compactor::CompactorState::load(dir);
    let tick = state.last_tick;

    let output_path = compactor::run_compaction(dir, tick)?;

    if json {
        let result = serde_json::json!({
            "path": output_path.display().to_string(),
            "status": "ok",
        });
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Compacted → {}", output_path.display());
    }

    Ok(())
}
