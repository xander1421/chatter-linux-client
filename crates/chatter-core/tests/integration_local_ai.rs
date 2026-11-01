//! Integration test for Local AI service using mistralrs.
//!
//! Run with: cargo test --test integration_local_ai --features local-ai -- --nocapture

#![cfg(feature = "local-ai")]

#[cfg(feature = "local-ai")]
mod local_ai_tests {
    use mistralrs::{
        IsqType, Model, PagedAttentionMetaBuilder, TextMessageRole, TextMessages, TextModelBuilder,
    };
    use std::sync::Arc;

    /// Test loading a model and doing a simple completion.
    #[tokio::test]
    async fn test_mistralrs_load_and_complete() {
        // Use a small model for testing
        // Options: "microsoft/Phi-3.5-mini-instruct", "Qwen/Qwen2.5-0.5B-Instruct", etc.
        let model_id = std::env::var("TEST_MODEL")
            .unwrap_or_else(|_| "Qwen/Qwen2.5-0.5B-Instruct".to_string());

        println!("Loading model: {}", model_id);
        println!("This may take a while on first run (downloading weights)...\n");

        // Build model with Q4K quantization for faster inference
        let model = match TextModelBuilder::new(&model_id)
            .with_isq(IsqType::Q4K)
            .with_logging()
            .with_paged_attn(
                PagedAttentionMetaBuilder::default()
                    .build()
                    .expect("PagedAttention config"),
            )
            .build()
            .await
        {
            Ok(m) => {
                println!("✅ Model loaded successfully!");
                Arc::new(m)
            }
            Err(e) => {
                println!("❌ Failed to load model: {}", e);
                println!("\nPossible issues:");
                println!("  - Model not found on HuggingFace");
                println!("  - CUDA not available");
                println!("  - Out of GPU memory");
                return;
            }
        };

        // Simple completion test
        let prompt = "What is 2 + 2? Answer with just the number.";
        println!("\nPrompt: {}", prompt);

        let messages = TextMessages::new().add_message(TextMessageRole::User, prompt);

        match model.send_chat_request(messages).await {
            Ok(response) => {
                if let Some(choice) = response.choices.first() {
                    if let Some(content) = &choice.message.content {
                        println!("\n=== AI Response ===");
                        println!("{}", content);
                        println!("===================\n");
                    }
                }
                println!("✅ Completion successful!");
            }
            Err(e) => {
                println!("❌ Completion failed: {}", e);
            }
        }
    }
}

/// Stub tests when local-ai feature is disabled.
#[cfg(not(feature = "local-ai"))]
mod stub_tests {
    #[test]
    fn test_local_ai_disabled() {
        println!("local-ai feature is disabled");
        println!("To run local AI tests:");
        println!("  cargo test --test integration_local_ai --features local-ai -- --nocapture");
    }
}

/// Test the LocalAIActorHandle API (works with or without local-ai feature).
mod actor_tests {
    use chatter_core::services::ai::{LocalAIActorHandle, SampleParams};

    #[tokio::test]
    async fn test_actor_handle_load_and_complete() {
        println!("Testing LocalAIActorHandle...\n");

        // Spawn the actor
        let handle = LocalAIActorHandle::spawn();

        // Use a small model
        let model_id = std::env::var("TEST_MODEL")
            .unwrap_or_else(|_| "Qwen/Qwen2.5-0.5B-Instruct".to_string());

        println!("Loading model via actor: {}", model_id);

        // Load model
        match handle.load_model(model_id.clone(), None).await {
            Ok(info) => {
                println!("✅ Model loaded: {:?}", info);
            }
            Err(e) => {
                println!("❌ Failed to load model: {}", e);
                #[cfg(not(feature = "local-ai"))]
                {
                    println!("\nNote: local-ai feature not enabled, using stub");
                }
                return;
            }
        }

        // Check model info
        match handle.get_model_info().await {
            Ok(Some(info)) => {
                println!(
                    "Model info: {} ({})",
                    info.model_id,
                    info.quantization.as_deref().unwrap_or("default")
                );
            }
            Ok(None) => {
                println!("No model loaded");
            }
            Err(e) => {
                println!("Failed to get model info: {}", e);
            }
        }

        // Test completion
        let prompt = "Say hello in one word.".to_string();
        println!("\nPrompt: {}", prompt);

        match handle.complete(prompt, SampleParams::default()).await {
            Ok(response) => {
                println!("\n=== AI Response ===");
                println!("{}", response);
                println!("===================\n");
            }
            Err(e) => {
                println!("❌ Completion failed: {}", e);
            }
        }

        // Unload
        if let Err(e) = handle.unload_model().await {
            println!("Failed to unload: {}", e);
        } else {
            println!("✅ Model unloaded");
        }
    }

    #[tokio::test]
    async fn test_actor_no_model() {
        let handle = LocalAIActorHandle::spawn();

        // Try to complete without loading a model
        let result = handle
            .complete("Hello".to_string(), SampleParams::default())
            .await;

        match result {
            Err(chatter_core::services::ai::LocalAIError::NoModel) => {
                println!("✅ Correctly returned NoModel error");
            }
            Err(e) => {
                println!("Got error (expected NoModel): {}", e);
            }
            Ok(response) => {
                println!("Got response (stub mode?): {}", response);
            }
        }
    }
}
