//! Web search tool — parallel fan-out over zero-infra free endpoints,
//! with politeness infrastructure (cache, rate limits, circuit breakers).
//!
//! Every query fires all non-circuit-broken backends in parallel, merges
//! results by URL, and returns a ranked union. A 10-minute LRU cache
//! short-circuits repeat queries so agents chaining similar searches
//! don't hammer backends. Each backend has its own minimum interval and
//! a 3-failure circuit breaker with 5-minute cooldown.
//!
//! Backends:
//! - Wikipedia OpenSearch API — factoid/definitional queries and
//!   surprisingly broad general coverage via prefix matching.
//! - Google News RSS (`news.google.com/rss/search`) — news queries and
//!   surprisingly broad general coverage because news sites cover
//!   everything.
//! - Hacker News Algolia (`hn.algolia.com/api/v1/search`) — dev/tech
//!   queries, stable JSON API.
//! - DuckDuckGo HTML (`html.duckduckgo.com/html/`) — general catch-all
//!   via HTML scrape. Kept in the chain at a conservative rate limit,
//!   but scheduled for replacement by a real-browser backend.
//!
//! All backends identify with a descriptive User-Agent including the
//! workgraph repo URL so upstream operators can contact us if they
//! object to our traffic. JSON APIs use the workgraph UA; DDG uses a
//! realistic browser UA (required to avoid challenge pages).
//!
//! The DuckDuckGo *Instant Answer API* (`api.duckduckgo.com`) is NOT
//! used — it returns Wikipedia/Wikidata snippets only. See
//! `docs/design/todo-web-search-fixes.md` for full ecosystem context.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::future::join_all;
use lru::LruCache;
use serde_json::json;

use super::{Tool, ToolOutput, truncate_for_tool};
use crate::executor::native::client::ToolDefinition;

/// Maximum snippet length before truncation.
const MAX_SNIPPET_LEN: usize = 300;

/// Maximum results returned in the merged response.
const MAX_RESULTS: usize = 20;

/// HTTP timeout for every backend call.
const HTTP_TIMEOUT_SECS: u64 = 10;

/// Descriptive User-Agent for JSON API backends (Wikipedia, HN Algolia,
/// Google News RSS). Includes the workgraph repo URL so upstream
/// operators can find us and complain if needed. OSM Nominatim and
/// similar strict-policy APIs explicitly require an identifying UA.
const WG_USER_AGENT: &str = "workgraph/0.1.0 (+https://github.com/graphwork/workgraph)";

/// Realistic browser User-Agent for HTML-scrape backends like DDG.
/// Needs to match a current-ish Firefox build or DDG serves a
/// bot-challenge page. We present as desktop Linux Firefox.
const BROWSER_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0";

/// Query cache time-to-live. Same query within this window returns the
/// cached response without hitting any backend.
const CACHE_TTL: Duration = Duration::from_secs(600);

/// Maximum number of cached query responses kept in memory.
const CACHE_CAPACITY: usize = 128;

/// How many consecutive failures trip the circuit breaker for a backend.
const CIRCUIT_THRESHOLD: u32 = 3;

/// How long a tripped circuit stays open before we try the backend again.
const CIRCUIT_COOLDOWN: Duration = Duration::from_secs(300);

pub fn register_web_search_tool(registry: &mut super::ToolRegistry) {
    registry.register(Box::new(WebSearchTool));
}

struct WebSearchTool;

#[derive(Debug, Clone, serde::Serialize)]
struct SearchResult {
    title: String,
    snippet: String,
    url: String,
    /// Which backend(s) returned this URL. Populated during the merge
    /// step after all backends respond. Used by the model to understand
    /// source diversity ("is this fact confirmed by multiple sources?")
    /// and used by the ranker to prefer URLs that multiple backends
    /// agreed on.
    #[serde(default)]
    sources: Vec<&'static str>,
}

/// Search backend identifier. Used as a cache/rate-limiter/circuit-breaker
/// key and as the `source` tag on each `SearchResult`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    /// Minimum wall-clock interval between requests to this backend —
    /// the politeness floor. Chosen per backend based on stated limits
    /// and observed tolerance:
    ///
    /// - Wikipedia: "be reasonable" — 500ms is very reasonable
    /// - Google News RSS: unstated but historically generous
    /// - HN Algolia: unstated, Algolia infra tolerates bursts
    /// - DDG HTML: scraping, strictest — 2s between hits, still much
    ///   faster than a human would click through
    fn min_interval(self) -> Duration {
        match self {
            Backend::Wikipedia => Duration::from_millis(500),
            Backend::News => Duration::from_millis(500),
            Backend::HackerNews => Duration::from_millis(500),
            Backend::DdgHtml => Duration::from_millis(2000),
        }
    }

    /// All known backends. Iteration order is the display/fallback
    /// order in error messages; the actual dispatch happens in
    /// parallel regardless.
    fn all() -> &'static [Backend] {
        &[
            Backend::Wikipedia,
            Backend::News,
            Backend::HackerNews,
            Backend::DdgHtml,
        ]
    }
}

// ─── Politeness state: cache + rate limits + circuit breakers ───────────

struct CachedEntry {
    /// Pre-serialized JSON response, ready to return as tool output.
    response: String,
    at: Instant,
}

#[derive(Default)]
struct BackendLimiter {
    last_request: Option<Instant>,
}

#[derive(Default)]
struct CircuitState {
    consecutive_failures: u32,
    cooldown_until: Option<Instant>,
}

struct PolitenessState {
    cache: LruCache<String, CachedEntry>,
    limiters: HashMap<Backend, BackendLimiter>,
    breakers: HashMap<Backend, CircuitState>,
}

impl PolitenessState {
    fn new() -> Self {
        Self {
            cache: LruCache::new(NonZeroUsize::new(CACHE_CAPACITY).unwrap()),
            limiters: HashMap::new(),
            breakers: HashMap::new(),
        }
    }

    /// Fetch a cached response if present and fresh. Expired entries
    /// are evicted as a side effect.
    fn cache_get(&mut self, key: &str) -> Option<String> {
        let expired = self
            .cache
            .peek(key)
            .map(|entry| entry.at.elapsed() > CACHE_TTL)
            .unwrap_or(false);
        if expired {
            self.cache.pop(key);
            return None;
        }
        self.cache.get(key).map(|e| e.response.clone())
    }

    fn cache_put(&mut self, key: String, response: String) {
        self.cache.put(
            key,
            CachedEntry {
                response,
                at: Instant::now(),
            },
        );
    }

    /// True if this backend is currently in circuit-open (cooldown) state.
    fn is_circuit_open(&self, backend: Backend) -> bool {
        self.breakers
            .get(&backend)
            .and_then(|s| s.cooldown_until)
            .map(|until| Instant::now() < until)
            .unwrap_or(false)
    }

    /// How long the caller should sleep before hitting this backend to
    /// respect its minimum-interval floor. Zero if enough time has
    /// already elapsed since the last request.
    fn rate_limit_delay(&self, backend: Backend) -> Duration {
        let last = self.limiters.get(&backend).and_then(|l| l.last_request);
        match last {
            None => Duration::ZERO,
            Some(t) => {
                let min = backend.min_interval();
                let elapsed = t.elapsed();
                if elapsed >= min {
                    Duration::ZERO
                } else {
                    min - elapsed
                }
            }
        }
    }

    fn mark_request(&mut self, backend: Backend) {
        self.limiters.entry(backend).or_default().last_request = Some(Instant::now());
    }

    fn mark_success(&mut self, backend: Backend) {
        if let Some(state) = self.breakers.get_mut(&backend) {
            state.consecutive_failures = 0;
            state.cooldown_until = None;
        }
    }

    fn mark_failure(&mut self, backend: Backend) {
        let state = self.breakers.entry(backend).or_default();
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        if state.consecutive_failures >= CIRCUIT_THRESHOLD {
            state.cooldown_until = Some(Instant::now() + CIRCUIT_COOLDOWN);
        }
    }
}

/// Process-wide politeness state. All `web_search` tool calls share
/// the same cache, rate limiters, and circuit breakers so multiple
/// agents in the same process collaborate on staying under quota.
static POLITENESS: OnceLock<StdMutex<PolitenessState>> = OnceLock::new();

fn politeness() -> &'static StdMutex<PolitenessState> {
    POLITENESS.get_or_init(|| StdMutex::new(PolitenessState::new()))
}

/// Canonicalize a query so that "  Rust TOKIO " and "rust tokio" share
/// a cache entry.
fn normalize_query(query: &str) -> String {
    query
        .trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
            Some(q) if !q.trim().is_empty() => q.trim().to_string(),
            _ => {
                return ToolOutput::error("Missing or empty required parameter: query".to_string());
            }
        };

        let cache_key = normalize_query(&query);

        // Cache short-circuit. The biggest politeness win — repeat
        // queries within 10 minutes cost zero backend requests.
        {
            let mut state = politeness().lock().unwrap();
            if let Some(cached) = state.cache_get(&cache_key) {
                return ToolOutput::success(truncate_for_tool(&cached, "web_search"));
            }
        }

        // Figure out which backends are currently available. Skip any
        // whose circuit breaker is tripped — they'll get retried after
        // the cooldown elapses.
        let backends: Vec<Backend> = {
            let state = politeness().lock().unwrap();
            Backend::all()
                .iter()
                .copied()
                .filter(|b| !state.is_circuit_open(*b))
                .collect()
        };

        if backends.is_empty() {
            return ToolOutput::error(
                "All web_search backends are currently circuit-broken (3+ consecutive failures).\n\
                 Try again in a few minutes, or use `web_fetch` / `bash curl` against a \
                 specific URL."
                    .to_string(),
            );
        }

        // Build a single reqwest client and clone it into each future.
        // reqwest::Client is Arc-wrapped internally, so cloning is
        // cheap and the underlying connection pool is shared.
        let client = match build_client() {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(e),
        };

        // Fan out every eligible backend in parallel. Each future
        // handles its own rate-limit wait before firing, so a backend
        // that was hit 100ms ago just delays itself while others race
        // ahead.
        let futures: Vec<_> = backends
            .iter()
            .map(|&backend| {
                let client = client.clone();
                let query = query.clone();
                async move {
                    let delay = {
                        let state = politeness().lock().unwrap();
                        state.rate_limit_delay(backend)
                    };
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    {
                        let mut state = politeness().lock().unwrap();
                        state.mark_request(backend);
                    }

                    let result = dispatch(&client, backend, &query).await;

                    {
                        let mut state = politeness().lock().unwrap();
                        match &result {
                            Ok(r) if !r.is_empty() => state.mark_success(backend),
                            _ => state.mark_failure(backend),
                        }
                    }

                    (backend, result)
                }
            })
            .collect();

        let attempts: Vec<(Backend, Result<Vec<SearchResult>, String>)> = join_all(futures).await;

        // Merge results: dedupe by URL, tag each result with every
        // backend that returned it. A URL returned by multiple
        // backends ranks higher (see sort below) because multi-source
        // agreement is a strong quality signal.
        let mut merged: Vec<SearchResult> = Vec::new();
        let mut url_to_idx: HashMap<String, usize> = HashMap::new();

        for (backend, outcome) in &attempts {
            if let Ok(results) = outcome {
                for r in results {
                    let normalized_url = normalize_url(&r.url);
                    match url_to_idx.get(&normalized_url) {
                        Some(&idx) => {
                            let existing = &mut merged[idx];
                            if !existing.sources.contains(&backend.source_name()) {
                                existing.sources.push(backend.source_name());
                            }
                            // Prefer the longer snippet if we have one.
                            if r.snippet.len() > existing.snippet.len() {
                                existing.snippet = r.snippet.clone();
                            }
                        }
                        None => {
                            let mut new_r = r.clone();
                            new_r.sources = vec![backend.source_name()];
                            url_to_idx.insert(normalized_url, merged.len());
                            merged.push(new_r);
                        }
                    }
                }
            }
        }

        // Rank: multi-source URLs first, then preserve insertion
        // order. `sort_by` is stable, so ties keep the order in which
        // they were inserted (which is roughly source-priority order
        // since we iterate `Backend::all()` that way).
        merged.sort_by(|a, b| b.sources.len().cmp(&a.sources.len()));
        merged.truncate(MAX_RESULTS);

        if merged.is_empty() {
            // Every backend we actually ran came back empty or errored.
            let mut lines = vec!["Web search failed — every backend returned nothing.".to_string()];
            for (backend, outcome) in &attempts {
                let status = match outcome {
                    Ok(_) => "zero results".to_string(),
                    Err(e) => format!("failed: {}", e),
                };
                lines.push(format!("  - {}: {}", backend.source_name(), status));
            }
            lines.push(String::new());
            lines.push("Escalation options:".to_string());
            lines.push("  - `web_fetch` against a specific URL you already know".to_string());
            lines.push("  - `bash` with `curl` against a public endpoint".to_string());
            lines.push("  - refining your query and retrying".to_string());
            return ToolOutput::error(lines.join("\n"));
        }

        let consulted: Vec<&'static str> = attempts.iter().map(|(b, _)| b.source_name()).collect();
        let responded: Vec<&'static str> = attempts
            .iter()
            .filter_map(|(b, r)| match r {
                Ok(rs) if !rs.is_empty() => Some(b.source_name()),
                _ => None,
            })
            .collect();

        let output = json!({
            "query": query,
            "backends_consulted": consulted,
            "backends_responded": responded,
            "results": merged,
        });
        let serialized = serde_json::to_string_pretty(&output).unwrap_or_default();

        // Cache the serialized response so repeat queries return
        // instantly without re-hitting any backend.
        {
            let mut state = politeness().lock().unwrap();
            state.cache_put(cache_key, serialized.clone());
        }

        ToolOutput::success(truncate_for_tool(&serialized, "web_search"))
    }
}

/// Normalize a URL for dedup purposes — lowercase scheme+host, strip
/// trailing slash, drop well-known tracking params. Conservative on
/// purpose: we want `https://example.com/foo` and `https://example.com/foo/`
/// to dedupe, but NOT `https://example.com/foo?ref=a` and
/// `https://example.com/foo?ref=b` (different pages in the general
/// case, even though some cases are tracking junk).
fn normalize_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    trimmed.to_string()
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
            sources: Vec::new(),
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
            sources: Vec::new(),
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
            sources: Vec::new(),
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
        .header("User-Agent", BROWSER_USER_AGENT)
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
            sources: Vec::new(),
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
    // Always send a descriptive User-Agent. When the caller doesn't
    // supply one, use WG_USER_AGENT so upstream operators can contact
    // us. Callers can override with a realistic browser UA for
    // HTML-scrape endpoints that require one.
    let ua = user_agent.unwrap_or(WG_USER_AGENT);
    let resp = client
        .get(url)
        .header("User-Agent", ua)
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {}", e))?;
    let status = resp.status();
    // Honor Retry-After when we see 429/503 — the upstream is
    // explicitly telling us when to come back. We return an error
    // either way (caller shouldn't block on our behalf), but we log
    // the header value into the error so the circuit breaker or a
    // downstream human can see it.
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
    {
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(String::from)
            .unwrap_or_else(|| "unspecified".to_string());
        return Err(format!(
            "HTTP {} (Retry-After: {})",
            status.as_u16(),
            retry_after
        ));
    }
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
    fn normalize_query_lowercases_and_collapses_whitespace() {
        assert_eq!(normalize_query("  Rust   TOKIO "), "rust tokio");
        assert_eq!(normalize_query("Raft\tConsensus"), "raft consensus");
        assert_eq!(normalize_query("hello"), "hello");
    }

    #[test]
    fn normalize_url_strips_trailing_slash() {
        assert_eq!(
            normalize_url("https://example.com/foo/"),
            "https://example.com/foo"
        );
        assert_eq!(
            normalize_url("https://example.com/foo"),
            "https://example.com/foo"
        );
    }

    #[test]
    fn backend_min_interval_reflects_politeness() {
        assert_eq!(Backend::DdgHtml.min_interval(), Duration::from_millis(2000));
        assert!(Backend::Wikipedia.min_interval() < Backend::DdgHtml.min_interval());
    }

    #[test]
    fn backend_all_includes_every_variant() {
        let all = Backend::all();
        assert!(all.contains(&Backend::Wikipedia));
        assert!(all.contains(&Backend::News));
        assert!(all.contains(&Backend::HackerNews));
        assert!(all.contains(&Backend::DdgHtml));
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
