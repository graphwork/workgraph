//! Web fetch tool: fetches a URL, extracts main content, converts to markdown.

use std::io::Cursor;
use std::time::Duration;

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
                          Strips navigation, ads, scripts, and boilerplate. \
                          Preserves code blocks, tables, headings, and lists."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL of the web page to fetch"
                    }
                },
                "required": ["url"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let url_str = match input.get("url").and_then(|v| v.as_str()) {
            Some(u) if !u.is_empty() => u,
            Some(_) => return ToolOutput::error("URL must not be empty".to_string()),
            None => return ToolOutput::error("Missing required parameter: url".to_string()),
        };

        let parsed_url = match Url::parse(url_str) {
            Ok(u) => u,
            Err(e) => return ToolOutput::error(format!("Invalid URL: {}", e)),
        };

        // Fetch the page
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(self.fetch_timeout_secs))
            .user_agent("workgraph-agent/0.1")
            .build()
        {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("Failed to create HTTP client: {}", e)),
        };

        let response = match client.get(url_str).send().await {
            Ok(resp) => resp,
            Err(e) => return ToolOutput::error(format!("Failed to fetch URL: {}", e)),
        };

        if !response.status().is_success() {
            return ToolOutput::error(format!("HTTP {} fetching {}", response.status(), url_str));
        }

        let html = match response.text().await {
            Ok(text) => text,
            Err(e) => return ToolOutput::error(format!("Failed to read response body: {}", e)),
        };

        // Extract main content using readability
        let markdown = extract_to_markdown(&html, &parsed_url);

        // Truncate to limit
        let truncated = truncate_tool_output(&markdown, self.max_content_chars);

        ToolOutput::success(truncated)
    }
}

/// Extract main content from HTML and convert to markdown.
fn extract_to_markdown(html: &str, url: &Url) -> String {
    let mut cursor = Cursor::new(html.as_bytes());

    match readability::extractor::extract(&mut cursor, url) {
        Ok(product) => {
            // product.content is cleaned HTML; convert to markdown
            let mut markdown = html2md::parse_html(&product.content);

            // Prepend title if available
            if !product.title.is_empty() {
                markdown = format!("# {}\n\n{}", product.title, markdown);
            }

            // Clean up excessive whitespace
            clean_markdown(&markdown)
        }
        Err(_) => {
            // Readability failed (e.g., minimal HTML). Fall back to raw HTML→markdown.
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
        // Should produce some markdown output (readability may or may not extract perfectly
        // for minimal test HTML, but html2md fallback will work)
        assert!(!markdown.is_empty());
    }

    #[test]
    fn test_truncation_behavior() {
        // Generate content longer than DEFAULT_MAX_CONTENT_CHARS
        let long_content = "x".repeat(DEFAULT_MAX_CONTENT_CHARS + 5000);
        let truncated = truncate_tool_output(&long_content, DEFAULT_MAX_CONTENT_CHARS);
        assert!(truncated.len() < long_content.len());
        assert!(truncated.contains("chars omitted"));
    }

    #[test]
    fn test_clean_markdown_collapses_blanks() {
        let input = "line1\n\n\n\n\n\nline2\n\n\nline3";
        let result = clean_markdown(input);
        // Should have at most 2 consecutive blank lines
        assert!(!result.contains("\n\n\n"));
    }

    #[test]
    fn test_extract_to_markdown_fallback() {
        // Minimal HTML that readability might not handle well
        let html = "<p>Just a paragraph</p>";
        let url = Url::parse("https://example.com").unwrap();
        let markdown = extract_to_markdown(html, &url);
        assert!(markdown.contains("Just a paragraph"));
    }
}
