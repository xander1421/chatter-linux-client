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
    let pem =
        crate::config::TLS_ROOT_CA_PEM.ok_or_else(|| "no pinned TLS CA for storage".to_string())?;
    let ca = reqwest::Certificate::from_pem(pem.as_bytes())
        .map_err(|e| format!("invalid storage TLS CA: {}", e.without_url()))?;
    reqwest::Client::builder()
        .tls_built_in_root_certs(false)
        .add_root_certificate(ca)
        .https_only(true)
        // Presigned URLs are already final destinations. Never forward their
        // capability query or upload body to a redirect target.
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("could not build pinned storage client: {}", e.without_url()))
}

#[cfg(test)]
mod tests {
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
