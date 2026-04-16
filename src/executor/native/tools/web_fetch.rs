//! Web fetch tool: fetch a URL, extract main content, **write to a
//! file artifact**, and return a compact metadata+preview entry that
//! the agent can then explore with `bash cat/head/grep`.
//!
//! Two-tier fetch architecture:
//!
//! 1. **`rquest` with Chrome-136 emulation (primary path)**. Presents
//!    as a real Chrome browser at the TLS (JA3/JA4), HTTP/2, and header
//!    levels — not just User-Agent spoofing. Most anti-bot systems that
//!    block plain `reqwest` cannot distinguish us from a human browsing
//!    at interactive rates.
//!
//! 2. **Headless Chrome process (fallback)**. For the residual cases
//!    where even TLS-level emulation isn't enough (some Cloudflare
//!    Turnstile configurations, JS-rendered content, cookie walls),
//!    drop into the shared chromiumoxide `BrowserHandle` and navigate
//!    to the URL for real. Same `BrowserHandle` the `web_search`
//!    Browser backend uses, so cost is amortized across both tools.
//!
//! File artifact architecture:
//!
//! Every successful fetch writes the extracted markdown to
//! `<workgraph_dir>/nex-sessions/fetched-pages/NNNNN-<slug>.md`. The
//! tool then returns ~1 KB of metadata (path, size, line count, first
//! 20 lines preview) plus explicit bash hints for how to read the
//! file. This keeps the full page OUT of the model's context on every
//! turn — the agent reads what it needs via bash, exactly like it
//! already does for large file_read outputs. The artifact survives
//! the session for user inspection.
//!
//! Measurement: the metadata response includes `path_used`
//! (`rquest_chrome136` | `headless_chrome`) and `duration_ms` per
//! fetch so sessions can be analyzed later to measure how often the
//! browser fallback is actually needed.

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;
use url::Url;

use super::{Tool, ToolOutput};
use crate::executor::native::client::ToolDefinition;

/// Cap on the size of any single fetched page written to disk. Real
/// pages beyond this cap are truncated and the tool response says so.
/// Prevents pathological fetches (100 MB HTML bombs) from filling the
/// session dir.
const DEFAULT_MAX_CONTENT_CHARS: usize = 16_000;

/// Default HTTP request timeout.
const DEFAULT_FETCH_TIMEOUT_SECS: u64 = 30;

/// How many lines of the fetched page to inline into the tool
/// response as a preview. The agent gets a taste of the content
/// without loading the whole page into context.
const PREVIEW_LINES: usize = 20;

/// Monotonic counter for fetched-page filenames within a single
/// process. Each fetch gets a unique number regardless of URL, so
/// two fetches of the same URL produce two distinct artifacts.
static FETCH_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Register the web_fetch tool. `workgraph_dir` is the root of the
/// `.workgraph/` directory — fetched pages go under
/// `<workgraph_dir>/nex-sessions/fetched-pages/`.
pub fn register_web_fetch_tool(registry: &mut super::ToolRegistry, workgraph_dir: PathBuf) {
    registry.register(Box::new(WebFetchTool {
        workgraph_dir,
        max_content_chars: DEFAULT_MAX_CONTENT_CHARS,
        fetch_timeout_secs: DEFAULT_FETCH_TIMEOUT_SECS,
    }));
}

/// Register the web_fetch tool with custom config values.
pub fn register_web_fetch_tool_with_config(
    registry: &mut super::ToolRegistry,
    workgraph_dir: PathBuf,
    max_content_chars: usize,
    fetch_timeout_secs: u64,
) {
    registry.register(Box::new(WebFetchTool {
        workgraph_dir,
        max_content_chars,
        fetch_timeout_secs,
    }));
}

struct WebFetchTool {
    workgraph_dir: PathBuf,
    max_content_chars: usize,
    fetch_timeout_secs: u64,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_fetch".to_string(),
            description: "Fetch a web page and save it as a local markdown file artifact. \
                          Returns metadata (path, size, title) and the first 20 lines as a \
                          preview. To read the full page, use `bash` with `cat`, `head`, \
                          `tail`, `sed`, or `grep` on the returned path. \
                          \n\n\
                          Presents as a real Chrome browser (TLS + HTTP/2 + client-hints \
                          fingerprint via rquest) so anti-bot systems see a human, not a \
                          script. Falls back to a headless Chrome process if TLS emulation \
                          isn't enough. \
                          \n\n\
                          IMPORTANT: Prefer URLs returned by `web_search` over guessing \
                          URLs. Hallucinated URLs (like 'en.wikipedia.org/wiki/Naples_pizza' \
                          when the real page is 'Neapolitan_pizza') will return 404 — \
                          `web_fetch` cannot conjure pages that don't exist. Use `web_search` \
                          first, then fetch a URL from its results."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL of the web page to fetch. Must be a real URL, \
                                        typically one returned by a prior `web_search` call."
                    }
                },
                "required": ["url"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let url_str = match input.get("url").and_then(|v| v.as_str()) {
            Some(u) if !u.is_empty() => u.to_string(),
            Some(_) => return ToolOutput::error("URL must not be empty".to_string()),
            None => return ToolOutput::error("Missing required parameter: url".to_string()),
        };

        let parsed_url = match Url::parse(&url_str) {
            Ok(u) => u,
            Err(e) => return ToolOutput::error(format!("Invalid URL: {}", e)),
        };

        let overall_started = Instant::now();

        // Primary path: rquest with Chrome-136 emulation.
        let primary_result = fetch_via_rquest(&url_str, self.fetch_timeout_secs).await;

        let fetched = match primary_result {
            Ok(body) => (body, "rquest_chrome136"),
            Err(primary_err) => {
                // rquest-with-Chrome-emulation failed. Try headless Chrome.
                match fetch_via_browser(&url_str).await {
                    Ok(body) => (FetchedBody::Html(body), "headless_chrome"),
                    Err(browser_err) => {
                        return ToolOutput::error(format!(
                            "Failed to fetch URL (both paths):\n\
                             - rquest_chrome136: {}\n\
                             - headless_chrome: {}\n\n\
                             If the URL came from a web_search result, this is a transient \
                             failure — retry or use `bash` with `curl` as a last resort. If \
                             you guessed the URL, it likely doesn't exist — use `web_search` \
                             to find real URLs first.",
                            primary_err, browser_err
                        ));
                    }
                }
            }
        };
        let (body, path_used) = fetched;

        // Handle PDF vs HTML content.
        let (title, markdown) = match body {
            FetchedBody::Binary {
                ref content_type,
                ref bytes,
            } if content_type.contains("pdf") => {
                match extract_pdf_text(bytes, &self.workgraph_dir) {
                    Ok(text) => ("(PDF)".to_string(), text),
                    Err(e) => {
                        return ToolOutput::error(format!(
                            "Fetched PDF from {} ({} bytes) but failed to extract text: {}\n\n\
                             Make sure `pdftotext` is installed: `sudo apt install poppler-utils`",
                            url_str,
                            bytes.len(),
                            e
                        ));
                    }
                }
            }
            FetchedBody::Binary {
                ref content_type,
                ref bytes,
            } => {
                return ToolOutput::error(format!(
                    "Fetched {} but content-type is '{}' ({} bytes) — \
                     web_fetch only handles HTML and PDF. For other binary \
                     formats, use `bash` with `curl -o <file> <url>`.",
                    url_str,
                    content_type,
                    bytes.len()
                ));
            }
            FetchedBody::Html(ref html) => extract_to_markdown(html, &parsed_url),
        };

        // Write to a file artifact under <workgraph>/nex-sessions/fetched-pages/.
        // The agent can then `cat`/`head`/`grep` it without loading the
        // whole page into context on every turn.
        let capped_markdown = if markdown.len() > self.max_content_chars {
            let end = markdown
                .char_indices()
                .nth(self.max_content_chars)
                .map(|(i, _)| i)
                .unwrap_or(markdown.len());
            format!(
                "{}\n\n[... content truncated at {} chars; upstream page was larger ...]\n",
                &markdown[..end],
                self.max_content_chars
            )
        } else {
            markdown
        };

        let artifact_path = match self.write_artifact(&url_str, &title, &capped_markdown) {
            Ok(p) => p,
            Err(e) => {
                return ToolOutput::error(format!(
                    "Fetched {} successfully via {} but failed to write artifact file: {}",
                    url_str, path_used, e
                ));
            }
        };

        let total_bytes = capped_markdown.len();
        let total_lines = capped_markdown.lines().count();
        let duration_ms = overall_started.elapsed().as_millis() as u64;

        let mut preview = String::new();
        for (i, line) in capped_markdown.lines().take(PREVIEW_LINES).enumerate() {
            preview.push_str(&format!("{:>4}: {}\n", i + 1, line));
        }

        // Large-page guidance: if the page is bigger than a threshold,
        // surface the `summarize` tool as the preferred path for
        // extracting specific info without reading the whole thing.
        // The agent can still `cat`/`grep` the file for small slices,
        // but summarize is the right primitive for "give me X from
        // this long article" queries.
        const SUMMARIZE_SUGGEST_LINES: usize = 80;
        const SUMMARIZE_SUGGEST_BYTES: usize = 6_000;
        let suggest_summarize =
            total_lines > SUMMARIZE_SUGGEST_LINES || total_bytes > SUMMARIZE_SUGGEST_BYTES;
        let summarize_hint = if suggest_summarize {
            format!(
                "\nThis page is large ({lines} lines, {bytes} bytes). For focused \
                 extraction of specific info, prefer the `summarize` tool over reading \
                 the whole file:\n\
                 \n\
                 summarize(source='{path}', instruction='<what you want to extract>')\n\
                 \n\
                 Examples:\n\
                 • instruction='list the three best pizzerias mentioned with their addresses'\n\
                 • instruction='extract all dates and what happened on each'\n\
                 • instruction='summarize the author's main argument in 3 bullet points'\n\
                 \n\
                 The summarize tool recursively map-reduces the text so you never have \
                 to load more than one chunk at a time — use it whenever you only need \
                 a subset of what's on the page.\n",
                lines = total_lines,
                bytes = total_bytes,
                path = artifact_path.display(),
            )
        } else {
            String::new()
        };

        // Compact one-line header FIRST so the nex default display
        // mode picks a useful summary line, same treatment as
        // web_search. The grounding details + full preview follow
        // below and are visible in chatty mode.
        let response = format!(
            "web_fetch: {url} → {lines} lines, {bytes} bytes via {path_used} ({ms} ms) \
             → {path}\n\
             \n\
             Title:   {title}\n\
             \n\
             Preview (first {preview_lines} lines):\n\
             ────────────────────────────────────────────────────\n\
             {preview}\
             ────────────────────────────────────────────────────\n\
             \n\
             To read the full page, use `bash` on the path above:\n\
             • Whole file:    cat '{path}'\n\
             • First N lines: head -n 100 '{path}'\n\
             • Last N lines:  tail -n 100 '{path}'\n\
             • Search:        grep -in 'pattern' '{path}'\n\
             • Line range:    sed -n '50,120p' '{path}'\n\
             {summarize_hint}",
            url = url_str,
            title = if title.is_empty() {
                "(untitled)"
            } else {
                title.as_str()
            },
            path = artifact_path.display(),
            bytes = total_bytes,
            lines = total_lines,
            path_used = path_used,
            ms = duration_ms,
            preview_lines = PREVIEW_LINES,
            preview = preview,
            summarize_hint = summarize_hint,
        );

        ToolOutput::success(response)
    }
}

impl WebFetchTool {
    /// Write the fetched page to `<workgraph>/nex-sessions/fetched-pages/`
    /// under a counter-prefixed, URL-slug-based filename. Returns the
    /// canonical absolute path for the agent to reference.
    fn write_artifact(&self, url: &str, title: &str, markdown: &str) -> Result<PathBuf, String> {
        let dir = self
            .workgraph_dir
            .join("nex-sessions")
            .join("fetched-pages");
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("create_dir_all {}: {}", dir.display(), e))?;

        let n = FETCH_COUNTER.fetch_add(1, Ordering::SeqCst);
        let slug = slug_from_url(url);
        let filename = format!("{:05}-{}.md", n, slug);
        let path = dir.join(filename);

        // Prepend a small provenance header so the artifact is self-
        // documenting when the user opens it later.
        let header = format!(
            "<!-- web_fetch artifact -->\n\
             <!-- url: {} -->\n\
             <!-- title: {} -->\n\
             <!-- fetched: {} -->\n\n",
            url,
            title,
            chrono::Utc::now().to_rfc3339()
        );
        let body = format!("{}{}", header, markdown);

        std::fs::write(&path, body).map_err(|e| format!("write {}: {}", path.display(), e))?;

        Ok(std::fs::canonicalize(&path).unwrap_or(path))
    }
}

/// Short filesystem-safe slug from a URL's host + path, capped at 40
/// chars, with non-alphanumeric collapsed to `-`. Used in the
/// artifact filename so users opening the fetched-pages directory
/// can eyeball which file corresponds to which URL.
fn slug_from_url(url: &str) -> String {
    let parsed = Url::parse(url).ok();
    let host = parsed
        .as_ref()
        .and_then(|u| u.host_str())
        .unwrap_or("unknown");
    let path = parsed
        .as_ref()
        .map(|u| u.path().trim_matches('/').to_string())
        .unwrap_or_default();
    let combined = if path.is_empty() {
        host.to_string()
    } else {
        format!("{}-{}", host, path)
    };
    let cleaned: String = combined
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    // Collapse runs of dashes
    let mut out = String::with_capacity(cleaned.len());
    let mut prev_dash = false;
    for c in cleaned.chars() {
        if c == '-' {
            if !prev_dash {
                out.push(c);
            }
            prev_dash = true;
        } else {
            out.push(c);
            prev_dash = false;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.len() > 40 {
        trimmed[..40].to_string()
    } else {
        trimmed.to_string()
    }
}

/// Fetch via `rquest` with Chrome-136 emulation. This is the primary
/// path. Returns the response body on HTTP 2xx, otherwise an error
/// with the status or underlying reqwest error.
/// Result of a fetch: either HTML text or raw bytes with a content type.
enum FetchedBody {
    /// HTML/text content, decoded from the response charset.
    Html(String),
    /// Binary content (PDF, images, etc.) — raw bytes + content-type header.
    Binary {
        content_type: String,
        bytes: Vec<u8>,
    },
}

async fn fetch_via_rquest(url: &str, timeout_secs: u64) -> Result<FetchedBody, String> {
    let client = rquest::Client::builder()
        .emulation(rquest_util::Emulation::Chrome136)
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| format!("client build: {}", e))?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("request: {}", e))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status));
    }

    // Check content-type to decide whether to read as text or binary.
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    if content_type.contains("application/pdf") || url.to_lowercase().ends_with(".pdf") {
        let bytes = resp.bytes().await.map_err(|e| format!("body: {}", e))?;
        Ok(FetchedBody::Binary {
            content_type: "application/pdf".to_string(),
            bytes: bytes.to_vec(),
        })
    } else {
        let text = resp.text().await.map_err(|e| format!("body: {}", e))?;
        Ok(FetchedBody::Html(text))
    }
}

/// Fetch via headless Chrome. Uses the same shared `BrowserHandle`
/// instance that the `web_search` Browser backend uses, so launch
/// cost is amortized across both tools.
async fn fetch_via_browser(url: &str) -> Result<String, String> {
    use super::web_search::get_or_launch_browser_for_fetch;

    let cell = get_or_launch_browser_for_fetch().await?;

    let page = {
        let guard = cell.lock().await;
        let handle = guard
            .as_ref()
            .ok_or_else(|| "browser handle missing".to_string())?;
        handle
            .browser
            .new_page(url)
            .await
            .map_err(|e| format!("new_page: {}", e))?
    };

    // Small settle window for late JS rendering. DDG-style static
    // pages don't need this, but JS-rendered content does.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let content = match page.content().await {
        Ok(c) => c,
        Err(e) => {
            let _ = page.close().await;
            return Err(format!("content read: {}", e));
        }
    };
    let _ = page.close().await;

    Ok(content)
}

/// Extract text from a PDF using `pdftotext` (from poppler-utils).
/// Writes the raw PDF to a temp file, runs pdftotext, reads the
/// output. Returns the extracted text or an error if pdftotext
/// isn't installed or fails.
fn extract_pdf_text(bytes: &[u8], workgraph_dir: &std::path::Path) -> Result<String, String> {
    use std::process::Command;

    // Write PDF to a temp file
    let pdf_dir = workgraph_dir.join("nex-sessions").join("fetched-pages");
    std::fs::create_dir_all(&pdf_dir).map_err(|e| format!("create dir: {}", e))?;

    let n = super::web_fetch::FETCH_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let pdf_path = pdf_dir.join(format!("{:05}-download.pdf", n));
    let txt_path = pdf_dir.join(format!("{:05}-download.txt", n));

    std::fs::write(&pdf_path, bytes).map_err(|e| format!("write PDF: {}", e))?;

    // Run pdftotext — part of poppler-utils on most Linux distros
    let output = Command::new("pdftotext")
        .arg("-layout")
        .arg(&pdf_path)
        .arg(&txt_path)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "pdftotext not found — install with: sudo apt install poppler-utils".to_string()
            } else {
                format!("pdftotext exec: {}", e)
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "pdftotext exited {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    let text =
        std::fs::read_to_string(&txt_path).map_err(|e| format!("read extracted text: {}", e))?;

    // Clean up the temp files (best effort)
    let _ = std::fs::remove_file(&pdf_path);
    // Keep the txt as an artifact — useful for the user to inspect

    Ok(text)
}

/// Extract main content from HTML and convert to markdown. Returns
/// `(title, markdown)` — title may be empty if readability failed to
/// find one.
fn extract_to_markdown(html: &str, url: &Url) -> (String, String) {
    let mut cursor = Cursor::new(html.as_bytes());

    match readability::extractor::extract(&mut cursor, url) {
        Ok(product) => {
            let markdown = html2md::parse_html(&product.content);
            let cleaned = clean_markdown(&markdown);
            (product.title, cleaned)
        }
        Err(_) => {
            let markdown = html2md::parse_html(html);
            (String::new(), clean_markdown(&markdown))
        }
    }
}

/// Collapse excessive blank lines in markdown output.
fn clean_markdown(md: &str) -> String {
    let mut result = String::with_capacity(md.len());
    let mut blank_count = 0;

    for line in md.lines() {
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count <= 1 {
                result.push('\n');
            }
        } else {
            blank_count = 0;
            result.push_str(line);
            result.push('\n');
        }
    }

    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_tool() -> WebFetchTool {
        WebFetchTool {
            workgraph_dir: std::env::temp_dir().join("wg-test-fetch"),
            max_content_chars: DEFAULT_MAX_CONTENT_CHARS,
            fetch_timeout_secs: DEFAULT_FETCH_TIMEOUT_SECS,
        }
    }

    #[tokio::test]
    async fn test_web_fetch_empty_url() {
        let tool = default_tool();
        let input = json!({"url": ""});
        let output = tool.execute(&input).await;
        assert!(output.is_error);
        assert!(output.content.contains("empty"));
    }

    #[tokio::test]
    async fn test_web_fetch_missing_url() {
        let tool = default_tool();
        let input = json!({});
        let output = tool.execute(&input).await;
        assert!(output.is_error);
        assert!(output.content.contains("Missing required parameter"));
    }

    #[tokio::test]
    async fn test_web_fetch_invalid_url() {
        let tool = default_tool();
        let input = json!({"url": "not a url"});
        let output = tool.execute(&input).await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid URL"));
    }

    #[tokio::test]
    async fn test_web_fetch_read_only() {
        let tool = default_tool();
        assert!(tool.is_read_only());
    }

    #[test]
    fn test_extract_to_markdown_basic() {
        let html = r#"
        <html>
        <head><title>Test Page</title></head>
        <body>
            <nav>Navigation links here</nav>
            <article>
                <h1>Main Content</h1>
                <p>This is the main article content with some important text.</p>
                <p>Another paragraph with more details about the topic.</p>
            </article>
            <footer>Footer stuff</footer>
        </body>
        </html>"#;

        let url = Url::parse("https://example.com/test").unwrap();
        let (_title, markdown) = extract_to_markdown(html, &url);
        assert!(!markdown.is_empty());
    }

    #[test]
    fn test_clean_markdown_collapses_blanks() {
        let input = "line1\n\n\n\n\n\nline2\n\n\nline3";
        let result = clean_markdown(input);
        assert!(!result.contains("\n\n\n"));
    }

    #[test]
    fn test_extract_to_markdown_fallback() {
        let html = "<p>Just a paragraph</p>";
        let url = Url::parse("https://example.com").unwrap();
        let (_title, markdown) = extract_to_markdown(html, &url);
        assert!(markdown.contains("Just a paragraph"));
    }

    #[test]
    fn test_slug_from_url() {
        assert_eq!(
            slug_from_url("https://en.wikipedia.org/wiki/Neapolitan_pizza"),
            "en-wikipedia-org-wiki-neapolitan-pizza"
        );
        assert_eq!(slug_from_url("https://example.com/"), "example-com");
        assert_eq!(slug_from_url("not a url"), "unknown");
        let long = slug_from_url(
            "https://a.very.long.hostname.example.com/path/that/is/very/long/indeed/seriously",
        );
        assert!(long.len() <= 40);
    }
}
