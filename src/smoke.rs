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
pub fn run_scenarios(scenarios: &[&Scenario], manifest_dir: &Path) -> GateReport {
    let mut results = Vec::with_capacity(scenarios.len());
    for s in scenarios {
        results.push(run_scenario(s, manifest_dir));
    }
    GateReport { results }
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
