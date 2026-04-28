//! HTTP bridge for communicating with Agency's evaluation and assignment APIs.

use serde::{Deserialize, Serialize};

use super::store::AgencyError;
use super::types::Evaluation;
use crate::config::AgencyConfig;
use std::fs;

/// Known dimension names in evaluation order.
const DIMENSION_NAMES: &[&str] = &[
    "correctness",
    "completeness",
    "efficiency",
    "style_adherence",
    "downstream_usability",
    "coordination_overhead",
    "blocking_impact",
];

/// Encode an evaluation's dimensional scores into a structured text line.
///
/// Format: `[WG-EVAL] correctness=0.85 completeness=0.90 ...`
/// Dimensions not present in the evaluation are omitted.
pub fn serialize_dimensional_scores(evaluation: &Evaluation) -> String {
    let mut parts = Vec::new();
    for &name in DIMENSION_NAMES {
        if let Some(&score) = evaluation.dimensions.get(name) {
            parts.push(format!("{}={:.2}", name, score));
        }
    }
    // Include any extra dimensions not in the canonical list
    for (name, &score) in &evaluation.dimensions {
        if !DIMENSION_NAMES.contains(&name.as_str()) {
            parts.push(format!("{}={:.2}", name, score));
        }
    }
    format!("[WG-EVAL] {}", parts.join(" "))
}

/// POST evaluation results to Agency's API.
///
/// Graceful degradation: returns `Ok(())` when:
/// - `agency_server_url` is not configured
/// - `agency_token_path` is not configured or the file cannot be read
/// - The server is unreachable or returns an error
///
/// Only returns `Err` for internal serialization failures (which shouldn't happen).
pub fn post_evaluation_to_agency(
    evaluation: &Evaluation,
    agency_task_id: &str,
    config: &AgencyConfig,
) -> Result<(), AgencyError> {
    let server_url = match &config.agency_server_url {
        Some(url) if !url.is_empty() => url,
        _ => return Ok(()),
    };

    let token = match &config.agency_token_path {
        Some(path) => match fs::read_to_string(path) {
            Ok(t) => t.trim().to_string(),
            Err(e) => {
                eprintln!(
                    "Warning: could not read agency token from '{}': {}",
                    path, e
                );
                return Ok(());
            }
        },
        None => String::new(),
    };

    let url = format!(
        "{}/tasks/{}/evaluation",
        server_url.trim_end_matches('/'),
        agency_task_id
    );

    let dimensional_scores = serialize_dimensional_scores(evaluation);

    let payload = serde_json::json!({
        "score": evaluation.score,
        "dimensions": evaluation.dimensions,
        "dimensional_text": dimensional_scores,
        "notes": evaluation.notes,
        "evaluator": evaluation.evaluator,
        "source": evaluation.source,
        "timestamp": evaluation.timestamp,
        "task_id": evaluation.task_id,
        "agent_id": evaluation.agent_id,
    });

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build();

    let client = match client {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "Warning: could not build HTTP client for agency bridge: {}",
                e
            );
            return Ok(());
        }
    };

    let mut request = client.post(&url).json(&payload);
    if !token.is_empty() {
        request = request.bearer_auth(&token);
    }

    match request.send() {
        Ok(resp) => {
            if !resp.status().is_success() {
                eprintln!(
                    "Warning: agency bridge POST to {} returned status {}",
                    url,
                    resp.status()
                );
            }
        }
        Err(e) => {
            eprintln!("Warning: agency bridge POST to {} failed: {}", url, e);
        }
    }

    Ok(())
}

/// Response from Agency's assignment endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgencyAssignmentResponse {
    pub rendered_prompt: String,
    pub agency_task_id: String,
    pub primitive_ids: Vec<String>,
}

/// Request assignment from the Agency server.
///
/// POSTs to `{agency_server_url}/projects/{project_id}/assign` with the
/// task title and description. Returns the rendered prompt, Agency's task
/// ID, and the primitive IDs used in the composition.
///
/// Returns `Err` on connection failure or missing configuration — the
/// caller is responsible for fallback to native assignment.
pub fn request_agency_assignment(
    title: &str,
    description: &str,
    config: &AgencyConfig,
) -> Result<AgencyAssignmentResponse, AgencyError> {
    let server_url = config
        .agency_server_url
        .as_deref()
        .filter(|u| !u.is_empty())
        .ok_or_else(|| AgencyError::NotFound("agency_server_url not configured".to_string()))?;

    let project_id = config
        .agency_project_id
        .as_deref()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| AgencyError::NotFound("agency_project_id not configured".to_string()))?;

    let token = match &config.agency_token_path {
        Some(path) => match fs::read_to_string(path) {
            Ok(t) => t.trim().to_string(),
            Err(e) => {
                return Err(AgencyError::Io(std::io::Error::other(format!(
                    "could not read agency token from '{}': {}",
                    path, e
                ))));
            }
        },
        None => String::new(),
    };

    let url = format!(
        "{}/projects/{}/assign",
        server_url.trim_end_matches('/'),
        project_id
    );

    let payload = serde_json::json!({
        "title": title,
        "description": description,
    });

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| {
            AgencyError::Io(std::io::Error::other(format!(
                "could not build HTTP client: {}",
                e
            )))
        })?;

    let mut request = client.post(&url).json(&payload);
    if !token.is_empty() {
        request = request.bearer_auth(&token);
    }

    let resp = request.send().map_err(|e| {
        AgencyError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            format!("agency assignment request to {} failed: {}", url, e),
        ))
    })?;

    if !resp.status().is_success() {
        return Err(AgencyError::Io(std::io::Error::other(format!(
            "agency assignment request to {} returned status {}",
            url,
            resp.status()
        ))));
    }

    let body = resp.text().map_err(|e| {
        AgencyError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("could not read agency assignment response body: {}", e),
        ))
    })?;

    let response: AgencyAssignmentResponse = serde_json::from_str(&body)?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn sample_evaluation(dimensions: HashMap<String, f64>) -> Evaluation {
        Evaluation {
            id: "eval-test".into(),
            task_id: "task-1".into(),
            agent_id: "agent-abc".into(),
            role_id: "role-1".into(),
            tradeoff_id: "tradeoff-1".into(),
            score: 0.85,
            dimensions,
            notes: "Good work".into(),
            evaluator: "test-evaluator".into(),
            timestamp: "2025-06-01T12:00:00Z".into(),
            model: None,
            source: "llm".to_string(),
            loop_iteration: 0,
        }
    }

    #[test]
    fn test_agency_bridge_serialize_dimensional_scores_full() {
        let mut dims = HashMap::new();
        dims.insert("correctness".to_string(), 0.85);
        dims.insert("completeness".to_string(), 0.90);
        dims.insert("efficiency".to_string(), 0.75);
        dims.insert("style_adherence".to_string(), 0.80);
        dims.insert("downstream_usability".to_string(), 0.70);
        dims.insert("coordination_overhead".to_string(), 0.95);
        dims.insert("blocking_impact".to_string(), 0.60);

        let eval = sample_evaluation(dims);
        let result = serialize_dimensional_scores(&eval);

        assert!(result.starts_with("[WG-EVAL] "));
        assert!(result.contains("correctness=0.85"));
        assert!(result.contains("completeness=0.90"));
        assert!(result.contains("efficiency=0.75"));
        assert!(result.contains("style_adherence=0.80"));
        assert!(result.contains("downstream_usability=0.70"));
        assert!(result.contains("coordination_overhead=0.95"));
        assert!(result.contains("blocking_impact=0.60"));
    }

    #[test]
    fn test_agency_bridge_serialize_dimensional_scores_partial() {
        let mut dims = HashMap::new();
        dims.insert("correctness".to_string(), 0.85);
        dims.insert("completeness".to_string(), 0.90);

        let eval = sample_evaluation(dims);
        let result = serialize_dimensional_scores(&eval);

        assert!(result.starts_with("[WG-EVAL] "));
        assert!(result.contains("correctness=0.85"));
        assert!(result.contains("completeness=0.90"));
        assert!(!result.contains("efficiency="));
    }

    #[test]
    fn test_agency_bridge_serialize_dimensional_scores_empty() {
        let eval = sample_evaluation(HashMap::new());
        let result = serialize_dimensional_scores(&eval);
        assert_eq!(result, "[WG-EVAL] ");
    }

    #[test]
    fn test_agency_bridge_serialize_dimensional_scores_extra_dimensions() {
        let mut dims = HashMap::new();
        dims.insert("correctness".to_string(), 0.85);
        dims.insert("custom_metric".to_string(), 0.50);

        let eval = sample_evaluation(dims);
        let result = serialize_dimensional_scores(&eval);

        assert!(result.starts_with("[WG-EVAL] "));
        assert!(result.contains("correctness=0.85"));
        assert!(result.contains("custom_metric=0.50"));
    }

    #[test]
    fn test_agency_bridge_post_no_server_url() {
        let eval = sample_evaluation(HashMap::new());
        let config = AgencyConfig::default();
        // agency_server_url is None by default
        let result = post_evaluation_to_agency(&eval, "agency-task-1", &config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_agency_bridge_post_empty_server_url() {
        let eval = sample_evaluation(HashMap::new());
        let mut config = AgencyConfig::default();
        config.agency_server_url = Some(String::new());
        let result = post_evaluation_to_agency(&eval, "agency-task-1", &config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_agency_bridge_post_unreachable_server() {
        let eval = sample_evaluation(HashMap::new());
        let mut config = AgencyConfig::default();
        // Use a non-routable address that will fail fast
        config.agency_server_url = Some("http://127.0.0.1:1".to_string());
        let result = post_evaluation_to_agency(&eval, "agency-task-1", &config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_agency_bridge_post_bad_token_path() {
        let eval = sample_evaluation(HashMap::new());
        let mut config = AgencyConfig::default();
        config.agency_server_url = Some("http://127.0.0.1:1".to_string());
        config.agency_token_path = Some("/nonexistent/path/to/token".to_string());
        let result = post_evaluation_to_agency(&eval, "agency-task-1", &config);
        // Graceful: returns Ok even though token file doesn't exist
        assert!(result.is_ok());
    }

    // --- Assignment client tests ---

    #[test]
    fn test_agency_assignment_no_server_url() {
        let config = AgencyConfig::default();
        let result = request_agency_assignment("Task", "desc", &config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("agency_server_url"), "error: {}", err);
    }

    #[test]
    fn test_agency_assignment_no_project_id() {
        let mut config = AgencyConfig::default();
        config.agency_server_url = Some("http://127.0.0.1:1".to_string());
        let result = request_agency_assignment("Task", "desc", &config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("agency_project_id"), "error: {}", err);
    }

    #[test]
    fn test_agency_assignment_unreachable_server() {
        let mut config = AgencyConfig::default();
        config.agency_server_url = Some("http://127.0.0.1:1".to_string());
        config.agency_project_id = Some("proj-1".to_string());
        let result = request_agency_assignment("Task", "desc", &config);
        assert!(result.is_err(), "should fail on unreachable server");
    }

    #[test]
    fn test_agency_assignment_response_parse() {
        let json = r#"{
            "rendered_prompt": "You are a Programmer...",
            "agency_task_id": "agency-42",
            "primitive_ids": ["prim-a", "prim-b"]
        }"#;
        let resp: AgencyAssignmentResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.rendered_prompt, "You are a Programmer...");
        assert_eq!(resp.agency_task_id, "agency-42");
        assert_eq!(resp.primitive_ids, vec!["prim-a", "prim-b"]);
    }
}
