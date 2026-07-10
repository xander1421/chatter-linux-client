//! Minimal Arti onion-connectivity probe — isolates the Tor path from the app.
//!
//! Does exactly what `network/grpc/onion_transport.rs` does: install the ring
//! rustls CryptoProvider, bootstrap a default `TorClient`, then
//! `connect((onion, 443))`. Prints the FULL error chain on failure so we can see
//! where the HS connection actually breaks (descriptor fetch / intro / rendezvous).
//!
//! Run:
//!   CHATTER_ONION_ADDR=<hash>.onion \
//!     cargo run -p chatter-core --example onion_probe --features onion
//!
//! Optional: RUST_LOG=info,tor_hsclient=debug,tor_circmgr=debug for the trace.

#[cfg(feature = "onion")]
#[tokio::main]
async fn main() {
    use arti_client::{TorClient, TorClientConfig};

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,tor_hsclient=debug,tor_circmgr=info,tor_proto=info".into()
            }),
        )
        .init();

    // Same as onion_transport.rs: Arti's rustls runtime needs a process-wide
    // default CryptoProvider.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();

    let raw = std::env::var("CHATTER_ONION_ADDR").expect("set CHATTER_ONION_ADDR=<hash>.onion");
    // Normalize to a bare host (mirror how the URI host is extracted in the app).
    let host = raw
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .split(':')
        .next()
        .unwrap()
        .to_string();

    println!("[probe] bootstrapping Tor (this walks the directory)…");
    let t0 = std::time::Instant::now();
    let tor = match TorClient::create_bootstrapped(TorClientConfig::default()).await {
        Ok(c) => c,
        Err(e) => {
            println!("[probe] BOOTSTRAP FAILED after {:?}: {e:?}", t0.elapsed());
            return;
        }
    };
    println!("[probe] bootstrapped in {:?}", t0.elapsed());

    println!("[probe] connecting to {host}:443 over Tor…");
    let t1 = std::time::Instant::now();
    match tor.connect((host.as_str(), 443u16)).await {
        Ok(_stream) => {
            println!(
                "[probe] SUCCESS — onion stream established in {:?}. The app path should work too.",
                t1.elapsed()
            );
        }
        Err(e) => {
            // {e:?} prints the full source chain — the actual reason.
            println!("[probe] CONNECT FAILED after {:?}:\n{e:?}", t1.elapsed());
        }
    }
}

#[cfg(not(feature = "onion"))]
fn main() {
    eprintln!("rebuild with --features onion");
}
