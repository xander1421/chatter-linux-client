//! Integration tests for Jarvis AI assistant.
//!
//! Tests memory storage, knowledge graph, and service functionality.

#![cfg(feature = "jarvis")]

use std::sync::Arc;
use tempfile::TempDir;

use chatter_core::domain::jarvis::{
    EntityType, JarvisContext, JarvisConversation, JarvisEntity, JarvisFact, JarvisRelationship,
    Role, SubjectType,
};
use chatter_core::domain::{ChannelId, UserId};
use chatter_core::storage::Database;
use chatter_core::storage::repositories::{JarvisGraphRepository, JarvisMemoryRepository};

// ============================================================================
// Test Helpers
// ============================================================================

fn create_test_db() -> (Arc<Database>, TempDir) {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test_jarvis.db");

    let db = Database::open(&db_path, "test_passphrase").expect("Failed to open database");
    db.run_migrations().expect("Failed to run migrations");

    (Arc::new(db), temp_dir)
}

// ============================================================================
// Memory Repository Tests
// ============================================================================

#[test]
fn test_jarvis_conversation_crud() {
    let (db, _temp) = create_test_db();
    let repo = JarvisMemoryRepository::new(db);

    let channel_id = ChannelId::new();
    let context = JarvisContext::channel(channel_id);

    // Insert a user message
    let user_msg = JarvisConversation::user_message(&context, "Hello Jarvis".to_string());
    repo.insert_conversation(&user_msg)
        .expect("Failed to insert");

    // Insert an assistant message
    let assistant_msg =
        JarvisConversation::assistant_message(&context, "Hello! How can I help?".to_string());
    repo.insert_conversation(&assistant_msg)
        .expect("Failed to insert");

    // Retrieve recent conversations
    let recent = repo
        .get_recent_conversations(&context, 10)
        .expect("Failed to get");
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].role, Role::User);
    assert_eq!(recent[0].content, "Hello Jarvis");
    assert_eq!(recent[1].role, Role::Assistant);
}

#[test]
fn test_jarvis_fact_upsert_and_reinforce() {
    let (db, _temp) = create_test_db();
    let repo = JarvisMemoryRepository::new(db);

    let user_id = UserId::new();

    // Insert a fact
    let fact = JarvisFact::about_user(&user_id, "works_on", "Rust project");
    repo.upsert_fact(&fact).expect("Failed to insert fact");

    // Verify it was inserted
    let facts = repo
        .get_facts_about(SubjectType::User, &user_id.to_string())
        .expect("Failed to get facts");
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].predicate, "works_on");
    assert_eq!(facts[0].object, "Rust project");

    // Upsert the same predicate with different object
    let fact2 = JarvisFact::about_user(&user_id, "works_on", "Chatter project");
    repo.upsert_fact(&fact2).expect("Failed to upsert fact");

    // Should still have one fact, but with updated object
    let facts = repo
        .get_facts_about(SubjectType::User, &user_id.to_string())
        .expect("Failed to get facts");
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].object, "Chatter project");
    // Confidence stays at max (1.0) since initial was already 1.0 and SQL clamps to 1.0
    assert!(
        (facts[0].confidence - 1.0).abs() < f64::EPSILON,
        "Confidence should remain at 1.0 after reinforce"
    );
}

#[test]
fn test_jarvis_fact_search() {
    let (db, _temp) = create_test_db();
    let repo = JarvisMemoryRepository::new(db);

    let user_id = UserId::new();

    // Insert multiple facts (each with unique predicate to avoid upsert collisions)
    repo.upsert_fact(&JarvisFact::about_user(
        &user_id,
        "interested_in_ml",
        "machine learning",
    ))
    .unwrap();
    repo.upsert_fact(&JarvisFact::about_user(&user_id, "uses", "Rust"))
        .unwrap();
    repo.upsert_fact(&JarvisFact::about_user(
        &user_id,
        "interested_in_crypto",
        "cryptography",
    ))
    .unwrap();

    // Search for "machine" — matches object "machine learning"
    let results = repo.search_facts("machine", 10).expect("Failed to search");
    assert_eq!(results.len(), 1);
    assert!(results[0].object.contains("machine learning"));

    // Search for "interested" — matches predicates "interested_in_ml" and "interested_in_crypto"
    let results = repo
        .search_facts("interested", 10)
        .expect("Failed to search");
    assert_eq!(results.len(), 2);
}

#[test]
fn test_jarvis_context_types() {
    let (db, _temp) = create_test_db();
    let repo = JarvisMemoryRepository::new(db);

    let channel_id = ChannelId::new();
    let dm_channel_id = ChannelId::new();

    let channel_ctx = JarvisContext::channel(channel_id);
    let dm_ctx = JarvisContext::dm(dm_channel_id);

    // Insert messages in different contexts
    repo.insert_conversation(&JarvisConversation::user_message(
        &channel_ctx,
        "Channel msg".to_string(),
    ))
    .unwrap();
    repo.insert_conversation(&JarvisConversation::user_message(
        &dm_ctx,
        "DM msg".to_string(),
    ))
    .unwrap();

    // Each context should only see its own messages
    let channel_msgs = repo.get_recent_conversations(&channel_ctx, 10).unwrap();
    assert_eq!(channel_msgs.len(), 1);
    assert_eq!(channel_msgs[0].content, "Channel msg");

    let dm_msgs = repo.get_recent_conversations(&dm_ctx, 10).unwrap();
    assert_eq!(dm_msgs.len(), 1);
    assert_eq!(dm_msgs[0].content, "DM msg");
}

// ============================================================================
// Graph Repository Tests
// ============================================================================

#[test]
fn test_jarvis_entity_upsert() {
    let (db, _temp) = create_test_db();
    let repo = JarvisGraphRepository::new(db);

    let entity = JarvisEntity::new(EntityType::Person, "Alice Smith".to_string());
    let id = repo.upsert_entity(&entity).expect("Failed to upsert");

    // Verify entity was created
    let fetched = repo
        .get_entity(&id)
        .expect("Failed to get")
        .expect("Entity not found");
    assert_eq!(fetched.name, "Alice Smith");
    assert_eq!(fetched.entity_type, EntityType::Person);
    assert_eq!(fetched.mention_count, 1);

    // Upsert again should increment mention count
    let entity2 = JarvisEntity::new(EntityType::Person, "Alice Smith".to_string());
    let id2 = repo.upsert_entity(&entity2).expect("Failed to upsert");
    assert_eq!(id.as_str(), id2.as_str());

    let fetched = repo
        .get_entity(&id)
        .expect("Failed to get")
        .expect("Entity not found");
    assert_eq!(fetched.mention_count, 2);
}

#[test]
fn test_jarvis_entity_search() {
    let (db, _temp) = create_test_db();
    let repo = JarvisGraphRepository::new(db);

    // Create entities
    repo.upsert_entity(&JarvisEntity::new(
        EntityType::Technology,
        "Rust".to_string(),
    ))
    .unwrap();
    repo.upsert_entity(&JarvisEntity::new(
        EntityType::Technology,
        "TypeScript".to_string(),
    ))
    .unwrap();
    repo.upsert_entity(&JarvisEntity::new(
        EntityType::Technology,
        "RustFFI".to_string(),
    ))
    .unwrap();
    repo.upsert_entity(&JarvisEntity::new(
        EntityType::Project,
        "Chatter".to_string(),
    ))
    .unwrap();

    // Search for "rust"
    let results = repo.search_entities("rust", 10).expect("Failed to search");
    assert_eq!(results.len(), 2); // "Rust" and "RustFFI"

    // Search for "chat"
    let results = repo.search_entities("chat", 10).expect("Failed to search");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "Chatter");
}

#[test]
fn test_jarvis_entity_by_name() {
    let (db, _temp) = create_test_db();
    let repo = JarvisGraphRepository::new(db);

    repo.upsert_entity(&JarvisEntity::new(
        EntityType::Project,
        "Rust Compiler".to_string(),
    ))
    .unwrap();

    // Case-insensitive lookup
    let fetched = repo
        .get_entity_by_name("rust compiler")
        .expect("Failed to get")
        .expect("Not found");
    assert_eq!(fetched.name, "Rust Compiler");

    // With different casing
    let fetched = repo
        .get_entity_by_name("RUST COMPILER")
        .expect("Failed to get")
        .expect("Not found");
    assert_eq!(fetched.name, "Rust Compiler");
}

#[test]
fn test_jarvis_relationships() {
    let (db, _temp) = create_test_db();
    let repo = JarvisGraphRepository::new(db);

    // Create entities
    let alice = JarvisEntity::new(EntityType::Person, "Alice".to_string());
    let chatter = JarvisEntity::new(EntityType::Project, "Chatter".to_string());
    let rust = JarvisEntity::new(EntityType::Technology, "Rust".to_string());

    let alice_id = repo.upsert_entity(&alice).unwrap();
    let chatter_id = repo.upsert_entity(&chatter).unwrap();
    let rust_id = repo.upsert_entity(&rust).unwrap();

    // Create relationships
    let rel1 =
        JarvisRelationship::new(alice_id.clone(), chatter_id.clone(), "works_on".to_string());
    let rel2 = JarvisRelationship::new(chatter_id.clone(), rust_id.clone(), "uses".to_string());

    repo.upsert_relationship(&rel1).unwrap();
    repo.upsert_relationship(&rel2).unwrap();

    // Get relationships from Alice
    let from_alice = repo
        .get_relationships_from(&alice_id)
        .expect("Failed to get");
    assert_eq!(from_alice.len(), 1);
    assert_eq!(from_alice[0].0.relationship_type, "works_on");
    assert_eq!(from_alice[0].1.name, "Chatter");

    // Get relationships to Chatter
    let to_chatter = repo
        .get_relationships_to(&chatter_id)
        .expect("Failed to get");
    assert_eq!(to_chatter.len(), 1);
    assert_eq!(to_chatter[0].0.relationship_type, "works_on");
    assert_eq!(to_chatter[0].1.name, "Alice");
}

#[test]
fn test_jarvis_connected_entities() {
    let (db, _temp) = create_test_db();
    let repo = JarvisGraphRepository::new(db);

    // Create a small graph: Alice -> Chatter -> Rust -> WASM
    let alice_id = repo
        .upsert_entity(&JarvisEntity::new(EntityType::Person, "Alice".to_string()))
        .unwrap();
    let chatter_id = repo
        .upsert_entity(&JarvisEntity::new(
            EntityType::Project,
            "Chatter".to_string(),
        ))
        .unwrap();
    let rust_id = repo
        .upsert_entity(&JarvisEntity::new(
            EntityType::Technology,
            "Rust".to_string(),
        ))
        .unwrap();
    let wasm_id = repo
        .upsert_entity(&JarvisEntity::new(
            EntityType::Technology,
            "WASM".to_string(),
        ))
        .unwrap();

    repo.upsert_relationship(&JarvisRelationship::new(
        alice_id.clone(),
        chatter_id.clone(),
        "works_on".to_string(),
    ))
    .unwrap();
    repo.upsert_relationship(&JarvisRelationship::new(
        chatter_id.clone(),
        rust_id.clone(),
        "uses".to_string(),
    ))
    .unwrap();
    repo.upsert_relationship(&JarvisRelationship::new(
        rust_id.clone(),
        wasm_id.clone(),
        "compiles_to".to_string(),
    ))
    .unwrap();

    // Get entities connected to Alice within 2 hops
    let connected = repo
        .get_connected_entities(&alice_id, 2)
        .expect("Failed to get");

    // Should find Chatter (depth 1) and Rust (depth 2)
    assert!(connected.len() >= 2);
    let names: Vec<&str> = connected.iter().map(|(e, _)| e.name.as_str()).collect();
    assert!(names.contains(&"Chatter"));
    assert!(names.contains(&"Rust"));
}

#[test]
fn test_jarvis_entities_by_type() {
    let (db, _temp) = create_test_db();
    let repo = JarvisGraphRepository::new(db);

    // Create entities of different types
    repo.upsert_entity(&JarvisEntity::new(EntityType::Person, "Alice".to_string()))
        .unwrap();
    repo.upsert_entity(&JarvisEntity::new(EntityType::Person, "Bob".to_string()))
        .unwrap();
    repo.upsert_entity(&JarvisEntity::new(
        EntityType::Technology,
        "Rust".to_string(),
    ))
    .unwrap();
    repo.upsert_entity(&JarvisEntity::new(
        EntityType::Project,
        "Chatter".to_string(),
    ))
    .unwrap();

    // Get only people
    let people = repo
        .get_entities_by_type(EntityType::Person, 10)
        .expect("Failed to get");
    assert_eq!(people.len(), 2);

    // Get only technologies
    let tech = repo
        .get_entities_by_type(EntityType::Technology, 10)
        .expect("Failed to get");
    assert_eq!(tech.len(), 1);
    assert_eq!(tech[0].name, "Rust");
}

// Domain type tests are in src/domain/jarvis.rs (no database needed)

// ============================================================================
// Temporal consolidation (P3/P4): contradiction supersedes, history preserved.
// ============================================================================
#[test]
fn fact_contradiction_supersedes_and_preserves_history() {
    let (db, _temp) = create_test_db();
    let repo = JarvisMemoryRepository::new(db);
    let user = UserId::new();

    // ADD
    repo.upsert_fact(&JarvisFact::about_user(
        &user,
        "works_on",
        "the Chatter app",
    ))
    .unwrap();
    // REINFORCE (same object, case-insensitive)
    repo.upsert_fact(&JarvisFact::about_user(
        &user,
        "works_on",
        "The Chatter App",
    ))
    .unwrap();
    let current = repo
        .get_facts_about(SubjectType::User, &user.to_string())
        .unwrap();
    assert_eq!(current.len(), 1, "reinforce must not duplicate");
    assert!(
        current[0].confidence > 1.0 - 1e-9,
        "reinforce must bump confidence"
    );

    // SUPERSEDE (contradiction)
    repo.upsert_fact(&JarvisFact::about_user(
        &user,
        "works_on",
        "a new game engine",
    ))
    .unwrap();

    let current = repo
        .get_facts_about(SubjectType::User, &user.to_string())
        .unwrap();
    assert_eq!(current.len(), 1, "only the new fact is current");
    assert_eq!(current[0].object, "a new game engine");
    assert!(current[0].valid_until.is_none());

    let history = repo
        .get_fact_history(SubjectType::User, &user.to_string(), "works_on")
        .unwrap();
    assert_eq!(history.len(), 2, "superseded fact must keep its row");
    let old = history
        .iter()
        .find(|f| f.object == "the Chatter app")
        .unwrap();
    assert!(old.valid_until.is_some(), "old fact must carry valid_until");
    assert_eq!(
        old.superseded_by.as_deref(),
        Some(current[0].id.as_str()),
        "old fact must point at its replacement"
    );
}
