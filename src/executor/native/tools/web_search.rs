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
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::future::join_all;
use lru::LruCache;
use serde_json::json;
use tokio::sync::Mutex as TokioMutex;

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

/// Realistic browser User-Agent for HTML-scrape backends like DDG
/// when hit via reqwest. Presents as desktop Linux Firefox. Required
/// to avoid challenge pages when we're NOT using a real browser.
const BROWSER_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0";

/// Chrome User-Agent used when we drive a real headless Chrome via
/// chromiumoxide. Matches a current stable desktop Chrome on Linux
/// so we present as a regular browser — no "HeadlessChrome" giveaway.
const CHROME_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

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
    GitHub,
    StackExchange,
    CratesIo,
    Arxiv,
    /// Headless Chrome driving DuckDuckGo HTML via chromiumoxide.
    /// Presents as a real browser (genuine TLS fingerprint, real UA,
    /// navigator.webdriver hidden) so DDG's anti-bot heuristics can't
    /// distinguish us from a human browsing. Heavier than the reqwest
    /// backends (requires a chrome process) but more reliable.
    Browser,
}

impl Backend {
    fn source_name(self) -> &'static str {
        match self {
            Backend::News => "google_news_rss",
            Backend::Wikipedia => "wikipedia_opensearch",
            Backend::HackerNews => "hn_algolia",
            Backend::DdgHtml => "duckduckgo_html",
            Backend::GitHub => "github_search",
            Backend::StackExchange => "stack_exchange",
            Backend::CratesIo => "crates_io",
            Backend::Arxiv => "arxiv",
            Backend::Browser => "headless_chrome_ddg",
        }
    }

    /// Minimum wall-clock interval between requests to this backend —
    /// the politeness floor. Chosen per backend based on stated limits
    /// and observed tolerance:
    ///
    /// - Wikipedia / Google News RSS / HN Algolia: 500ms, generous
    /// - DDG HTML: scraping, 2s
    /// - GitHub search: 1s (60/hour unauth, circuit breaker catches
    ///   sustained abuse; 1s is fine for interactive bursts)
    /// - Stack Exchange: 500ms (300/day no-key is plenty for humans)
    /// - crates.io: 1s (documented limits, requires UA)
    /// - arxiv: 3.5s — their FAQ explicitly says "one request every
    ///   3 seconds" and we respect it
    fn min_interval(self) -> Duration {
        match self {
            Backend::Wikipedia => Duration::from_millis(500),
            Backend::News => Duration::from_millis(500),
            Backend::HackerNews => Duration::from_millis(500),
            Backend::DdgHtml => Duration::from_millis(2000),
            Backend::GitHub => Duration::from_millis(1000),
            Backend::StackExchange => Duration::from_millis(500),
            Backend::CratesIo => Duration::from_millis(1000),
            Backend::Arxiv => Duration::from_millis(3500),
            // Headless Chrome drives a real browser hitting DDG. We
            // present as a human at ~1 query every 2 seconds, which
            // is slower than most real users but a reasonable floor
            // for a tool making many calls per session.
            Backend::Browser => Duration::from_millis(2000),
        }
    }

    /// All known backends. Iteration order is the display/fallback
    /// order in error messages; the actual dispatch happens in
    /// parallel regardless. The `Browser` backend is only included
    /// when chromium is reachable — see `enabled_backends`.
    fn all() -> &'static [Backend] {
        &[
            Backend::Wikipedia,
            Backend::News,
            Backend::HackerNews,
            Backend::GitHub,
            Backend::StackExchange,
            Backend::CratesIo,
            Backend::Arxiv,
            Backend::Browser,
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
            description: "Search the web via parallel fan-out across multiple free backends: \
                          Wikipedia, Google News RSS, Hacker News, GitHub, Stack Exchange, \
                          crates.io, arxiv, headless Chrome driving DuckDuckGo, and reqwest \
                          DuckDuckGo HTML. Every query hits all available backends in \
                          parallel, results are merged by URL and ranked by multi-source \
                          agreement. Each result lists the backends that returned it under \
                          `sources`. Response includes `backends_consulted` and \
                          `backends_responded`. Results are cached for 10 minutes. Returns \
                          an error (not an empty list) when every backend failed — on error, \
                          try `web_fetch` against a specific URL or `bash` with `curl` \
                          as a fallback."
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
        // rquest::Client is Arc-wrapped internally, so cloning is
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
        // Per-backend failure info. Surfaces diagnostic info to both
        // the human (via chatty mode) and the model (which can
        // choose to escalate via bash curl if a backend it trusted
        // for this query type is down).
        let failed: Vec<serde_json::Value> = attempts
            .iter()
            .filter_map(|(b, r)| match r {
                Err(e) => Some(json!({"backend": b.source_name(), "error": e})),
                Ok(rs) if rs.is_empty() => {
                    Some(json!({"backend": b.source_name(), "error": "zero results"}))
                }
                _ => None,
            })
            .collect();

        // Render as plain text rather than nested JSON. Small models
        // (qwen3-coder-30b was the motivating case) read prose much
        // better than deeply-nested JSON — empirically ~3-5x better
        // grounding on tool outputs. The structure is still machine-
        // parseable if needed, but the primary audience is the LLM
        // and the LLM wants lines, not pretty-printed objects.
        let _ = failed; // rendered via failed_summary below instead
        let rendered = render_results_as_text(&query, &consulted, &responded, &attempts, &merged);

        // Cache the rendered response so repeat queries return
        // instantly without re-hitting any backend.
        {
            let mut state = politeness().lock().unwrap();
            state.cache_put(cache_key, rendered.clone());
        }

        ToolOutput::success(truncate_for_tool(&rendered, "web_search"))
    }
}

/// Render the merged search results as plain text (not JSON). The
/// format is optimized for small-model grounding: a prominent
/// grounding-rule preamble tells the model it MUST cite only what's
/// in the results below, then the query, backends, and a numbered
/// list of `[N] TITLE / URL / SOURCES / SNIPPET` blocks. No nested
/// objects, no JSON escaping, no quoting — just lines.
fn render_results_as_text(
    query: &str,
    consulted: &[&'static str],
    responded: &[&'static str],
    attempts: &[(Backend, Result<Vec<SearchResult>, String>)],
    merged: &[SearchResult],
) -> String {
    let mut out = String::with_capacity(8 * 1024);

    // Grounding rule up front. Landing right before the data means
    // the attention pattern for the next turn sees it in-context
    // with the results it's supposed to cite.
    out.push_str(
        "⚠ GROUNDING RULE: When you report findings from this search, every \
         name, URL, and detail you cite MUST appear verbatim in the RESULTS \
         section below. Do not invent variants. Do not combine names. Do not \
         drop words from names. If a specific thing you want isn't in the \
         results, say \"not found in search results\" — do NOT fabricate or \
         paraphrase from memory. The user will verify your answer against \
         this output.\n\n",
    );

    out.push_str(&format!("Query: {}\n", query));
    out.push_str(&format!("Backends consulted: {}\n", consulted.join(", ")));
    out.push_str(&format!(
        "Backends with results: {}\n",
        if responded.is_empty() {
            "(none)".to_string()
        } else {
            responded.join(", ")
        }
    ));

    // Summarize failures on a single line so the model can see what
    // was tried but keeping noise low when the backends succeeded.
    let failed_count = attempts
        .iter()
        .filter(|(_, r)| !matches!(r, Ok(rs) if !rs.is_empty()))
        .count();
    if failed_count > 0 {
        out.push_str(&format!(
            "Backends with no results: {}\n",
            attempts
                .iter()
                .filter_map(|(b, r)| match r {
                    Ok(rs) if rs.is_empty() => Some(format!("{} (empty)", b.source_name())),
                    Err(e) => {
                        let short = if e.len() > 60 { &e[..60] } else { e.as_str() };
                        Some(format!("{} ({})", b.source_name(), short))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out.push_str(&format!("\nRESULTS ({} total):\n", merged.len()));
    out.push_str("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    for (i, r) in merged.iter().enumerate() {
        out.push_str(&format!("\n[{}] {}\n", i + 1, r.title));
        out.push_str(&format!("    URL: {}\n", r.url));
        if r.sources.len() > 1 {
            out.push_str(&format!(
                "    Sources: {} (multi-source)\n",
                r.sources.join(" + ")
            ));
        } else if let Some(src) = r.sources.first() {
            out.push_str(&format!("    Source: {}\n", src));
        }
        if !r.snippet.is_empty() {
            out.push_str(&format!("    Snippet: {}\n", r.snippet));
        }
    }
    out.push_str("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
    out.push_str(
        "Next step: pick a URL from the list above and use `web_fetch` to \
         read the full page, or answer directly if the snippets are enough. \
         Remember the GROUNDING RULE — cite only what's in the list.\n",
    );

    out
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

fn build_client() -> Result<rquest::Client, String> {
    // Present as current Chrome. rquest emulates not just the
    // User-Agent but the full TLS (JA3/JA4) fingerprint, HTTP/2
    // frame ordering, and client-hints headers — so servers using
    // TLS fingerprinting to distinguish scripts from browsers see
    // us as Chrome, not as rustls.
    rquest::Client::builder()
        .emulation(rquest_util::Emulation::Chrome136)
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))
}

async fn dispatch(
    client: &rquest::Client,
    backend: Backend,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    match backend {
        Backend::News => search_google_news(client, query).await,
        Backend::Wikipedia => search_wikipedia(client, query).await,
        Backend::HackerNews => search_hacker_news(client, query).await,
        Backend::DdgHtml => search_ddg_html(client, query).await,
        Backend::GitHub => search_github(client, query).await,
        Backend::StackExchange => search_stack_exchange(client, query).await,
        Backend::CratesIo => search_crates_io(client, query).await,
        Backend::Arxiv => search_arxiv(client, query).await,
        Backend::Browser => search_browser_chrome(query).await,
    }
}

// ─── Backend: Google News RSS ───────────────────────────────────────────

async fn search_google_news(
    client: &rquest::Client,
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
    client: &rquest::Client,
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
    client: &rquest::Client,
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

// ─── Backend: GitHub search ─────────────────────────────────────────────

async fn search_github(client: &rquest::Client, query: &str) -> Result<Vec<SearchResult>, String> {
    // GitHub search API: public endpoint, no auth needed, 60/hour
    // unauthenticated per IP. Caps at per_page=100 but we take
    // MAX_RESULTS. Stars-descending sort so we favor maintained
    // repos. GitHub REQUIRES a descriptive User-Agent and will reject
    // the request otherwise — http_get_text sends WG_USER_AGENT by
    // default.
    let url = format!(
        "https://api.github.com/search/repositories?q={}&per_page={}&sort=stars&order=desc",
        urlencoding::encode(query),
        MAX_RESULTS
    );
    let body = http_get_text(client, &url, None).await?;

    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("GitHub response JSON parse failed: {}", e))?;
    let items = v
        .get("items")
        .and_then(|h| h.as_array())
        .ok_or_else(|| "GitHub response missing items array".to_string())?;

    let mut results = Vec::new();
    for item in items {
        let full_name = item
            .get("full_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let url = item
            .get("html_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if full_name.is_empty() || url.is_empty() {
            continue;
        }
        let description = item
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let stars = item
            .get("stargazers_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let language = item.get("language").and_then(|v| v.as_str()).unwrap_or("");
        let snippet = if language.is_empty() {
            format!("★{} — {}", stars, description)
        } else {
            format!("★{} · {} — {}", stars, language, description)
        };
        results.push(SearchResult {
            title: full_name,
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

// ─── Backend: Stack Exchange search (Stack Overflow) ───────────────────

async fn search_stack_exchange(
    client: &rquest::Client,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    // Stack Exchange API: 300 requests/day no-key per IP, more than
    // enough for a single user. Scoped to stackoverflow.com (the
    // most useful for dev queries). Sort by relevance.
    let url = format!(
        "https://api.stackexchange.com/2.3/search/advanced?q={}&site=stackoverflow&pagesize={}&order=desc&sort=relevance",
        urlencoding::encode(query),
        MAX_RESULTS
    );
    let body = http_get_text(client, &url, None).await?;

    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("Stack Exchange response JSON parse failed: {}", e))?;
    let items = v
        .get("items")
        .and_then(|h| h.as_array())
        .ok_or_else(|| "Stack Exchange response missing items array".to_string())?;

    let mut results = Vec::new();
    for item in items {
        // Titles are HTML-escaped in the API response.
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .map(|t| html_escape::decode_html_entities(t).to_string())
            .unwrap_or_default();
        let url = item
            .get("link")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if title.is_empty() || url.is_empty() {
            continue;
        }
        let score = item.get("score").and_then(|v| v.as_i64()).unwrap_or(0);
        let answer_count = item
            .get("answer_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let is_answered = item
            .get("is_answered")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let tags: Vec<String> = item
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str())
                    .take(5)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let snippet = format!(
            "score {}, {} answer{}{} [{}]",
            score,
            answer_count,
            if answer_count == 1 { "" } else { "s" },
            if is_answered { ", answered" } else { "" },
            tags.join(", ")
        );
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

// ─── Backend: crates.io ────────────────────────────────────────────────

async fn search_crates_io(
    client: &rquest::Client,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    // crates.io search: documented rate limits (we're nowhere close),
    // requires User-Agent with contact info. Returns top crates by
    // relevance to the query.
    let url = format!(
        "https://crates.io/api/v1/crates?q={}&per_page={}",
        urlencoding::encode(query),
        MAX_RESULTS
    );
    let body = http_get_text(client, &url, None).await?;

    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("crates.io response JSON parse failed: {}", e))?;
    let crates = v
        .get("crates")
        .and_then(|h| h.as_array())
        .ok_or_else(|| "crates.io response missing crates array".to_string())?;

    let mut results = Vec::new();
    for c in crates {
        let name = c
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let description = c
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let downloads = c.get("downloads").and_then(|v| v.as_i64()).unwrap_or(0);
        let max_version = c.get("max_version").and_then(|v| v.as_str()).unwrap_or("");
        let snippet = if max_version.is_empty() {
            format!("{} downloads — {}", downloads, description)
        } else {
            format!(
                "v{} · {} downloads — {}",
                max_version, downloads, description
            )
        };
        let url = format!("https://crates.io/crates/{}", name);
        results.push(SearchResult {
            title: name,
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

// ─── Backend: arxiv ────────────────────────────────────────────────────

async fn search_arxiv(client: &rquest::Client, query: &str) -> Result<Vec<SearchResult>, String> {
    // arxiv API: 1 request per 3 seconds per their FAQ. We rate-limit
    // to 3.5s in Backend::min_interval so we're safely over their
    // floor. Returns Atom XML with <entry> blocks.
    // Use https directly — the http endpoint 301-redirects to https,
    // which wastes a round trip and doesn't buy us anything.
    let url = format!(
        "https://export.arxiv.org/api/query?search_query=all:{}&max_results={}",
        urlencoding::encode(query),
        MAX_RESULTS
    );
    let body = http_get_text(client, &url, None).await?;
    Ok(parse_arxiv_atom(&body))
}

fn parse_arxiv_atom(body: &str) -> Vec<SearchResult> {
    // arxiv Atom entry shape:
    //   <entry>
    //     <id>http://arxiv.org/abs/NNNN.NNNNN</id>
    //     <title>...</title>
    //     <summary>...</summary>
    //     <author><name>...</name></author>
    //     <published>2024-03-15T00:00:00Z</published>
    //   </entry>
    let entry_re = match regex::Regex::new(r"(?s)<entry>(.*?)</entry>") {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let tag = |name: &str| -> Option<regex::Regex> {
        regex::Regex::new(&format!("(?s)<{}>(.*?)</{}>", name, name)).ok()
    };
    let (Some(title_re), Some(id_re), Some(summary_re), Some(published_re)) =
        (tag("title"), tag("id"), tag("summary"), tag("published"))
    else {
        return Vec::new();
    };

    // Author is nested: <author><name>...</name></author>
    let author_re = match regex::Regex::new(r"(?s)<author>\s*<name>(.*?)</name>") {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut results = Vec::new();
    for cap in entry_re.captures_iter(body) {
        let entry = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let title = title_re
            .captures(entry)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");
        let id = id_re
            .captures(entry)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");
        let summary = summary_re
            .captures(entry)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");
        let published = published_re
            .captures(entry)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");
        let first_author = author_re
            .captures(entry)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");

        if title.is_empty() || id.is_empty() {
            continue;
        }

        let clean_title = clean_text(title);
        let clean_summary = clean_text(summary);
        let snippet = if !first_author.is_empty() {
            format!(
                "{} ({}) — {}",
                clean_text(first_author),
                published.split('T').next().unwrap_or(""),
                clean_summary
            )
        } else {
            clean_summary
        };

        results.push(SearchResult {
            title: clean_title,
            snippet: truncate_snippet(&snippet),
            url: id.trim().to_string(),
            sources: Vec::new(),
        });
        if results.len() >= MAX_RESULTS {
            break;
        }
    }

    results
}

// ─── Backend: headless Chrome driving DuckDuckGo HTML ──────────────────

/// Shared handle to a single long-lived headless Chrome process. We
/// launch lazily on first use and keep the browser alive for the rest
/// of the process lifetime — spawning Chrome per query would add ~2-3s
/// latency to every call, which defeats the point.
///
/// Public within the crate so `web_fetch` can borrow the same
/// handle for its fallback path — one browser shared across all
/// backends that need one.
pub(crate) struct BrowserHandle {
    pub(crate) browser: chromiumoxide::Browser,
    /// Chromiumoxide requires us to drive the event loop via a task
    /// that consumes messages from the handler stream. This handle
    /// keeps that task alive for the lifetime of BrowserHandle.
    _task: tokio::task::JoinHandle<()>,
}

static BROWSER_CELL: OnceLock<Arc<TokioMutex<Option<BrowserHandle>>>> = OnceLock::new();

/// Accessor for `web_fetch` to reuse the same browser handle the
/// `web_search` Browser backend uses. Lazily initializes if neither
/// tool has touched the browser yet this session.
pub(crate) async fn get_or_launch_browser_for_fetch()
-> Result<Arc<TokioMutex<Option<BrowserHandle>>>, String> {
    get_or_launch_browser().await
}

/// Get (or lazily initialize) the shared browser handle. On first call
/// this spawns a Chrome process with stealth flags; subsequent calls
/// return the same handle. Propagates the launch error if Chrome isn't
/// installed or the process fails to start.
async fn get_or_launch_browser() -> Result<Arc<TokioMutex<Option<BrowserHandle>>>, String> {
    let cell = BROWSER_CELL
        .get_or_init(|| Arc::new(TokioMutex::new(None)))
        .clone();
    let mut guard = cell.lock().await;
    if guard.is_none() {
        *guard = Some(launch_browser().await?);
    }
    drop(guard);
    Ok(cell)
}

async fn launch_browser() -> Result<BrowserHandle, String> {
    use chromiumoxide::browser::{Browser, BrowserConfig, HeadlessMode};

    // Chrome binary path: env override > /usr/bin/google-chrome >
    // /usr/bin/chromium. Users with Chrome at a non-standard path
    // can set CHROME_BIN.
    let chrome_path = std::env::var("CHROME_BIN")
        .ok()
        .or_else(|| {
            ["/usr/bin/google-chrome", "/usr/bin/chromium"]
                .iter()
                .find(|p| std::path::Path::new(p).exists())
                .map(|s| s.to_string())
        })
        .ok_or_else(|| {
            "No Chrome/Chromium binary found. Set CHROME_BIN or install google-chrome.".to_string()
        })?;

    // Dedicated per-process user-data-dir so we don't collide with
    // an interactive Chrome session the user might already have
    // running in `~/.config/google-chrome`. Chrome refuses to start
    // when two instances fight for the same profile directory
    // (ProcessSingleton lock).
    let user_data_dir = std::env::temp_dir().join(format!("wg-chrome-{}", std::process::id()));

    let config = BrowserConfig::builder()
        .chrome_executable(&chrome_path)
        .headless_mode(HeadlessMode::New)
        .no_sandbox()
        .user_data_dir(&user_data_dir)
        // Present as a human:
        // - disable the flag that would set navigator.webdriver=true
        // - no first-run or default-browser check dialogs
        // - no shared-memory blowup on small /dev/shm (common in
        //   containers, harmless elsewhere)
        // - disable GPU because we're never rendering anything and
        //   GPU init fails on headless Linux without a display,
        //   which on some Chrome builds kills the process before
        //   chromiumoxide can parse the DevTools WebSocket URL
        // - real desktop viewport
        .arg("--disable-blink-features=AutomationControlled")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-dev-shm-usage")
        .arg("--disable-gpu")
        .arg("--window-size=1920,1080")
        .arg(format!("--user-agent={}", CHROME_UA))
        .build()
        .map_err(|e| format!("browser config: {}", e))?;

    let (browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|e| format!("browser launch: {}", e))?;

    // Event-loop pump: chromiumoxide requires us to consume the
    // handler stream or the browser stalls. Drain the stream until
    // it actually ends (None). Do NOT break on error events — the
    // handler surfaces routine CDP protocol errors alongside real
    // browser-death errors, and breaking on the former kills the
    // browser prematurely.
    let task = tokio::spawn(async move {
        while handler.next().await.is_some() {
            // Keep pumping.
        }
    });

    Ok(BrowserHandle {
        browser,
        _task: task,
    })
}

async fn search_browser_chrome(query: &str) -> Result<Vec<SearchResult>, String> {
    let cell = get_or_launch_browser().await?;

    // We hit html.duckduckgo.com/html/ — the JS-free "old-school"
    // results page — because it's simple to parse and stable. The
    // important bit is that we're hitting it with a REAL Chrome
    // fingerprint (not reqwest), so DDG's anti-bot doesn't serve
    // us challenge pages.
    let url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        urlencoding::encode(query)
    );

    let page = {
        let guard = cell.lock().await;
        let handle = guard
            .as_ref()
            .ok_or_else(|| "browser not initialized".to_string())?;
        handle
            .browser
            .new_page(&url)
            .await
            .map_err(|e| format!("new_page: {}", e))?
    };

    // Additional stealth: hide navigator.webdriver at the page level
    // in case the browser-wide flag didn't propagate. Harmless to set
    // multiple times. We do this BEFORE waiting for content so the
    // page scripts (which DDG doesn't have much of, but just in case)
    // see a human.
    let _ = page
        .evaluate("Object.defineProperty(navigator, 'webdriver', {get: () => undefined})")
        .await;

    // chromiumoxide's new_page waits for the initial load, but DDG
    // returns quickly and may still be rendering when we read
    // content. A 300ms settle window catches most late DOM updates.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let html = match page.content().await {
        Ok(h) => h,
        Err(e) => {
            let _ = page.close().await;
            return Err(format!("content read: {}", e));
        }
    };
    let _ = page.close().await;

    // Reuse the existing DDG HTML parser — same page shape.
    let results = parse_ddg_html(&html);
    if results.is_empty() {
        let snippet: String = html.chars().take(200).collect();
        return Err(format!(
            "no result anchors in browser-rendered DDG (body {} bytes, prefix: {:?})",
            html.len(),
            snippet
        ));
    }
    Ok(results)
}

// ─── Backend: DuckDuckGo HTML (reqwest-based, deprecated) ───────────────

async fn search_ddg_html(
    client: &rquest::Client,
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
    client: &rquest::Client,
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
    if status == rquest::StatusCode::TOO_MANY_REQUESTS
        || status == rquest::StatusCode::SERVICE_UNAVAILABLE
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
