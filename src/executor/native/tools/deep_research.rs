//! Deep research: decompose → fan out → synthesize.
//!
//! One tool call, one comprehensive answer. Internally:
//!
//! 1. **Decompose.** One LLM call turns a complex question into
//!    3-7 focused sub-questions. ("What's the full publication
//!    timeline of X's work on Y?" → "Who is X and where do they
//!    work?" / "List of X's publications on Y" / "Timeline of Y
//!    research milestones" / ...)
//! 2. **Research each.** For every sub-question, run the full
//!    `research` pipeline (search + fetch + summarize). Sub-queries
//!    are cheaper than a shotgun of variant queries on the same
//!    question — each one targets a specific gap in the answer.
//! 3. **Synthesize.** One more LLM call reads all the sub-briefs
//!    and produces a single cohesive answer to the original
//!    question, citing sources.
//!
//! Reduces query count by 10-100x for complex research questions:
//! instead of the model firing 50 variants of the same query as
//! it stumbles toward an answer, a planner up front decomposes
//! into the actual sub-questions and fans out once.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolOutput};
use crate::executor::native::client::{
    ContentBlock, Message, MessagesRequest, Role, ToolDefinition,
};

/// Hard cap on the number of sub-questions — keeps latency and
/// search-backend politeness bounded even if the decomposer is
/// enthusiastic.
const MAX_SUB_QUESTIONS: usize = 7;

/// Lower bound. One sub-question is fine — sometimes the question
/// is already focused enough and the decomposer just echoes it.
const MIN_SUB_QUESTIONS: usize = 1;

/// Cap on each sub-brief's length when feeding into the synthesis
/// prompt. Keeps the synthesis call from blowing its context budget
/// when several pages had a lot to say.
const MAX_BRIEF_CHARS_FOR_SYNTH: usize = 6_000;

/// Cap on the final synthesized answer length.
const MAX_OUTPUT_TOKENS: u32 = 4_096;

pub fn register_deep_research_tool(registry: &mut super::ToolRegistry, workgraph_dir: PathBuf) {
    registry.register(Box::new(DeepResearchTool { workgraph_dir }));
}

struct DeepResearchTool {
    workgraph_dir: PathBuf,
}

#[async_trait]
impl Tool for DeepResearchTool {
    fn name(&self) -> &str {
        "deep_research"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "deep_research".to_string(),
            description: "Research a complex question thoroughly. Decomposes the question \
                          into 3-7 specific sub-questions, runs a full research pipeline \
                          (search + fetch + summarize) on each, and synthesizes a single \
                          comprehensive answer with source citations. Use this when the \
                          question has multiple facets or requires cross-referencing \
                          several sources — e.g. 'publication timeline of X', 'how does \
                          Y compare to Z across dimensions A/B/C', 'what's the current \
                          consensus on W'. For a single focused lookup, use `research` \
                          instead; deep_research is slower but far more thorough."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The research question to investigate in depth"
                    },
                    "instruction": {
                        "type": "string",
                        "description": "Optional focus instruction passed to each \
                                        sub-research pipeline. E.g. 'extract publication \
                                        dates', 'focus on methodology'. Defaults to \
                                        a generic relevance prompt."
                    }
                },
                "required": ["question"]
            }),
        }
    }

    async fn execute_streaming(
        &self,
        input: &serde_json::Value,
        on_chunk: super::ToolStreamCallback,
    ) -> ToolOutput {
        // Install the progress callback for the duration of this
        // execute() — any `tool_progress!` call inside nested code
        // paths (decompose, research, synthesize) forwards to the
        // chat transcript and `wg session attach` tails as well as
        // stderr.
        super::progress::scope(
            super::progress::from_tool_stream_callback(on_chunk),
            self.execute(input),
        )
        .await
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let question = match input.get("question").and_then(|v| v.as_str()) {
            Some(q) if !q.trim().is_empty() => q.trim().to_string(),
            _ => {
                return ToolOutput::error(
                    "Missing or empty required parameter: question".to_string(),
                );
            }
        };

        let instruction = input
            .get("instruction")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| {
                format!(
                    "Extract information relevant to: {}. Include specific names, \
                     dates, numbers, and facts.",
                    question
                )
            });

        crate::tool_progress!(
            "\x1b[2m[deep_research] question: {:?}\x1b[0m",
            truncate(&question, 100)
        );

        // Resolve model + provider once — used by both the decompose
        // and synthesize LLM calls. Same resolution path the agent loop
        // uses so both calls hit the configured task_agent model.
        let config = crate::config::Config::load_or_default(&self.workgraph_dir);
        let model = std::env::var("WG_MODEL")
            .ok()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| {
                config
                    .resolve_model_for_role(crate::config::DispatchRole::TaskAgent)
                    .model
            });
        let provider =
            match crate::executor::native::provider::create_provider(&self.workgraph_dir, &model) {
                Ok(p) => p,
                Err(e) => {
                    return ToolOutput::error(format!(
                        "deep_research: failed to create LLM provider (model {}): {}",
                        model, e
                    ));
                }
            };

        // Step 1: decompose.
        let sub_questions = match decompose_question(provider.as_ref(), &question).await {
            Ok(sqs) if !sqs.is_empty() => sqs,
            Ok(_) => {
                // Decomposer returned empty — fall back to treating the
                // original question as its own only sub-question.
                vec![question.clone()]
            }
            Err(e) => {
                crate::tool_progress!(
                    "\x1b[2m[deep_research] decompose failed: {} — falling back to single-query research\x1b[0m",
                    e
                );
                vec![question.clone()]
            }
        };

        crate::tool_progress!(
            "\x1b[2m[deep_research] decomposed into {} sub-question(s)\x1b[0m",
            sub_questions.len()
        );
        for (i, sq) in sub_questions.iter().enumerate() {
            crate::tool_progress!("\x1b[2m  {}. {}\x1b[0m", i + 1, truncate(sq, 100));
        }

        // Step 2: research each. We instantiate ResearchTool directly
        // and call its execute(). Sequential for now — each research
        // call does its own parallel web_search fan-out, and running
        // N research pipelines in parallel would multiply that load.
        let researcher = super::research::ResearchTool {
            workgraph_dir: self.workgraph_dir.clone(),
        };
        let mut briefs: Vec<(String, String)> = Vec::new(); // (sub_q, brief)
        for (i, sq) in sub_questions.iter().enumerate() {
            crate::tool_progress!(
                "\x1b[2m[deep_research] ({}/{}) researching: {:?}\x1b[0m",
                i + 1,
                sub_questions.len(),
                truncate(sq, 80)
            );
            let research_input = json!({
                "query": sq,
                "instruction": instruction,
            });
            let output = researcher.execute(&research_input).await;
            // Tool outputs have .content; we accept success or error
            // transparently — a failed sub-query shouldn't kill the
            // whole deep_research. The synth step will see the error
            // blurb and handle it.
            briefs.push((sq.clone(), output.content));
        }

        // Step 3: synthesize.
        crate::tool_progress!("\x1b[2m[deep_research] synthesizing\x1b[0m");
        match synthesize(provider.as_ref(), &question, &briefs).await {
            Ok(answer) => ToolOutput::success(answer),
            Err(e) => {
                // Synthesis failed — return the raw briefs so the model
                // can still read them. Partial-credit is better than a
                // hard error that discards the research.
                let fallback = format_raw_briefs(&question, &briefs);
                ToolOutput::success(format!(
                    "{}\n\n---\n[deep_research] synthesis step failed ({}); \
                     showing raw sub-research briefs above so the answer \
                     isn't lost.",
                    fallback, e
                ))
            }
        }
    }
}

/// Ask the LLM to break `question` into a small list of specific,
/// searchable sub-questions. Returns them as a Vec of strings.
///
/// The prompt asks for JSON so parsing is deterministic. If the LLM
/// returns something else (small models often wrap their answer in
/// markdown fences), we strip those and try again.
async fn decompose_question(
    provider: &dyn crate::executor::native::provider::Provider,
    question: &str,
) -> Result<Vec<String>, String> {
    let prompt = format!(
        "Break this research question into {min}-{max} specific, searchable sub-questions. \
         Each sub-question must be answerable by searching the web — concrete enough that \
         a web search will return relevant results. Cover distinct facets of the original \
         question; avoid rephrasing the same thing multiple times.\n\
         \n\
         Return ONLY a JSON object of the form: \
         {{\"sub_questions\": [\"...\", \"...\"]}}. No markdown fences, no commentary.\n\
         \n\
         Question: {q}",
        min = MIN_SUB_QUESTIONS,
        max = MAX_SUB_QUESTIONS,
        q = question,
    );

    let request = MessagesRequest {
        model: provider.model().to_string(),
        max_tokens: 1024,
        system: Some(
            "You decompose research questions into searchable sub-questions. \
             Output is JSON only — no prose, no fences."
                .to_string(),
        ),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: prompt }],
        }],
        tools: vec![],
        stream: false,
    };

    // 120s timeout on the decompose LLM call. Prevents deep_research
    // from hanging on a dropped connection (no chunks arrive before
    // non-streaming completion).
    let response =
        match tokio::time::timeout(std::time::Duration::from_secs(120), provider.send(&request))
            .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(format!("decompose API call: {}", e)),
            Err(_) => return Err("decompose API call timed out after 120s".to_string()),
        };

    let text: String = response
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    parse_sub_questions(&text)
}

/// Parse a JSON object of the form `{"sub_questions": ["...", ...]}`
/// from a model response. Tolerates markdown fences and leading prose.
pub(crate) fn parse_sub_questions(text: &str) -> Result<Vec<String>, String> {
    // Strip common wrappers the model might add.
    let cleaned = text.trim();
    let cleaned = cleaned
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    // Find the first `{` and the matching `}` — models sometimes lead
    // with prose before the JSON despite the system prompt.
    let start = cleaned
        .find('{')
        .ok_or_else(|| "no JSON object in decompose response".to_string())?;
    let end = cleaned
        .rfind('}')
        .ok_or_else(|| "no closing brace in decompose response".to_string())?;
    if end <= start {
        return Err("malformed JSON braces in decompose response".to_string());
    }
    let json_slice = &cleaned[start..=end];

    let v: serde_json::Value =
        serde_json::from_str(json_slice).map_err(|e| format!("parse decompose JSON: {}", e))?;
    let arr = v
        .get("sub_questions")
        .and_then(|a| a.as_array())
        .ok_or_else(|| "missing sub_questions array".to_string())?;

    let mut out = Vec::new();
    for item in arr {
        if let Some(s) = item.as_str() {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
        if out.len() >= MAX_SUB_QUESTIONS {
            break;
        }
    }
    Ok(out)
}

/// Synthesize a final answer from the per-sub-question briefs.
async fn synthesize(
    provider: &dyn crate::executor::native::provider::Provider,
    question: &str,
    briefs: &[(String, String)],
) -> Result<String, String> {
    // Concat the briefs, truncating each to keep context bounded.
    let mut stitched = String::new();
    for (i, (sq, brief)) in briefs.iter().enumerate() {
        stitched.push_str(&format!("### Sub-question {}: {}\n\n", i + 1, sq));
        stitched.push_str(truncate(brief, MAX_BRIEF_CHARS_FOR_SYNTH));
        stitched.push_str("\n\n");
    }

    let prompt = format!(
        "You are synthesizing a comprehensive answer from multiple research briefs.\n\
         \n\
         Original question: {q}\n\
         \n\
         Below are research briefs, each answering one sub-question. Use them to compose \
         a single cohesive answer to the original question. Cite sources by URL or \
         publication name when the briefs include them. If the briefs contradict each \
         other, say so explicitly. If a specific piece of information isn't in the \
         briefs, say 'not found in the research' rather than guessing.\n\
         \n\
         Research briefs:\n\
         \n\
         {s}",
        q = question,
        s = stitched,
    );

    let request = MessagesRequest {
        model: provider.model().to_string(),
        max_tokens: MAX_OUTPUT_TOKENS,
        system: Some(
            "You synthesize research briefs into comprehensive, source-cited answers. \
             Only use information that appears in the provided briefs; never fabricate."
                .to_string(),
        ),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: prompt }],
        }],
        tools: vec![],
        stream: false,
    };

    // 180s timeout on synthesis. Synthesis processes multiple briefs
    // (up to MAX_SUB_QUESTIONS=7 of them) so it can legitimately take
    // longer than decompose. 180s caps the hang case.
    let response =
        match tokio::time::timeout(std::time::Duration::from_secs(180), provider.send(&request))
            .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(format!("synthesize API call: {}", e)),
            Err(_) => return Err("synthesize API call timed out after 180s".to_string()),
        };

    let text: String = response
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    if text.trim().is_empty() {
        return Err("empty synthesis response".to_string());
    }
    Ok(text)
}

/// Fallback output when synthesis fails. Concatenates raw briefs under
/// their sub-question headers so the model still sees the research.
fn format_raw_briefs(question: &str, briefs: &[(String, String)]) -> String {
    let mut s = String::new();
    s.push_str(&format!("Deep research on: {}\n\n", question));
    for (i, (sq, brief)) in briefs.iter().enumerate() {
        s.push_str(&format!("## Sub-question {}: {}\n\n", i + 1, sq));
        s.push_str(truncate(brief, MAX_BRIEF_CHARS_FOR_SYNTH));
        s.push_str("\n\n");
    }
    s
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..s.floor_char_boundary(max)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sub_questions_plain_json() {
        let text = r#"{"sub_questions": ["Who is X?", "What is Y?", "When did Z?"]}"#;
        let got = parse_sub_questions(text).unwrap();
        assert_eq!(got, vec!["Who is X?", "What is Y?", "When did Z?"]);
    }

    #[test]
    fn parse_sub_questions_strips_markdown_fences() {
        let text = "```json\n{\"sub_questions\": [\"A?\", \"B?\"]}\n```";
        let got = parse_sub_questions(text).unwrap();
        assert_eq!(got, vec!["A?", "B?"]);
    }

    #[test]
    fn parse_sub_questions_tolerates_leading_prose() {
        let text = "Sure, here you go:\n{\"sub_questions\": [\"Q1?\"]}";
        let got = parse_sub_questions(text).unwrap();
        assert_eq!(got, vec!["Q1?"]);
    }

    #[test]
    fn parse_sub_questions_caps_at_max() {
        // 10 items in input — parser should return at most MAX_SUB_QUESTIONS (7)
        let items: Vec<String> = (0..10).map(|i| format!("\"Q{}?\"", i)).collect();
        let text = format!("{{\"sub_questions\": [{}]}}", items.join(","));
        let got = parse_sub_questions(&text).unwrap();
        assert_eq!(got.len(), MAX_SUB_QUESTIONS);
        assert_eq!(got[0], "Q0?");
        assert_eq!(got[6], "Q6?");
    }

    #[test]
    fn parse_sub_questions_skips_blanks() {
        let text = r#"{"sub_questions": ["real", "", "  ", "also real"]}"#;
        let got = parse_sub_questions(text).unwrap();
        assert_eq!(got, vec!["real", "also real"]);
    }

    #[test]
    fn parse_sub_questions_errors_on_missing_field() {
        let text = r#"{"questions": ["oops"]}"#;
        assert!(parse_sub_questions(text).is_err());
    }

    #[test]
    fn parse_sub_questions_errors_on_non_json() {
        assert!(parse_sub_questions("not json at all").is_err());
    }
}
