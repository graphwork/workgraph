//! `wg secret` — manage API key credentials.

use anyhow::{Result, bail};
use std::io::{IsTerminal, Read};
use std::path::Path;

use workgraph::config::Config;
use workgraph::secret::{
    Backend, SecretsConfig, backend_status, check_ref_reachable, delete, get, list, set,
};

// ── set ───────────────────────────────────────────────────────────────────────

pub fn run_set(
    _workgraph_dir: &Path,
    name: &str,
    value: Option<&str>,
    backend_str: Option<&str>,
    from_stdin: bool,
) -> Result<()> {
    let cfg = SecretsConfig::load_global();
    let backend: Backend = if let Some(b) = backend_str {
        b.parse()?
    } else {
        cfg.default_backend.clone()
    };

    if from_stdin && value.is_some() {
        bail!("--from-stdin and --value are mutually exclusive");
    }

    let secret_value = if from_stdin {
        read_stdin_line()?
    } else {
        match value {
            Some(v) => {
                eprintln!(
                    "Warning: providing secrets via --value flag may expose them in shell history. \
                     Prefer --from-stdin for scripts or interactive prompt for terminals."
                );
                v.to_string()
            }
            None => {
                // If stdin is not a TTY (e.g. piped), fail with a helpful
                // error pointing at --from-stdin instead of silently
                // hanging on an interactive prompt.
                if !std::io::stdin().is_terminal() {
                    bail!(
                        "stdin is not a terminal; pass --from-stdin to read the value \
                         from stdin (one line), or pass --value <value>"
                    );
                }
                read_password()?
            }
        }
    };

    if secret_value.is_empty() {
        bail!("Secret value cannot be empty");
    }

    set(name, &secret_value, &backend, &cfg)?;
    println!("Secret '{}' stored in {} backend.", name, backend);
    Ok(())
}

fn read_password() -> Result<String> {
    use dialoguer::Password;
    let value = Password::new()
        .with_prompt("")
        .interact()
        .unwrap_or_default();
    Ok(value)
}

/// Read a single line of secret material from stdin. Trailing newline is
/// stripped. Empty input is a hard error (caught by the caller). Treats stdin
/// as already-trusted: never echoes; allows arbitrary printable content.
fn read_stdin_line() -> Result<String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| anyhow::anyhow!("failed to read secret value from stdin: {}", e))?;
    // Strip a trailing newline (single \n or \r\n) — the typical
    // `echo "v" | wg secret set foo --from-stdin` shape — but preserve
    // internal whitespace in case the secret legitimately contains it.
    if buf.ends_with('\n') {
        buf.pop();
        if buf.ends_with('\r') {
            buf.pop();
        }
    }
    Ok(buf)
}

// ── get ───────────────────────────────────────────────────────────────────────

pub fn run_get(
    _workgraph_dir: &Path,
    name: &str,
    backend_str: Option<&str>,
    reveal: bool,
) -> Result<()> {
    let cfg = SecretsConfig::load_global();
    let backend: Backend = if let Some(b) = backend_str {
        b.parse()?
    } else {
        cfg.default_backend.clone()
    };

    match get(name, &backend, &cfg)? {
        Some(value) => {
            if reveal {
                eprintln!("Warning: displaying secret value.");
                println!("{}", value);
            } else {
                let masked = if value.len() > 8 {
                    format!("{}****...{}", &value[..4], &value[value.len() - 4..])
                } else {
                    "****".to_string()
                };
                println!(
                    "Secret '{}' exists: {} (use --reveal to show full value)",
                    name, masked
                );
            }
        }
        None => {
            println!("Secret '{}' not found in {} backend.", name, backend);
            println!("Run: wg secret set {}", name);
        }
    }
    Ok(())
}

// ── list ──────────────────────────────────────────────────────────────────────

pub fn run_list(_workgraph_dir: &Path, json: bool) -> Result<()> {
    let cfg = SecretsConfig::load_global();
    let names = list(&cfg)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&names)?);
        return Ok(());
    }

    if names.is_empty() {
        println!("No secrets stored.");
        println!("Run: wg secret set <name>");
    } else {
        println!("Stored secrets (names only):");
        for name in &names {
            println!("  {}", name);
        }
    }
    Ok(())
}

// ── rm ────────────────────────────────────────────────────────────────────────

pub fn run_rm(
    _workgraph_dir: &Path,
    name: &str,
    backend_str: Option<&str>,
    yes: bool,
) -> Result<()> {
    let cfg = SecretsConfig::load_global();
    let backend: Backend = if let Some(b) = backend_str {
        b.parse()?
    } else {
        cfg.default_backend.clone()
    };

    if !yes {
        // Refuse to prompt for confirmation when stdin is not a TTY (CI /
        // pipes). Otherwise this hangs forever or auto-decides quietly.
        if !std::io::stdin().is_terminal() {
            bail!(
                "Refusing to delete '{}' without --yes (stdin is not a terminal). \
                 Re-run with --yes to confirm.",
                name
            );
        }
        use dialoguer::Confirm;
        let confirmed = Confirm::new()
            .with_prompt(format!(
                "Delete secret '{}' from {} backend?",
                name, backend
            ))
            .default(false)
            .interact()
            .unwrap_or(false);
        if !confirmed {
            println!("Aborted.");
            return Ok(());
        }
    }

    if delete(name, &backend, &cfg)? {
        println!("Secret '{}' deleted from {} backend.", name, backend);
    } else {
        println!("Secret '{}' not found in {} backend.", name, backend);
    }
    Ok(())
}

// ── backend ───────────────────────────────────────────────────────────────────

pub fn run_backend_show(_workgraph_dir: &Path) -> Result<()> {
    let cfg = SecretsConfig::load_global();
    println!("{}", backend_status(&cfg));
    Ok(())
}

pub fn run_backend_set(_workgraph_dir: &Path, backend_str: &str) -> Result<()> {
    let mut config = Config::load_global()?.unwrap_or_default();
    let backend: Backend = backend_str.parse()?;

    if backend == Backend::Plaintext && !config.secrets.allow_plaintext {
        bail!(
            "Cannot set default backend to plaintext: allow_plaintext is false.\n\
             First run: wg config --global ... or set secrets.allow_plaintext = true"
        );
    }

    config.secrets.default_backend = backend.clone();
    config.save_global()?;
    println!("Default secret backend set to: {}", backend);
    Ok(())
}

// ── check ─────────────────────────────────────────────────────────────────────

/// Pre-flight check: verify a specific api_key_ref is reachable.
pub fn run_check(_workgraph_dir: &Path, api_key_ref: &str) -> Result<()> {
    let cfg = SecretsConfig::load_global();
    match check_ref_reachable(api_key_ref, &cfg)? {
        true => println!("Secret ref '{}' is reachable.", api_key_ref),
        false => {
            println!("Secret ref '{}' is NOT reachable (not found).", api_key_ref);
            if let Some(name) = api_key_ref.strip_prefix("keyring:") {
                println!("Run: wg secret set {}", name);
            } else if let Some(name) = api_key_ref.strip_prefix("keystore:") {
                println!("Run: wg secret set {} --backend keystore", name);
            } else if let Some(name) = api_key_ref.strip_prefix("plain:") {
                println!("Run: wg secret set {} --backend plaintext", name);
            }
        }
    }
    Ok(())
}

// ── migrate secrets ───────────────────────────────────────────────────────────

/// Walk existing configs with `api_key_env`, offer to migrate to `api_key_ref`.
pub fn run_migrate_secrets(
    workgraph_dir: &Path,
    dry_run: bool,
    global: bool,
    local: bool,
    no_copy: bool,
) -> Result<()> {
    let use_global = global || !local;
    let use_local = local || !global;

    let mut any_found = false;

    if use_global {
        if let Some(mut cfg) = Config::load_global()? {
            let changed = migrate_endpoints_in_config(&mut cfg, dry_run, no_copy, "global")?;
            if changed && !dry_run {
                cfg.save_global()?;
                println!("[global] Config updated.");
            }
            any_found = any_found || changed;
        }
    }

    if use_local {
        let local_path = workgraph_dir.join("config.toml");
        if local_path.exists() {
            let mut cfg = Config::load(workgraph_dir)?;
            let changed = migrate_endpoints_in_config(&mut cfg, dry_run, no_copy, "local")?;
            if changed && !dry_run {
                cfg.save(workgraph_dir)?;
                println!("[local] Config updated.");
            }
            any_found = any_found || changed;
        }
    }

    if !any_found {
        println!("No endpoints with api_key_env found. Nothing to migrate.");
    } else if dry_run {
        println!("\nDry run complete. Run without --dry-run to apply changes.");
    }

    Ok(())
}

fn migrate_endpoints_in_config(
    config: &mut Config,
    dry_run: bool,
    no_copy: bool,
    label: &str,
) -> Result<bool> {
    let mut changed = false;

    for ep in &mut config.llm_endpoints.endpoints {
        if ep.api_key_ref.is_some() {
            continue;
        }
        let env_name = match &ep.api_key_env {
            Some(e) => e.clone(),
            None => continue,
        };

        let secret_name = ep.name.clone();
        println!(
            "[{}] Endpoint '{}' uses api_key_env = {:?}",
            label, ep.name, env_name
        );

        if !no_copy {
            if let Ok(env_value) = std::env::var(&env_name) {
                if !env_value.is_empty() {
                    println!(
                        "  Found value in ${} — will store in keyring as '{}'",
                        env_name, secret_name
                    );
                    if !dry_run {
                        workgraph::secret::keyring_set(&secret_name, &env_value)?;
                        println!("  Stored '{}' in keyring.", secret_name);
                    }
                } else {
                    println!("  ${} is set but empty — skipping value copy.", env_name);
                }
            } else {
                println!(
                    "  ${} is not set in current shell — run `wg secret set {}` manually.",
                    env_name, secret_name
                );
            }
        }

        let new_ref = format!("keyring:{}", secret_name);
        println!("  Rewriting api_key_env → api_key_ref = {:?}", new_ref);

        if !dry_run {
            ep.api_key_env = None;
            ep.api_key_ref = Some(new_ref);
        }

        changed = true;
    }

    Ok(changed)
}
