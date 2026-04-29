//! Secure credential storage for API keys.
//!
//! Resolution order for `api_key_ref` URIs (first hit wins):
//! 1. `literal:<value>` — inline value; warns loudly, test use only
//! 2. `op://<path>` / `pass:<path>` — delegates to external tool (1Password, pass)
//! 3. `keyring:<name>` — OS native credential store (macOS Keychain, libsecret /
//!    secret-service on Linux, Windows Credential Manager). Falls back to the
//!    file keystore on systems where the OS keyring is unreachable.
//! 4. `keystore:<name>` — explicit secure file keystore at `~/.wg/keystore/<name>`
//!    (0600 perms, 0700 dir). Always available; no OS keyring required.
//! 5. `env:<VAR>` — explicit, opt-in env forwarding
//! 6. `plain:<name>` — plaintext file at `~/.wg/secrets/<name>` (requires allow_plaintext=true)
//!
//! The `keyring` backend tries the OS keyring first. If it's not reachable
//! (headless Linux without D-Bus / secret-service, missing platform support,
//! etc.), it falls back to the file keystore with a one-time stderr warning.
//! Use `wg secret backend show` to see which backend is actually reachable.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// OS keyring "service" name. Each secret is stored under this service with
/// the secret name as the "username" — `Entry::new("wg", name)`.
const KEYRING_SERVICE: &str = "wg";

// ── Backend selection ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// OS native credential store (macOS Keychain, secret-service, etc.) with
    /// automatic fallback to the file keystore when unreachable.
    Keyring,
    /// Explicit file keystore at `~/.wg/keystore/<name>` (0600 / 0700 perms).
    Keystore,
    /// Plaintext file at `~/.wg/secrets/<name>` (requires allow_plaintext=true).
    Plaintext,
}

impl Default for Backend {
    fn default() -> Self {
        Self::Keyring
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keyring => write!(f, "keyring"),
            Self::Keystore => write!(f, "keystore"),
            Self::Plaintext => write!(f, "plaintext"),
        }
    }
}

impl std::str::FromStr for Backend {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "keyring" => Ok(Self::Keyring),
            "keystore" => Ok(Self::Keystore),
            "plaintext" | "plain" => Ok(Self::Plaintext),
            other => bail!(
                "Unknown backend '{}'. Choose: keyring, keystore, plaintext",
                other
            ),
        }
    }
}

// ── Secrets config section ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SecretsConfig {
    /// Enable the plaintext file backend. Off by default for safety.
    #[serde(default)]
    pub allow_plaintext: bool,

    /// Default backend for `wg secret set` when no --backend is given.
    #[serde(default)]
    pub default_backend: Backend,
}

impl SecretsConfig {
    /// Load the global secrets config from `~/.wg/config.toml`.
    /// Returns defaults if the file doesn't exist or can't be read.
    pub fn load_global() -> Self {
        let path = match dirs::home_dir() {
            Some(h) => h.join(".wg").join("config.toml"),
            None => return Self::default(),
        };
        if !path.exists() {
            return Self::default();
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return Self::default(),
        };
        #[derive(serde::Deserialize, Default)]
        struct Partial {
            #[serde(default)]
            secrets: SecretsConfig,
        }
        toml::from_str::<Partial>(&content)
            .map(|p| p.secrets)
            .unwrap_or_default()
    }
}

// ── Path helpers ──────────────────────────────────────────────────────────────

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Secret name cannot be empty");
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.contains('\0') {
        bail!("Secret name '{}' contains invalid characters", name);
    }
    Ok(())
}

fn keystore_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join(".wg").join("keystore"))
}

fn keystore_path(name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(keystore_dir()?.join(name))
}

fn secrets_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join(".wg").join("secrets"))
}

fn secrets_file(name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(secrets_dir()?.join(name))
}

// ── Shared file I/O ───────────────────────────────────────────────────────────

#[cfg(unix)]
fn write_secret_file(path: &std::path::Path, value: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(path, value)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret_file(path: &std::path::Path, value: &str) -> Result<()> {
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir)?;
    std::fs::write(path, value)?;
    Ok(())
}

fn read_secret_file(path: &std::path::Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let value = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read secret file {}", path.display()))?;
    Ok(Some(value.trim().to_string()))
}

fn delete_secret_file(path: &std::path::Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(path)?;
    Ok(true)
}

fn list_secret_files(dir: &std::path::Path) -> Result<Vec<String>> {
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut names = vec![];
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

// ── OS keyring backend (real keyring crate) ──────────────────────────────────

/// Probe whether the OS keyring is reachable. Cached per-process.
fn os_keyring_available() -> bool {
    use std::sync::OnceLock;
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        // Try to construct an Entry and run a benign operation. If the
        // platform backend can't even be initialized (e.g. no D-Bus on
        // headless Linux), this returns Err.
        match keyring::Entry::new(KEYRING_SERVICE, "__wg_probe__") {
            Ok(entry) => {
                // A get on a non-existent entry should return NoEntry, not a
                // service-level error. NoEntry == reachable.
                match entry.get_password() {
                    Ok(_) => true,
                    Err(keyring::Error::NoEntry) => true,
                    Err(_) => false,
                }
            }
            Err(_) => false,
        }
    })
}

/// Index of names we've ever stored in the OS keyring. The keyring crate has
/// no portable list API, so we keep a sidecar index. Stored as one name per
/// line at `~/.wg/keyring-index`. Only names — never values.
fn keyring_index_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join(".wg").join("keyring-index"))
}

fn keyring_index_read() -> Result<Vec<String>> {
    let path = keyring_index_path()?;
    if !path.exists() {
        return Ok(vec![]);
    }
    let content = std::fs::read_to_string(&path)?;
    let mut names: Vec<String> = content
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

fn keyring_index_write(names: &[String]) -> Result<()> {
    let path = keyring_index_path()?;
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let body = names.join("\n") + "\n";
    std::fs::write(&path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn keyring_index_add(name: &str) -> Result<()> {
    let mut names = keyring_index_read().unwrap_or_default();
    if !names.iter().any(|n| n == name) {
        names.push(name.to_string());
        names.sort();
        keyring_index_write(&names)?;
    }
    Ok(())
}

fn keyring_index_remove(name: &str) -> Result<()> {
    let mut names = keyring_index_read().unwrap_or_default();
    let before = names.len();
    names.retain(|n| n != name);
    if names.len() != before {
        keyring_index_write(&names)?;
    }
    Ok(())
}

fn os_keyring_set(name: &str, value: &str) -> Result<()> {
    validate_name(name)?;
    let entry = keyring::Entry::new(KEYRING_SERVICE, name)
        .with_context(|| format!("Failed to construct OS keyring entry for '{}'", name))?;
    entry
        .set_password(value)
        .with_context(|| format!("Failed to write '{}' to OS keyring", name))?;
    keyring_index_add(name)?;
    Ok(())
}

fn os_keyring_get(name: &str) -> Result<Option<String>> {
    validate_name(name)?;
    let entry = keyring::Entry::new(KEYRING_SERVICE, name)
        .with_context(|| format!("Failed to construct OS keyring entry for '{}'", name))?;
    match entry.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("OS keyring read failed for '{}': {}", name, e)),
    }
}

fn os_keyring_delete(name: &str) -> Result<bool> {
    validate_name(name)?;
    let entry = keyring::Entry::new(KEYRING_SERVICE, name)
        .with_context(|| format!("Failed to construct OS keyring entry for '{}'", name))?;
    let result = match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(anyhow::anyhow!(
            "OS keyring delete failed for '{}': {}",
            name,
            e
        )),
    };
    // Always update index regardless — remove if present.
    let _ = keyring_index_remove(name);
    result
}

fn os_keyring_list() -> Result<Vec<String>> {
    keyring_index_read()
}

/// Describe the OS keyring's reachability for `backend show`. Includes the
/// most likely backend name on the running platform.
fn os_keyring_status() -> String {
    if os_keyring_available() {
        let platform = if cfg!(target_os = "macos") {
            "macOS Keychain"
        } else if cfg!(target_os = "windows") {
            "Windows Credential Manager"
        } else if cfg!(target_os = "linux") {
            "secret-service (libsecret / GNOME Keyring / KWallet)"
        } else {
            "OS native keyring"
        };
        format!("reachable ({})", platform)
    } else {
        "unreachable".to_string()
    }
}

// ── Keyring backend (smart: OS first, file keystore fallback) ────────────────
//
// The `keyring:` URI scheme writes to the OS keyring when reachable. If the OS
// keyring can't be opened (typically headless Linux without D-Bus), it falls
// back transparently to the file keystore at `~/.wg/keystore/<name>` and
// prints a one-time stderr warning.

fn warn_fallback_once() {
    use std::sync::OnceLock;
    static WARNED: OnceLock<()> = OnceLock::new();
    WARNED.get_or_init(|| {
        eprintln!(
            "Warning: OS keyring is not reachable on this system — \
             falling back to file keystore at ~/.wg/keystore/ (0600 perms). \
             To use OS native credential storage, install secret-service / \
             gnome-keyring / kwallet (Linux) or use macOS / Windows."
        );
    });
}

pub fn keyring_set(name: &str, value: &str) -> Result<()> {
    if os_keyring_available() {
        os_keyring_set(name, value)
    } else {
        warn_fallback_once();
        keystore_set(name, value)
    }
}

pub fn keyring_get(name: &str) -> Result<Option<String>> {
    if os_keyring_available() {
        match os_keyring_get(name)? {
            Some(v) => Ok(Some(v)),
            // Allow legacy reads from the file keystore so users with prior
            // `~/.wg/keystore/<name>` files don't lose access.
            None => keystore_get(name),
        }
    } else {
        warn_fallback_once();
        keystore_get(name)
    }
}

pub fn keyring_delete(name: &str) -> Result<bool> {
    let mut deleted = false;
    if os_keyring_available() {
        if os_keyring_delete(name)? {
            deleted = true;
        }
    }
    // Always also clean up the file keystore (covers fallback writes and
    // legacy data).
    if keystore_delete(name)? {
        deleted = true;
    }
    Ok(deleted)
}

pub fn keyring_list() -> Result<Vec<String>> {
    let mut names = std::collections::BTreeSet::new();
    for n in os_keyring_list().unwrap_or_default() {
        names.insert(n);
    }
    for n in keystore_list()? {
        names.insert(n);
    }
    Ok(names.into_iter().collect())
}

// ── Keystore backend (explicit file store at ~/.wg/keystore/) ────────────────

pub fn keystore_set(name: &str, value: &str) -> Result<()> {
    let path = keystore_path(name)?;
    write_secret_file(&path, value)
        .with_context(|| format!("Failed to write to keystore for '{}'", name))
}

pub fn keystore_get(name: &str) -> Result<Option<String>> {
    read_secret_file(&keystore_path(name)?)
}

pub fn keystore_delete(name: &str) -> Result<bool> {
    delete_secret_file(&keystore_path(name)?)
}

pub fn keystore_list() -> Result<Vec<String>> {
    list_secret_files(&keystore_dir()?)
}

// ── Plaintext backend (opt-in, requires allow_plaintext = true) ───────────────

fn plaintext_set(name: &str, value: &str) -> Result<()> {
    let path = secrets_file(name)?;
    write_secret_file(&path, value)
        .with_context(|| format!("Failed to write plaintext secret for '{}'", name))
}

fn plaintext_get(name: &str) -> Result<Option<String>> {
    read_secret_file(&secrets_file(name)?)
}

fn plaintext_delete(name: &str) -> Result<bool> {
    delete_secret_file(&secrets_file(name)?)
}

fn plaintext_list() -> Result<Vec<String>> {
    list_secret_files(&secrets_dir()?)
}

// ── Pass-through resolver ─────────────────────────────────────────────────────

fn resolve_passthrough(uri: &str) -> Result<Option<String>> {
    if let Some(op_path) = uri.strip_prefix("op://") {
        let output = std::process::Command::new("op")
            .arg("read")
            .arg(format!("op://{}", op_path))
            .output()
            .context("Failed to run `op` (1Password CLI). Is it installed and authenticated?")?;
        if output.status.success() {
            let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Ok(Some(value))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("1Password CLI error for '{}': {}", uri, stderr.trim())
        }
    } else if let Some(pass_path) = uri.strip_prefix("pass:") {
        let output = std::process::Command::new("pass")
            .arg("show")
            .arg(pass_path)
            .output()
            .context("Failed to run `pass`. Is it installed?")?;
        if output.status.success() {
            let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let first = value.lines().next().unwrap_or("").trim().to_string();
            Ok(Some(first))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("pass error for '{}': {}", uri, stderr.trim())
        }
    } else {
        Ok(None)
    }
}

// ── Public CRUD API ───────────────────────────────────────────────────────────

/// Store a secret using the chosen backend.
pub fn set(name: &str, value: &str, backend: &Backend, cfg: &SecretsConfig) -> Result<()> {
    match backend {
        Backend::Keyring => keyring_set(name, value),
        Backend::Keystore => keystore_set(name, value),
        Backend::Plaintext => {
            if !cfg.allow_plaintext {
                bail!(
                    "Plaintext backend is disabled. Set `secrets.allow_plaintext = true` in \
                     ~/.wg/config.toml to enable it."
                );
            }
            plaintext_set(name, value)
        }
    }
}

/// Retrieve a secret from the specified backend.
pub fn get(name: &str, backend: &Backend, cfg: &SecretsConfig) -> Result<Option<String>> {
    match backend {
        Backend::Keyring => keyring_get(name),
        Backend::Keystore => keystore_get(name),
        Backend::Plaintext => {
            if !cfg.allow_plaintext {
                bail!(
                    "Plaintext backend is disabled. Set `secrets.allow_plaintext = true` in \
                     ~/.wg/config.toml to enable it."
                );
            }
            plaintext_get(name)
        }
    }
}

/// Delete a secret from the specified backend.
pub fn delete(name: &str, backend: &Backend, cfg: &SecretsConfig) -> Result<bool> {
    match backend {
        Backend::Keyring => keyring_delete(name),
        Backend::Keystore => keystore_delete(name),
        Backend::Plaintext => {
            if !cfg.allow_plaintext {
                bail!("Plaintext backend is disabled.");
            }
            plaintext_delete(name)
        }
    }
}

/// List all secret names across reachable backends (names only, never values).
pub fn list(cfg: &SecretsConfig) -> Result<Vec<String>> {
    let mut names = std::collections::BTreeSet::new();
    // OS keyring (via index)
    for n in os_keyring_list().unwrap_or_default() {
        names.insert(format!("keyring:{}", n));
    }
    // File keystore
    for n in keystore_list()? {
        names.insert(format!("keystore:{}", n));
    }
    if cfg.allow_plaintext {
        for n in plaintext_list()? {
            names.insert(format!("plain:{}", n));
        }
    }
    Ok(names.into_iter().collect())
}

// ── ref URI resolver ──────────────────────────────────────────────────────────

/// Resolve an `api_key_ref` URI to its actual value.
///
/// URI schemes:
/// - `keyring:<name>` — OS native credential store (with file fallback)
/// - `keystore:<name>` — explicit file keystore at `~/.wg/keystore/<name>`
/// - `plain:<name>` — look up in plaintext file (requires allow_plaintext)
/// - `env:<VAR>` — read from environment variable (opt-in, explicit)
/// - `op://<path>` — 1Password CLI
/// - `pass:<path>` — pass CLI
/// - `literal:<value>` — inline value (warns loudly; test use only)
pub fn resolve_ref(api_key_ref: &str, cfg: &SecretsConfig) -> Result<Option<String>> {
    if let Some(name) = api_key_ref.strip_prefix("keyring:") {
        return keyring_get(name);
    }

    if let Some(name) = api_key_ref.strip_prefix("keystore:") {
        return keystore_get(name);
    }

    if let Some(name) = api_key_ref.strip_prefix("plain:") {
        if !cfg.allow_plaintext {
            bail!(
                "Secret ref '{}' uses plaintext backend but it is disabled. \
                 Set `secrets.allow_plaintext = true` in ~/.wg/config.toml.",
                api_key_ref
            );
        }
        return plaintext_get(name);
    }

    if let Some(var) = api_key_ref.strip_prefix("env:") {
        return Ok(std::env::var(var).ok());
    }

    if api_key_ref.starts_with("op://") || api_key_ref.starts_with("pass:") {
        return resolve_passthrough(api_key_ref);
    }

    if let Some(value) = api_key_ref.strip_prefix("literal:") {
        eprintln!(
            "WARNING: secret ref uses literal: scheme — this is for testing only. \
             Never use literal: in production config."
        );
        return Ok(Some(value.to_string()));
    }

    bail!(
        "Unknown api_key_ref scheme in '{}'. \
         Supported: keyring:<name>, keystore:<name>, plain:<name>, env:<VAR>, op://<path>, pass:<path>",
        api_key_ref
    )
}

/// Check whether a ref is reachable (for pre-flight checks).
/// Returns Ok(true) if the secret exists, Ok(false) if not found, Err on config problems.
pub fn check_ref_reachable(api_key_ref: &str, cfg: &SecretsConfig) -> Result<bool> {
    match resolve_ref(api_key_ref, cfg) {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Return a human-readable description of the default backend state.
pub fn backend_status(cfg: &SecretsConfig) -> String {
    let mut parts = vec![];
    match cfg.default_backend {
        Backend::Keyring => {
            parts.push(format!(
                "Default backend: keyring (OS native — {})",
                os_keyring_status()
            ));
            if !os_keyring_available() {
                parts.push(
                    "  → keyring writes/reads will fall back to file keystore at \
                     ~/.wg/keystore/ (0600 perms)."
                        .to_string(),
                );
            }
        }
        Backend::Keystore => {
            parts.push(
                "Default backend: keystore (secure file at ~/.wg/keystore/, 0600 perms)"
                    .to_string(),
            );
        }
        Backend::Plaintext => {
            parts.push("Default backend: plaintext (file at ~/.wg/secrets/)".to_string());
        }
    }
    parts.push(format!("Keyring (OS native): {}", os_keyring_status()));
    parts.push(
        "Keystore (file at ~/.wg/keystore/): always available (0600 perms)".to_string(),
    );
    if cfg.allow_plaintext {
        parts.push("Plaintext backend: enabled (allow_plaintext = true)".to_string());
    } else {
        parts.push(
            "Plaintext backend: disabled (set secrets.allow_plaintext = true to enable)"
                .to_string(),
        );
    }
    parts.join("\n")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static HOME_MUTEX: Mutex<()> = Mutex::new(());

    fn with_home(f: impl FnOnce()) -> TempDir {
        let _guard = HOME_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".wg");
        std::fs::create_dir_all(&wg_dir).unwrap();
        unsafe { std::env::set_var("HOME", tmp.path()) };
        f();
        tmp
    }

    #[test]
    fn test_keystore_set_get_delete() {
        let _tmp = with_home(|| {
            let cfg = SecretsConfig::default();

            keystore_set("testkey", "sk-abc123").unwrap();
            let val = keystore_get("testkey").unwrap();
            assert_eq!(val.as_deref(), Some("sk-abc123"));

            let deleted = keystore_delete("testkey").unwrap();
            assert!(deleted);

            let val2 = keystore_get("testkey").unwrap();
            assert!(val2.is_none());

            let deleted2 = keystore_delete("testkey").unwrap();
            assert!(!deleted2);

            // list includes the key while it exists
            keystore_set("listkey", "val").unwrap();
            let names = list(&cfg).unwrap();
            assert!(names.iter().any(|n| n.contains("listkey")));
            keystore_delete("listkey").unwrap();
        });
    }

    /// Keystore-backed CRUD via the public API also works (no OS keyring needed).
    #[test]
    fn test_set_get_delete_via_keystore_backend() {
        let _tmp = with_home(|| {
            let cfg = SecretsConfig::default();
            set("explicit", "value-1", &Backend::Keystore, &cfg).unwrap();
            let v = get("explicit", &Backend::Keystore, &cfg).unwrap();
            assert_eq!(v.as_deref(), Some("value-1"));
            assert!(delete("explicit", &Backend::Keystore, &cfg).unwrap());
            assert!(get("explicit", &Backend::Keystore, &cfg).unwrap().is_none());
        });
    }

    #[test]
    fn test_plaintext_set_get_list_delete() {
        let _tmp = with_home(|| {
            let cfg = SecretsConfig {
                allow_plaintext: true,
                default_backend: Backend::Plaintext,
            };

            set("mykey", "sk-test-value", &Backend::Plaintext, &cfg).unwrap();
            let val = get("mykey", &Backend::Plaintext, &cfg).unwrap();
            assert_eq!(val.as_deref(), Some("sk-test-value"));

            let names = list(&cfg).unwrap();
            assert!(names.iter().any(|n| n.contains("mykey")));

            let deleted = delete("mykey", &Backend::Plaintext, &cfg).unwrap();
            assert!(deleted);

            let val2 = get("mykey", &Backend::Plaintext, &cfg).unwrap();
            assert!(val2.is_none());

            let deleted2 = delete("mykey", &Backend::Plaintext, &cfg).unwrap();
            assert!(!deleted2);
        });
    }

    #[test]
    fn test_plaintext_disabled_by_default() {
        let _tmp = with_home(|| {
            let cfg = SecretsConfig::default();
            let result = set("key", "val", &Backend::Plaintext, &cfg);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("allow_plaintext"));
        });
    }

    #[test]
    fn test_resolve_ref_literal() {
        let cfg = SecretsConfig::default();
        let val = resolve_ref("literal:test-key", &cfg).unwrap();
        assert_eq!(val.as_deref(), Some("test-key"));
    }

    #[test]
    fn test_resolve_ref_env() {
        let cfg = SecretsConfig::default();
        unsafe { std::env::set_var("WG_TEST_SECRET_VAR_XYZ", "env-value") };
        let val = resolve_ref("env:WG_TEST_SECRET_VAR_XYZ", &cfg).unwrap();
        assert_eq!(val.as_deref(), Some("env-value"));
        unsafe { std::env::remove_var("WG_TEST_SECRET_VAR_XYZ") };
    }

    #[test]
    fn test_resolve_ref_env_missing() {
        let cfg = SecretsConfig::default();
        unsafe { std::env::remove_var("WG_NONEXISTENT_VAR_12345") };
        let val = resolve_ref("env:WG_NONEXISTENT_VAR_12345", &cfg).unwrap();
        assert!(val.is_none());
    }

    #[test]
    fn test_resolve_ref_keystore() {
        let _tmp = with_home(|| {
            let cfg = SecretsConfig::default();
            keystore_set("myapikey", "sk-12345").unwrap();
            let val = resolve_ref("keystore:myapikey", &cfg).unwrap();
            assert_eq!(val.as_deref(), Some("sk-12345"));
            keystore_delete("myapikey").unwrap();
        });
    }

    /// keyring:<name> URI resolves via keystore fallback when OS keyring is
    /// unreachable (typical CI / smoke environment).
    #[test]
    fn test_resolve_ref_keyring_falls_back_to_keystore() {
        let _tmp = with_home(|| {
            let cfg = SecretsConfig::default();
            // Stage a value in the file keystore with the same name.
            keystore_set("legacy", "sk-legacy-value").unwrap();
            let val = resolve_ref("keyring:legacy", &cfg).unwrap();
            assert_eq!(val.as_deref(), Some("sk-legacy-value"));
            keystore_delete("legacy").unwrap();
        });
    }

    #[test]
    fn test_resolve_ref_plain() {
        let _tmp = with_home(|| {
            let cfg = SecretsConfig {
                allow_plaintext: true,
                default_backend: Backend::Plaintext,
            };
            plaintext_set("myapikey", "sk-plain").unwrap();
            let val = resolve_ref("plain:myapikey", &cfg).unwrap();
            assert_eq!(val.as_deref(), Some("sk-plain"));
            plaintext_delete("myapikey").unwrap();
        });
    }

    #[test]
    fn test_resolve_ref_unknown_scheme() {
        let cfg = SecretsConfig::default();
        let result = resolve_ref("fakescheme:something", &cfg);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Unknown api_key_ref scheme")
        );
    }

    #[test]
    fn test_check_ref_reachable_missing() {
        let _tmp = with_home(|| {
            let cfg = SecretsConfig {
                allow_plaintext: true,
                default_backend: Backend::Plaintext,
            };
            let reachable = check_ref_reachable("plain:no-such-key", &cfg).unwrap();
            assert!(!reachable);
        });
    }

    #[test]
    fn test_secret_name_rejects_path_traversal() {
        let result = validate_name("../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn test_secret_name_rejects_slash() {
        let result = validate_name("subdir/key");
        assert!(result.is_err());
    }

    #[test]
    fn test_backend_parse_includes_keystore() {
        use std::str::FromStr;
        assert_eq!(Backend::from_str("keystore").unwrap(), Backend::Keystore);
        assert_eq!(Backend::from_str("keyring").unwrap(), Backend::Keyring);
        assert_eq!(Backend::from_str("plaintext").unwrap(), Backend::Plaintext);
        assert_eq!(Backend::from_str("plain").unwrap(), Backend::Plaintext);
        assert!(Backend::from_str("nonsense").is_err());
    }

    #[test]
    fn test_backend_status_mentions_all_backends() {
        let _tmp = with_home(|| {
            let cfg = SecretsConfig::default();
            let status = backend_status(&cfg);
            // Honest naming: status mentions both keyring and keystore explicitly.
            assert!(status.contains("Keyring"), "status missing 'Keyring': {}", status);
            assert!(
                status.contains("Keystore"),
                "status missing 'Keystore': {}",
                status
            );
            // Status no longer claims keyring is "secure file store" — that
            // was the old misnomer.
            assert!(
                !status.contains("Default backend: keyring (secure file store"),
                "status still uses misleading keyring label: {}",
                status
            );
        });
    }
}
