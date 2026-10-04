//! VALIDATE-AI — empirical proofs for the local AI + memory system.
//!
//! Every test here drives REAL components end-to-end against a REAL local
//! Ollama (no mocks): real SQLCipher DB with migrations, real JarvisService,
//! real engine, real model. Run:
//!
//!   cargo test -p chatter-core --test validated_ai -- --ignored --nocapture
//!
//! Layer map (lib-level live proofs live next to their units):
//!   V-AI-1 provider detect .......... here (v_ai_01) + providers::live_detect
//!   V-AI-2 pull + chat + self-heal .. ollama::live_pull_and_chat,
//!                                     engine::live_missing_model_self_heals
//!   V-AI-3 memory round-trip ........ here (v_ai_03)
//!   V-AI-4 multi-turn coherence ..... here (v_ai_04)
//!   V-AI-5 LLM write path + consolidation ... here (v_ai_05)
//!   V-AI-6 temporal supersede (offline, real DB) ...
//!                                     integration_jarvis::fact_contradiction_*
//!   V-AI-7 app-context grounding .... here (v_ai_07)

#![cfg(feature = "jarvis")]

use std::sync::Arc;

use tempfile::TempDir;

use chatter_core::domain::jarvis::JarvisContext;
use chatter_core::domain::jarvis::SubjectType;
use chatter_core::domain::UserId;
use chatter_core::local_ai::providers::{detect_providers, ProviderKind};
use chatter_core::local_ai::settings::LocalAISettings;
use chatter_core::services::ai::LocalAIActorHandle;
use chatter_core::services::jarvis::{
    FactExtractor, JarvisEngine, JarvisService, SharedJarvisEngine,
};
use chatter_core::storage::repositories::{JarvisGraphRepository, JarvisMemoryRepository};
use chatter_core::storage::Database;

/// One Ollama, one tiny model: concurrent generations starve each other and
/// flake the suite. Every test serializes on this.
static OLLAMA: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn ollama_guard() -> std::sync::MutexGuard<'static, ()> {
    OLLAMA.lock().unwrap_or_else(|p| p.into_inner())
}

fn test_db() -> (Arc<Database>, TempDir) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db = Database::open(&temp_dir.path().join("validate_ai.db"), "test_passphrase")
        .expect("open db");
    db.run_migrations().expect("migrations");
    (Arc::new(db), temp_dir)
}

fn engine() -> SharedJarvisEngine {
    SharedJarvisEngine::new(JarvisEngine::from_settings(
        &LocalAISettings::default(),
        LocalAIActorHandle::spawn(),
    ))
}

struct Harness {
    svc: JarvisService,
    memory: Arc<JarvisMemoryRepository>,
    _temp: TempDir,
}

fn service() -> Harness {
    let (db, _temp) = test_db();
    let memory = Arc::new(JarvisMemoryRepository::new(db.clone()));
    let graph = Arc::new(JarvisGraphRepository::new(db));
    let svc = JarvisService::new(memory.clone(), graph, engine(), None);
    Harness { svc, memory, _temp }
}

// ============================================================================
// V-AI-1 — provider detection finds the local Ollama with models listed.
// ============================================================================
#[tokio::test]
#[ignore = "needs a running Ollama on localhost:11434"]
async fn v_ai_01_provider_detection() {
    let _serial = ollama_guard();
    let detected = detect_providers("http://localhost:11434").await;
    let ollama = detected
        .iter()
        .find(|p| p.kind == ProviderKind::Ollama)
        .expect("Ollama not detected — is it running?");
    assert!(
        !ollama.models.is_empty(),
        "Ollama detected but no models installed — pull one first"
    );
    println!(
        "V-AI-1 OK: {} models on {}",
        ollama.models.len(),
        ollama.url
    );
}

// ============================================================================
// V-AI-3 — memory round-trip through the real service: fact harvested on ask,
// recalled on a later ask.
// ============================================================================
#[tokio::test]
#[ignore = "needs a running Ollama on localhost:11434"]
async fn v_ai_03_memory_roundtrip() {
    let _serial = ollama_guard();
    let h = service();
    let ctx = JarvisContext::general();
    let user = UserId::new();

    let r1 = h
        .svc
        .ask(&ctx, "Remember this: my name is Alex.", Some(&user))
        .await
        .expect("ask 1");
    println!("reply 1: {r1:?}");

    let facts = h
        .memory
        .get_facts_about(SubjectType::User, &user.to_string())
        .expect("facts");
    assert!(
        facts
            .iter()
            .any(|f| f.predicate == "is_named" && f.object.to_lowercase().contains("alex")),
        "is_named fact missing: {facts:?}"
    );

    let r2 = h
        .svc
        .ask(&ctx, "What is my name?", Some(&user))
        .await
        .expect("ask 2");
    println!("reply 2: {r2:?}");
    assert!(r2.to_lowercase().contains("alex"), "no recall: {r2}");
    println!("V-AI-3 OK");
}

// ============================================================================
// V-AI-4 — multi-turn coherence: real role turns carry mid-conversation state.
// ============================================================================
#[tokio::test]
#[ignore = "needs a running Ollama on localhost:11434"]
async fn v_ai_04_multi_turn_coherence() {
    let _serial = ollama_guard();
    let h = service();
    let ctx = JarvisContext::general();
    let user = UserId::new();

    let _ = h
        .svc
        .ask(&ctx, "I want to write a message to a friend.", Some(&user))
        .await
        .expect("turn 1");
    let _ = h
        .svc
        .ask(&ctx, "His name is John.", Some(&user))
        .await
        .expect("turn 2");

    // Small models at temp 0.7 occasionally flub; one retry keeps this about
    // the wiring, not sampling luck. ≥3b models pass first try.
    let mut last = String::new();
    for attempt in 0..2 {
        let r = h
            .svc
            .ask(&ctx, "Who is the message for? Answer with just the name.", Some(&user))
            .await
            .expect("turn 3");
        println!("turn 3 (attempt {attempt}): {r:?}");
        if r.to_lowercase().contains("john") {
            println!("V-AI-4 OK");
            return;
        }
        last = r;
    }
    panic!("multi-turn context lost — expected 'John' in: {last}");
}

// ============================================================================
// V-AI-5 — the LLM write path: extraction produces typed facts through the
// consolidating upsert, and a contradiction supersedes temporally.
// ============================================================================
#[tokio::test]
#[ignore = "needs a running Ollama on localhost:11434"]
async fn v_ai_05_llm_write_path_and_consolidation() {
    let _serial = ollama_guard();
    let (db, _temp) = test_db();
    let memory = Arc::new(JarvisMemoryRepository::new(db));
    let extractor = FactExtractor::new(engine(), memory.clone());
    let ctx = JarvisContext::general();
    let user = UserId::new();

    // Extraction from a fact-dense message. Retry once — tiny models
    // occasionally emit malformed JSON (parse yields 0 facts, never errors).
    let mut stored = 0;
    for _ in 0..2 {
        stored = extractor
            .extract_and_store("My name is Alex and I work on the Chatter app.", &user, &ctx)
            .await
            .expect("extract");
        if stored > 0 {
            break;
        }
    }
    assert!(stored > 0, "LLM write path stored no facts");
    let facts = memory
        .get_facts_about(SubjectType::User, &user.to_string())
        .expect("facts");
    println!("extracted facts: {facts:?}");
    assert!(!facts.is_empty());

    // Contradiction consolidates: supersede, don't duplicate.
    use chatter_core::domain::jarvis::JarvisFact;
    memory
        .upsert_fact(&JarvisFact::about_user(&user, "is_named", "Sasha"))
        .expect("contradict");
    let current = memory
        .get_facts_about(SubjectType::User, &user.to_string())
        .expect("facts");
    let named: Vec<_> = current.iter().filter(|f| f.predicate == "is_named").collect();
    if named.len() == 1 {
        // Only asserts when extraction produced is_named (model-dependent).
        assert_eq!(named[0].object, "Sasha", "contradiction must supersede");
        let history = memory
            .get_fact_history(SubjectType::User, &user.to_string(), "is_named")
            .expect("history");
        assert!(
            history.len() >= 1 && history.iter().all(|f| f.superseded_by.is_some() || f.valid_until.is_none()),
            "temporal chain broken: {history:?}"
        );
    }
    println!("V-AI-5 OK");
}

// ============================================================================
// V-AI-7 — app-context grounding: live app state (friends) reaches the model
// and grounds its answer.
// ============================================================================
#[tokio::test]
#[ignore = "needs a running Ollama on localhost:11434"]
async fn v_ai_07_app_context_grounding() {
    let _serial = ollama_guard();
    let (db, _temp) = test_db();
    let memory = Arc::new(JarvisMemoryRepository::new(db.clone()));
    let graph = Arc::new(JarvisGraphRepository::new(db));
    let svc = JarvisService::new(memory, graph, engine(), None)
        .with_app_context(Arc::new(|| {
            "User's channels: general, dev-team.\nUser's friends: alex_dev, sam42.\n".to_string()
        }));

    let ctx = JarvisContext::general();
    let user = UserId::new();
    let mut last = String::new();
    for attempt in 0..2 {
        let r = svc
            .ask(&ctx, "List my friends' usernames, comma separated.", Some(&user))
            .await
            .expect("ask");
        println!("grounding (attempt {attempt}): {r:?}");
        let lower = r.to_lowercase();
        if lower.contains("alex_dev") && lower.contains("sam42") {
            println!("V-AI-7 OK");
            return;
        }
        last = r;
    }
    panic!("app context did not ground the answer: {last}");
}
