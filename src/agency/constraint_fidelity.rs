//! Constraint-fidelity lint: detect orchestrator-fabricated constraints in task descriptions.
//!
//! When an orchestrator translates a user message into a task description, it can
//! hallucinate constraints that weren't in the user's words. This module provides
//! a deterministic (non-LLM) lint that:
//!
//! 1. Extracts gating language from a task description ("do NOT", "never", "always",
//!    "leave as draft", "wait for review", etc.)
//! 2. Optionally compares against the user's originating message
//! 3. Scores how many constraints are "anchored" (justified by user words) vs fabricated
//!
//! FLIP doesn't catch fabricated constraints because it measures self-consistency
//! of the description vs agent behavior — both inherit the fabricated constraint,
//! so they look consistent.

use std::sync::LazyLock;

use regex::Regex;

/// A constraint phrase found in a task description.
#[derive(Debug, Clone)]
pub struct ConstraintPhrase {
    /// The matched text.
    pub text: String,
    /// What kind of gating pattern was matched.
    pub pattern_type: &'static str,
    /// Byte offset in the description.
    pub byte_offset: usize,
    /// The surrounding sentence/context for display.
    pub context: String,
}

/// Finding for a single constraint: was it anchored in the user message?
#[derive(Debug, Clone)]
pub struct ConstraintFinding {
    /// The constraint that was found.
    pub constraint: ConstraintPhrase,
    /// Whether the constraint is anchored in the user message.
    pub anchored: bool,
    /// Which anchoring term was found (if any).
    pub anchor_term: Option<String>,
}

/// Result of a constraint-fidelity lint.
#[derive(Debug, Clone)]
pub struct ConstraintFidelityResult {
    /// Score from 0.0 (all constraints fabricated) to 1.0 (all anchored or none found).
    pub score: f64,
    /// Total number of constraint phrases detected.
    pub total_constraints: usize,
    /// How many were anchored in the user message.
    pub anchored_constraints: usize,
    /// How many were NOT anchored.
    pub unanchored_constraints: usize,
    /// Per-constraint findings.
    pub findings: Vec<ConstraintFinding>,
    /// Whether a user message was available for comparison.
    pub has_user_message: bool,
}

// ---------------------------------------------------------------------------
// Pattern definitions
// ---------------------------------------------------------------------------

struct GatingPattern {
    name: &'static str,
    regex: Regex,
}

static GATING_PATTERNS: LazyLock<Vec<GatingPattern>> = LazyLock::new(|| {
    vec![
        GatingPattern {
            name: "prohibition",
            regex: Regex::new(
                r"(?i)\b(do\s+not|don'?t|must\s+not|shall\s+not|should\s+not)\s+\w+",
            )
            .unwrap(),
        },
        GatingPattern {
            name: "absolute_never",
            regex: Regex::new(r"(?i)\bnever\s+\w+").unwrap(),
        },
        GatingPattern {
            name: "gating_action",
            regex: Regex::new(
                r"(?i)\b(leave\s+as\s+draft|wait\s+for\s+review|do\s+not\s+auto[- ]?publish|drafts?\s+only|require\s+approval|hold\s+for\s+\w+|pending\s+review|await\s+confirmation)",
            )
            .unwrap(),
        },
        GatingPattern {
            name: "restrictive_conditional",
            regex: Regex::new(r"(?i)\b(only\s+if|only\s+when|only\s+after|not\s+until)\b")
                .unwrap(),
        },
    ]
});

/// Anchoring concept groups: if the user message contains ANY term from a group,
/// constraints related to that concept are considered anchored.
static ANCHORING_CONCEPTS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    vec![
        (
            "draft_review",
            Regex::new(r"(?i)\b(draft|review|approval|approve|manual\s+review|human\s+review|code\s+review)\b").unwrap(),
        ),
        (
            "gating",
            Regex::new(r"(?i)\b(gate|gating|hold|pause|wait|block|pending|confirm|confirmation)\b").unwrap(),
        ),
        (
            "restriction",
            Regex::new(r"(?i)\b(do\s+not|don'?t|never|must\s+not|shall\s+not|should\s+not|restrict|limit|forbid|prohibit)\b").unwrap(),
        ),
        (
            "publishing",
            Regex::new(r"(?i)\b(publish|auto[- ]?publish|deploy|release|merge|ship|push\s+to\s+prod)\b").unwrap(),
        ),
    ]
});

// ---------------------------------------------------------------------------
// Core logic
// ---------------------------------------------------------------------------

/// Extract constraint phrases from a task description.
pub fn extract_constraints(description: &str) -> Vec<ConstraintPhrase> {
    let mut constraints = Vec::new();

    for pattern in GATING_PATTERNS.iter() {
        for m in pattern.regex.find_iter(description) {
            let context = extract_sentence_context(description, m.start(), m.end());
            constraints.push(ConstraintPhrase {
                text: m.as_str().to_string(),
                pattern_type: pattern.name,
                byte_offset: m.start(),
                context,
            });
        }
    }

    // Deduplicate overlapping matches (keep the longer match)
    constraints.sort_by_key(|c| c.byte_offset);
    let mut deduped: Vec<ConstraintPhrase> = Vec::new();
    for c in constraints {
        if let Some(last) = deduped.last() {
            let last_end = last.byte_offset + last.text.len();
            if c.byte_offset < last_end {
                // Overlapping — keep the longer one
                if c.text.len() > last.text.len() {
                    deduped.pop();
                    deduped.push(c);
                }
                continue;
            }
        }
        deduped.push(c);
    }

    deduped
}

/// Check which anchoring concepts are present in the user message.
fn find_anchoring_concepts(user_message: &str) -> Vec<&'static str> {
    ANCHORING_CONCEPTS
        .iter()
        .filter(|(_, regex)| regex.is_match(user_message))
        .map(|(name, _)| *name)
        .collect()
}

/// Extract the sentence containing a match for context display.
fn extract_sentence_context(text: &str, match_start: usize, match_end: usize) -> String {
    let before = &text[..match_start];
    let after = &text[match_end..];

    let sentence_start = before
        .rfind(|c: char| c == '.' || c == '\n' || c == '!' || c == '?')
        .map(|i| i + 1)
        .unwrap_or(0);

    let sentence_end = after
        .find(|c: char| c == '.' || c == '\n' || c == '!' || c == '?')
        .map(|i| match_end + i + 1)
        .unwrap_or(text.len());

    let start = sentence_start.max(match_start.saturating_sub(80));
    let end = sentence_end.min(match_end + 80);

    let safe_start = text.floor_char_boundary(start);
    let safe_end = text.ceil_char_boundary(end);
    text[safe_start..safe_end].trim().to_string()
}

/// Run the constraint-fidelity lint on a task description.
///
/// When `user_message` is provided, each constraint is checked for anchoring
/// in the user's words. When absent, all constraints are flagged as suspicious
/// (standalone mode).
pub fn lint_task_description(
    description: &str,
    user_message: Option<&str>,
) -> ConstraintFidelityResult {
    let constraints = extract_constraints(description);

    if constraints.is_empty() {
        return ConstraintFidelityResult {
            score: 1.0,
            total_constraints: 0,
            anchored_constraints: 0,
            unanchored_constraints: 0,
            findings: Vec::new(),
            has_user_message: user_message.is_some(),
        };
    }

    let findings: Vec<ConstraintFinding> = if let Some(user_msg) = user_message {
        let user_concepts = find_anchoring_concepts(user_msg);

        constraints
            .into_iter()
            .map(|constraint| {
                // A constraint is anchored if the user message contains ANY
                // gating-related concept. We're generous here — the presence
                // of ANY gating language in the user message gives benefit of
                // the doubt to all constraints.
                let (anchored, anchor_term) = if !user_concepts.is_empty() {
                    // Check if the constraint relates to a concept the user mentioned,
                    // including adjacent concepts (e.g., "draft" anchors "publishing")
                    let constraint_concepts = find_constraint_related_concepts(&constraint);
                    let expanded_user = expand_with_adjacency(&user_concepts);
                    let overlap: Vec<&&str> = expanded_user
                        .iter()
                        .filter(|uc| constraint_concepts.contains(uc))
                        .collect();

                    if !overlap.is_empty() {
                        (true, Some(overlap[0].to_string()))
                    } else if user_concepts
                        .iter()
                        .any(|c| *c == "restriction" || *c == "gating")
                    {
                        (true, Some("generic_gating".to_string()))
                    } else {
                        (false, None)
                    }
                } else {
                    (false, None)
                };

                ConstraintFinding {
                    constraint,
                    anchored,
                    anchor_term,
                }
            })
            .collect()
    } else {
        // No user message — standalone mode, all constraints flagged
        constraints
            .into_iter()
            .map(|constraint| ConstraintFinding {
                constraint,
                anchored: false,
                anchor_term: None,
            })
            .collect()
    };

    let total = findings.len();
    let anchored = findings.iter().filter(|f| f.anchored).count();
    let unanchored = total - anchored;

    let score = if total == 0 {
        1.0
    } else if user_message.is_some() {
        // With user message: proportion of anchored constraints
        anchored as f64 / total as f64
    } else {
        // Without user message: penalize based on constraint count
        // More constraints = more suspicious, but don't go to 0 since
        // we can't confirm they're fabricated without a user message
        (1.0 - 0.15 * unanchored as f64).max(0.1)
    };

    ConstraintFidelityResult {
        score,
        total_constraints: total,
        anchored_constraints: anchored,
        unanchored_constraints: unanchored,
        findings,
        has_user_message: user_message.is_some(),
    }
}

/// Concept adjacency: these pairs are semantically linked, so a user expressing
/// one concept anchors constraints about the adjacent concept.
/// E.g., "leave as draft" (draft_review) anchors "do NOT auto-publish" (publishing).
const ADJACENT_CONCEPTS: &[(&str, &str)] = &[
    ("draft_review", "publishing"),
    ("publishing", "draft_review"),
    ("gating", "restriction"),
    ("restriction", "gating"),
];

/// Expand a set of concepts with adjacent concepts.
fn expand_with_adjacency(concepts: &[&'static str]) -> Vec<&'static str> {
    let mut expanded: Vec<&'static str> = concepts.to_vec();
    for &concept in concepts {
        for &(from, to) in ADJACENT_CONCEPTS {
            if concept == from && !expanded.contains(&to) {
                expanded.push(to);
            }
        }
    }
    expanded
}

/// Determine which anchoring concept groups a constraint phrase relates to.
fn find_constraint_related_concepts(constraint: &ConstraintPhrase) -> Vec<&'static str> {
    let text_lower = constraint.text.to_lowercase();
    let context_lower = constraint.context.to_lowercase();
    let combined = format!("{} {}", text_lower, context_lower);

    let mut concepts = Vec::new();

    if combined.contains("draft")
        || combined.contains("review")
        || combined.contains("approv")
    {
        concepts.push("draft_review");
    }
    if combined.contains("publish")
        || combined.contains("deploy")
        || combined.contains("release")
        || combined.contains("merge")
        || combined.contains("ship")
    {
        concepts.push("publishing");
    }
    if combined.contains("gate")
        || combined.contains("hold")
        || combined.contains("pause")
        || combined.contains("wait")
        || combined.contains("block")
        || combined.contains("confirm")
    {
        concepts.push("gating");
    }
    if combined.contains("not ")
        || combined.contains("never")
        || combined.contains("restrict")
        || combined.contains("limit")
        || combined.contains("forbid")
    {
        concepts.push("restriction");
    }

    concepts
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_constraints_scores_1() {
        let result = lint_task_description("Implement the feature with tests.", None);
        assert!((result.score - 1.0).abs() < f64::EPSILON);
        assert_eq!(result.total_constraints, 0);
    }

    #[test]
    fn test_do_not_auto_publish_flagged_without_user_message() {
        let desc = "Implement the autopoietic cycle. Do NOT auto-publish results.";
        let result = lint_task_description(desc, None);
        assert!(result.total_constraints > 0);
        assert!(result.score < 1.0);
        assert!(result.unanchored_constraints > 0);
    }

    #[test]
    fn test_do_not_auto_publish_flagged_without_anchor() {
        let desc = "Implement the autopoietic cycle. Do NOT auto-publish results.";
        let user_msg = "keep it rolling autonomously";
        let result = lint_task_description(desc, Some(user_msg));
        assert!(result.total_constraints > 0);
        assert_eq!(result.anchored_constraints, 0);
        assert_eq!(result.score, 0.0);
    }

    #[test]
    fn test_do_not_auto_publish_anchored_when_user_says_draft() {
        let desc = "Implement the autopoietic cycle. Do NOT auto-publish results.";
        let user_msg = "leave everything as draft until I review";
        let result = lint_task_description(desc, Some(user_msg));
        assert!(result.total_constraints > 0);
        assert!(result.anchored_constraints > 0);
        assert!(result.score > 0.0);
    }

    #[test]
    fn test_user_says_pause_anchors_gating() {
        let desc = "Hold for review before merging.";
        let user_msg = "pause and wait for me before publishing";
        let result = lint_task_description(desc, Some(user_msg));
        assert!(result.total_constraints > 0, "expected constraints, found none");
        assert!(result.anchored_constraints > 0);
    }

    #[test]
    fn test_never_pattern_detected() {
        let desc = "Never auto-deploy to production.";
        let result = lint_task_description(desc, None);
        assert!(result.total_constraints > 0);
        assert!(
            result.findings.iter().any(|f| f.constraint.pattern_type == "absolute_never"),
            "expected 'absolute_never' pattern"
        );
    }

    #[test]
    fn test_drafts_only_pattern_detected() {
        let desc = "Drafts only — never auto-publish.";
        let result = lint_task_description(desc, None);
        assert!(result.total_constraints >= 1);
        assert!(
            result.findings.iter().any(|f| f.constraint.pattern_type == "gating_action"),
            "expected 'gating_action' pattern"
        );
    }

    #[test]
    fn test_leave_as_draft_pattern_detected() {
        let desc = "Leave as draft and require approval before publishing.";
        let result = lint_task_description(desc, None);
        assert!(result.total_constraints >= 1);
    }

    #[test]
    fn test_multiple_constraints_all_flagged() {
        let desc = "Do NOT auto-publish. Never deploy without approval. Leave as draft.";
        let user_msg = "keep it rolling autonomously";
        let result = lint_task_description(desc, Some(user_msg));
        assert!(result.total_constraints >= 2);
        assert_eq!(result.anchored_constraints, 0);
        assert_eq!(result.score, 0.0);
    }

    #[test]
    fn test_multiple_constraints_all_anchored() {
        let desc = "Do NOT auto-publish. Never deploy without review.";
        let user_msg = "don't publish anything without my review first";
        let result = lint_task_description(desc, Some(user_msg));
        assert!(result.total_constraints >= 1);
        assert!(result.anchored_constraints >= 1);
        assert!(result.score > 0.0);
    }

    #[test]
    fn test_normal_task_description_no_false_positives() {
        let desc = "## Description\n\
                     Implement the auth endpoint.\n\n\
                     ## Validation\n\
                     - [ ] Tests pass\n\
                     - [ ] cargo build succeeds\n\
                     - [ ] Endpoint returns 401 for bad tokens";
        let result = lint_task_description(desc, None);
        assert_eq!(result.total_constraints, 0);
        assert!((result.score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_validation_section_not_flagged() {
        let desc = "## Validation\n\
                     - [ ] cargo test passes\n\
                     - [ ] Implementation matches spec";
        let result = lint_task_description(desc, None);
        assert_eq!(result.total_constraints, 0);
    }

    #[test]
    fn test_only_if_pattern() {
        let desc = "Proceed only if the tests pass and review is approved.";
        let result = lint_task_description(desc, None);
        assert!(result.total_constraints > 0);
    }

    #[test]
    fn test_wait_for_review_pattern() {
        let desc = "Complete the feature but wait for review before merging.";
        let result = lint_task_description(desc, None);
        assert!(result.total_constraints > 0);
    }

    #[test]
    fn test_score_with_user_message_partial_anchoring() {
        let desc = "Do NOT auto-publish. Do NOT modify the database schema.";
        // User mentioned publishing but not database constraints
        let user_msg = "don't publish yet";
        let result = lint_task_description(desc, Some(user_msg));
        assert!(result.total_constraints >= 2);
        // The publish constraint should be anchored, the database one may or may not
        // depending on generic restriction matching
        assert!(result.score > 0.0);
    }

    #[test]
    fn test_standalone_mode_penalty_increases_with_constraints() {
        let one = lint_task_description("Do NOT auto-publish.", None);
        let three = lint_task_description(
            "Do NOT auto-publish. Never deploy. Leave as draft.",
            None,
        );
        assert!(three.score < one.score, "more constraints should lower the standalone score");
    }

    #[test]
    fn test_extract_constraints_deduplicates_overlaps() {
        let desc = "Do not auto-publish results.";
        let constraints = extract_constraints(desc);
        // "Do not auto-publish" should match as one phrase, not multiple overlapping
        assert_eq!(constraints.len(), 1);
    }

    #[test]
    fn test_should_not_match_only_alone() {
        let desc = "Only implement the core feature.";
        let result = lint_task_description(desc, None);
        // "Only implement" should NOT match — we only match "only if/when/after"
        assert_eq!(result.total_constraints, 0);
    }

    #[test]
    fn test_case_insensitivity() {
        let desc = "DO NOT auto-publish. NEVER deploy without review.";
        let result = lint_task_description(desc, None);
        assert!(result.total_constraints >= 2);
    }
}
