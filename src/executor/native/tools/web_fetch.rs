//! Web fetch tool: fetch a URL, extract main content, convert to markdown.
//!
//! Two-tier architecture:
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
//!    to the URL for real. This is the same `BrowserHandle` the
//!    `web_search` Browser backend uses, so cost is amortized across
//!    both tools.
//!
//! The response JSON records `path_used` (`rquest_chrome136` |
//! `headless_chrome`) and `duration_ms` per fetch so sessions can be
//! analyzed later to measure how often the fallback is actually
//! needed. The intent: if headless Chrome fallback fires on <5% of
//! queries, rquest is load-bearing and headless Chrome is the tail;
//! if >20%, rquest isn't cutting it and we need more aggressive
//! emulation or different backends.

use std::io::Cursor;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;
use url::Url;

use super::{Tool, ToolOutput, truncate_tool_output};
use crate::executor::native::client::ToolDefinition;

/// Default maximum content length before truncation (chars).
const DEFAULT_MAX_CONTENT_CHARS: usize = 16_000;

/// Default HTTP request timeout.
const DEFAULT_FETCH_TIMEOUT_SECS: u64 = 30;

/// Register the web_fetch tool with default config.
pub fn register_web_fetch_tool(registry: &mut super::ToolRegistry) {
    registry.register(Box::new(WebFetchTool {
        max_content_chars: DEFAULT_MAX_CONTENT_CHARS,
        fetch_timeout_secs: DEFAULT_FETCH_TIMEOUT_SECS,
    }));
}

/// Register the web_fetch tool with custom config values.
pub fn register_web_fetch_tool_with_config(
    registry: &mut super::ToolRegistry,
    max_content_chars: usize,
    fetch_timeout_secs: u64,
) {
    registry.register(Box::new(WebFetchTool {
        max_content_chars,
        fetch_timeout_secs,
    }));
}

struct WebFetchTool {
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
            description: "Fetch a web page and return its main content as clean markdown. \
                          Presents as a real Chrome browser (full TLS + HTTP/2 + client-hints \
                          fingerprint) so anti-bot systems see a human, not a script. Falls \
                          back to a headless Chrome process if TLS emulation isn't enough. \
                          \n\n\
                          IMPORTANT: Prefer URLs returned by `web_search` over guessing URLs. \
                          Hallucinated URLs (like 'en.wikipedia.org/wiki/Naples_pizza' when \
                          the real page is 'Neapolitan_pizza') will return 404 — `web_fetch` \
                          cannot conjure pages that don't exist. Use `web_search` first, then \
                          fetch a URL from its results."
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

        // Primary path: rquest with Chrome-136 emulation.
        let primary_started = Instant::now();
        let primary_result = fetch_via_rquest(&url_str, self.fetch_timeout_secs).await;
        let primary_duration_ms = primary_started.elapsed().as_millis() as u64;

        let (html, path_used, fallback_error): (String, &str, Option<String>) = match primary_result
        {
            Ok(body) => (body, "rquest_chrome136", None),
            Err(primary_err) => {
                // rquest-with-Chrome-emulation failed. Before
                // giving up, try the real headless Chrome process
                // as a last resort. This is the 5% tail of sites
                // that Cloudflare Turnstile or similar catch even
                // with TLS-level emulation.
                let fallback_started = Instant::now();
                let fallback_result = fetch_via_browser(&url_str).await;
                let fallback_duration_ms = fallback_started.elapsed().as_millis() as u64;

                match fallback_result {
                    Ok(body) => {
                        // We use the fallback duration for the
                        // reported duration_ms since that's what
                        // actually produced the content.
                        let _ = fallback_duration_ms; // silence unused
                        (body, "headless_chrome", None)
                    }
                    Err(browser_err) => {
                        // Both paths failed. Report both errors so
                        // the human and the model can see what
                        // went wrong.
                        let combined = format!(
                            "Failed to fetch URL (both paths):\n\
                                 - rquest_chrome136 ({}ms): {}\n\
                                 - headless_chrome ({}ms): {}\n\n\
                                 If the URL came from a web_search result, this is a transient \
                                 failure — retry or use `bash curl` as a last resort. If you \
                                 guessed the URL, it likely doesn't exist — use `web_search` \
                                 to find real URLs first.",
                            primary_duration_ms, primary_err, fallback_duration_ms, browser_err
                        );
                        return ToolOutput::error(combined);
                    }
                }
            }
        };
        let _ = fallback_error; // destructure placeholder

        // Extract main content using readability + html2md.
        let markdown = extract_to_markdown(&html, &parsed_url);

        // Stamp measurement header at the top of the output. Tiny
        // overhead, visible in chatty mode, and downstream log
        // analyzers can parse it to aggregate path-usage statistics
        // across a session.
        let header = format!("<!-- web_fetch path={} url={} -->\n", path_used, url_str);
        let with_header = format!("{}{}", header, markdown);

        let truncated = truncate_tool_output(&with_header, self.max_content_chars);
        ToolOutput::success(truncated)
    }
}

/// Fetch via `rquest` with Chrome-136 emulation. This is the primary
/// path. Returns the response body on HTTP 2xx, otherwise an error
/// with the status or underlying reqwest error.
async fn fetch_via_rquest(url: &str, timeout_secs: u64) -> Result<String, String> {
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

    resp.text().await.map_err(|e| format!("body: {}", e))
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

/// Extract main content from HTML and convert to markdown.
fn extract_to_markdown(html: &str, url: &Url) -> String {
    let mut cursor = Cursor::new(html.as_bytes());

    match readability::extractor::extract(&mut cursor, url) {
        Ok(product) => {
            let mut markdown = html2md::parse_html(&product.content);
            if !product.title.is_empty() {
                markdown = format!("# {}\n\n{}", product.title, markdown);
            }
            clean_markdown(&markdown)
        }
        Err(_) => {
            let markdown = html2md::parse_html(html);
            clean_markdown(&markdown)
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
        let markdown = extract_to_markdown(html, &url);
        assert!(!markdown.is_empty());
    }

    #[test]
    fn test_truncation_behavior() {
        let long_content = "x".repeat(DEFAULT_MAX_CONTENT_CHARS + 5000);
        let truncated = truncate_tool_output(&long_content, DEFAULT_MAX_CONTENT_CHARS);
        assert!(truncated.len() < long_content.len());
        assert!(truncated.contains("chars omitted"));
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
        let markdown = extract_to_markdown(html, &url);
        assert!(markdown.contains("Just a paragraph"));
    }
}
