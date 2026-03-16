//! Jarvis AI assistant domain types.
//!
//! Domain types for Jarvis memory, facts, and context management.
//! Jarvis remembers conversations, learns facts about users and topics,
//! and tracks entity relationships for context-aware responses.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{ChannelId, UserId, AnonDmId, AnonGroupId};

// ============================================================================
// Identifiers
// ============================================================================

/// Unique identifier for a Jarvis conversation entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JarvisConversationId(String);

impl JarvisConversationId {
    /// Create a new random ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Create from a string.
    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    /// Get the ID as a string reference.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for JarvisConversationId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for JarvisConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a Jarvis fact.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JarvisFactId(String);

impl JarvisFactId {
    /// Create a new random ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Create from a string.
    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    /// Get the ID as a string reference.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for JarvisFactId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for JarvisFactId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a Jarvis summary.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JarvisSummaryId(String);

impl JarvisSummaryId {
    /// Create a new random ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Create from a string.
    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    /// Get the ID as a string reference.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for JarvisSummaryId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for JarvisSummaryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a Jarvis entity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JarvisEntityId(String);

impl JarvisEntityId {
    /// Create a new random ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Create from a string.
    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    /// Get the ID as a string reference.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for JarvisEntityId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for JarvisEntityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a Jarvis relationship.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JarvisRelationshipId(String);

impl JarvisRelationshipId {
    /// Create a new random ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Create from a string.
    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    /// Get the ID as a string reference.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for JarvisRelationshipId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for JarvisRelationshipId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ============================================================================
// Enums
// ============================================================================

/// Context type for Jarvis conversations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(i32)]
pub enum ContextType {
    /// Regular channel.
    #[default]
    Channel = 0,
    /// Direct message.
    Dm = 1,
    /// Anonymous DM.
    AnonDm = 2,
    /// Anonymous group.
    AnonGroup = 3,
}

impl ContextType {
    /// Create from integer value.
    pub fn from_i32(value: i32) -> Self {
        match value {
            1 => Self::Dm,
            2 => Self::AnonDm,
            3 => Self::AnonGroup,
            _ => Self::Channel,
        }
    }

    /// Convert to integer value.
    pub fn to_i32(self) -> i32 {
        self as i32
    }
}

impl From<i32> for ContextType {
    fn from(value: i32) -> Self {
        Self::from_i32(value)
    }
}

/// Role in a Jarvis conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(i32)]
pub enum Role {
    /// User message.
    #[default]
    User = 0,
    /// Assistant (Jarvis) response.
    Assistant = 1,
    /// System message (context, instructions).
    System = 2,
}

impl Role {
    /// Create from integer value.
    pub fn from_i32(value: i32) -> Self {
        match value {
            1 => Self::Assistant,
            2 => Self::System,
            _ => Self::User,
        }
    }

    /// Convert to integer value.
    pub fn to_i32(self) -> i32 {
        self as i32
    }
}

impl From<i32> for Role {
    fn from(value: i32) -> Self {
        Self::from_i32(value)
    }
}

/// Subject type for Jarvis facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(i32)]
pub enum SubjectType {
    /// Fact about a user.
    #[default]
    User = 0,
    /// Fact about a topic.
    Topic = 1,
    /// Fact about a project.
    Project = 2,
    /// Fact about a preference.
    Preference = 3,
}

impl SubjectType {
    /// Create from integer value.
    pub fn from_i32(value: i32) -> Self {
        match value {
            1 => Self::Topic,
            2 => Self::Project,
            3 => Self::Preference,
            _ => Self::User,
        }
    }

    /// Convert to integer value.
    pub fn to_i32(self) -> i32 {
        self as i32
    }
}

impl From<i32> for SubjectType {
    fn from(value: i32) -> Self {
        Self::from_i32(value)
    }
}

/// Entity type for the knowledge graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(i32)]
pub enum EntityType {
    /// A person (user).
    #[default]
    Person = 0,
    /// A project.
    Project = 1,
    /// A topic or concept.
    Topic = 2,
    /// An organization.
    Organization = 3,
    /// A technology (language, framework, tool).
    Technology = 4,
}

impl EntityType {
    /// Create from integer value.
    pub fn from_i32(value: i32) -> Self {
        match value {
            1 => Self::Project,
            2 => Self::Topic,
            3 => Self::Organization,
            4 => Self::Technology,
            _ => Self::Person,
        }
    }

    /// Convert to integer value.
    pub fn to_i32(self) -> i32 {
        self as i32
    }

    /// Parse from a string (used in entity extraction).
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "person" | "user" => Self::Person,
            "project" => Self::Project,
            "topic" | "concept" => Self::Topic,
            "organization" | "org" | "company" => Self::Organization,
            "technology" | "tech" | "framework" | "language" | "tool" => Self::Technology,
            _ => Self::Topic,
        }
    }
}

impl From<i32> for EntityType {
    fn from(value: i32) -> Self {
        Self::from_i32(value)
    }
}

// ============================================================================
// Context Reference
// ============================================================================

/// Reference to a conversation context (channel, DM, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JarvisContext {
    /// The type of context.
    pub context_type: ContextType,
    /// The context identifier (serialized).
    pub context_id: String,
}

impl JarvisContext {
    /// Create a context for a channel.
    pub fn channel(channel_id: ChannelId) -> Self {
        Self {
            context_type: ContextType::Channel,
            context_id: channel_id.to_string(),
        }
    }

    /// Create a context for a DM (uses channel ID).
    pub fn dm(channel_id: ChannelId) -> Self {
        Self {
            context_type: ContextType::Dm,
            context_id: channel_id.to_string(),
        }
    }

    /// Create a context for an anonymous DM.
    pub fn anon_dm(anon_dm_id: AnonDmId) -> Self {
        Self {
            context_type: ContextType::AnonDm,
            context_id: anon_dm_id.to_string(),
        }
    }

    /// Create a context for an anonymous group.
    pub fn anon_group(anon_group_id: AnonGroupId) -> Self {
        Self {
            context_type: ContextType::AnonGroup,
            context_id: anon_group_id.to_string(),
        }
    }

    /// Create a general context (Jarvis panel conversations not tied to a specific channel).
    pub fn general() -> Self {
        Self {
            context_type: ContextType::Channel,
            context_id: uuid::Uuid::nil().to_string(),
        }
    }

    /// Create from raw parts.
    pub fn from_parts(context_type: ContextType, context_id: String) -> Self {
        Self {
            context_type,
            context_id,
        }
    }
}

// ============================================================================
// Domain Types
// ============================================================================

/// A conversation entry in Jarvis memory.
#[derive(Debug, Clone)]
pub struct JarvisConversation {
    /// Unique identifier.
    pub id: JarvisConversationId,
    /// Context type.
    pub context_type: ContextType,
    /// Context identifier.
    pub context_id: String,
    /// Role of this message.
    pub role: Role,
    /// Message content.
    pub content: String,
    /// Timestamp (nanoseconds since epoch).
    pub timestamp: u64,
    /// Optional metadata as JSON.
    pub metadata_json: Option<String>,
    /// When this entry was created.
    pub created_at: DateTime<Utc>,
}

impl JarvisConversation {
    /// Create a new conversation entry.
    pub fn new(
        context: &JarvisContext,
        role: Role,
        content: String,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: JarvisConversationId::new(),
            context_type: context.context_type,
            context_id: context.context_id.clone(),
            role,
            content,
            timestamp: now.timestamp_nanos_opt().unwrap_or(0) as u64,
            metadata_json: None,
            created_at: now,
        }
    }

    /// Create a user message.
    pub fn user_message(context: &JarvisContext, content: String) -> Self {
        Self::new(context, Role::User, content)
    }

    /// Create an assistant message.
    pub fn assistant_message(context: &JarvisContext, content: String) -> Self {
        Self::new(context, Role::Assistant, content)
    }

    /// Create a system message.
    pub fn system_message(context: &JarvisContext, content: String) -> Self {
        Self::new(context, Role::System, content)
    }

    /// Add metadata to the entry.
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata_json = Some(metadata.to_string());
        self
    }

    /// Add author ID to metadata.
    pub fn with_author(mut self, author_id: &UserId) -> Self {
        let metadata = serde_json::json!({
            "author_id": author_id.to_string()
        });
        self.metadata_json = Some(metadata.to_string());
        self
    }
}

/// A fact extracted from conversations.
#[derive(Debug, Clone)]
pub struct JarvisFact {
    /// Unique identifier.
    pub id: JarvisFactId,
    /// Type of the subject.
    pub subject_type: SubjectType,
    /// Subject identifier.
    pub subject_id: String,
    /// Predicate (relationship type).
    pub predicate: String,
    /// Object (the value).
    pub object: String,
    /// Confidence score (0.0 to 1.0).
    pub confidence: f64,
    /// Context where this fact was learned.
    pub source_context_id: Option<String>,
    /// When this fact was last reinforced.
    pub last_reinforced_at: DateTime<Utc>,
    /// Decay rate per day.
    pub decay_rate: f64,
    /// When this fact was first learned.
    pub created_at: DateTime<Utc>,
}

impl JarvisFact {
    /// Create a new fact about a user.
    pub fn about_user(user_id: &UserId, predicate: &str, object: &str) -> Self {
        let now = Utc::now();
        Self {
            id: JarvisFactId::new(),
            subject_type: SubjectType::User,
            subject_id: user_id.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
            confidence: 1.0,
            source_context_id: None,
            last_reinforced_at: now,
            decay_rate: 0.01,
            created_at: now,
        }
    }

    /// Create a new fact about a topic.
    pub fn about_topic(topic: &str, predicate: &str, object: &str) -> Self {
        let now = Utc::now();
        Self {
            id: JarvisFactId::new(),
            subject_type: SubjectType::Topic,
            subject_id: topic.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
            confidence: 1.0,
            source_context_id: None,
            last_reinforced_at: now,
            decay_rate: 0.01,
            created_at: now,
        }
    }

    /// Set the source context.
    pub fn with_source(mut self, context_id: &str) -> Self {
        self.source_context_id = Some(context_id.to_string());
        self
    }

    /// Set custom confidence.
    pub fn with_confidence(mut self, confidence: f64) -> Self {
        self.confidence = confidence.clamp(0.0, 1.0);
        self
    }

    /// Set custom decay rate.
    pub fn with_decay_rate(mut self, rate: f64) -> Self {
        self.decay_rate = rate.clamp(0.0, 1.0);
        self
    }

    /// Calculate current confidence after decay.
    pub fn decayed_confidence(&self) -> f64 {
        let days_since = (Utc::now() - self.last_reinforced_at).num_days() as f64;
        (self.confidence * (-self.decay_rate * days_since).exp()).max(0.0)
    }

    /// Check if this fact should be pruned (confidence too low).
    pub fn should_prune(&self) -> bool {
        self.decayed_confidence() < 0.1
    }

    /// Reinforce this fact (boost confidence).
    pub fn reinforce(&mut self) {
        self.confidence = (self.confidence + 0.2).min(1.0);
        self.last_reinforced_at = Utc::now();
    }
}

/// A compressed conversation summary.
#[derive(Debug, Clone)]
pub struct JarvisSummary {
    /// Unique identifier.
    pub id: JarvisSummaryId,
    /// Context type.
    pub context_type: ContextType,
    /// Context identifier.
    pub context_id: String,
    /// The summary text.
    pub summary: String,
    /// Start timestamp of summarized messages.
    pub message_range_start: u64,
    /// End timestamp of summarized messages.
    pub message_range_end: u64,
    /// Number of messages summarized.
    pub message_count: u32,
    /// When this summary was created.
    pub created_at: DateTime<Utc>,
}

impl JarvisSummary {
    /// Create a new summary.
    pub fn new(
        context: &JarvisContext,
        summary: String,
        message_range_start: u64,
        message_range_end: u64,
        message_count: u32,
    ) -> Self {
        Self {
            id: JarvisSummaryId::new(),
            context_type: context.context_type,
            context_id: context.context_id.clone(),
            summary,
            message_range_start,
            message_range_end,
            message_count,
            created_at: Utc::now(),
        }
    }
}

/// An entity in the knowledge graph.
#[derive(Debug, Clone)]
pub struct JarvisEntity {
    /// Unique identifier.
    pub id: JarvisEntityId,
    /// Entity type.
    pub entity_type: EntityType,
    /// Display name.
    pub name: String,
    /// Normalized name for matching (lowercase, trimmed).
    pub normalized_name: String,
    /// Alternative names as JSON array.
    pub aliases_json: Option<String>,
    /// Additional properties as JSON.
    pub properties_json: Option<String>,
    /// When first mentioned.
    pub first_seen_at: DateTime<Utc>,
    /// When last mentioned.
    pub last_seen_at: DateTime<Utc>,
    /// Number of times mentioned.
    pub mention_count: u32,
    /// When created.
    pub created_at: DateTime<Utc>,
}

impl JarvisEntity {
    /// Create a new entity.
    pub fn new(entity_type: EntityType, name: String) -> Self {
        let now = Utc::now();
        let normalized = name.trim().to_lowercase();
        Self {
            id: JarvisEntityId::new(),
            entity_type,
            name,
            normalized_name: normalized,
            aliases_json: None,
            properties_json: None,
            first_seen_at: now,
            last_seen_at: now,
            mention_count: 1,
            created_at: now,
        }
    }

    /// Add aliases.
    pub fn with_aliases(mut self, aliases: Vec<String>) -> Self {
        if !aliases.is_empty() {
            self.aliases_json = Some(serde_json::to_string(&aliases).unwrap_or_default());
        }
        self
    }

    /// Add properties.
    pub fn with_properties(mut self, props: serde_json::Value) -> Self {
        self.properties_json = Some(props.to_string());
        self
    }

    /// Get aliases as vector.
    pub fn aliases(&self) -> Vec<String> {
        self.aliases_json
            .as_ref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }

    /// Record a new mention.
    pub fn record_mention(&mut self) {
        self.last_seen_at = Utc::now();
        self.mention_count += 1;
    }
}

/// A relationship between entities.
#[derive(Debug, Clone)]
pub struct JarvisRelationship {
    /// Unique identifier.
    pub id: JarvisRelationshipId,
    /// Source entity.
    pub from_entity_id: JarvisEntityId,
    /// Target entity.
    pub to_entity_id: JarvisEntityId,
    /// Relationship type (e.g., "knows", "works_on", "interested_in").
    pub relationship_type: String,
    /// Relationship strength (0.0 to 1.0).
    pub strength: f64,
    /// When last observed.
    pub last_seen_at: DateTime<Utc>,
    /// Number of times this relationship was observed.
    pub context_count: u32,
    /// When created.
    pub created_at: DateTime<Utc>,
}

impl JarvisRelationship {
    /// Create a new relationship.
    pub fn new(
        from_entity_id: JarvisEntityId,
        to_entity_id: JarvisEntityId,
        relationship_type: String,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: JarvisRelationshipId::new(),
            from_entity_id,
            to_entity_id,
            relationship_type,
            strength: 1.0,
            last_seen_at: now,
            context_count: 1,
            created_at: now,
        }
    }

    /// Record observation of this relationship.
    pub fn record_observation(&mut self) {
        self.last_seen_at = Utc::now();
        self.context_count += 1;
        // Boost strength with observations
        self.strength = (self.strength + 0.1).min(1.0);
    }
}

/// An entity mention in a specific context.
#[derive(Debug, Clone)]
pub struct JarvisEntityMention {
    /// Unique identifier.
    pub id: String,
    /// The entity that was mentioned.
    pub entity_id: JarvisEntityId,
    /// Context type.
    pub context_type: ContextType,
    /// Context identifier.
    pub context_id: String,
    /// Message ID where mentioned (optional).
    pub message_id: Option<String>,
    /// When mentioned.
    pub timestamp: u64,
}

impl JarvisEntityMention {
    /// Create a new mention.
    pub fn new(
        entity_id: JarvisEntityId,
        context: &JarvisContext,
        message_id: Option<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            entity_id,
            context_type: context.context_type,
            context_id: context.context_id.clone(),
            message_id,
            timestamp: Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64,
        }
    }
}

/// Knowledge about an entity (aggregated from graph).
#[derive(Debug, Clone, Default)]
pub struct EntityKnowledge {
    /// The entity.
    pub entity: Option<JarvisEntity>,
    /// Related entities with relationship types.
    pub related: Vec<(JarvisRelationship, JarvisEntity)>,
    /// Relevant facts.
    pub facts: Vec<JarvisFact>,
}

impl EntityKnowledge {
    /// Format as a user-friendly summary string.
    pub fn to_summary(&self) -> String {
        self.to_context_string()
    }

    /// Format as context string for LLM.
    pub fn to_context_string(&self) -> String {
        let mut parts = Vec::new();

        if let Some(entity) = &self.entity {
            parts.push(format!(
                "{} ({}): mentioned {} times",
                entity.name,
                format!("{:?}", entity.entity_type).to_lowercase(),
                entity.mention_count
            ));
        }

        if !self.related.is_empty() {
            let relationships: Vec<String> = self
                .related
                .iter()
                .map(|(rel, ent)| format!("{} {}", rel.relationship_type, ent.name))
                .collect();
            parts.push(format!("Relationships: {}", relationships.join(", ")));
        }

        if !self.facts.is_empty() {
            let facts: Vec<String> = self
                .facts
                .iter()
                .filter(|f| f.decayed_confidence() > 0.3)
                .map(|f| format!("{}: {}", f.predicate, f.object))
                .collect();
            if !facts.is_empty() {
                parts.push(format!("Facts: {}", facts.join("; ")));
            }
        }

        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_type_conversion() {
        assert_eq!(ContextType::from_i32(0), ContextType::Channel);
        assert_eq!(ContextType::from_i32(1), ContextType::Dm);
        assert_eq!(ContextType::from_i32(2), ContextType::AnonDm);
        assert_eq!(ContextType::from_i32(3), ContextType::AnonGroup);
        assert_eq!(ContextType::from_i32(99), ContextType::Channel);
    }

    #[test]
    fn test_role_conversion() {
        assert_eq!(Role::from_i32(0), Role::User);
        assert_eq!(Role::from_i32(1), Role::Assistant);
        assert_eq!(Role::from_i32(2), Role::System);
        assert_eq!(Role::from_i32(99), Role::User);
    }

    #[test]
    fn test_subject_type_conversion() {
        assert_eq!(SubjectType::from_i32(0), SubjectType::User);
        assert_eq!(SubjectType::from_i32(1), SubjectType::Topic);
        assert_eq!(SubjectType::from_i32(2), SubjectType::Project);
        assert_eq!(SubjectType::from_i32(3), SubjectType::Preference);
    }

    #[test]
    fn test_entity_type_parse() {
        assert_eq!(EntityType::parse("person"), EntityType::Person);
        assert_eq!(EntityType::parse("project"), EntityType::Project);
        assert_eq!(EntityType::parse("technology"), EntityType::Technology);
        assert_eq!(EntityType::parse("unknown"), EntityType::Topic);
    }

    #[test]
    fn test_jarvis_context_channel() {
        let channel_id = ChannelId::new();
        let ctx = JarvisContext::channel(channel_id);
        assert_eq!(ctx.context_type, ContextType::Channel);
        assert_eq!(ctx.context_id, channel_id.to_string());
    }

    #[test]
    fn test_jarvis_fact_decay() {
        let fact = JarvisFact::about_user(
            &UserId::new(),
            "works_on",
            "Rust project",
        );
        // Fresh fact should have confidence ~1.0
        assert!(fact.decayed_confidence() > 0.99);
        assert!(!fact.should_prune());
    }

    #[test]
    fn test_jarvis_entity_aliases() {
        let entity = JarvisEntity::new(EntityType::Person, "John Smith".to_string())
            .with_aliases(vec!["John".to_string(), "JS".to_string()]);

        let aliases = entity.aliases();
        assert_eq!(aliases.len(), 2);
        assert!(aliases.contains(&"John".to_string()));
    }

    #[test]
    fn test_entity_knowledge_format() {
        let entity = JarvisEntity::new(EntityType::Person, "Alice".to_string());
        let knowledge = EntityKnowledge {
            entity: Some(entity),
            related: vec![],
            facts: vec![],
        };

        let context = knowledge.to_context_string();
        assert!(context.contains("Alice"));
        assert!(context.contains("person"));
    }
}
