use chatter_linux::network::grpc::{GrpcClientPool, auth_client::AuthClient};
use chatter_linux::crypto::signal::identity::SignalIdentityKeyPair;
use libsignal_protocol::IdentityKeyPair;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Testing registration challenge...");
    
    // Initialize gRPC client
    let pool = GrpcClientPool::new("http://localhost:50051")?;
    pool.connect().await?;
    
    // Create auth client (no auth interceptor needed for registration challenge)
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    
    // Generate test identity key
    let identity_key_pair = IdentityKeyPair::generate(&mut rand::rngs::OsRng);
    let identity_public_key = identity_key_pair.identity_key().serialize();
    
    println!("Identity public key length: {} bytes", identity_public_key.len());
    println!("Identity public key (hex): {}", hex::encode(&identity_public_key));
    
    // Test username (already hashed)
    let username = "test_user_hash_1234567890abcdef";
    
    // Request registration challenge
    println!("\nRequesting registration challenge...");
    println!("Username: {}", username);
    println!("Identity key: {} bytes", identity_public_key.len());
    
    match auth_client.request_registration_challenge(username, identity_public_key.as_ref()).await {
        Ok(response) => {
            println!("✓ Challenge received!");
            println!("  Challenge ID: {}", response.challenge_id);
            println!("  Challenge nonce length: {} bytes", response.challenge_nonce.len());
        }
        Err(e) => {
            println!("✗ Failed to get challenge: {}", e);
            return Err(e.into());
        }
    }
    
    println!("\nTest completed successfully!");
    Ok(())
}