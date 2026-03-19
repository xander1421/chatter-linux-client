use std::sync::Arc;
use chatter_core::app::UIContext;
use chatter_core::network::grpc::GifClient;
use chatter_core::mailbox::blind_signature::UnblindedToken;

#[tokio::test]
async fn test_gif_client_anonymous_search() {
    // This test assumes a running backend at localhost:50051
    // or tests the logic of token injection.
    
    let channel = match tonic::transport::Channel::from_static("http://localhost:50051")
        .connect()
        .await {
            Ok(c) => c,
            Err(_) => {
                println!("Skipping test: backend not reachable");
                return;
            }
        };

    let interceptor = chatter_core::network::grpc::interceptors::AuthInterceptor::new();
    
    // Create a fake token to test spending logic
    let token = UnblindedToken {
        public_key: vec![0u8; 32],
        signature: vec![0u8; 256],
        created_at: 0,
    };

    interceptor.set_anonymous(token.clone());
    
    let mut client = GifClient::new(channel, interceptor.clone());
    
    // Call trending - if backend is running and whitelist worked, this should succeed
    // Even with a fake token, if it hits the whitelist, it might return results
    // Or it might fail with "invalid anonymous credential" if it tries to verify it
    let result = client.trending(5, Some(token)).await;
    
    match result {
        Ok(gifs) => {
            println!("Success: fetched {} trending gifs anonymously", gifs.len());
        }
        Err(e) => {
            println!("Note: Request failed as expected with fake token (or server down): {}", e);
            // Verify it didn't fail with "authentication required" (which means whitelist failed)
            let err_str = e.to_string();
            assert!(!err_str.contains("authentication required"), "Whitelist failed: {}", err_str);
        }
    }
}
