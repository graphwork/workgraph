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
            description: "Fetch a web page, extract main content to markdown, save as a \
                          local file artifact, return metadata + a 20-line preview. Use \
                          `bash` on the returned path to read the full page, or pass the \
                          path to `summarize` / `reader` for LLM-answered queries over the \
                          content.\n\
                          \n\
                          Presents as a real Chrome browser (TLS + HTTP/2 fingerprint via \
                          rquest). Falls back to headless Chrome if TLS emulation isn't \
                          enough. URLs must be real — prefer ones returned by `web_search`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "URL to fetch. Typically from a prior web_search."
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
                // Non-PDF binary: save raw bytes to the fetched-pages artifact
                // directory and return a metadata entry. Agent can then do
                // whatever it needs (display, pass to a vision model, upload,
                // etc.) without having to fall back to `bash curl` — which was
                // the previous "refuse" behavior and made image/asset workflows
                // impossible through the tool alone.
                match save_binary_artifact(
                    &self.workgraph_dir,
                    &url_str,
                    content_type,
                    bytes,
                ) {
                    Ok(metadata) => return ToolOutput::success(metadata),
                    Err(e) => {
                        return ToolOutput::error(format!(
                            "Fetched {} ({}, {} bytes) but failed to save artifact: {}",
                            url_str,
                            content_type,
                            bytes.len(),
                            e
                        ));
                    }
                }
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

        let response = format!(
            "SAVED TO: {path}\n\
             (Full page content is at the path above. The preview below is ONLY the \
             first {preview_lines} lines — to read the rest, use `read_file`, `bash cat`, \
             `summarize`, or `reader` on that path.)\n\
             \n\
             URL:     {url}\n\
             Title:   {title}\n\
             Size:    {lines} lines, {bytes} bytes (fetched via {path_used} in {ms} ms)\n\
             \n\
             Preview (first {preview_lines} lines of {lines}):\n\
             ────────────────────────────────────────────────────\n\
             {preview}\
             ────────────────────────────────────────────────────\n",
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

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    // Always read bytes first, then sniff. Servers lie about content-type
    // (a `.pdf` URL returning an HTML "File not found" page with Content-Type:
    // text/html was the concrete trigger), and URL suffixes lie too. Trust
    // the bytes: PDFs begin with "%PDF-" magic.
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("body: {}", e))?
        .to_vec();

    if bytes.starts_with(b"%PDF-") {
        return Ok(FetchedBody::Binary {
            content_type: "application/pdf".to_string(),
            bytes,
        });
    }

    if is_text_content_type(&content_type) {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        return Ok(FetchedBody::Html(text));
    }

    // Non-text content-type that wasn't PDF magic — preserve as binary with
    // its real content-type so the save_binary_artifact path can pick the
    // right extension.
    Ok(FetchedBody::Binary { content_type, bytes })
}

/// True when the content-type is text-like (HTML, XML, JSON, plain text,
/// JS/CSS, or empty/unknown). These are decoded as UTF-8 and flow through
/// the html2md pipeline. Everything else is treated as a binary artifact.
fn is_text_content_type(ct: &str) -> bool {
    if ct.is_empty() {
        return true;
    }
    let prefix = ct.split(';').next().unwrap_or("").trim();
    prefix.starts_with("text/")
        || prefix == "application/xhtml+xml"
        || prefix == "application/xml"
        || prefix == "application/json"
        || prefix == "application/javascript"
        || prefix == "application/rss+xml"
        || prefix == "application/atom+xml"
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

/// Convert the full HTML to markdown. Returns `(title, markdown)`.
///
/// Previously this went through the `readability` crate to pick "the
/// main article," then ran `html2md` only on that fragment. That
/// extraction pattern silently dropped most of the page on any site
/// with multiple content regions (directory pages, product listings,
/// anything that isn't a single article). The bug report on the
/// Tennessee State Parks cabin page was the concrete trigger:
/// readability returned one `<article>` block of ~2KB from a 16KB
/// page with five sections of relevant content.
///
/// We now convert the whole HTML to markdown via `fast_html2md` (based
/// on Cloudflare's `lol_html`, benchmarked as the fastest + lowest-
/// memory inclusive extractor in the Rust ecosystem). Boilerplate
/// (nav, footer, cookie banners) comes through too — that's the
/// deliberate tradeoff. The alternative was silent content loss,
/// and noisy-complete always beats clean-incomplete for both human
/// inspection and LLM consumption.
///
/// Title is pulled from the `<title>` tag directly.
fn extract_to_markdown(html: &str, _url: &Url) -> (String, String) {
    let title = extract_title(html).unwrap_or_default();
    // `fast_html2md` exports its library as `html2md`; the fast (rewriter)
    // path is `rewrite_html(html, commonmark)`. `commonmark=false` keeps
    // the default markdown flavor.
    let markdown = html2md::rewrite_html(html, false);
    let cleaned = clean_markdown(&markdown);
    (title, cleaned)
}

/// Pull the `<title>` tag contents out of raw HTML. Case-insensitive,
/// tolerant of attributes, returns `None` if no title tag exists.
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let start_tag = lower.find("<title")?;
    let after_open = lower[start_tag..].find('>')? + start_tag + 1;
    let end_tag = lower[after_open..].find("</title>")? + after_open;
    let raw = &html[after_open..end_tag];
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        // Basic HTML-entity decode for the common cases (&amp; &lt; &gt; &quot; &#39;)
        let decoded = trimmed
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'");
        Some(decoded)
    }
}

/// Save a binary (non-HTML, non-PDF) response to the fetched-pages
/// artifact directory and return a metadata summary. Returns an error
/// only if the write itself fails.
fn save_binary_artifact(
    workgraph_dir: &std::path::Path,
    url: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<String, String> {
    let dir = workgraph_dir.join("nex-sessions").join("fetched-pages");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("create artifact dir {:?}: {}", dir, e))?;
    let counter = FETCH_COUNTER.fetch_add(1, Ordering::SeqCst);
    // Infer extension from content-type; fall back to .bin.
    let ext = binary_extension_for(content_type);
    let slug = slug_from_url(url);
    let filename = format!("{:05}-{}.{}", counter, slug, ext);
    let path = dir.join(&filename);
    std::fs::write(&path, bytes)
        .map_err(|e| format!("write {:?}: {}", path, e))?;
    Ok(format!(
        "Saved binary artifact.\n\
         URL:          {}\n\
         Content-Type: {}\n\
         Size:         {} bytes\n\
         Path:         {}\n\
         \n\
         This is a binary resource (not HTML or PDF). The raw bytes are \
         at the path above. Use `bash` to inspect — `file`, `identify`, \
         `hexdump -C`, etc. — or pass the path to another tool. \
         web_fetch does not attempt to interpret the content.",
        url,
        content_type,
        bytes.len(),
        path.display()
    ))
}

/// Map a content-type to a reasonable file extension.
fn binary_extension_for(content_type: &str) -> &'static str {
    let ct = content_type.split(';').next().unwrap_or("").trim().to_lowercase();
    match ct.as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/tiff" => "tiff",
        "image/bmp" => "bmp",
        "image/avif" => "avif",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" => "ogg",
        "audio/wav" | "audio/x-wav" => "wav",
        "application/zip" => "zip",
        "application/x-tar" => "tar",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/json" => "json",
        "application/xml" | "text/xml" => "xml",
        "text/csv" => "csv",
        _ => "bin",
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
    fn test_is_text_content_type() {
        assert!(is_text_content_type(""));
        assert!(is_text_content_type("text/html"));
        assert!(is_text_content_type("text/html; charset=utf-8"));
        assert!(is_text_content_type("text/plain"));
        assert!(is_text_content_type("application/xhtml+xml"));
        assert!(is_text_content_type("application/json"));
        assert!(is_text_content_type("application/xml"));
        assert!(is_text_content_type("application/rss+xml"));
        assert!(!is_text_content_type("application/pdf"));
        assert!(!is_text_content_type("image/png"));
        assert!(!is_text_content_type("application/octet-stream"));
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
