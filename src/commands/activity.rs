//! `wg activity` — at-a-glance view of what every active agent is doing.
//!
//! Shows agent ID, task, uptime, token counts, and latest activity summary.

use anyhow::Result;
use std::path::Path;

use workgraph::graph::{format_tokens, TokenUsage};
use workgraph::service::AgentRegistry;
use workgraph::stream_event;

use super::is_process_alive;

/// Info gathered for a single active agent.
pub struct AgentActivity {
    pub agent_id: String,
    pub task_id: String,
    pub uptime: String,
    pub tokens_display: String,
    pub latest_activity: String,
}

/// Gather activity data for all active agents (reusable by TUI).
pub fn gather_activities(dir: &Path) -> Result<Vec<AgentActivity>> {
    let registry = AgentRegistry::load(dir)?;
    let agents = registry.list_agents();

    // Filter to effectively alive agents
    let alive: Vec<_> = agents
        .into_iter()
        .filter(|a| a.is_alive() && is_process_alive(a.pid))
        .collect();

    let mut activities: Vec<AgentActivity> = Vec::new();

    for agent in &alive {
        let uptime = agent.uptime_human();

        let output_path = Path::new(&agent.output_file);
        let agent_dir = output_path.parent();

        // Get token usage from stream.jsonl (canonical source)
        let token_usage = agent_dir
            .and_then(|d| stream_event::parse_token_usage_from_stream(d));
        let tokens_display = format_token_summary(&token_usage);
        let latest_activity = agent_dir
            .and_then(|d| extract_latest_activity(d))
            .unwrap_or_else(|| "-".to_string());

        activities.push(AgentActivity {
            agent_id: agent.id.clone(),
            task_id: agent.task_id.clone(),
            uptime,
            tokens_display,
            latest_activity,
        });
    }

    Ok(activities)
}

/// Run the activity command.
pub fn run(dir: &Path, json: bool) -> Result<()> {
    let activities = gather_activities(dir)?;

    if json {
        output_json(&activities)
    } else {
        output_table(&activities);
        Ok(())
    }
}

fn format_token_summary(usage: &Option<TokenUsage>) -> String {
    match usage {
        Some(u) => {
            let input_total = u.input_tokens + u.cache_read_input_tokens + u.cache_creation_input_tokens;
            format!("{}/{}", format_tokens(input_total), format_tokens(u.output_tokens))
        }
        None => "-".to_string(),
    }
}

/// Extract the latest human-readable activity from an agent's stream files.
/// Reads the tail of raw_stream.jsonl and finds the last assistant text block.
fn extract_latest_activity(agent_dir: &Path) -> Option<String> {
    // Try raw_stream.jsonl first (Claude CLI output)
    let raw_path = agent_dir.join(stream_event::RAW_STREAM_FILE_NAME);
    if raw_path.exists() {
        if let Some(text) = extract_last_assistant_text(&raw_path) {
            return Some(truncate_activity(&text, 60));
        }
    }

    // Fall back to checkpoint summaries from checkpoint files
    let checkpoints_dir = agent_dir.join("checkpoints");
    if checkpoints_dir.is_dir() {
        if let Some(text) = latest_checkpoint_summary(&checkpoints_dir) {
            return Some(truncate_activity(&text, 60));
        }
    }

    None
}

/// Read the tail of a raw_stream.jsonl file and extract the last assistant text message.
fn extract_last_assistant_text(raw_path: &Path) -> Option<String> {
    // Read last ~64KB to find the most recent assistant message
    let metadata = std::fs::metadata(raw_path).ok()?;
    let file_size = metadata.len();
    let read_offset = if file_size > 65536 {
        file_size - 65536
    } else {
        0
    };

    let content = if read_offset > 0 {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(raw_path).ok()?;
        file.seek(SeekFrom::Start(read_offset)).ok()?;
        let mut buf = String::new();
        file.read_to_string(&mut buf).ok()?;
        // Skip the first partial line
        if let Some(pos) = buf.find('\n') {
            buf[pos + 1..].to_string()
        } else {
            buf
        }
    } else {
        std::fs::read_to_string(raw_path).ok()?
    };

    let mut last_text: Option<String> = None;

    for line in content.lines().rev() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }

        let val: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if val.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }

        // Extract text blocks from message.content
        if let Some(content_blocks) = val
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        {
            for block in content_blocks.iter().rev() {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        let cleaned = text.trim();
                        if !cleaned.is_empty() {
                            last_text = Some(cleaned.to_string());
                            break;
                        }
                    }
                }
            }
        }

        if last_text.is_some() {
            break;
        }
    }

    last_text
}

/// Get the most recent checkpoint summary from the checkpoints directory.
fn latest_checkpoint_summary(checkpoints_dir: &Path) -> Option<String> {
    let mut entries: Vec<_> = std::fs::read_dir(checkpoints_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map_or(false, |ext| ext == "json")
        })
        .collect();

    entries.sort_by_key(|e| std::cmp::Reverse(e.file_name()));

    for entry in entries {
        if let Ok(content) = std::fs::read_to_string(entry.path()) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(summary) = val.get("summary").and_then(|s| s.as_str()) {
                    if !summary.is_empty() {
                        return Some(summary.to_string());
                    }
                }
            }
        }
    }

    None
}

fn truncate_activity(text: &str, max_len: usize) -> String {
    // Take only the last line (most recent statement), clean up whitespace
    let line = text.lines().last().unwrap_or(text).trim();

    if line.len() <= max_len {
        line.to_string()
    } else {
        format!("{}...", &line[..max_len - 3])
    }
}

fn output_table(activities: &[AgentActivity]) {
    if activities.is_empty() {
        println!("No active agents.");
        return;
    }

    let agent_w = activities
        .iter()
        .map(|a| a.agent_id.len())
        .max()
        .unwrap_or(5)
        .max(5);
    let task_w = activities
        .iter()
        .map(|a| a.task_id.len())
        .max()
        .unwrap_or(4)
        .clamp(4, 30);

    println!(
        "{:<agent_w$}  {:<task_w$}  {:>6}  {:>10}  LATEST ACTIVITY",
        "AGENT", "TASK", "UPTIME", "TOKENS",
    );

    for a in activities {
        let task_display = if a.task_id.len() > task_w {
            format!("{}...", &a.task_id[..task_w - 3])
        } else {
            a.task_id.clone()
        };

        println!(
            "{:<agent_w$}  {:<task_w$}  {:>6}  {:>10}  {}",
            a.agent_id, task_display, a.uptime, a.tokens_display, a.latest_activity,
        );
    }

    println!();
    println!("{} active agent(s)", activities.len());
}

fn output_json(activities: &[AgentActivity]) -> Result<()> {
    let output: Vec<_> = activities
        .iter()
        .map(|a| {
            serde_json::json!({
                "agent_id": a.agent_id,
                "task_id": a.task_id,
                "uptime": a.uptime,
                "tokens": a.tokens_display,
                "latest_activity": a.latest_activity,
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::WorkGraph;
    use workgraph::parser::save_graph;

    #[test]
    fn test_empty_registry() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_empty_registry_json() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_format_token_summary_none() {
        assert_eq!(format_token_summary(&None), "-");
    }

    #[test]
    fn test_format_token_summary_some() {
        let usage = TokenUsage {
            cost_usd: 0.0,
            input_tokens: 1_000_000,
            output_tokens: 5_000,
            cache_read_input_tokens: 500_000,
            cache_creation_input_tokens: 0,
        };
        let result = format_token_summary(&Some(usage));
        assert_eq!(result, "1.5M/5.0k");
    }

    #[test]
    fn test_truncate_activity_short() {
        assert_eq!(truncate_activity("hello", 60), "hello");
    }

    #[test]
    fn test_truncate_activity_long() {
        let long = "a".repeat(100);
        let result = truncate_activity(&long, 60);
        assert_eq!(result.len(), 60);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_truncate_activity_multiline() {
        let text = "First line\nSecond line\nThird line";
        assert_eq!(truncate_activity(text, 60), "Third line");
    }

    #[test]
    fn test_extract_last_assistant_text_from_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("raw_stream.jsonl");

        let content = r#"{"type":"system","session_id":"s1"}
{"type":"assistant","message":{"content":[{"type":"text","text":"Working on step 1"}],"usage":{"input_tokens":100,"output_tokens":50}}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}},{"type":"text","text":"Running tests now"}],"usage":{"input_tokens":200,"output_tokens":100}}}
"#;
        std::fs::write(&path, content).unwrap();

        let result = extract_last_assistant_text(&path);
        assert_eq!(result, Some("Running tests now".to_string()));
    }

    #[test]
    fn test_extract_last_assistant_text_no_text() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("raw_stream.jsonl");

        let content = r#"{"type":"system","session_id":"s1"}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}],"usage":{"input_tokens":100,"output_tokens":50}}}
"#;
        std::fs::write(&path, content).unwrap();

        let result = extract_last_assistant_text(&path);
        assert!(result.is_none());
    }

    #[test]
    fn test_latest_checkpoint_summary() {
        let dir = TempDir::new().unwrap();
        let cp_dir = dir.path().join("checkpoints");
        std::fs::create_dir(&cp_dir).unwrap();

        std::fs::write(
            cp_dir.join("001.json"),
            r#"{"summary":"Started implementation"}"#,
        )
        .unwrap();
        std::fs::write(
            cp_dir.join("002.json"),
            r#"{"summary":"Finished tests"}"#,
        )
        .unwrap();

        let result = latest_checkpoint_summary(&cp_dir);
        assert_eq!(result, Some("Finished tests".to_string()));
    }
}
