//! File tools: read_file, write_file, edit_file, glob, grep.

use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use serde_json::json;
use tokio::sync::Mutex;

use super::file_cache::FileCache;
use super::{Tool, ToolOutput, ToolRegistry, truncate_for_tool};
use crate::executor::native::client::ToolDefinition;

/// Resolve a user-provided path against the current working directory and
/// reject anything that escapes the cwd subtree.
///
/// This is the load-bearing sandbox for `write_file` and `edit_file`: a
/// hallucinating model that emits `/home/user/some-other-repo/src/foo.rs`
/// would otherwise happily write there. With this gate, the write is
/// refused before it touches disk.
///
/// Allowed:
///   - relative paths under cwd (e.g. `src/foo.rs`)
///   - absolute paths inside cwd (e.g. cwd-prefixed)
///
/// Rejected:
///   - absolute paths outside cwd (`/etc/passwd`, `/home/other/...`)
///   - relative paths that escape via `..` after canonicalization
///
/// Non-existent targets are handled by canonicalizing the deepest existing
/// ancestor and appending the remaining (not-yet-created) components.
fn resolve_inside_cwd(input: &str) -> Result<PathBuf, String> {
    let cwd = std::env::current_dir()
        .map_err(|e| format!("cannot determine current working directory: {}", e))?;
    let cwd_canonical = cwd
        .canonicalize()
        .map_err(|e| format!("cannot canonicalize cwd {:?}: {}", cwd, e))?;

    let raw = Path::new(input);
    let target = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };

    // If the target exists, canonicalize it directly. Otherwise walk up to
    // find the deepest existing ancestor, canonicalize that, then append the
    // remaining (non-existent) tail components.
    let canonical = match target.canonicalize() {
        Ok(c) => c,
        Err(_) => {
            let mut ancestor = target.as_path();
            while !ancestor.exists() {
                ancestor = match ancestor.parent() {
                    Some(p) => p,
                    None => {
                        return Err(format!("cannot resolve path: {}", input));
                    }
                };
            }
            let real_ancestor = ancestor
                .canonicalize()
                .map_err(|e| format!("cannot canonicalize {:?}: {}", ancestor, e))?;
            let suffix = target
                .strip_prefix(ancestor)
                .map_err(|_| format!("internal path resolution error for: {}", input))?;
            real_ancestor.join(suffix)
        }
    };

    if !canonical.starts_with(&cwd_canonical) {
        return Err(format!(
            "path '{}' resolves to '{}' which is outside the working directory '{}'. \
             Writes are restricted to the cwd subtree. Use a path inside the current \
             working directory, or tell the user the action you intended so they can \
             take it themselves.",
            input,
            canonical.display(),
            cwd_canonical.display()
        ));
    }
    Ok(canonical)
}

/// Register all file tools into the registry.
pub fn register_file_tools(registry: &mut ToolRegistry, workgraph_dir: PathBuf) {
    let cache = Arc::new(Mutex::new(FileCache::new()));
    registry.register(Box::new(ReadFileTool {
        cache,
        workgraph_dir,
    }));
    registry.register(Box::new(WriteFileTool));
    registry.register(Box::new(EditFileTool));
    registry.register(Box::new(GlobTool));
    registry.register(Box::new(GrepTool));
}

// ── read_file ───────────────────────────────────────────────────────────

struct ReadFileTool {
    cache: Arc<Mutex<FileCache>>,
    /// Needed to resolve a provider for `query` mode (LLM sub-call).
    workgraph_dir: PathBuf,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".to_string(),
            description: "Read a file. Two modes:\n\
                          \n\
                          - Without `query`: returns numbered lines (bytes).\n\
                          - With `query`: runs an LLM sub-call that reads the file \
                          and returns a text answer to your query. For large files \
                          an internal cursor-based traversal with compaction is used. \
                          Prefer query-mode whenever you want an answer ABOUT a file \
                          rather than the raw contents — much cheaper than reading \
                          the whole file into context yourself."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line number to start reading from (1-based). \
                                        In query mode, restricts the query to lines \
                                        offset..offset+limit."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read (default: 2000). \
                                        In query mode, bounds the slice the query sees."
                    },
                    "query": {
                        "type": "string",
                        "description": "Optional. When set, the tool returns an LLM-generated \
                                        answer to this question, computed over the file \
                                        contents (or the offset/limit slice). Without this \
                                        parameter the tool returns raw lines."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let path_str = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolOutput::error("Missing required parameter: path".to_string()),
        };

        let offset = input.get("offset").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(2000) as usize;

        // Query mode: delegate to the file_query backend. This runs an
        // LLM sub-call (single shot for small files, cursor-traversal
        // with compaction for large ones) and returns the answer text.
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());
        if let Some(query) = query {
            let offset_opt = input.get("offset").and_then(|v| v.as_u64()).map(|n| n as usize);
            let limit_opt = input.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize);
            return match super::file_query::run_query_on_file(
                &self.workgraph_dir,
                path_str,
                query,
                offset_opt,
                limit_opt,
            )
            .await
            {
                Ok(answer) => ToolOutput::success(super::truncate_for_tool(&answer, "read_file")),
                Err(e) => ToolOutput::error(format!("read_file query failed: {}", e)),
            };
        }

        // Get mtime for cache validation; error on stat failure.
        let mtime = match fs::metadata(path_str).and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(e) => {
                return ToolOutput::error(format!("Failed to read file '{}': {}", path_str, e));
            }
        };

        let path_buf = PathBuf::from(path_str);

        // Try cache first
        let cached: Option<String> = {
            let mut cache = self.cache.lock().await;
            cache.get(&path_buf, mtime)
        };

        let (content, from_cache) = if let Some(hit) = cached {
            (hit, true)
        } else {
            match fs::read_to_string(path_str) {
                Ok(content) => {
                    let mut cache = self.cache.lock().await;
                    cache.insert(path_buf, content.clone(), mtime);
                    (content, false)
                }
                Err(e) => {
                    return ToolOutput::error(format!("Failed to read file '{}': {}", path_str, e));
                }
            }
        };

        let lines: Vec<&str> = content.lines().collect();
        let start = if offset > 0 { offset - 1 } else { 0 };
        let end = (start + limit).min(lines.len());

        // Bounds check: return error if offset exceeds file length
        if start >= lines.len() {
            return ToolOutput::error(format!(
                "File has {} lines, offset {} is out of range",
                lines.len(),
                offset
            ));
        }

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

        if from_cache {
            output.push_str("\n[cached read, file unchanged]\n");
        }

        // Loud truncation notice: when the file has more lines than we
        // just returned, the model needs to know. Point at the escape
        // hatches (explicit offset+limit, query mode, reader tool) so
        // the model can escalate rather than silently thinking it saw
        // everything.
        let total_lines = lines.len();
        if end < total_lines {
            output.push_str(&format!(
                "\n[TRUNCATED at line {}. File has {} lines total ({} more below). \
                 To see more: call read_file again with a higher `offset`, pass a `query` \
                 for an LLM-answered summary of the whole file, or use `reader` for a \
                 multi-turn survey with a working directory.]\n",
                end,
                total_lines,
                total_lines - end
            ));
        }

        ToolOutput::success(truncate_for_tool(&output, "read_file"))
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
            description: "Save content to a file on disk at the given path. Writes are \
                          restricted to the current working directory tree — paths outside \
                          cwd are rejected. Use this when the user explicitly asks to save, \
                          store, create, or modify a file. Do NOT use this to display or \
                          return content to the user — include it in your text response \
                          instead."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write (must resolve inside cwd)"
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
        let path_input = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolOutput::error("Missing required parameter: path".to_string()),
        };
        let content = match input.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return ToolOutput::error("Missing required parameter: content".to_string()),
        };

        let safe_path = match resolve_inside_cwd(path_input) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };

        if let Some(parent) = safe_path.parent()
            && !parent.exists()
            && let Err(e) = fs::create_dir_all(parent)
        {
            return ToolOutput::error(format!("Failed to create directories: {}", e));
        }

        match fs::write(&safe_path, content) {
            Ok(()) => ToolOutput::success(format!(
                "Successfully wrote {} bytes to {}",
                content.len(),
                safe_path.display()
            )),
            Err(e) => ToolOutput::error(format!(
                "Failed to write file '{}': {}",
                safe_path.display(),
                e
            )),
        }
    }
}

struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit_file".to_string(),
            description: "Perform a string replacement in a file. The old_string must appear exactly once in the file. Optional normalization flags can help match strings with whitespace or line ending differences.".to_string(),
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
                    },
                    "normalize_whitespace": {
                        "type": "boolean",
                        "description": "Normalize whitespace (spaces, tabs) before matching. When enabled, sequences of whitespace characters are treated as equivalent. Default: false"
                    },
                    "normalize_line_endings": {
                        "type": "boolean",
                        "description": "Treat \\n and \\r\\n as equivalent when matching. When enabled, both Windows and Unix line endings are treated as the same. Default: false"
                    }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let path_input = match input.get("path").and_then(|v| v.as_str()) {
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
        let normalize_whitespace = input
            .get("normalize_whitespace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let normalize_line_endings = input
            .get("normalize_line_endings")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let safe_path = match resolve_inside_cwd(path_input) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        let path = safe_path.to_string_lossy().into_owned();
        let path: &str = &path;

        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("Failed to read file '{}': {}", path, e)),
        };

        // Determine the normalized versions of content and old_string for matching
        let (normalized_content, normalized_old) = if normalize_whitespace || normalize_line_endings
        {
            let content_normalized = if normalize_line_endings {
                normalize_line_endings_str(&content)
            } else {
                content.clone()
            };
            let old_normalized = if normalize_line_endings {
                normalize_line_endings_str(old_string)
            } else {
                old_string.to_string()
            };

            // Apply whitespace normalization if requested
            let content_normalized = if normalize_whitespace {
                normalize_whitespace_str(&content_normalized)
            } else {
                content_normalized
            };
            let old_normalized = if normalize_whitespace {
                normalize_whitespace_str(&old_normalized)
            } else {
                old_normalized
            };

            (content_normalized, old_normalized)
        } else {
            (content.clone(), old_string.to_string())
        };

        let count = normalized_content.matches(&normalized_old).count();
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

        // Find the actual position in the original (non-normalized) content
        let start_pos = match normalized_content.find(&normalized_old) {
            Some(pos) => pos,
            None => {
                return ToolOutput::error(format!(
                    "old_string not found in '{}'. Make sure the string matches exactly.",
                    path
                ));
            }
        };

        // Calculate the end position in the normalized string
        let end_pos = start_pos + normalized_old.len();

        // Now find the corresponding positions in the original content
        let original_start = if normalize_whitespace || normalize_line_endings {
            find_original_position(
                &content,
                &normalized_content,
                start_pos,
                normalize_whitespace,
                normalize_line_endings,
            )
        } else {
            start_pos
        };
        let original_end = if normalize_whitespace || normalize_line_endings {
            find_original_position(
                &content,
                &normalized_content,
                end_pos,
                normalize_whitespace,
                normalize_line_endings,
            )
        } else {
            end_pos
        };

        // Perform the replacement using the original positions
        let mut new_content = content[..original_start].to_string();
        new_content.push_str(new_string);
        new_content.push_str(&content[original_end..]);

        match fs::write(path, &new_content) {
            Ok(()) => ToolOutput::success(format!("Successfully edited {}", path)),
            Err(e) => ToolOutput::error(format!("Failed to write file '{}': {}", path, e)),
        }
    }
}

/// Normalize line endings: convert \r\n to \n
fn normalize_line_endings_str(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// Normalize whitespace: collapse multiple whitespace to single space
fn normalize_whitespace_str(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut last_was_whitespace = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_was_whitespace {
                result.push(' ');
                last_was_whitespace = true;
            }
        } else {
            result.push(c);
            last_was_whitespace = false;
        }
    }
    result
}

/// Find the corresponding position in the original string for a position in the normalized string.
/// This is needed because normalization can change string length.
fn find_original_position(
    original: &str,
    normalized: &str,
    normalized_pos: usize,
    normalize_ws: bool,
    normalize_le: bool,
) -> usize {
    if !normalize_ws && !normalize_le {
        return normalized_pos;
    }

    let mut orig_pos = 0;
    let mut norm_pos = 0;
    let mut orig_chars = original.chars().peekable();
    let mut norm_chars = normalized.chars().peekable();

    while norm_pos < normalized_pos {
        let norm_char = match norm_chars.next() {
            Some(c) => c,
            None => break,
        };

        // Advance through original to find matching position
        if normalize_le && norm_char == '\n' {
            // In normalized string, \n represents both \n and \r\n
            let remaining: String = orig_chars.clone().collect();
            if remaining.starts_with("\r\n") {
                orig_chars.next();
                orig_chars.next();
                orig_pos += 2;
            } else if remaining.starts_with('\n') {
                orig_chars.next();
                orig_pos += 1;
            }
            norm_pos += 1;
        } else if normalize_ws && norm_char == ' ' {
            // Skip all whitespace in original
            while let Some(&c) = orig_chars.peek() {
                if c.is_whitespace() {
                    orig_chars.next();
                    orig_pos += c.len_utf8();
                } else {
                    break;
                }
            }
            norm_pos += 1;
        } else {
            // Regular character - consume one from original
            if let Some(c) = orig_chars.next() {
                orig_pos += c.len_utf8();
            }
            norm_pos += 1;
        }
    }

    orig_pos
}

// ── glob ────────────────────────────────────────────────────────────────

struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn is_read_only(&self) -> bool {
        true
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
                    ToolOutput::success(truncate_for_tool(&results.join("\n"), "glob"))
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

    fn is_read_only(&self) -> bool {
        true
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
            ToolOutput::success(truncate_for_tool(&output, "grep"))
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

#[cfg(test)]
mod tests {
    use super::*;

    // ─── resolve_inside_cwd sandbox tests ──────────────────────────────
    //
    // These tests mutate the process-wide cwd, so they're serialized
    // against other cwd-sensitive tests via serial_test. Using a fresh
    // TempDir per test isolates them from each other.

    #[test]
    #[serial_test::serial]
    fn test_sandbox_allows_relative_path_inside_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let resolved = resolve_inside_cwd("a/b/c.txt").expect("relative path should be allowed");
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
        assert!(resolved.ends_with("a/b/c.txt"));
        std::env::set_current_dir(prev).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn test_sandbox_allows_absolute_path_inside_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let canon_cwd = tmp.path().canonicalize().unwrap();
        let abs = canon_cwd.join("foo.txt");
        let resolved =
            resolve_inside_cwd(abs.to_str().unwrap()).expect("abs path inside cwd should be OK");
        assert_eq!(resolved, abs);
        std::env::set_current_dir(prev).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn test_sandbox_rejects_absolute_path_outside_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let err = resolve_inside_cwd("/etc/passwd").expect_err("should reject escape");
        assert!(err.contains("outside the working directory"), "got: {}", err);
        std::env::set_current_dir(prev).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn test_sandbox_rejects_dotdot_escape() {
        // cwd = tmp/inner, user passes "../outside.txt" which resolves to tmp/outside.txt
        let tmp = tempfile::tempdir().unwrap();
        let inner = tmp.path().join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&inner).unwrap();
        let err = resolve_inside_cwd("../outside.txt").expect_err("dotdot escape should be rejected");
        assert!(err.contains("outside the working directory"), "got: {}", err);
        std::env::set_current_dir(prev).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn test_sandbox_permits_nonexistent_target_inside_cwd() {
        // The target file doesn't exist yet; sandbox should still validate its parent.
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let resolved = resolve_inside_cwd("does/not/exist/yet.txt").expect("new paths OK");
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
        std::env::set_current_dir(prev).unwrap();
    }

    #[tokio::test]
    async fn test_read_file_offset_beyond_end_returns_error() {
        use crate::executor::native::tools::file_cache::FileCache;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let cache = Arc::new(Mutex::new(FileCache::new()));
        let tool = ReadFileTool {
            cache,
            workgraph_dir: std::env::temp_dir().join("wg-test-readfile"),
        };

        // Create a temp file with exactly 3 lines
        let temp_file = tempfile::NamedTempFile::new().unwrap();
        let temp_path = temp_file.path().to_str().unwrap();
        std::fs::write(temp_path, "line1\nline2\nline3\n").unwrap();

        // Call read_file with offset=10 (beyond the 3 lines in the file)
        let input = serde_json::json!({
            "path": temp_path,
            "offset": 10
        });

        let output = tool.execute(&input).await;

        // Should return an error, not panic
        assert!(
            output.is_error,
            "Expected error for offset beyond file length, got: {:?}",
            output
        );
        assert!(
            output.content.contains("out of range"),
            "Error message should mention 'out of range', got: {:?}",
            output.content
        );
    }
}
