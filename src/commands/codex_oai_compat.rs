//! OAI-compatible endpoint configuration helpers for the `codex` CLI.
//!
//! When wg is configured with a custom endpoint (`WG_ENDPOINT_URL`) and an
//! API key (`WG_API_KEY`), the codex_handler must redirect the spawned
//! `codex exec` process away from the default `api.openai.com` to the user's
//! endpoint. Codex documents this via `model_providers.<id>` entries in
//! `~/.codex/config.toml`; we use `codex exec --config <toml-override>`
//! command-line overrides instead, which avoids touching the user's config
//! file and keeps the redirection scoped to the spawned subprocess.
//!
//! See `docs/research/thin-wrapper-executors-2026-04.md` for the rationale.

/// Stable model-provider id wg writes into the codex `--config` overrides.
/// Codex looks this up in its `model_providers.<id>` table to decide where
/// to send requests. A stable name keeps successive invocations using the
/// same provider definition (no leakage across sessions).
pub const PROVIDER_ID: &str = "wg";

/// Env var name codex will read for the API key (set via `env_key` in the
/// generated provider definition). We standardize on `OPENAI_API_KEY`
/// because it is the most-common name for OAI-compatible servers and the
/// codex_handler exports it explicitly when `WG_API_KEY` is present.
pub const ENV_KEY_NAME: &str = "OPENAI_API_KEY";

/// Build the list of `--config <toml>` value strings to pass to
/// `codex exec` so it talks to the given OAI-compatible endpoint URL
/// instead of `api.openai.com`.
///
/// Each returned string is a `key=value` TOML override. The caller passes
/// each one with a preceding `--config` flag, e.g.
///
/// ```text
/// codex exec --config 'model_provider="wg"' --config 'model_providers.wg.base_url="…"' …
/// ```
///
/// Values are quoted as TOML strings (codex parses the value portion as
/// TOML; bare unquoted strings are not valid scalars).
///
/// `wire_api` is set to `"responses"`. Codex 0.120+ removed support for
/// `wire_api = "chat"` (see openai/codex#7782), so the OpenAI Responses
/// API is the only wire format codex now speaks. Endpoints that only
/// implement OAI Chat Completions will reject codex's POST to
/// `<base_url>/responses`; that limitation is the wrapper-target's
/// problem to solve, not wg's — we wire what codex accepts and surface
/// failures clearly via handler.log.
pub fn config_overrides(endpoint_url: &str) -> Vec<String> {
    vec![
        format!("model_provider=\"{}\"", PROVIDER_ID),
        format!("model_providers.{}.name=\"{}\"", PROVIDER_ID, PROVIDER_ID),
        format!(
            "model_providers.{}.base_url=\"{}\"",
            PROVIDER_ID, endpoint_url
        ),
        format!(
            "model_providers.{}.env_key=\"{}\"",
            PROVIDER_ID, ENV_KEY_NAME
        ),
        format!("model_providers.{}.wire_api=\"responses\"", PROVIDER_ID),
    ]
}

/// Read the endpoint URL from the wg session env vars set by the
/// spawn-task path. Returns `None` (and the caller falls back to codex's
/// own default of `api.openai.com`) when no custom endpoint is configured.
///
/// Resolution order:
/// 1. `WG_ENDPOINT_URL` — set by `spawn/execution.rs` when the task's
///    endpoint config has a `url` field.
/// 2. `OPENAI_BASE_URL` — codex's own convention; lets users override via
///    a standard env var without going through wg.
pub fn endpoint_url_from_env() -> Option<String> {
    std::env::var("WG_ENDPOINT_URL")
        .ok()
        .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
        .filter(|s| !s.is_empty())
}

/// Resolve the API key from wg session env vars.
///
/// Order: `WG_API_KEY` (set by the spawn-task path from session config),
/// then `OPENAI_API_KEY` (likely already set in the user's shell).
/// Returns `None` if no key is present — codex will then fall back to its
/// own auth (default `api.openai.com` flow, which fails for custom
/// endpoints; that is the case this helper exists to fix).
pub fn api_key_from_env() -> Option<String> {
    std::env::var("WG_API_KEY")
        .ok()
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_overrides_includes_base_url() {
        let got = config_overrides("http://stub.test:1234");
        let joined = got.join(" || ");
        assert!(
            joined.contains(r#"model_providers.wg.base_url="http://stub.test:1234""#),
            "expected base_url override, got: {}",
            joined
        );
    }

    #[test]
    fn config_overrides_includes_provider_selection() {
        let got = config_overrides("https://x");
        assert!(
            got.iter().any(|s| s == r#"model_provider="wg""#),
            "expected model_provider override, got: {:?}",
            got
        );
    }

    #[test]
    fn config_overrides_specifies_responses_wire_api() {
        let got = config_overrides("https://x");
        assert!(
            got.iter().any(|s| s.ends_with(r#"wire_api="responses""#)),
            "expected wire_api=responses override, got: {:?}",
            got
        );
    }

    #[test]
    fn config_overrides_specifies_env_key() {
        let got = config_overrides("https://x");
        assert!(
            got.iter().any(|s| s.ends_with(r#"env_key="OPENAI_API_KEY""#)),
            "expected env_key=OPENAI_API_KEY override, got: {:?}",
            got
        );
    }
}
