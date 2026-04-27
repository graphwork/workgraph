//! Smoke gate: structured manifest of regression-protecting scenarios that
//! `wg done` runs before letting a task be marked complete.
//!
//! The contract is intentionally minimal: each scenario is a script that
//! the manifest declares as a permanent regression check, owned by one or
//! more tasks. When `wg done <task>` runs, it executes every scenario whose
//! `owners` list contains the task id (or every scenario when `--full-smoke`
//! is passed). A scenario that exits non-zero blocks `wg done` and the
//! caller is told exactly which scenario broke.
//!
//! Scenarios use exit codes to communicate three states:
//!   * 0   → PASS
//!   * 77  → loud SKIP (endpoint unreachable, missing credential, etc.)
//!   * any other non-zero → FAIL
//!
//! 77 is the GNU autotools convention for "skipped"; we reuse it so scripts
//! can express "I cannot run here" without lying about a pass.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Default smoke fixture root. Bash-side `wg_smoke_root` in `_helpers.sh`
/// and the Rust-side sweeper agree on this path so a leak left by either
/// side is reachable from either side.
const DEFAULT_SMOKE_ROOT_NAME: &str = "wgsmoke";

/// Default location of the smoke manifest, relative to the repo root.
pub const DEFAULT_MANIFEST_PATH: &str = "tests/smoke/manifest.toml";

/// Exit code that scenario scripts must use to signal a loud SKIP.
pub const SKIP_EXIT_CODE: i32 = 77;

/// Default per-scenario timeout when not specified in the manifest.
const DEFAULT_TIMEOUT_SECS: u64 = 180;

#[derive(Debug, Deserialize, Clone)]
pub struct Manifest {
    #[serde(default, rename = "scenario")]
    pub scenarios: Vec<Scenario>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Scenario {
    pub name: String,
    pub script: String,
    #[serde(default)]
    pub owners: Vec<String>,
    #[serde(default)]
    pub description: String,
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScenarioOutcome {
    Pass,
    Fail { exit_code: i32, stderr_tail: String },
    Skip { reason: String },
    Error { message: String },
}

#[derive(Debug, Clone)]
pub struct ScenarioResult {
    pub name: String,
    pub outcome: ScenarioOutcome,
}

impl Manifest {
    /// Load a manifest from a specific path. Returns an empty manifest if the
    /// file does not exist (smoke gate is a no-op when no manifest is defined).
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Manifest {
                scenarios: vec![],
            });
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read smoke manifest at {}", path.display()))?;
        let mut manifest: Manifest = toml::from_str(&text)
            .with_context(|| format!("failed to parse smoke manifest at {}", path.display()))?;
        // Detect duplicate scenario names eagerly — the manifest is grow-only
        // and a name collision masks a regression.
        let mut seen = std::collections::HashSet::new();
        for s in &manifest.scenarios {
            if !seen.insert(s.name.clone()) {
                anyhow::bail!(
                    "smoke manifest at {} contains duplicate scenario name '{}'",
                    path.display(),
                    s.name
                );
            }
        }
        // Normalise owner ids — trim whitespace, drop empties.
        for s in &mut manifest.scenarios {
            s.owners = std::mem::take(&mut s.owners)
                .into_iter()
                .map(|o| o.trim().to_string())
                .filter(|o| !o.is_empty())
                .collect();
        }
        Ok(manifest)
    }

    /// Resolve the manifest path used by `wg done`. Order:
    ///   1. `WG_SMOKE_MANIFEST` env var (absolute or relative to `dir`).
    ///   2. `<dir>/tests/smoke/manifest.toml`.
    ///   3. `<git toplevel>/tests/smoke/manifest.toml` (when `dir` is inside
    ///      a worktree).
    pub fn resolve_path(dir: &Path) -> PathBuf {
        if let Ok(env_path) = std::env::var("WG_SMOKE_MANIFEST") {
            let p = PathBuf::from(&env_path);
            return if p.is_absolute() {
                p
            } else {
                dir.join(p)
            };
        }
        let local = dir.join(DEFAULT_MANIFEST_PATH);
        if local.exists() {
            return local;
        }
        if let Some(parent) = dir.parent() {
            let candidate = parent.join(DEFAULT_MANIFEST_PATH);
            if candidate.exists() {
                return candidate;
            }
        }
        if let Some(top) = git_toplevel(dir) {
            let candidate = top.join(DEFAULT_MANIFEST_PATH);
            if candidate.exists() {
                return candidate;
            }
        }
        local
    }

    /// Load the manifest using the standard resolution order.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = Self::resolve_path(dir);
        Self::load_from(&path)
    }

    /// Return scenarios owned by a specific task id.
    pub fn scenarios_for_task(&self, task_id: &str) -> Vec<&Scenario> {
        self.scenarios
            .iter()
            .filter(|s| s.owners.iter().any(|o| o == task_id))
            .collect()
    }
}

fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// Run a single scenario. The script path is resolved relative to the
/// directory containing the manifest; absolute paths are used as-is.
pub fn run_scenario(scenario: &Scenario, manifest_dir: &Path) -> ScenarioResult {
    let script_path = if Path::new(&scenario.script).is_absolute() {
        PathBuf::from(&scenario.script)
    } else {
        manifest_dir.join(&scenario.script)
    };

    if !script_path.exists() {
        return ScenarioResult {
            name: scenario.name.clone(),
            outcome: ScenarioOutcome::Error {
                message: format!("script not found: {}", script_path.display()),
            },
        };
    }

    let timeout = Duration::from_secs(scenario.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECS));

    // Use `timeout` if available so a hung scenario can't deadlock `wg done`.
    let mut cmd = if which_timeout() {
        let mut c = Command::new("timeout");
        c.arg("--preserve-status")
            .arg(format!("{}", timeout.as_secs()))
            .arg("bash")
            .arg(&script_path);
        c
    } else {
        let mut c = Command::new("bash");
        c.arg(&script_path);
        c
    };

    cmd.env("WG_SMOKE_SCENARIO", &scenario.name);
    cmd.env("WG_SMOKE_TIMEOUT_SECS", timeout.as_secs().to_string());

    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            return ScenarioResult {
                name: scenario.name.clone(),
                outcome: ScenarioOutcome::Error {
                    message: format!("failed to spawn script: {}", e),
                },
            };
        }
    };

    match output.status.code() {
        Some(0) => ScenarioResult {
            name: scenario.name.clone(),
            outcome: ScenarioOutcome::Pass,
        },
        Some(code) if code == SKIP_EXIT_CODE => {
            let reason = stderr_tail(&output.stderr, 4);
            ScenarioResult {
                name: scenario.name.clone(),
                outcome: ScenarioOutcome::Skip {
                    reason: if reason.is_empty() {
                        "scenario emitted SKIP (exit 77)".to_string()
                    } else {
                        reason
                    },
                },
            }
        }
        Some(code) => ScenarioResult {
            name: scenario.name.clone(),
            outcome: ScenarioOutcome::Fail {
                exit_code: code,
                stderr_tail: stderr_tail(&output.stderr, 12),
            },
        },
        None => ScenarioResult {
            name: scenario.name.clone(),
            outcome: ScenarioOutcome::Fail {
                exit_code: -1,
                stderr_tail: stderr_tail(&output.stderr, 12),
            },
        },
    }
}

fn which_timeout() -> bool {
    Command::new("timeout")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn stderr_tail(bytes: &[u8], n: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Aggregate result of running a collection of scenarios.
#[derive(Debug, Default, Clone)]
pub struct GateReport {
    pub results: Vec<ScenarioResult>,
}

impl GateReport {
    pub fn failures(&self) -> Vec<&ScenarioResult> {
        self.results
            .iter()
            .filter(|r| matches!(r.outcome, ScenarioOutcome::Fail { .. }))
            .collect()
    }

    pub fn errors(&self) -> Vec<&ScenarioResult> {
        self.results
            .iter()
            .filter(|r| matches!(r.outcome, ScenarioOutcome::Error { .. }))
            .collect()
    }

    pub fn skips(&self) -> Vec<&ScenarioResult> {
        self.results
            .iter()
            .filter(|r| matches!(r.outcome, ScenarioOutcome::Skip { .. }))
            .collect()
    }

    pub fn passes(&self) -> Vec<&ScenarioResult> {
        self.results
            .iter()
            .filter(|r| matches!(r.outcome, ScenarioOutcome::Pass))
            .collect()
    }

    /// Returns true when at least one scenario FAILED or had an Error.
    /// Skips never block the gate.
    pub fn blocks_done(&self) -> bool {
        !self.failures().is_empty() || !self.errors().is_empty()
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for r in &self.results {
            match &r.outcome {
                ScenarioOutcome::Pass => out.push_str(&format!("  PASS  {}\n", r.name)),
                ScenarioOutcome::Skip { reason } => {
                    out.push_str(&format!("  SKIP  {} — {}\n", r.name, reason))
                }
                ScenarioOutcome::Fail {
                    exit_code,
                    stderr_tail,
                } => {
                    out.push_str(&format!(
                        "  FAIL  {} (exit {})\n        {}\n",
                        r.name,
                        exit_code,
                        stderr_tail.replace('\n', "\n        ")
                    ));
                }
                ScenarioOutcome::Error { message } => {
                    out.push_str(&format!("  ERROR {} — {}\n", r.name, message));
                }
            }
        }
        out
    }
}

/// Run every scenario in `scenarios`. The manifest_dir is needed to resolve
/// relative script paths.
///
/// Sweeps stale smoke-test daemons + scratch dirs both before AND after the
/// run. Pre-sweep prevents leaked state from a prior crashed run from
/// influencing the current run. Post-sweep guarantees that even if a
/// scenario crashes past its bash trap (SIGKILL, OOM, hard panic), the
/// system is left clean. See `tests/smoke/scenarios/_helpers.sh` for the
/// matching bash-side `wg_smoke_sweep`.
///
/// The pre-sweep applies an age cutoff so a concurrent smoke run in
/// another worktree (sharing the same smoke root) is not killed mid-test;
/// the post-sweep applies the same cutoff for symmetry. Production leaks
/// in this codebase have always been hours old by the time anyone notices,
/// so the cutoff is generous.
pub fn run_scenarios(scenarios: &[&Scenario], manifest_dir: &Path) -> GateReport {
    sweep_smoke_leaks_older_than(LEAK_REAP_MIN_AGE);
    let mut results = Vec::with_capacity(scenarios.len());
    for s in scenarios {
        results.push(run_scenario(s, manifest_dir));
    }
    sweep_smoke_leaks_older_than(LEAK_REAP_MIN_AGE);
    GateReport { results }
}

/// Daemons / scratch dirs younger than this are left alone by the gate
/// sweep so a concurrent smoke run doesn't cannibalise itself. Per-scenario
/// bash traps in `_helpers.sh` are the primary teardown; this sweep is
/// defence in depth for genuinely leaked fixtures.
const LEAK_REAP_MIN_AGE: Duration = Duration::from_secs(600);

/// Resolve the smoke fixture root (`${WG_SMOKE_ROOT:-${TMPDIR:-/tmp}/wgsmoke}`).
/// Mirrors the bash `wg_smoke_root` in `tests/smoke/scenarios/_helpers.sh`.
pub fn smoke_root() -> PathBuf {
    if let Ok(env_root) = std::env::var("WG_SMOKE_ROOT")
        && !env_root.is_empty()
    {
        return PathBuf::from(env_root);
    }
    let tmp = std::env::var("TMPDIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/tmp".to_string());
    PathBuf::from(tmp).join(DEFAULT_SMOKE_ROOT_NAME)
}

/// Find and SIGTERM/SIGKILL any `wg service daemon` whose `--dir` argv lives
/// under the smoke root and is older than `min_age`, then remove every
/// matching subdir under the root. Idempotent.
///
/// Survives re-parenting: the scan walks `/proc/*/cmdline` so init-owned
/// orphans are caught the same as direct children. This is the defence
/// against trap handlers that did not fire on SIGKILL / panic / OOM and
/// left a daemon to accumulate over weeks.
///
/// `min_age` of `Duration::ZERO` reaps everything; pass a larger value when
/// concurrent smoke runs may be sharing the same root.
pub fn sweep_smoke_leaks_older_than(min_age: Duration) {
    sweep_smoke_leaks_under(&smoke_root(), min_age)
}

/// Convenience: reap everything under the smoke root regardless of age.
/// Public so tests and CLI helpers can drive an unconditional cleanup.
pub fn sweep_smoke_leaks() {
    sweep_smoke_leaks_under(&smoke_root(), Duration::ZERO);
}

/// Sweep against an explicit root (useful for tests, where pointing at a
/// shared global root would race with parallel test threads).
#[cfg(unix)]
pub fn sweep_smoke_leaks_under(root: &Path, min_age: Duration) {
    let root_str = root.to_string_lossy().to_string();
    let prefix = format!("{}/", root_str);

    let victims = find_smoke_daemons_older_than(&prefix, min_age);
    for &pid in &victims {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    if !victims.is_empty() {
        std::thread::sleep(Duration::from_millis(500));
        // SIGKILL anything still alive after the SIGTERM grace period.
        for &pid in &victims {
            if unsafe { libc::kill(pid as i32, 0) } == 0 {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
    }

    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            if !path_age_exceeds(&entry.path(), min_age) {
                continue;
            }
            let path = entry.path();
            // Don't follow symlinks; just rm whatever's there.
            let _ = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
        }
    }
}

#[cfg(not(unix))]
pub fn sweep_smoke_leaks_under(_root: &Path, _min_age: Duration) {}

#[cfg(unix)]
fn path_age_exceeds(path: &Path, min_age: Duration) -> bool {
    if min_age.is_zero() {
        return true;
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let modified = match metadata.modified() {
        Ok(m) => m,
        Err(_) => return false,
    };
    match std::time::SystemTime::now().duration_since(modified) {
        Ok(age) => age >= min_age,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn path_age_exceeds(_path: &Path, _min_age: Duration) -> bool {
    false
}

/// Scan /proc for `wg ... service daemon ... --dir <prefix>...` processes
/// older than `min_age` (process start time). Returns the matching PIDs.
#[cfg(target_os = "linux")]
fn find_smoke_daemons_older_than(dir_prefix: &str, min_age: Duration) -> Vec<u32> {
    let mut victims = Vec::new();
    let entries = match std::fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return victims,
    };
    for entry in entries.flatten() {
        let pid: u32 = match entry.file_name().to_string_lossy().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let cmdline = match std::fs::read(entry.path().join("cmdline")) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // cmdline is NUL-separated argv.
        let s = String::from_utf8_lossy(&cmdline);
        let args: Vec<&str> = s.split('\0').filter(|a| !a.is_empty()).collect();
        let has_service_daemon = args
            .windows(2)
            .any(|w| w[0] == "service" && w[1] == "daemon");
        if !has_service_daemon {
            continue;
        }
        let dir_match = args
            .windows(2)
            .any(|w| w[0] == "--dir" && w[1].starts_with(dir_prefix));
        if !dir_match {
            continue;
        }
        if !min_age.is_zero() && !proc_age_exceeds(pid, min_age) {
            continue;
        }
        victims.push(pid);
    }
    victims
}

#[cfg(all(unix, not(target_os = "linux")))]
fn find_smoke_daemons_older_than(_dir_prefix: &str, _min_age: Duration) -> Vec<u32> {
    // /proc-based discovery is Linux-specific; on other Unix platforms
    // the bash-side cleanup trap is the only line of defence. The smoke
    // gate still works — it just lacks the hard-kill backstop here.
    Vec::new()
}

/// True if `/proc/<pid>` exists and has been around at least `min_age`.
/// Uses the directory's mtime as a proxy for process start time.
#[cfg(target_os = "linux")]
fn proc_age_exceeds(pid: u32, min_age: Duration) -> bool {
    let p = format!("/proc/{}", pid);
    path_age_exceeds(Path::new(&p), min_age)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        let mut perm = fs::metadata(&p).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perm.set_mode(0o755);
        fs::set_permissions(&p, perm).unwrap();
        p
    }

    #[test]
    fn pass_outcome_for_zero_exit_script() {
        let td = TempDir::new().unwrap();
        write_script(td.path(), "ok.sh", "#!/usr/bin/env bash\nexit 0\n");
        let scenario = Scenario {
            name: "ok".to_string(),
            script: "ok.sh".to_string(),
            owners: vec!["task-a".to_string()],
            description: String::new(),
            timeout_seconds: Some(10),
        };
        let r = run_scenario(&scenario, td.path());
        assert_eq!(r.outcome, ScenarioOutcome::Pass);
    }

    #[test]
    fn fail_outcome_for_nonzero_exit_script() {
        let td = TempDir::new().unwrap();
        write_script(
            td.path(),
            "bad.sh",
            "#!/usr/bin/env bash\necho 'broken thing' 1>&2\nexit 3\n",
        );
        let scenario = Scenario {
            name: "bad".to_string(),
            script: "bad.sh".to_string(),
            owners: vec!["task-a".to_string()],
            description: String::new(),
            timeout_seconds: Some(10),
        };
        let r = run_scenario(&scenario, td.path());
        match r.outcome {
            ScenarioOutcome::Fail {
                exit_code,
                stderr_tail,
            } => {
                assert_eq!(exit_code, 3);
                assert!(stderr_tail.contains("broken thing"));
            }
            other => panic!("expected Fail, got {:?}", other),
        }
    }

    #[test]
    fn skip_outcome_for_exit_77() {
        let td = TempDir::new().unwrap();
        write_script(
            td.path(),
            "skip.sh",
            "#!/usr/bin/env bash\necho 'endpoint unreachable' 1>&2\nexit 77\n",
        );
        let scenario = Scenario {
            name: "skipme".to_string(),
            script: "skip.sh".to_string(),
            owners: vec!["task-a".to_string()],
            description: String::new(),
            timeout_seconds: Some(10),
        };
        let r = run_scenario(&scenario, td.path());
        match r.outcome {
            ScenarioOutcome::Skip { reason } => {
                assert!(reason.contains("endpoint unreachable"));
            }
            other => panic!("expected Skip, got {:?}", other),
        }
    }

    #[test]
    fn manifest_loader_filters_owners() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("manifest.toml");
        fs::write(
            &path,
            r#"
[[scenario]]
name = "alpha"
script = "alpha.sh"
owners = ["task-a", "task-b"]

[[scenario]]
name = "beta"
script = "beta.sh"
owners = ["task-c"]
"#,
        )
        .unwrap();
        let m = Manifest::load_from(&path).unwrap();
        assert_eq!(m.scenarios_for_task("task-a").len(), 1);
        assert_eq!(m.scenarios_for_task("task-c").len(), 1);
        assert_eq!(m.scenarios_for_task("task-z").len(), 0);
    }

    #[test]
    fn missing_manifest_yields_empty_manifest() {
        let td = TempDir::new().unwrap();
        let m = Manifest::load_from(&td.path().join("nope.toml")).unwrap();
        assert!(m.scenarios.is_empty());
    }

    #[test]
    fn duplicate_scenario_name_is_rejected() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("manifest.toml");
        fs::write(
            &path,
            r#"
[[scenario]]
name = "dup"
script = "a.sh"

[[scenario]]
name = "dup"
script = "b.sh"
"#,
        )
        .unwrap();
        let err = Manifest::load_from(&path).unwrap_err().to_string();
        assert!(err.contains("duplicate scenario name 'dup'"));
    }

    #[test]
    fn gate_report_blocks_only_on_fail_or_error() {
        let r1 = ScenarioResult {
            name: "a".into(),
            outcome: ScenarioOutcome::Pass,
        };
        let r2 = ScenarioResult {
            name: "b".into(),
            outcome: ScenarioOutcome::Skip {
                reason: "x".into(),
            },
        };
        let r3 = ScenarioResult {
            name: "c".into(),
            outcome: ScenarioOutcome::Fail {
                exit_code: 1,
                stderr_tail: "boom".into(),
            },
        };
        let report = GateReport {
            results: vec![r1.clone(), r2.clone()],
        };
        assert!(!report.blocks_done());
        let report2 = GateReport {
            results: vec![r1, r2, r3],
        };
        assert!(report2.blocks_done());
    }

    /// Combined into one test because env vars are process-global and
    /// cargo test runs tests in parallel — if we split the env-var case
    /// from the default case, the latter occasionally observes the former
    /// mid-flight and fails.
    #[test]
    fn smoke_root_resolution() {
        let prior = std::env::var("WG_SMOKE_ROOT").ok();

        unsafe { std::env::set_var("WG_SMOKE_ROOT", "/tmp/wgsmoke-test-override") };
        assert_eq!(smoke_root(), PathBuf::from("/tmp/wgsmoke-test-override"));

        unsafe { std::env::remove_var("WG_SMOKE_ROOT") };
        let r = smoke_root();
        assert!(
            r.ends_with(DEFAULT_SMOKE_ROOT_NAME),
            "expected default smoke root to end with '{}', got {}",
            DEFAULT_SMOKE_ROOT_NAME,
            r.display()
        );

        if let Some(p) = prior {
            unsafe { std::env::set_var("WG_SMOKE_ROOT", p) };
        }
    }

    #[test]
    fn sweep_removes_old_subdirs_only() {
        // Create a fresh root with one young dir and one mtime-aged dir.
        // The age-cutoff sweep must keep the young one and reap the old one.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("wgsmoke");
        std::fs::create_dir_all(&root).unwrap();
        let young = root.join("young.scenario.AAAAAA");
        let old = root.join("old.scenario.BBBBBB");
        std::fs::create_dir_all(&young).unwrap();
        std::fs::create_dir_all(&old).unwrap();

        // Backdate the "old" dir's mtime via `touch -d` so it counts as aged.
        // Avoids pulling in a new crate just for the test.
        let status = Command::new("touch")
            .arg("-d")
            .arg("2020-01-01")
            .arg(&old)
            .status()
            .expect("touch must be available for this test");
        assert!(status.success(), "touch -d failed");

        sweep_smoke_leaks_under(&root, Duration::from_secs(3600));

        assert!(young.is_dir(), "young dir should be kept");
        assert!(!old.exists(), "old dir should be reaped");
    }

    #[test]
    fn sweep_zero_age_reaps_everything() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("wgsmoke");
        std::fs::create_dir_all(&root).unwrap();
        let a = root.join("a.scenario.XX");
        let b = root.join("b.scenario.YY");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        sweep_smoke_leaks_under(&root, Duration::ZERO);

        assert!(!a.exists());
        assert!(!b.exists());
        assert!(root.is_dir(), "root itself should remain");
    }

    #[test]
    fn missing_script_is_error_outcome() {
        let td = TempDir::new().unwrap();
        let scenario = Scenario {
            name: "ghost".to_string(),
            script: "does-not-exist.sh".to_string(),
            owners: vec!["task-a".to_string()],
            description: String::new(),
            timeout_seconds: Some(10),
        };
        let r = run_scenario(&scenario, td.path());
        assert!(matches!(r.outcome, ScenarioOutcome::Error { .. }));
    }
}
