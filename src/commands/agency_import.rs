use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use workgraph::agency::{
    self, AccessControl, AccessPolicy, ComponentCategory, ContentRef, DesiredOutcome, Lineage,
    PerformanceRecord, RoleComponent, TradeoffConfig,
};
use workgraph::config::Config;

/// Counts of primitives imported from a CSV file.
#[derive(Debug, Clone, Default)]
pub struct ImportCounts {
    pub role_components: u32,
    pub desired_outcomes: u32,
    pub trade_off_configs: u32,
    pub skipped: u32,
}

/// Provenance manifest written after a successful CSV import.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImportManifest {
    pub source: String,
    pub version: String,
    pub imported_at: String,
    pub counts: ManifestCounts,
    pub content_hash: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ManifestCounts {
    pub role_components: u32,
    pub desired_outcomes: u32,
    pub trade_off_configs: u32,
}

/// Path to the import manifest within the workgraph agency directory.
pub fn manifest_path(workgraph_dir: &Path) -> std::path::PathBuf {
    workgraph_dir.join("agency/import-manifest.yaml")
}

/// Compute SHA-256 hex digest of file contents.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Write (or update) the import manifest after a successful import.
pub fn write_manifest(
    workgraph_dir: &Path,
    source: &str,
    csv_content: &[u8],
    counts: &ImportCounts,
) -> Result<()> {
    let manifest = ImportManifest {
        source: source.to_string(),
        version: format!("v{}", env!("CARGO_PKG_VERSION")),
        imported_at: chrono::Utc::now().to_rfc3339(),
        counts: ManifestCounts {
            role_components: counts.role_components,
            desired_outcomes: counts.desired_outcomes,
            trade_off_configs: counts.trade_off_configs,
        },
        content_hash: sha256_hex(csv_content),
    };
    let path = manifest_path(workgraph_dir);
    std::fs::write(&path, serde_yaml::to_string(&manifest)?)
        .context("Failed to write import manifest")?;
    Ok(())
}

/// Options for the import command (covers local file, URL, and upstream modes).
pub struct ImportOptions {
    pub csv_path: Option<String>,
    pub url: Option<String>,
    pub upstream: bool,
    pub dry_run: bool,
    pub tag: Option<String>,
    pub force: bool,
    pub check: bool,
}

/// Fetch CSV content from a remote URL.
fn fetch_csv(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("Failed to build HTTP client")?;

    let response = client
        .get(url)
        .send()
        .with_context(|| format!("Failed to fetch '{}'", url))?;

    if !response.status().is_success() {
        anyhow::bail!("HTTP {} fetching '{}'", response.status(), url);
    }

    let bytes = response
        .bytes()
        .with_context(|| format!("Failed to read response from '{}'", url))?;

    Ok(bytes.to_vec())
}

/// Read the existing import manifest, if any.
pub fn read_manifest(workgraph_dir: &Path) -> Result<Option<ImportManifest>> {
    let path = manifest_path(workgraph_dir);
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path).context("Failed to read import manifest")?;
    let manifest: ImportManifest =
        serde_yaml::from_str(&content).context("Failed to parse import manifest")?;
    Ok(Some(manifest))
}

/// Run import from raw CSV bytes (shared by local-file and URL-fetch paths).
pub fn run_from_bytes(
    workgraph_dir: &Path,
    source_label: &str,
    csv_bytes: &[u8],
    dry_run: bool,
    tag: Option<&str>,
) -> Result<ImportCounts> {
    let provenance_tag = tag.unwrap_or("agency-import");
    let agency_dir = workgraph_dir.join("agency");

    if !dry_run {
        agency::init(&agency_dir).context("Failed to initialize agency directory")?;
    }

    let csv_content = String::from_utf8_lossy(csv_bytes);
    let mut reader = csv::Reader::from_reader(csv_content.as_bytes());

    let format = detect_format(reader.headers().context("Failed to read CSV headers")?);

    let mut components_count = 0u32;
    let mut outcomes_count = 0u32;
    let mut tradeoffs_count = 0u32;
    let mut skipped = 0u32;

    for (row_idx, record) in reader.records().enumerate() {
        let record = record.with_context(|| format!("Failed to parse CSV row {}", row_idx + 1))?;

        let ptype = record.get(0).unwrap_or("").trim();
        let name = record.get(1).unwrap_or("").trim().to_string();
        let description = record.get(2).unwrap_or("").trim().to_string();

        let (quality_score, domain_tags, metadata, parent_content_hash) = match format {
            CsvFormat::Agency => parse_agency_columns(&record),
            CsvFormat::Legacy => parse_legacy_columns(&record),
        };

        let mut parent_ids = vec![];
        if let Some(ref pch) = parent_content_hash {
            if !pch.is_empty() {
                parent_ids.push(pch.clone());
            }
        }

        let lineage = Lineage {
            parent_ids,
            generation: 0,
            created_by: format!("{}-v{}", provenance_tag, env!("CARGO_PKG_VERSION")),
            created_at: chrono::Utc::now(),
        };

        let access_control = AccessControl {
            owner: provenance_tag.to_string(),
            policy: AccessPolicy::Open,
        };

        let performance = PerformanceRecord {
            task_count: 0,
            avg_score: quality_score,
            evaluations: vec![],
        };

        let normalized_type = match ptype {
            "skill" | "role_component" => "component",
            "outcome" | "desired_outcome" => "outcome",
            "tradeoff" | "trade_off_config" => "tradeoff",
            other => other,
        };

        match normalized_type {
            "component" => {
                let content = ContentRef::Inline(description.clone());
                let category = ComponentCategory::Translated;
                let id = agency::content_hash_component(&description, &category, &content);

                if dry_run {
                    println!("  [component] {} ({})", name, agency::short_hash(&id));
                } else {
                    let component = RoleComponent {
                        id: id.clone(),
                        name,
                        description,
                        category,
                        content,
                        performance,
                        lineage,
                        access_control,
                        domain_tags,
                        metadata,
                        former_agents: vec![],
                        former_deployments: vec![],
                    };
                    let dir = agency_dir.join("primitives/components");
                    agency::save_component(&component, &dir).with_context(|| {
                        format!("Failed to save component {}", agency::short_hash(&id))
                    })?;
                }
                components_count += 1;
            }
            "outcome" => {
                let success_criteria = match format {
                    CsvFormat::Legacy => {
                        let col5 = record.get(4).unwrap_or("").trim().to_string();
                        col5.split('\n')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    }
                    CsvFormat::Agency => vec![],
                };
                let id = agency::content_hash_outcome(&description, &success_criteria);

                if dry_run {
                    println!("  [outcome] {} ({})", name, agency::short_hash(&id));
                } else {
                    let outcome = DesiredOutcome {
                        id: id.clone(),
                        name,
                        description,
                        success_criteria,
                        performance,
                        lineage,
                        access_control,
                        requires_human_oversight: true,
                        domain_tags,
                        metadata,
                        former_agents: vec![],
                        former_deployments: vec![],
                    };
                    let dir = agency_dir.join("primitives/outcomes");
                    agency::save_outcome(&outcome, &dir).with_context(|| {
                        format!("Failed to save outcome {}", agency::short_hash(&id))
                    })?;
                }
                outcomes_count += 1;
            }
            "tradeoff" => {
                let (acceptable, unacceptable) = match format {
                    CsvFormat::Legacy => {
                        let col4 = record.get(3).unwrap_or("").trim().to_string();
                        let col5 = record.get(4).unwrap_or("").trim().to_string();
                        let acc: Vec<String> = col4
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        let unacc: Vec<String> = col5
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        (acc, unacc)
                    }
                    CsvFormat::Agency => {
                        (vec![], vec![])
                    }
                };
                let id = agency::content_hash_tradeoff(&acceptable, &unacceptable, &description);

                if dry_run {
                    println!("  [tradeoff] {} ({})", name, agency::short_hash(&id));
                } else {
                    let tradeoff = TradeoffConfig {
                        id: id.clone(),
                        name,
                        description,
                        acceptable_tradeoffs: acceptable,
                        unacceptable_tradeoffs: unacceptable,
                        performance,
                        lineage,
                        access_control,
                        domain_tags,
                        metadata,
                        former_agents: vec![],
                        former_deployments: vec![],
                    };
                    let dir = agency_dir.join("primitives/tradeoffs");
                    agency::save_tradeoff(&tradeoff, &dir).with_context(|| {
                        format!("Failed to save tradeoff {}", agency::short_hash(&id))
                    })?;
                }
                tradeoffs_count += 1;
            }
            _ => {
                skipped += 1;
                if !ptype.is_empty() {
                    eprintln!(
                        "Warning: skipping unknown type '{}' for '{}' (row {})",
                        ptype,
                        name,
                        row_idx + 1
                    );
                }
            }
        }
    }

    let counts = ImportCounts {
        role_components: components_count,
        desired_outcomes: outcomes_count,
        trade_off_configs: tradeoffs_count,
        skipped,
    };

    let mode = if dry_run { " (dry run)" } else { "" };
    println!("Agency import complete{}:", mode);
    println!("  Components: {}", components_count);
    println!("  Outcomes:   {}", outcomes_count);
    println!("  Tradeoffs:  {}", tradeoffs_count);
    if skipped > 0 {
        println!("  Skipped:    {}", skipped);
    }

    if !dry_run {
        write_manifest(workgraph_dir, source_label, csv_bytes, &counts)?;
    }

    Ok(counts)
}

/// Unified entry point for `wg agency import` supporting local file, URL, and upstream modes.
pub fn run_import(workgraph_dir: &Path, opts: ImportOptions) -> Result<ImportCounts> {
    // Determine the CSV source
    let source_count = opts.csv_path.is_some() as u8 + opts.url.is_some() as u8 + opts.upstream as u8;
    if source_count > 1 {
        anyhow::bail!("Specify only one of: CSV_PATH, --url, or --upstream");
    }

    if let Some(ref csv_path) = opts.csv_path {
        // Local file path — existing behavior
        return run(workgraph_dir, csv_path, opts.dry_run, opts.tag.as_deref());
    }

    // Resolve the URL (either explicit --url or --upstream from config)
    let url = if let Some(ref url) = opts.url {
        url.clone()
    } else if opts.upstream {
        let cfg = Config::load_merged(workgraph_dir)?;
        cfg.agency
            .upstream_url
            .ok_or_else(|| anyhow::anyhow!(
                "No upstream URL configured. Set agency.upstream_url in config:\n  wg config --set agency.upstream_url=<URL>"
            ))?
    } else {
        anyhow::bail!("Specify one of: CSV_PATH, --url <URL>, or --upstream");
    };

    // Change detection: compare hash of fetched CSV against manifest
    if !opts.force || opts.check {
        if let Some(existing_manifest) = read_manifest(workgraph_dir)? {
            // Fetch and check
            let csv_bytes = match fetch_csv(&url) {
                Ok(bytes) => bytes,
                Err(e) => {
                    if opts.check {
                        eprintln!("Warning: could not fetch upstream: {}", e);
                        std::process::exit(2);
                    }
                    return Err(e);
                }
            };
            let new_hash = sha256_hex(&csv_bytes);

            if opts.check {
                if new_hash == existing_manifest.content_hash {
                    println!("Up to date (hash: {}…)", &new_hash[..12]);
                    std::process::exit(1);
                } else {
                    println!("Upstream has changed (local: {}… remote: {}…)",
                        &existing_manifest.content_hash[..12], &new_hash[..12]);
                    std::process::exit(0);
                }
            }

            if !opts.force && new_hash == existing_manifest.content_hash {
                println!("Already up to date (hash: {}…)", &new_hash[..12]);
                return Ok(ImportCounts::default());
            }

            // Hash differs — import
            return run_from_bytes(workgraph_dir, &url, &csv_bytes, opts.dry_run, opts.tag.as_deref());
        }
    }

    // No existing manifest or --force: fetch and import
    let csv_bytes = match fetch_csv(&url) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("Warning: could not fetch upstream CSV: {}", e);
            if opts.check {
                std::process::exit(2);
            }
            return Err(e);
        }
    };

    if opts.check {
        // No manifest to compare against — treat as changed
        println!("No previous import found; upstream available");
        std::process::exit(0);
    }

    run_from_bytes(workgraph_dir, &url, &csv_bytes, opts.dry_run, opts.tag.as_deref())
}

/// Detected CSV format based on header or column count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CsvFormat {
    /// Old 7-column format: type,name,description,col4,col5,domain_tags,quality_score
    Legacy,
    /// Agency 9-column format: type,name,description,quality,domain_specificity,domain,origin_instance_id,parent_content_hash,scope
    Agency,
}

/// Detect the CSV format from the header row.
fn detect_format(headers: &csv::StringRecord) -> CsvFormat {
    // Check by column count first
    if headers.len() >= 9 {
        return CsvFormat::Agency;
    }
    // Also check by header names for explicit detection
    if let Some(col3) = headers.get(3) {
        let col3 = col3.trim().to_lowercase();
        if col3 == "quality" || col3 == "domain_specificity" {
            return CsvFormat::Agency;
        }
    }
    CsvFormat::Legacy
}

/// `wg agency import <csv-path>` -- import Agency's starter.csv primitives into WorkGraph.
///
/// Supports two CSV formats:
///
/// **Legacy (7 columns):** type,name,description,col4,col5,domain_tags,quality_score
///   - type: skill | outcome | tradeoff
///
/// **Agency (9 columns):** type,name,description,quality,domain_specificity,domain,origin_instance_id,parent_content_hash,scope
///   - type: role_component | desired_outcome | trade_off_config
///
/// Both formats are auto-detected. Legacy type names (skill/outcome/tradeoff) are also
/// accepted in the 9-column format and vice versa.
pub fn run(workgraph_dir: &Path, csv_path: &str, dry_run: bool, tag: Option<&str>) -> Result<ImportCounts> {
    let csv_bytes = std::fs::read(csv_path)
        .with_context(|| format!("Failed to read '{}'", csv_path))?;
    run_from_bytes(workgraph_dir, csv_path, &csv_bytes, dry_run, tag)
}

/// Parse columns from Agency's 9-column CSV format.
///
/// Columns: type(0), name(1), description(2), quality(3), domain_specificity(4),
///          domain(5), origin_instance_id(6), parent_content_hash(7), scope(8)
fn parse_agency_columns(
    record: &csv::StringRecord,
) -> (Option<f64>, Vec<String>, HashMap<String, String>, Option<String>) {
    // quality (col3): integer 0-100, map to avg_score as 0.0-1.0
    let quality_score: Option<f64> = record.get(3).and_then(|s| {
        let s = s.trim();
        s.parse::<f64>().ok().map(|v| v / 100.0)
    });

    // domain_specificity (col4): store as metadata
    let domain_specificity = record
        .get(4)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    // domain (col5): comma-separated tags
    let domain_tags: Vec<String> = record
        .get(5)
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();

    // origin_instance_id (col6): store as metadata
    let origin_instance_id = record
        .get(6)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    // parent_content_hash (col7): store in lineage.parent_ids
    let parent_content_hash = record.get(7).map(|s| s.trim().to_string());

    // scope (col8): store as metadata
    let scope = record
        .get(8)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    let mut metadata = HashMap::new();
    if !scope.is_empty() {
        metadata.insert("scope".to_string(), scope);
    }
    if !domain_specificity.is_empty() {
        metadata.insert("domain_specificity".to_string(), domain_specificity);
    }
    if !origin_instance_id.is_empty() {
        metadata.insert("origin_instance_id".to_string(), origin_instance_id);
    }
    if let Some(ref pch) = parent_content_hash {
        if !pch.is_empty() {
            metadata.insert("parent_content_hash".to_string(), pch.clone());
        }
    }

    (quality_score, domain_tags, metadata, parent_content_hash)
}

/// Parse columns from the legacy 7-column CSV format.
///
/// Columns: type(0), name(1), description(2), col4(3), col5(4), domain_tags(5), quality_score(6)
fn parse_legacy_columns(
    record: &csv::StringRecord,
) -> (Option<f64>, Vec<String>, HashMap<String, String>, Option<String>) {
    let quality_score: Option<f64> = record.get(6).and_then(|s| s.trim().parse().ok());

    let domain_tags: Vec<String> = record
        .get(5)
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();

    (quality_score, domain_tags, HashMap::new(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_fixture_csv(dir: &Path) -> std::path::PathBuf {
        let csv_path = dir.join("test_agency.csv");
        let mut f = std::fs::File::create(&csv_path).unwrap();
        writeln!(
            f,
            "type,name,description,col4,col5,domain_tags,quality_score"
        )
        .unwrap();
        writeln!(
            f,
            "skill,Code Review,Reviews code for correctness and style,Translated,Reviews code for correctness and style,programming,0.85"
        )
        .unwrap();
        // Use quoted field with literal newline for success criteria
        write!(
            f,
            "outcome,Working Code,Code compiles and passes tests,,\"All tests pass\nNo compiler warnings\",programming,0.90\n"
        )
        .unwrap();
        writeln!(
            f,
            "tradeoff,Speed vs Quality,Balances speed and quality,Fast execution,Incomplete analysis,general,0.75"
        )
        .unwrap();
        csv_path
    }

    fn write_agency_format_csv(dir: &Path) -> std::path::PathBuf {
        let csv_path = dir.join("test_agency_9col.csv");
        let mut f = std::fs::File::create(&csv_path).unwrap();
        writeln!(
            f,
            "type,name,description,quality,domain_specificity,domain,origin_instance_id,parent_content_hash,scope"
        )
        .unwrap();
        writeln!(
            f,
            "role_component,Identify Gaps,Identify gaps and errors in provided content,85,high,\"analysis,review\",inst-001,abc123,task"
        )
        .unwrap();
        writeln!(
            f,
            "desired_outcome,Accurate Analysis,Analysis is thorough and identifies all issues,90,medium,analysis,inst-002,,task"
        )
        .unwrap();
        writeln!(
            f,
            "trade_off_config,Prefer Depth,When depth and breadth conflict: prefer deeper analysis of fewer items over shallow coverage of many,70,low,\"analysis,research\",inst-003,def456,task"
        )
        .unwrap();
        // A meta-scope primitive
        writeln!(
            f,
            "role_component,Assign by Expertise,Match agent skills to task requirements using domain tags,80,high,meta,inst-004,,meta:assigner"
        )
        .unwrap();
        csv_path
    }

    #[test]
    fn test_agency_import_parses_csv() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv_path = write_fixture_csv(tmp.path());
        run(&wg_dir, csv_path.to_str().unwrap(), false, None).unwrap();

        // Verify files were created
        let components_dir = wg_dir.join("agency/primitives/components");
        let outcomes_dir = wg_dir.join("agency/primitives/outcomes");
        let tradeoffs_dir = wg_dir.join("agency/primitives/tradeoffs");

        let comp_count = std::fs::read_dir(&components_dir).unwrap().count();
        let out_count = std::fs::read_dir(&outcomes_dir).unwrap().count();
        let trade_count = std::fs::read_dir(&tradeoffs_dir).unwrap().count();

        assert_eq!(comp_count, 1, "Expected 1 component");
        assert_eq!(out_count, 1, "Expected 1 outcome");
        assert_eq!(trade_count, 1, "Expected 1 tradeoff");
    }

    #[test]
    fn test_agency_import_dry_run_no_files() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv_path = write_fixture_csv(tmp.path());
        run(&wg_dir, csv_path.to_str().unwrap(), true, None).unwrap();

        // Agency dir should not have been created (or should be empty)
        let agency_dir = wg_dir.join("agency");
        assert!(
            !agency_dir.exists() || !agency_dir.join("primitives/components").exists(),
            "Dry run should not create files"
        );
    }

    #[test]
    fn test_agency_import_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv_path = write_fixture_csv(tmp.path());

        // Import twice
        run(&wg_dir, csv_path.to_str().unwrap(), false, None).unwrap();
        run(&wg_dir, csv_path.to_str().unwrap(), false, None).unwrap();

        // Same count -- content hashing deduplicates
        let comp_count = std::fs::read_dir(wg_dir.join("agency/primitives/components"))
            .unwrap()
            .count();
        assert_eq!(comp_count, 1, "Re-import should not create duplicates");
    }

    #[test]
    fn test_agency_import_content_hash_stability() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv_path = write_fixture_csv(tmp.path());
        run(&wg_dir, csv_path.to_str().unwrap(), false, None).unwrap();

        // Record file names (which are content hashes)
        let components_dir = wg_dir.join("agency/primitives/components");
        let names1: Vec<String> = std::fs::read_dir(&components_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();

        // Import again into a fresh dir
        let tmp2 = tempfile::tempdir().unwrap();
        let wg_dir2 = tmp2.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir2).unwrap();
        run(&wg_dir2, csv_path.to_str().unwrap(), false, None).unwrap();

        let components_dir2 = wg_dir2.join("agency/primitives/components");
        let names2: Vec<String> = std::fs::read_dir(&components_dir2)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();

        assert_eq!(
            names1, names2,
            "Content hashes should be stable across imports"
        );
    }

    #[test]
    fn test_agency_import_9col_format() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv_path = write_agency_format_csv(tmp.path());
        run(&wg_dir, csv_path.to_str().unwrap(), false, None).unwrap();

        // Verify files were created: 2 components, 1 outcome, 1 tradeoff
        let components_dir = wg_dir.join("agency/primitives/components");
        let outcomes_dir = wg_dir.join("agency/primitives/outcomes");
        let tradeoffs_dir = wg_dir.join("agency/primitives/tradeoffs");

        let comp_count = std::fs::read_dir(&components_dir).unwrap().count();
        let out_count = std::fs::read_dir(&outcomes_dir).unwrap().count();
        let trade_count = std::fs::read_dir(&tradeoffs_dir).unwrap().count();

        assert_eq!(comp_count, 2, "Expected 2 components (task + meta:assigner)");
        assert_eq!(out_count, 1, "Expected 1 outcome");
        assert_eq!(trade_count, 1, "Expected 1 tradeoff");
    }

    #[test]
    fn test_agency_import_9col_metadata_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv_path = write_agency_format_csv(tmp.path());
        run(&wg_dir, csv_path.to_str().unwrap(), false, None).unwrap();

        // Read all components and check metadata
        let components_dir = wg_dir.join("agency/primitives/components");
        let mut found_task_scope = false;
        let mut found_meta_scope = false;

        for entry in std::fs::read_dir(&components_dir).unwrap() {
            let entry = entry.unwrap();
            let component: RoleComponent =
                agency::load_component(&entry.path()).unwrap();

            // Check that domain_tags are populated
            assert!(!component.domain_tags.is_empty(), "domain_tags should be populated");

            // Check scope metadata
            if let Some(scope) = component.metadata.get("scope") {
                if scope == "task" {
                    found_task_scope = true;
                    assert_eq!(
                        component.metadata.get("domain_specificity").map(|s| s.as_str()),
                        Some("high")
                    );
                    assert!(component.metadata.contains_key("origin_instance_id"));
                }
                if scope == "meta:assigner" {
                    found_meta_scope = true;
                }
            }
        }

        assert!(found_task_scope, "Should have a task-scope component");
        assert!(found_meta_scope, "Should have a meta:assigner-scope component");
    }

    #[test]
    fn test_agency_import_9col_quality_maps_to_score() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv_path = write_agency_format_csv(tmp.path());
        run(&wg_dir, csv_path.to_str().unwrap(), false, None).unwrap();

        // Read the outcome and check that quality (90) mapped to avg_score (0.90)
        let outcomes_dir = wg_dir.join("agency/primitives/outcomes");
        for entry in std::fs::read_dir(&outcomes_dir).unwrap() {
            let entry = entry.unwrap();
            let outcome: DesiredOutcome =
                agency::load_outcome(&entry.path()).unwrap();
            assert_eq!(outcome.performance.avg_score, Some(0.90));
        }
    }

    #[test]
    fn test_agency_import_9col_domain_maps_to_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv_path = write_agency_format_csv(tmp.path());
        run(&wg_dir, csv_path.to_str().unwrap(), false, None).unwrap();

        // Read the tradeoff and check domain tags
        let tradeoffs_dir = wg_dir.join("agency/primitives/tradeoffs");
        for entry in std::fs::read_dir(&tradeoffs_dir).unwrap() {
            let entry = entry.unwrap();
            let tradeoff: TradeoffConfig =
                agency::load_tradeoff(&entry.path()).unwrap();
            assert!(
                tradeoff.domain_tags.contains(&"analysis".to_string()),
                "Expected 'analysis' in domain_tags, got: {:?}",
                tradeoff.domain_tags
            );
            assert!(
                tradeoff.domain_tags.contains(&"research".to_string()),
                "Expected 'research' in domain_tags, got: {:?}",
                tradeoff.domain_tags
            );
        }
    }

    #[test]
    fn test_agency_import_9col_tradeoff_uses_description() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv_path = write_agency_format_csv(tmp.path());
        run(&wg_dir, csv_path.to_str().unwrap(), false, None).unwrap();

        // Tradeoff should use description as-is, with empty acceptable/unacceptable lists
        let tradeoffs_dir = wg_dir.join("agency/primitives/tradeoffs");
        for entry in std::fs::read_dir(&tradeoffs_dir).unwrap() {
            let entry = entry.unwrap();
            let tradeoff: TradeoffConfig =
                agency::load_tradeoff(&entry.path()).unwrap();
            assert!(
                tradeoff.acceptable_tradeoffs.is_empty(),
                "Agency format should not split description into acceptable list"
            );
            assert!(
                tradeoff.unacceptable_tradeoffs.is_empty(),
                "Agency format should not split description into unacceptable list"
            );
            assert!(
                tradeoff.description.contains("depth and breadth conflict"),
                "Description should be preserved as-is"
            );
        }
    }

    #[test]
    fn test_agency_import_9col_parent_content_hash_in_lineage() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv_path = write_agency_format_csv(tmp.path());
        run(&wg_dir, csv_path.to_str().unwrap(), false, None).unwrap();

        // The "Identify Gaps" component has parent_content_hash=abc123
        let components_dir = wg_dir.join("agency/primitives/components");
        let mut found_with_parent = false;
        for entry in std::fs::read_dir(&components_dir).unwrap() {
            let entry = entry.unwrap();
            let component: RoleComponent =
                agency::load_component(&entry.path()).unwrap();
            if component.name == "Identify Gaps" {
                assert!(
                    component.lineage.parent_ids.contains(&"abc123".to_string()),
                    "parent_content_hash should be in lineage.parent_ids"
                );
                found_with_parent = true;
            }
        }
        assert!(found_with_parent, "Should find the Identify Gaps component");
    }

    #[test]
    fn test_detect_format_agency() {
        let header = csv::StringRecord::from(vec![
            "type",
            "name",
            "description",
            "quality",
            "domain_specificity",
            "domain",
            "origin_instance_id",
            "parent_content_hash",
            "scope",
        ]);
        assert_eq!(detect_format(&header), CsvFormat::Agency);
    }

    #[test]
    fn test_detect_format_legacy() {
        let header = csv::StringRecord::from(vec![
            "type",
            "name",
            "description",
            "col4",
            "col5",
            "domain_tags",
            "quality_score",
        ]);
        assert_eq!(detect_format(&header), CsvFormat::Legacy);
    }

    #[test]
    fn test_import_writes_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv_path = write_agency_format_csv(tmp.path());
        let counts = run(&wg_dir, csv_path.to_str().unwrap(), false, None).unwrap();

        assert_eq!(counts.role_components, 2);
        assert_eq!(counts.desired_outcomes, 1);
        assert_eq!(counts.trade_off_configs, 1);

        // Manifest should exist
        let mp = manifest_path(&wg_dir);
        assert!(mp.exists(), "Manifest should be written after import");

        let manifest: ImportManifest =
            serde_yaml::from_str(&std::fs::read_to_string(&mp).unwrap()).unwrap();
        assert_eq!(manifest.counts.role_components, 2);
        assert_eq!(manifest.counts.desired_outcomes, 1);
        assert_eq!(manifest.counts.trade_off_configs, 1);
        assert!(!manifest.content_hash.is_empty());
    }

    #[test]
    fn test_import_dry_run_no_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv_path = write_agency_format_csv(tmp.path());
        run(&wg_dir, csv_path.to_str().unwrap(), true, None).unwrap();

        let mp = manifest_path(&wg_dir);
        assert!(
            !mp.exists(),
            "Manifest should NOT be written on dry run"
        );
    }

    #[test]
    fn test_reimport_updates_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv_path = write_agency_format_csv(tmp.path());

        // First import
        run(&wg_dir, csv_path.to_str().unwrap(), false, None).unwrap();
        let mp = manifest_path(&wg_dir);
        let manifest1: ImportManifest =
            serde_yaml::from_str(&std::fs::read_to_string(&mp).unwrap()).unwrap();

        // Re-import (idempotent — same content hash)
        run(&wg_dir, csv_path.to_str().unwrap(), false, None).unwrap();
        let manifest2: ImportManifest =
            serde_yaml::from_str(&std::fs::read_to_string(&mp).unwrap()).unwrap();

        assert_eq!(manifest1.content_hash, manifest2.content_hash);
        assert_eq!(manifest1.counts.role_components, manifest2.counts.role_components);
    }

    // --- Tests for the new URL/upstream import functionality ---

    #[test]
    fn test_agency_pull_run_from_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv = b"type,name,description,quality,domain_specificity,domain,origin_instance_id,parent_content_hash,scope\n\
                     role_component,Test Skill,Does testing,80,high,testing,inst-001,,task\n";

        let counts = run_from_bytes(&wg_dir, "test://fixture.csv", csv, false, None).unwrap();
        assert_eq!(counts.role_components, 1);
        assert_eq!(counts.desired_outcomes, 0);
        assert_eq!(counts.trade_off_configs, 0);

        // Verify manifest was written with source URL
        let manifest = read_manifest(&wg_dir).unwrap().unwrap();
        assert_eq!(manifest.source, "test://fixture.csv");
        assert_eq!(manifest.counts.role_components, 1);
    }

    #[test]
    fn test_agency_pull_read_manifest_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let result = read_manifest(&wg_dir).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_agency_pull_read_manifest_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv_path = write_agency_format_csv(tmp.path());
        run(&wg_dir, csv_path.to_str().unwrap(), false, None).unwrap();

        let manifest = read_manifest(&wg_dir).unwrap().unwrap();
        assert!(!manifest.content_hash.is_empty());
        assert_eq!(manifest.counts.role_components, 2);
    }

    #[test]
    fn test_agency_pull_change_detection_same_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv = b"type,name,description,quality,domain_specificity,domain,origin_instance_id,parent_content_hash,scope\n\
                     role_component,Detect,Detection test,75,low,test,inst-001,,task\n";

        // First import writes manifest
        let counts1 = run_from_bytes(&wg_dir, "test://same.csv", csv, false, None).unwrap();
        assert_eq!(counts1.role_components, 1);

        let manifest = read_manifest(&wg_dir).unwrap().unwrap();
        let hash = sha256_hex(csv);
        assert_eq!(manifest.content_hash, hash);
    }

    #[test]
    fn test_agency_pull_change_detection_different_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv1 = b"type,name,description,quality,domain_specificity,domain,origin_instance_id,parent_content_hash,scope\n\
                      role_component,V1,Version one,75,low,test,inst-001,,task\n";
        let csv2 = b"type,name,description,quality,domain_specificity,domain,origin_instance_id,parent_content_hash,scope\n\
                      role_component,V1,Version one,75,low,test,inst-001,,task\n\
                      role_component,V2,Version two,80,medium,test,inst-002,,task\n";

        run_from_bytes(&wg_dir, "test://v1.csv", csv1, false, None).unwrap();
        let m1 = read_manifest(&wg_dir).unwrap().unwrap();

        run_from_bytes(&wg_dir, "test://v2.csv", csv2, false, None).unwrap();
        let m2 = read_manifest(&wg_dir).unwrap().unwrap();

        assert_ne!(m1.content_hash, m2.content_hash);
        assert_eq!(m2.counts.role_components, 2);
    }

    #[test]
    fn test_agency_pull_import_from_local_via_run_import() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv_path = write_agency_format_csv(tmp.path());

        let opts = ImportOptions {
            csv_path: Some(csv_path.to_str().unwrap().to_string()),
            url: None,
            upstream: false,
            dry_run: false,
            tag: None,
            force: false,
            check: false,
        };
        let counts = run_import(&wg_dir, opts).unwrap();
        assert_eq!(counts.role_components, 2);
        assert_eq!(counts.desired_outcomes, 1);
        assert_eq!(counts.trade_off_configs, 1);
    }

    #[test]
    fn test_agency_pull_error_multiple_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let opts = ImportOptions {
            csv_path: Some("file.csv".to_string()),
            url: Some("http://example.com/file.csv".to_string()),
            upstream: false,
            dry_run: false,
            tag: None,
            force: false,
            check: false,
        };
        let result = run_import(&wg_dir, opts);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Specify only one"));
    }

    #[test]
    fn test_agency_pull_error_no_source() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let opts = ImportOptions {
            csv_path: None,
            url: None,
            upstream: false,
            dry_run: false,
            tag: None,
            force: false,
            check: false,
        };
        let result = run_import(&wg_dir, opts);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Specify one of"));
    }

    #[test]
    fn test_agency_pull_upstream_no_config() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let opts = ImportOptions {
            csv_path: None,
            url: None,
            upstream: true,
            dry_run: false,
            tag: None,
            force: false,
            check: false,
        };
        let result = run_import(&wg_dir, opts);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No upstream URL configured"));
    }

    #[test]
    fn test_agency_pull_url_network_error_graceful() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        // Use a URL that will fail to connect (invalid host)
        let opts = ImportOptions {
            csv_path: None,
            url: Some("http://192.0.2.1:1/nonexistent.csv".to_string()),
            upstream: false,
            dry_run: false,
            tag: None,
            force: false,
            check: false,
        };
        let result = run_import(&wg_dir, opts);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to fetch") || err_msg.contains("error"),
            "Error should describe network failure, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_agency_pull_run_from_bytes_dry_run() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv = b"type,name,description,quality,domain_specificity,domain,origin_instance_id,parent_content_hash,scope\n\
                     role_component,Dry,Dry run test,80,high,testing,inst-001,,task\n";

        let counts = run_from_bytes(&wg_dir, "test://dry.csv", csv, true, None).unwrap();
        assert_eq!(counts.role_components, 1);

        // No manifest should be written
        assert!(read_manifest(&wg_dir).unwrap().is_none());
        // No agency directory should be created
        assert!(!wg_dir.join("agency/primitives/components").exists());
    }

    #[test]
    fn test_agency_pull_run_from_bytes_with_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv = b"type,name,description,quality,domain_specificity,domain,origin_instance_id,parent_content_hash,scope\n\
                     role_component,Tagged,Tagged import,80,high,testing,inst-001,,task\n";

        run_from_bytes(&wg_dir, "test://tagged.csv", csv, false, Some("custom-tag")).unwrap();

        // Verify component was saved with custom tag provenance
        let components_dir = wg_dir.join("agency/primitives/components");
        let entries: Vec<_> = std::fs::read_dir(&components_dir).unwrap().collect();
        assert_eq!(entries.len(), 1);
        let component: RoleComponent =
            agency::load_component(&entries[0].as_ref().unwrap().path()).unwrap();
        assert!(component.lineage.created_by.starts_with("custom-tag"));
    }

    #[test]
    fn test_agency_pull_additive_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv1 = b"type,name,description,quality,domain_specificity,domain,origin_instance_id,parent_content_hash,scope\n\
                      role_component,First,First component,80,high,testing,inst-001,,task\n";
        let csv2 = b"type,name,description,quality,domain_specificity,domain,origin_instance_id,parent_content_hash,scope\n\
                      role_component,Second,Second component,85,high,testing,inst-002,,task\n";

        run_from_bytes(&wg_dir, "test://v1.csv", csv1, false, None).unwrap();
        let count1 = std::fs::read_dir(wg_dir.join("agency/primitives/components"))
            .unwrap()
            .count();
        assert_eq!(count1, 1);

        run_from_bytes(&wg_dir, "test://v2.csv", csv2, false, None).unwrap();
        let count2 = std::fs::read_dir(wg_dir.join("agency/primitives/components"))
            .unwrap()
            .count();
        // Second import should ADD the new component, not remove the first
        assert_eq!(count2, 2);
    }
}
