//! LLM-powered fact extraction — the memory write path.
//!
//! After each exchange, a background pass asks the model for typed facts
//! about the user from what they said. Extracted facts flow through
//! `JarvisMemoryRepository::upsert_fact`, which consolidates them
//! (ADD / REINFORCE / SUPERSEDE with temporal validity). Runs async off the
//! ask path — extraction latency never delays a chat reply.

use std::sync::Arc;

use crate::domain::jarvis::{JarvisContext, JarvisFact};
use crate::domain::UserId;
use crate::services::ai::SampleParams;
use crate::storage::repositories::JarvisMemoryRepository;

use super::engine::SharedJarvisEngine;
use super::error::JarvisError;

/// One fact extracted by the LLM.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedFact {
    /// snake_case relation, e.g. "is_named", "works_on", "lives_in".
    pub predicate: String,
    pub object: String,
    /// Model-reported certainty, clamped to [0.1, 1.0].
    pub confidence: f64,
}

/// LLM fact extractor + store.
pub struct FactExtractor {
    ai: SharedJarvisEngine,
    memory_repo: Arc<JarvisMemoryRepository>,
}

impl FactExtractor {
    pub fn new(ai: SharedJarvisEngine, memory_repo: Arc<JarvisMemoryRepository>) -> Self {
        Self { ai, memory_repo }
    }

    /// Extract facts from a user message and consolidate them into memory.
    /// Returns how many facts were stored. Designed to be `tokio::spawn`ed.
    pub async fn extract_and_store(
        &self,
        message: &str,
        user_id: &UserId,
        context: &JarvisContext,
    ) -> Result<usize, JarvisError> {
        if message.trim().len() < 8 {
            return Ok(0);
        }

        let prompt = Self::build_prompt(message);
        let params = SampleParams {
            max_tokens: 256,
            temperature: 0.1, // deterministic extraction
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.0,
        };

        let response = self.ai.complete(prompt, params).await?;
        let facts = Self::parse_response(&response);
        let count = facts.len();

        for f in facts {
            let fact = JarvisFact::about_user(user_id, &f.predicate, &f.object)
                .with_confidence(f.confidence)
                .with_source(&context.context_id);
            self.memory_repo.upsert_fact(&fact)?;
        }

        Ok(count)
    }

    fn build_prompt(message: &str) -> String {
        format!(
            r#"Extract durable personal facts about the speaker from this message.
Only facts worth remembering long-term (name, job, projects, preferences,
relationships, location). Ignore small talk, questions, and transient state.

Message: "{}"

Return ONLY a JSON array, no other text. Each item:
{{"predicate": "snake_case_relation", "object": "value", "confidence": 0.0-1.0}}

Examples: {{"predicate": "is_named", "object": "Alex", "confidence": 0.95}},
{{"predicate": "works_on", "object": "the Chatter app", "confidence": 0.9}}

If there are no durable facts, return: []
JSON:"#,
            message.replace('"', "\\\"")
        )
    }

    /// Parse the LLM response; garbage yields an empty list, never an error.
    fn parse_response(response: &str) -> Vec<ExtractedFact> {
        let json_str = match (response.find('['), response.rfind(']')) {
            (Some(start), Some(end)) if end > start => &response[start..=end],
            _ => return Vec::new(),
        };

        let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(json_str) else {
            return Vec::new();
        };

        items
            .into_iter()
            .filter_map(|item| {
                let predicate = item.get("predicate")?.as_str()?.trim().to_lowercase();
                let object = item.get("object")?.as_str()?.trim().to_string();
                if predicate.is_empty() || object.is_empty() || object.len() > 200 {
                    return None;
                }
                let confidence = item
                    .get("confidence")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.7)
                    .clamp(0.1, 1.0);
                Some(ExtractedFact {
                    predicate: predicate.replace(' ', "_"),
                    object,
                    confidence,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_json() {
        let facts = FactExtractor::parse_response(
            r#"[{"predicate": "is_named", "object": "Alex", "confidence": 0.95}]"#,
        );
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].predicate, "is_named");
        assert_eq!(facts[0].object, "Alex");
    }

    #[test]
    fn parses_json_wrapped_in_prose() {
        let facts = FactExtractor::parse_response(
            "Sure! Here are the facts:\n[{\"predicate\": \"lives in\", \"object\": \"Berlin\"}]\nDone.",
        );
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].predicate, "lives_in", "spaces normalized to snake_case");
        assert!((facts[0].confidence - 0.7).abs() < 1e-9, "missing confidence defaults");
    }

    #[test]
    fn garbage_and_empty_yield_nothing() {
        assert!(FactExtractor::parse_response("no json here").is_empty());
        assert!(FactExtractor::parse_response("[]").is_empty());
        assert!(FactExtractor::parse_response("[{\"predicate\": \"\", \"object\": \"x\"}]").is_empty());
    }

    #[test]
    fn confidence_is_clamped() {
        let facts = FactExtractor::parse_response(
            r#"[{"predicate": "p", "object": "o", "confidence": 7.0}]"#,
        );
        assert!((facts[0].confidence - 1.0).abs() < 1e-9);
    }
}
