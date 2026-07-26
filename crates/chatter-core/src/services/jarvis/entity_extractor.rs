//! Entity extraction from messages using LocalAI.
//!
//! Extracts named entities (people, projects, topics, technologies) from
//! message text using the local LLM.

use crate::domain::jarvis::{EntityType, JarvisEntity};
use crate::services::ai::SampleParams;

use super::engine::SharedJarvisEngine;
use crate::local_ai::policy::AiAccess;
use super::error::JarvisError;

/// Entity extractor using local AI.
pub struct EntityExtractor {
    ai: SharedJarvisEngine,
}

/// An extracted entity from a message.
#[derive(Debug, Clone)]
pub struct ExtractedEntity {
    /// Entity type.
    pub entity_type: EntityType,
    /// Entity name.
    pub name: String,
    /// Context around the entity mention.
    pub context: Option<String>,
}

impl ExtractedEntity {
    /// Convert to a JarvisEntity.
    pub fn to_jarvis_entity(&self) -> JarvisEntity {
        JarvisEntity::new(self.entity_type, self.name.clone())
    }
}

impl EntityExtractor {
    /// Create a new entity extractor.
    pub fn new(ai: SharedJarvisEngine) -> Self {
        Self { ai }
    }

    /// Extract entities from a message.
    ///
    /// Uses the local LLM to identify named entities in the text.
    pub async fn extract(
        &self,
        access: AiAccess,
        message: &str,
    ) -> Result<Vec<ExtractedEntity>, JarvisError> {
        if message.trim().is_empty() {
            return Ok(vec![]);
        }

        // Skip very short messages (unlikely to contain meaningful entities)
        if message.len() < 10 {
            return Ok(vec![]);
        }

        let prompt = Self::build_extraction_prompt(message);

        let params = SampleParams {
            max_tokens: 256,
            temperature: 0.1, // Low temperature for more deterministic extraction
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.0,
            disable_thinking: true,
            json_schema: None,
        };

        let response = self.ai.complete_gated(access, prompt, params).await?;
        Self::parse_extraction_response(&response)
    }

    /// Build the entity extraction prompt.
    fn build_extraction_prompt(message: &str) -> String {
        format!(
            r#"Extract named entities from this message. Return a JSON array.
Categories: person, project, topic, technology, organization

Message: "{}"

Return format (ONLY valid JSON, no other text):
[{{"type": "person", "name": "...", "context": "..."}}]

If no entities found, return: []
JSON:"#,
            message.replace('"', "\\\"")
        )
    }

    /// Parse the LLM response into extracted entities.
    fn parse_extraction_response(response: &str) -> Result<Vec<ExtractedEntity>, JarvisError> {
        // Try to extract JSON array from response
        let json_str = Self::extract_json_array(response);

        match serde_json::from_str::<Vec<serde_json::Value>>(&json_str) {
            Ok(entities) => {
                let mut result = Vec::new();

                for entity in entities {
                    if let (Some(type_str), Some(name)) = (
                        entity.get("type").and_then(|v| v.as_str()),
                        entity.get("name").and_then(|v| v.as_str()),
                    ) {
                        // Skip empty names
                        if name.trim().is_empty() {
                            continue;
                        }

                        let entity_type = EntityType::parse(type_str);
                        let context = entity
                            .get("context")
                            .and_then(|v| v.as_str())
                            .map(String::from);

                        result.push(ExtractedEntity {
                            entity_type,
                            name: name.trim().to_string(),
                            context,
                        });
                    }
                }

                Ok(result)
            }
            Err(_) => {
                // If parsing fails, try to extract entities manually
                Ok(Self::fallback_extraction(response))
            }
        }
    }

    /// Extract JSON array from response text.
    fn extract_json_array(response: &str) -> String {
        // Try to find JSON array in response
        if let Some(start) = response.find('[') {
            if let Some(end) = response.rfind(']') {
                if end > start {
                    return response[start..=end].to_string();
                }
            }
        }

        // Default to empty array
        "[]".to_string()
    }

    /// Fallback extraction when JSON parsing fails.
    ///
    /// Uses simple heuristics to extract potential entities.
    fn fallback_extraction(response: &str) -> Vec<ExtractedEntity> {
        let mut entities = Vec::new();

        // Look for capitalized words as potential names/entities
        let words: Vec<&str> = response.split_whitespace().collect();
        for word in words {
            // Simple heuristic: capitalized word that's not at sentence start
            if word.len() > 2
                && word.chars().next().is_some_and(|c| c.is_uppercase())
                && !word.chars().all(|c| c.is_uppercase())
            {
                let name = word.trim_matches(|c: char| !c.is_alphanumeric());
                if !name.is_empty() && name.len() > 2 {
                    entities.push(ExtractedEntity {
                        entity_type: EntityType::Topic, // Default to topic
                        name: name.to_string(),
                        context: None,
                    });
                }
            }
        }

        // Deduplicate
        entities.sort_by_key(|a| a.name.to_lowercase());
        entities.dedup_by(|a, b| a.name.to_lowercase() == b.name.to_lowercase());

        entities
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_json() {
        let response = r#"[{"type": "person", "name": "Alice", "context": "mentioned Alice"}]"#;
        let entities = EntityExtractor::parse_extraction_response(response).unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].name, "Alice");
        assert_eq!(entities[0].entity_type, EntityType::Person);
    }

    #[test]
    fn test_parse_multiple_entities() {
        let response = r#"[
            {"type": "person", "name": "Bob"},
            {"type": "project", "name": "Chatter"},
            {"type": "technology", "name": "Rust"}
        ]"#;
        let entities = EntityExtractor::parse_extraction_response(response).unwrap();
        assert_eq!(entities.len(), 3);
    }

    #[test]
    fn test_parse_empty_array() {
        let response = "[]";
        let entities = EntityExtractor::parse_extraction_response(response).unwrap();
        assert!(entities.is_empty());
    }

    #[test]
    fn test_extract_json_from_text() {
        let response = "Here are the entities: [{\"type\": \"person\", \"name\": \"Alice\"}]";
        let json = EntityExtractor::extract_json_array(response);
        assert!(json.starts_with('['));
        assert!(json.ends_with(']'));
    }

    #[test]
    fn test_fallback_extraction() {
        let response = "Found entities: Alice, Rust, Chatter";
        let entities = EntityExtractor::fallback_extraction(response);
        // Should find capitalized words
        assert!(!entities.is_empty());
    }
}
