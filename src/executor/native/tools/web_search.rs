//! Web search tool — multi-backend router over zero-infra free endpoints.
//!
//! The query is classified by shape and routed to the backend most likely
//! to answer it well. If the chosen backend returns zero results or fails
//! outright, we try `html.duckduckgo.com` (with real browser headers) as a
//! general catch-all. If THAT also fails, the tool returns a hard error —
//! never an empty-success — so the agent has actionable signal to escalate
//! to `web_fetch` or `bash curl`.
//!
//! Backends:
//! - Google News RSS (`news.google.com/rss/search`) — news-flavored
//!   queries. No key, stable for years.
//! - Wikipedia OpenSearch API — factoid/definitional queries. Rock-solid,
//!   no key, no practical rate limit.
//! - Hacker News Algolia (`hn.algolia.com/api/v1/search`) — dev/tech
//!   queries. Clean JSON, no key.
//! - DuckDuckGo HTML (`html.duckduckgo.com/html/`) — general catch-all,
//!   used both for queries that don't match the above heuristics and as
//!   the single fallback when the primary backend fails.
//!
//! The DuckDuckGo *Instant Answer API* (`api.duckduckgo.com`) is NOT
//! used — it returns Wikipedia/Wikidata snippets only and is the wrong
//! endpoint for general web search. See
//! `docs/design/todo-web-search-fixes.md` for the full ecosystem context.

use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolOutput, truncate_for_tool};
use crate::executor::native::client::ToolDefinition;

/// Maximum snippet length before truncation.
const MAX_SNIPPET_LEN: usize = 300;

/// Maximum results returned per query.
const MAX_RESULTS: usize = 15;

/// HTTP timeout for every backend call.
const HTTP_TIMEOUT_SECS: u64 = 10;

/// User-Agent used for DuckDuckGo HTML. Needs to look like a real browser
/// or DDG serves a challenge page.
const DDG_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0";

pub fn register_web_search_tool(registry: &mut super::ToolRegistry) {
    registry.register(Box::new(WebSearchTool));
}

struct WebSearchTool;

#[derive(Debug, serde::Serialize)]
struct SearchResult {
    title: String,
    snippet: String,
    url: String,
}

/// Which backend a query was routed to. Used for (a) the `source` field
/// in the returned JSON so the model knows where the results came from,
/// and (b) deciding the fallback chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    News,
    Wikipedia,
    HackerNews,
    DdgHtml,
}

impl Backend {
    fn source_name(self) -> &'static str {
        match self {
            Backend::News => "google_news_rss",
            Backend::Wikipedia => "wikipedia_opensearch",
            Backend::HackerNews => "hn_algolia",
            Backend::DdgHtml => "duckduckgo_html",
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_search".to_string(),
            description: "Search the web. Routes news-flavored queries to Google News RSS, \
                          factoid queries to Wikipedia, dev/tech queries to Hacker News, and \
                          everything else to DuckDuckGo HTML. Returns an error (not an empty \
                          result list) when the backend fails — on error, try `web_fetch` \
                          against a specific URL or `bash` with `curl` as a fallback."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let query = match input.get("query").and_then(|v| v.as_str()) {
            Some(q) if !q.trim().is_empty() => q.trim(),
            _ => {
                return ToolOutput::error("Missing or empty required parameter: query".to_string());
            }
        };

        let client = match build_client() {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(e),
        };

        // Try the classified primary backend first, then walk a
        // conservative fallback chain of the backends most likely to
        // return useful results across arbitrary queries. The order
        // prioritizes the zero-infra endpoints that have been stable
        // for years (Wikipedia, Google News RSS, HN Algolia) before
        // falling back to DDG HTML, which is increasingly fragile.
        let primary = classify_query(query);
        let chain: Vec<Backend> = {
            let candidates = [
                primary,
                Backend::Wikipedia,
                Backend::News,
                Backend::HackerNews,
                Backend::DdgHtml,
            ];
            let mut seen = Vec::with_capacity(candidates.len());
            for c in candidates {
                if !seen.contains(&c) {
                    seen.push(c);
                }
            }
            seen
        };

        let mut attempts: Vec<(Backend, Result<Vec<SearchResult>, String>)> =
            Vec::with_capacity(chain.len());
        for backend in chain {
            let result = dispatch(&client, backend, query).await;
            match &result {
                Ok(results) if !results.is_empty() => {
                    return success_output(query, backend, results);
                }
                _ => attempts.push((backend, result)),
            }
        }

        // Every backend in the chain is dead or empty. Build a
        // per-backend diagnostic error so the model can see what was
        // tried and choose how to escalate.
        let mut lines =
            vec!["Web search failed — all backends returned no usable results.".to_string()];
        for (backend, outcome) in &attempts {
            let status = match outcome {
                Ok(_) => "returned zero results".to_string(),
                Err(e) => format!("failed: {}", e),
            };
            lines.push(format!("  - {}: {}", backend.source_name(), status));
        }
        lines.push(String::new());
        lines.push("Escalation options:".to_string());
        lines.push("  - `web_fetch` against a specific URL you already know".to_string());
        lines.push("  - `bash` with `curl` against a public endpoint, e.g.:".to_string());
        lines.push(
            "      curl 'https://news.google.com/rss/search?q=YOUR+QUERY&hl=en-US&gl=US&ceid=US:en'"
                .to_string(),
        );
        lines.push(
            "      curl 'https://en.wikipedia.org/w/api.php?action=opensearch&search=YOUR+QUERY&format=json'"
                .to_string(),
        );
        lines.push("  - refining your query and retrying".to_string());

        ToolOutput::error(lines.join("\n"))
    }
}

fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))
}

async fn dispatch(
    client: &reqwest::Client,
    backend: Backend,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    match backend {
        Backend::News => search_google_news(client, query).await,
        Backend::Wikipedia => search_wikipedia(client, query).await,
        Backend::HackerNews => search_hacker_news(client, query).await,
        Backend::DdgHtml => search_ddg_html(client, query).await,
    }
}

fn success_output(query: &str, backend: Backend, results: &[SearchResult]) -> ToolOutput {
    let output = json!({
        "query": query,
        "source": backend.source_name(),
        "results": results,
    });
    ToolOutput::success(truncate_for_tool(
        &serde_json::to_string_pretty(&output).unwrap_or_default(),
        "web_search",
    ))
}

// ─── Query classification ───────────────────────────────────────────────

/// Words/phrases that strongly suggest a news query. Matched
/// case-insensitively as whole-word substrings.
const NEWS_WORDS: &[&str] = &[
    "news",
    "today",
    "latest",
    "recent",
    "breaking",
    "yesterday",
    "this week",
    "this month",
    "headlines",
    "happened",
    "announced",
];

/// Prefixes that strongly suggest a factoid/definitional query.
/// Checked case-insensitively at the start of the trimmed query.
const FACTOID_PREFIXES: &[&str] = &[
    "what is ",
    "what are ",
    "who is ",
    "who was ",
    "when was ",
    "when did ",
    "where is ",
    "where was ",
    "define ",
    "definition of ",
    "meaning of ",
];

/// Keywords that suggest a developer/tech query. Matched
/// case-insensitively as whole-word substrings. Kept short on purpose —
/// only words that are unambiguously technical. HN is a good fallback
/// signal but we don't want to route every generic English query here.
const DEV_WORDS: &[&str] = &[
    "rust",
    "golang",
    "kotlin",
    "tokio",
    "async",
    "kubernetes",
    "k8s",
    "docker",
    "webpack",
    "postgres",
    "sqlite",
    "redis",
    "nginx",
    "systemd",
    "cargo",
    "rustc",
    "llvm",
    "wasm",
    "reqwest",
    "serde",
    "axum",
    "actix",
    "tonic",
    "stdin",
    "stdout",
    "mutex",
    "cve-",
    "rfc ",
    "rfc-",
    "npm ",
    "pip ",
    "crate ",
];

fn classify_query(query: &str) -> Backend {
    let lower = query.to_lowercase();

    if FACTOID_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return Backend::Wikipedia;
    }

    if NEWS_WORDS.iter().any(|w| contains_word(&lower, w)) {
        return Backend::News;
    }

    if DEV_WORDS.iter().any(|w| contains_word(&lower, w)) {
        return Backend::HackerNews;
    }

    Backend::DdgHtml
}

/// Substring match with word-boundary awareness at the edges. Avoids
/// matching "rust" inside "trust" or "news" inside "newsletter".
fn contains_word(haystack: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_word_char(haystack.as_bytes()[abs - 1]);
        let after_idx = abs + needle.len();
        let after_ok = after_idx == haystack.len() || !is_word_char(haystack.as_bytes()[after_idx]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

// ─── Backend: Google News RSS ───────────────────────────────────────────

async fn search_google_news(
    client: &reqwest::Client,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    let url = format!(
        "https://news.google.com/rss/search?q={}&hl=en-US&gl=US&ceid=US:en",
        urlencoding::encode(query)
    );
    let body = http_get_text(client, &url, None).await?;
    Ok(parse_google_news_rss(&body))
}

fn parse_google_news_rss(body: &str) -> Vec<SearchResult> {
    // Google News RSS items are simple and regular:
    //   <item>
    //     <title>...</title>
    //     <link>...</link>
    //     <pubDate>...</pubDate>
    //     <description>&lt;a href="..."&gt;...&lt;/a&gt;&lt;font color="..."&gt;Source&lt;/font&gt;</description>
    //     ...
    //   </item>
    //
    // A tiny regex-based parser is sturdier than pulling in a full XML
    // dep for this one shape, and Google News has been stable for years.
    let item_re = regex::Regex::new(r"(?s)<item>(.*?)</item>").ok();
    let tag_re = |name: &str| -> Option<regex::Regex> {
        regex::Regex::new(&format!("(?s)<{}>(.*?)</{}>", name, name)).ok()
    };
    let title_re = tag_re("title");
    let link_re = tag_re("link");
    let pub_re = tag_re("pubDate");
    let desc_re = tag_re("description");

    let Some(item_re) = item_re else {
        return Vec::new();
    };
    let (Some(title_re), Some(link_re), Some(pub_re), Some(desc_re)) =
        (title_re, link_re, pub_re, desc_re)
    else {
        return Vec::new();
    };

    let mut results = Vec::new();
    for cap in item_re.captures_iter(body) {
        let item = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let title = title_re
            .captures(item)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");
        let link = link_re
            .captures(item)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");
        let pub_date = pub_re
            .captures(item)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");
        let description = desc_re
            .captures(item)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");

        if title.is_empty() || link.is_empty() {
            continue;
        }

        // The description is HTML-escaped HTML: <a>title</a><font>source</font>.
        // After decoding, extract the source name (text inside <font>) and
        // compose a clean snippet.
        let decoded = html_escape::decode_html_entities(description).to_string();
        let source = extract_tag_text(&decoded, "font");
        let snippet = if let Some(src) = source {
            if pub_date.is_empty() {
                format!("Source: {}", src.trim())
            } else {
                format!("Source: {} — {}", src.trim(), pub_date.trim())
            }
        } else if !pub_date.is_empty() {
            pub_date.trim().to_string()
        } else {
            String::new()
        };

        results.push(SearchResult {
            title: clean_text(title),
            snippet: truncate_snippet(&snippet),
            url: link.trim().to_string(),
        });

        if results.len() >= MAX_RESULTS {
            break;
        }
    }

    results
}

fn extract_tag_text(html: &str, tag: &str) -> Option<String> {
    let re = regex::Regex::new(&format!(r"(?s)<{}[^>]*>(.*?)</{}>", tag, tag)).ok()?;
    re.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

// ─── Backend: Wikipedia OpenSearch ──────────────────────────────────────

async fn search_wikipedia(
    client: &reqwest::Client,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    let url = format!(
        "https://en.wikipedia.org/w/api.php?action=opensearch&search={}&limit={}&format=json",
        urlencoding::encode(query),
        MAX_RESULTS
    );
    let body = http_get_text(client, &url, None).await?;

    // Wikipedia OpenSearch returns a 4-element tuple:
    //   [query_string, titles[], descriptions[], urls[]]
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("Wikipedia response JSON parse failed: {}", e))?;
    let arr = v
        .as_array()
        .ok_or_else(|| "Wikipedia response was not a JSON array".to_string())?;
    if arr.len() < 4 {
        return Err("Wikipedia response missing expected fields".to_string());
    }

    let titles = arr[1].as_array();
    let descs = arr[2].as_array();
    let urls = arr[3].as_array();

    let (Some(titles), Some(descs), Some(urls)) = (titles, descs, urls) else {
        return Err("Wikipedia response has unexpected shape".to_string());
    };

    let mut results = Vec::new();
    for i in 0..titles.len().min(urls.len()) {
        let title = titles[i].as_str().unwrap_or("").to_string();
        let url = urls[i].as_str().unwrap_or("").to_string();
        if title.is_empty() || url.is_empty() {
            continue;
        }
        let snippet = descs
            .get(i)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        results.push(SearchResult {
            title,
            snippet: truncate_snippet(&snippet),
            url,
        });
        if results.len() >= MAX_RESULTS {
            break;
        }
    }

    Ok(results)
}

// ─── Backend: Hacker News Algolia ───────────────────────────────────────

async fn search_hacker_news(
    client: &reqwest::Client,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    let url = format!(
        "https://hn.algolia.com/api/v1/search?query={}&hitsPerPage={}",
        urlencoding::encode(query),
        MAX_RESULTS
    );
    let body = http_get_text(client, &url, None).await?;

    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("HN Algolia response JSON parse failed: {}", e))?;
    let hits = v
        .get("hits")
        .and_then(|h| h.as_array())
        .ok_or_else(|| "HN Algolia response missing hits array".to_string())?;

    let mut results = Vec::new();
    for hit in hits {
        let title = hit
            .get("title")
            .and_then(|v| v.as_str())
            .or_else(|| hit.get("story_title").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        // Prefer the external url; fall back to the HN item page.
        let url = hit
            .get("url")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| {
                hit.get("objectID")
                    .and_then(|v| v.as_str())
                    .map(|id| format!("https://news.ycombinator.com/item?id={}", id))
            })
            .unwrap_or_default();
        if title.is_empty() || url.is_empty() {
            continue;
        }
        let points = hit.get("points").and_then(|v| v.as_i64()).unwrap_or(0);
        let author = hit.get("author").and_then(|v| v.as_str()).unwrap_or("");
        let created = hit.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        let snippet = format!("{} points by {} on {}", points, author, created);
        results.push(SearchResult {
            title,
            snippet: truncate_snippet(&snippet),
            url,
        });
        if results.len() >= MAX_RESULTS {
            break;
        }
    }

    Ok(results)
}

// ─── Backend: DuckDuckGo HTML ───────────────────────────────────────────

async fn search_ddg_html(
    client: &reqwest::Client,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    let request = client
        .post("https://html.duckduckgo.com/html/")
        .header("User-Agent", DDG_USER_AGENT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.5")
        .header("DNT", "1")
        .header("Upgrade-Insecure-Requests", "1")
        .form(&[("q", query), ("kl", "us-en"), ("b", ""), ("df", "")]);

    let resp = request
        .send()
        .await
        .map_err(|e| format!("DDG HTML request failed: {}", e))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("DDG HTML returned HTTP {}", status));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("DDG HTML response body read failed: {}", e))?;

    // Don't pattern-match on page content — the word "challenge" or
    // "anomaly" can legitimately appear in a result snippet. The
    // authoritative signal is "does the page contain any `result__a`
    // anchors": if yes, parse; if no, the backend gave us nothing
    // usable (challenge page, rate limit, shape change, or genuine
    // zero-result). Return an error with a body-prefix snippet so
    // future diagnosis can see what DDG actually sent.
    if !body.contains("result__a") {
        let snippet: String = body.chars().take(200).collect();
        return Err(format!(
            "no result anchors in DDG HTML response (body {} bytes, prefix: {:?})",
            body.len(),
            snippet
        ));
    }

    Ok(parse_ddg_html(&body))
}

fn parse_ddg_html(html: &str) -> Vec<SearchResult> {
    // DDG HTML wraps each result in:
    //   <a class="result__a" href="URL">TITLE</a>
    //   <a class="result__snippet" ...>SNIPPET</a>
    //   <a class="result__url" ...>URL_DISPLAY</a>
    //
    // We extract them as parallel streams (ordered by appearance) and
    // zip them up. DDG HTML doesn't have a robust block structure in the
    // lite-format so parallel-stream is simpler than trying to chunk.
    let title_re =
        regex::Regex::new(r#"(?s)<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).ok();
    let snippet_re = regex::Regex::new(r#"(?s)<a[^>]*class="result__snippet"[^>]*>(.*?)</a>"#).ok();

    let Some(title_re) = title_re else {
        return Vec::new();
    };
    let Some(snippet_re) = snippet_re else {
        return Vec::new();
    };

    let titles_urls: Vec<(String, String)> = title_re
        .captures_iter(html)
        .filter_map(|c| {
            let url = c.get(1)?.as_str().to_string();
            let title = clean_text(c.get(2)?.as_str());
            if url.is_empty() || title.is_empty() {
                return None;
            }
            // DDG wraps the target URL in a redirect on some results:
            // /l/?uddg=ENCODED_URL&rut=... — unwrap it.
            let real_url = extract_ddg_redirect(&url).unwrap_or(url);
            Some((title, real_url))
        })
        .collect();

    let snippets: Vec<String> = snippet_re
        .captures_iter(html)
        .filter_map(|c| c.get(1).map(|m| clean_text(m.as_str())))
        .collect();

    let mut results = Vec::with_capacity(titles_urls.len().min(MAX_RESULTS));
    for (i, (title, url)) in titles_urls.into_iter().enumerate() {
        if i >= MAX_RESULTS {
            break;
        }
        let snippet = snippets
            .get(i)
            .cloned()
            .unwrap_or_else(|| format!("Result from: {}", url));
        results.push(SearchResult {
            title,
            snippet: truncate_snippet(&snippet),
            url,
        });
    }

    results
}

fn extract_ddg_redirect(url: &str) -> Option<String> {
    // DDG result links look like: /l/?uddg=https%3A%2F%2Fexample.com%2F&rut=...
    // Some are absolute (https://duckduckgo.com/l/?uddg=...) and some
    // are root-relative (/l/?uddg=...). Both get the same unwrap.
    let idx = url.find("uddg=")?;
    let after = &url[idx + 5..];
    let encoded = after.split('&').next()?;
    let decoded = urlencoding::decode(encoded).ok()?.into_owned();
    if decoded.starts_with("http") {
        Some(decoded)
    } else {
        None
    }
}

// ─── HTTP helper ────────────────────────────────────────────────────────

async fn http_get_text(
    client: &reqwest::Client,
    url: &str,
    user_agent: Option<&str>,
) -> Result<String, String> {
    let mut req = client.get(url);
    if let Some(ua) = user_agent {
        req = req.header("User-Agent", ua);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {}", e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status));
    }
    resp.text()
        .await
        .map_err(|e| format!("response body read failed: {}", e))
}

// ─── Text helpers ───────────────────────────────────────────────────────

fn truncate_snippet(snippet: &str) -> String {
    let cleaned = clean_text(snippet);
    if cleaned.len() > MAX_SNIPPET_LEN {
        let end = cleaned.floor_char_boundary(MAX_SNIPPET_LEN);
        format!("{}...", &cleaned[..end])
    } else {
        cleaned
    }
}

fn clean_text(input: &str) -> String {
    let no_tags = regex::Regex::new(r"<[^>]+>")
        .ok()
        .map(|re| re.replace_all(input, "").to_string())
        .unwrap_or_else(|| input.to_string());
    html_escape::decode_html_entities(&no_tags)
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_news_queries() {
        assert_eq!(classify_query("latest news headlines"), Backend::News);
        assert_eq!(classify_query("recent global events"), Backend::News);
        assert_eq!(classify_query("what happened today"), Backend::News);
        assert_eq!(classify_query("ukraine war news this week"), Backend::News);
    }

    #[test]
    fn classify_factoid_queries() {
        assert_eq!(classify_query("what is rust"), Backend::Wikipedia);
        assert_eq!(classify_query("who is linus torvalds"), Backend::Wikipedia);
        assert_eq!(classify_query("definition of monad"), Backend::Wikipedia);
    }

    #[test]
    fn classify_dev_queries() {
        assert_eq!(classify_query("rust tokio spawn"), Backend::HackerNews);
        assert_eq!(
            classify_query("kubernetes operator pattern"),
            Backend::HackerNews
        );
        assert_eq!(
            classify_query("docker networking bridge"),
            Backend::HackerNews
        );
    }

    #[test]
    fn classify_general_falls_back_to_ddg() {
        assert_eq!(classify_query("best pizza in paris"), Backend::DdgHtml);
        assert_eq!(classify_query("how tall is eiffel tower"), Backend::DdgHtml);
        assert_eq!(classify_query("stuff and things"), Backend::DdgHtml);
    }

    #[test]
    fn contains_word_respects_boundaries() {
        assert!(contains_word("i trust rust", "rust"));
        assert!(!contains_word("i trust you", "rust"));
        assert!(contains_word("rust lang", "rust"));
        assert!(!contains_word("trusted", "rust"));
        assert!(contains_word("rust", "rust"));
    }

    #[test]
    fn truncate_snippet_short() {
        let short = "This is a short snippet";
        assert_eq!(truncate_snippet(short), short);
    }

    #[test]
    fn truncate_snippet_long() {
        let long = "a".repeat(500);
        let result = truncate_snippet(&long);
        assert!(result.ends_with("..."));
        assert!(result.len() <= MAX_SNIPPET_LEN + 3);
    }

    #[test]
    fn clean_text_strips_html_and_whitespace() {
        let input = "<p>Hello &amp;  <b>world</b></p>\n  ";
        let result = clean_text(input);
        assert_eq!(result, "Hello & world");
    }

    #[test]
    fn parse_google_news_rss_minimal() {
        let body = r##"<?xml version="1.0"?><rss><channel>
            <item>
                <title>Example Story One</title>
                <link>https://example.com/a</link>
                <pubDate>Mon, 14 Apr 2026 12:00:00 GMT</pubDate>
                <description>&lt;a href="https://example.com/a"&gt;Example Story One&lt;/a&gt;&lt;font color="#6f6f6f"&gt;Example News&lt;/font&gt;</description>
            </item>
            <item>
                <title>Example Story Two</title>
                <link>https://example.com/b</link>
                <pubDate>Mon, 14 Apr 2026 13:00:00 GMT</pubDate>
                <description>no source tag here</description>
            </item>
        </channel></rss>"##;
        let r = parse_google_news_rss(body);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "Example Story One");
        assert_eq!(r[0].url, "https://example.com/a");
        assert!(r[0].snippet.contains("Example News"));
        assert_eq!(r[1].title, "Example Story Two");
    }

    #[test]
    fn parse_ddg_html_extracts_results() {
        let body = r##"
            <div class="result">
                <a class="result__a" href="/l/?uddg=https%3A%2F%2Fexample.com%2Fone&rut=abc">First Result</a>
                <a class="result__snippet" href="#">first snippet text</a>
                <a class="result__url" href="#">example.com</a>
            </div>
            <div class="result">
                <a class="result__a" href="https://example.com/two">Second Result</a>
                <a class="result__snippet" href="#">second snippet text</a>
                <a class="result__url" href="#">example.com</a>
            </div>
        "##;
        let r = parse_ddg_html(body);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "First Result");
        assert_eq!(r[0].url, "https://example.com/one");
        assert_eq!(r[0].snippet, "first snippet text");
        assert_eq!(r[1].title, "Second Result");
        assert_eq!(r[1].url, "https://example.com/two");
    }

    #[tokio::test]
    async fn web_search_empty_query_errors() {
        let tool = WebSearchTool;
        let input = serde_json::json!({});
        let output = tool.execute(&input).await;
        assert!(output.is_error);
        assert!(output.content.contains("Missing or empty"));
    }

    #[tokio::test]
    async fn web_search_is_read_only() {
        let tool = WebSearchTool;
        assert!(tool.is_read_only());
    }
}
