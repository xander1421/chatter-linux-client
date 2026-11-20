use chatter_core::testing::network::grpc::client_pool::GrpcClientPool;
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("chatter_core::network::grpc=info")
        .init();
    let origin = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://chat.offai.app".into());
    let pool = GrpcClientPool::new(&origin)?;
    tokio::time::timeout(std::time::Duration::from_secs(10), pool.connect()).await??;
    assert!(
        pool.channel().is_http3(),
        "direct RPC channel did not select HTTP/3"
    );
    let started = std::time::Instant::now();
    let mut public = pool.anonymous_mailbox_client();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        public.get_blind_signature_public_key(),
    )
    .await??;
    println!(
        "HTTP/3 selected; public gRPC read succeeded in {} ms",
        started.elapsed().as_millis()
    );
    Ok(())
}
