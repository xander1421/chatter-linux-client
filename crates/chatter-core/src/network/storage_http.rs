//! Shared HTTP policy for presigned attachment, backup and link-archive storage.
//!
//! Trust only the configured Chatter CA, require HTTPS and never redirect
//! capability URLs or upload bodies. Cache construction failures as well as the
//! connection pool. Connect and read-idle deadlines bound unreachable/stalled
//! storage without imposing a whole-transfer deadline on large, progressing blobs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

type CachedClient = Result<reqwest::Client, String>;

pub(crate) fn storage_client() -> CachedClient {
    static CLIENTS: OnceLock<Mutex<HashMap<tokio::runtime::Id, Weak<CachedClient>>>> =
        OnceLock::new();
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| "storage HTTP requires a running async runtime".to_string())?;
    let mut clients = CLIENTS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "storage HTTP client cache lock poisoned".to_string())?;
    clients.retain(|_, client| client.strong_count() > 0);
    if let Some(client) = clients.get(&runtime.id()).and_then(Weak::upgrade) {
        return (*client).clone();
    }

    // Pooled sockets are driven by the runtime that opened them. Sharing the
    // pool across runtimes can stall a transfer or cancel it when another
    // client/test runtime shuts down. Keep one pool per runtime, including a
    // cached construction failure, and release it when that runtime stops.
    let client = Arc::new(build_client());
    clients.insert(runtime.id(), Arc::downgrade(&client));
    drop(clients);
    let retained = Arc::clone(&client);
    runtime.spawn(async move {
        std::future::pending::<()>().await;
        drop(retained);
    });
    (*client).clone()
}

fn build_client() -> CachedClient {
    // Explicit config: no process-global provider, no platform/WebPKI roots
    // beside the pinned Chatter CA, post-quantum key exchange only.
    let tls =
        crate::network::pq_tls::pinned_client_config().map_err(|e| format!("storage TLS: {e}"))?;
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .https_only(true)
        // Presigned URLs are already final destinations. Never forward their
        // capability query or upload body to a redirect target.
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("could not build pinned storage client: {}", e.without_url()))
}

/// Drop a transfer as soon as its route is retired, including while dialing or
/// reading the body. A retired generation cannot start a new request.
pub(crate) async fn fence_cdn_transfer<T, E>(
    generation: tokio_util::sync::CancellationToken,
    transport_changed: E,
    transfer: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, E> {
    tokio::select! {
        biased;
        _ = generation.cancelled() => Err(transport_changed),
        result = transfer => {
            if generation.is_cancelled() {
                Err(transport_changed)
            } else {
                result
            }
        },
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn retired_route_cancels_a_cdn_transfer_and_refuses_to_start_another() {
        use tokio_util::sync::CancellationToken;

        let generation = CancellationToken::new();
        let old = generation.clone();
        let cancelled = super::fence_cdn_transfer(generation.clone(), "transport changed", async {
            old.cancel();
            std::future::pending::<Result<(), &str>>().await
        })
        .await;
        assert_eq!(cancelled, Err("transport changed"));
        assert_eq!(
            super::fence_cdn_transfer::<(), _>(generation, "transport changed", async {
                panic!("retired CDN route started")
            })
            .await,
            Err("transport changed")
        );
    }

    #[test]
    #[ignore = "requires live private-CA storage health endpoint"]
    fn storage_client_survives_runtime_restart() {
        let url = std::env::var("CHATTER_TEST_STORAGE_HEALTH_URL")
            .expect("set CHATTER_TEST_STORAGE_HEALTH_URL to the storage HTTPS health endpoint");
        for _ in 0..3 {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let response = super::storage_client()
                    .unwrap()
                    .get(&url)
                    .send()
                    .await
                    .unwrap_or_else(|error| {
                        panic!("storage request failed: {:?}", error.without_url())
                    });
                assert!(response.status().is_success());
                response.bytes().await.unwrap_or_else(|error| {
                    panic!("storage body failed: {:?}", error.without_url())
                });
            });
        }
    }

    #[test]
    #[ignore = "requires live private-CA storage health endpoint"]
    fn storage_client_does_not_depend_on_another_runtime() {
        let url = std::env::var("CHATTER_TEST_STORAGE_HEALTH_URL")
            .expect("set CHATTER_TEST_STORAGE_HEALTH_URL to the storage HTTPS health endpoint");
        let runtimes: Vec<_> = (0..2)
            .map(|_| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
            })
            .collect();
        // Keep the first reactor alive but idle while the second runs. A
        // process-global pool must not hand it a socket driven by the first.
        for runtime in &runtimes {
            runtime.block_on(async {
                let response = super::storage_client()
                    .unwrap()
                    .get(&url)
                    .timeout(std::time::Duration::from_secs(3))
                    .send()
                    .await
                    .unwrap_or_else(|error| {
                        panic!("storage request failed: {:?}", error.without_url())
                    });
                assert!(response.status().is_success());
                response.bytes().await.unwrap_or_else(|error| {
                    panic!("storage body failed: {:?}", error.without_url())
                });
            });
        }
    }
}
