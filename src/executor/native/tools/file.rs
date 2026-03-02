//! File tools: read_file, write_file, edit_file, glob, grep.

use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use regex::Regex;
use serde_json::json;

use super::{Tool, ToolOutput, ToolRegistry, truncate_output};
use crate::executor::native::client::ToolDefinition;

/// Register all file tools into the registry.
pub fn register_file_tools(registry: &mut ToolRegistry) {
    registry.register(Box::new(ReadFileTool));
    registry.register(Box::new(WriteFileTool));
    registry.register(Box::new(EditFileTool));
    registry.register(Box::new(GlobTool));
    registry.register(Box::new(GrepTool));
}

// ── read_file ───────────────────────────────────────────────────────────

struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".to_string(),
            description: "Read the contents of a file. Returns numbered lines.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line number to start reading from (1-based)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read (default: 2000)"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolOutput::error("Missing required parameter: path".to_string()),
        };

        let offset = input.get("offset").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(2000) as usize;

        match fs::read_to_string(path) {
            Ok(content) => {
                let lines: Vec<&str> = content.lines().collect();
                let start = if offset > 0 { offset - 1 } else { 0 };
                let end = (start + limit).min(lines.len());

                let mut output = String::new();
                for (i, line) in lines[start..end].iter().enumerate() {
                    let line_num = start + i + 1;
                    // Truncate long lines
                    let truncated = if line.len() > 2000 {
                        &line[..line.floor_char_boundary(2000)]
                    } else {
                        line
                    };
                    output.push_str(&format!("{:>6}\t{}\n", line_num, truncated));
                }

                ToolOutput::success(truncate_output(output))
            }
            Err(e) => ToolOutput::error(format!("Failed to read file '{}': {}", path, e)),
        }
    }
}

// ── write_file ──────────────────────────────────────────────────────────

struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".to_string(),
            description: "Write content to a file. Creates parent directories if needed."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolOutput::error("Missing required parameter: path".to_string()),
        };
        let content = match input.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return ToolOutput::error("Missing required parameter: content".to_string()),
        };

        let path = Path::new(path);

        // Create parent directories if needed
        if let Some(parent) = path.parent()
            && !parent.exists()
            && let Err(e) = fs::create_dir_all(parent)
        {
            return ToolOutput::error(format!("Failed to create directories: {}", e));
        }

        match fs::write(path, content) {
            Ok(()) => ToolOutput::success(format!(
                "Successfully wrote {} bytes to {}",
                content.len(),
                path.display()
            )),
            Err(e) => {
                ToolOutput::error(format!("Failed to write file '{}': {}", path.display(), e))
            }
        }
    }
}

// ── edit_file ───────────────────────────────────────────────────────────

struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit_file".to_string(),
            description: "Perform a string replacement in a file. The old_string must appear exactly once in the file.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to edit"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "The exact text to find and replace"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "The replacement text"
                    }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolOutput::error("Missing required parameter: path".to_string()),
        };
        let old_string = match input.get("old_string").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return ToolOutput::error("Missing required parameter: old_string".to_string()),
        };
        let new_string = match input.get("new_string").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return ToolOutput::error("Missing required parameter: new_string".to_string()),
        };

        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("Failed to read file '{}': {}", path, e)),
        };

        let count = content.matches(old_string).count();
        if count == 0 {
            return ToolOutput::error(format!(
                "old_string not found in '{}'. Make sure the string matches exactly.",
                path
            ));
        }
        if count > 1 {
            return ToolOutput::error(format!(
                "old_string found {} times in '{}'. It must be unique. Provide more context.",
                count, path
            ));
        }

        let new_content = content.replacen(old_string, new_string, 1);
        match fs::write(path, &new_content) {
            Ok(()) => ToolOutput::success(format!("Successfully edited {}", path)),
            Err(e) => ToolOutput::error(format!("Failed to write file '{}': {}", path, e)),
        }
    }
}

// ── glob ────────────────────────────────────────────────────────────────

struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "glob".to_string(),
            description: "Find files matching a glob pattern.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern to match files (e.g., '**/*.rs', 'src/**/*.ts')"
                    },
                    "path": {
                        "type": "string",
                        "description": "Base directory to search in (default: current directory)"
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let pattern = match input.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolOutput::error("Missing required parameter: pattern".to_string()),
        };

        let base = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        // Combine base path with pattern
        let full_pattern = if pattern.starts_with('/') {
            pattern.to_string()
        } else {
            format!("{}/{}", base, pattern)
        };

        match glob::glob(&full_pattern) {
            Ok(paths) => {
                let mut results: Vec<String> = Vec::new();
                for entry in paths {
                    match entry {
                        Ok(path) => results.push(path.display().to_string()),
                        Err(e) => results.push(format!("[error: {}]", e)),
                    }
                }
                if results.is_empty() {
                    ToolOutput::success("No files matched the pattern.".to_string())
                } else {
                    ToolOutput::success(truncate_output(results.join("\n")))
                }
            }
            Err(e) => ToolOutput::error(format!("Invalid glob pattern '{}': {}", pattern, e)),
        }
    }
}

// ── grep ────────────────────────────────────────────────────────────────

struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".to_string(),
            description: "Search file contents using a regex pattern. Returns matching lines with file paths and line numbers.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern to search for"
                    },
                    "path": {
                        "type": "string",
                        "description": "File or directory to search in (default: current directory)"
                    },
                    "glob": {
                        "type": "string",
                        "description": "Glob pattern to filter files (e.g., '*.rs')"
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let pattern_str = match input.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolOutput::error("Missing required parameter: pattern".to_string()),
        };

        let search_path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let glob_filter = input.get("glob").and_then(|v| v.as_str());

        let re = match Regex::new(pattern_str) {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("Invalid regex '{}': {}", pattern_str, e)),
        };

        let path = PathBuf::from(search_path);
        let mut results = Vec::new();
        let max_results = 500;

        if path.is_file() {
            search_file(&path, &re, &mut results, max_results);
        } else if path.is_dir() {
            let glob_pattern = glob_filter.and_then(|g| glob::Pattern::new(g).ok());

            for entry in walkdir::WalkDir::new(&path)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
            {
                if results.len() >= max_results {
                    break;
                }

                let entry_path = entry.path();

                // Apply glob filter if specified
                if let Some(ref pat) = glob_pattern
                    && let Some(name) = entry_path.file_name().and_then(|n| n.to_str())
                    && !pat.matches(name)
                {
                    continue;
                }

                // Skip binary files and hidden directories
                if is_likely_binary(entry_path)
                    || entry_path
                        .components()
                        .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
                {
                    continue;
                }

                search_file(entry_path, &re, &mut results, max_results);
            }
        } else {
            return ToolOutput::error(format!("Path not found: {}", search_path));
        }

        if results.is_empty() {
            ToolOutput::success("No matches found.".to_string())
        } else {
            let truncated = results.len() >= max_results;
            let mut output = results.join("\n");
            if truncated {
                output.push_str(&format!(
                    "\n\n[Results truncated at {} matches]",
                    max_results
                ));
            }
            ToolOutput::success(truncate_output(output))
        }
    }
}

fn search_file(path: &Path, re: &Regex, results: &mut Vec<String>, max: usize) {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };

    let reader = std::io::BufReader::new(file);
    for (line_num, line) in reader.lines().enumerate() {
        if results.len() >= max {
            break;
        }
        if let Ok(line) = line
            && re.is_match(&line)
        {
            results.push(format!("{}:{}: {}", path.display(), line_num + 1, line));
        }
    }
}

fn is_likely_binary(path: &Path) -> bool {
    let binary_extensions = [
        "png", "jpg", "jpeg", "gif", "bmp", "ico", "svg", "woff", "woff2", "ttf", "eot", "mp3",
        "mp4", "avi", "mov", "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "pdf", "doc", "docx",
        "xls", "xlsx", "ppt", "pptx", "exe", "dll", "so", "dylib", "o", "a", "class", "jar", "pyc",
        "wasm", "zst",
    ];

    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| binary_extensions.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}
