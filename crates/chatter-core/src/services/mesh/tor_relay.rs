//! `RelayConnector` over the Tor client chatter already owns.
//!
//! `chatter_mesh_nostr` opens no sockets: its relay client takes a
//! [`RelayConnector`] so the integration layer decides how a relay is reached.
//! For mesh traffic that decision is not optional — a Nostr relay sees every
//! geohash message a device publishes, and a direct connection would hand it the
//! user's IP alongside their approximate location.
//!
//! So this dials over **the existing Arti client**, the same
//! [`SharedTorClient`] the gRPC pool bootstraps, on the same dedicated Tor
//! runtime. There is no second Tor stack here, and adding one would be a bug:
//! bootstrap is slow and a second directory manager would duplicate all of it.
//!
//! # Circuit isolation
//!
//! Relay traffic runs on an **isolated** client derived from the shared one, in
//! the same way the anonymous mailbox channel does. Without it, a relay
//! connection and an authenticated gRPC call could share a circuit, and the
//! whole point of keeping the mesh identity separate from the chatter account is
//! that the two must not be joinable.
//!
//! [`RelayConnector`]: chatter_mesh_nostr::relay::RelayConnector

#![cfg(feature = "onion")]

use std::sync::Arc;
use std::time::Duration;

use arti_client::{DataStream, TorClient};
use chatter_mesh_nostr::NostrError;
use chatter_mesh_nostr::relay::{
    BoxFuture, RelayConnection, RelayConnector, RelaySink, RelayStream,
};
use tokio::sync::OnceCell;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tor_rtcompat::PreferredRuntime;

use crate::network::grpc::onion_transport::{SharedTorClient, tor_runtime};

use super::ws;

/// How long a bootstrap may take before it is a retryable error rather than a
/// permanent wedge. Matches the gRPC connector.
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(120);
/// How long one relay dial may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(90);

fn relay_error(message: impl Into<String>) -> NostrError {
    NostrError::Relay(message.into())
}

/// A relay connector that dials over Tor.
pub struct TorRelayConnector {
    /// The pool's shared client. Bootstrapped once, by whichever path gets there
    /// first.
    shared: SharedTorClient,
    /// This connector's isolated circuit group, derived from `shared` on first
    /// use so every relay dial reuses one isolation group distinct from the
    /// authenticated RPC traffic.
    isolated: OnceCell<Arc<TorClient<PreferredRuntime>>>,
}

impl TorRelayConnector {
    #[must_use]
    pub fn new(shared: SharedTorClient) -> Self {
        Self {
            shared,
            isolated: OnceCell::new(),
        }
    }

    /// Bootstrap (once) and hand back the isolated client.
    async fn client(&self) -> Result<Arc<TorClient<PreferredRuntime>>, NostrError> {
        let base = self
            .shared
            .get_or_try_init(|| async {
                // Spawned onto the dedicated Tor runtime rather than awaited
                // inline, so Arti binds to those threads however the init race
                // resolves. This must stay identical to the gRPC connector's
                // init or the two paths fight over which runtime owns Arti.
                let config = crate::network::grpc::onion_transport::tor_client_config()
                    .map_err(relay_error)?;
                let started = tor_runtime().spawn(async {
                    // Arti's rustls runtime needs a process-wide default
                    // provider. Idempotent: an error means someone installed one
                    // already.
                    let _ =
                        tokio_rustls::rustls::crypto::ring::default_provider().install_default();
                    TorClient::create_bootstrapped(config).await
                });
                match tokio::time::timeout(BOOTSTRAP_TIMEOUT, started).await {
                    Err(_) => Err(relay_error("Tor bootstrap timed out after 120s")),
                    Ok(Err(join)) => Err(relay_error(format!("Tor bootstrap panicked: {join}"))),
                    Ok(Ok(Err(error))) => {
                        Err(relay_error(format!("Tor bootstrap failed: {error}")))
                    }
                    Ok(Ok(Ok(client))) => Ok(client),
                }
            })
            .await?;

        let isolated = self
            .isolated
            .get_or_init(|| async { base.isolated_client() })
            .await;
        Ok(isolated.clone())
    }

    /// Open a Tor stream to `host:port`.
    async fn dial(&self, host: &str, port: u16) -> Result<DataStream, NostrError> {
        let client = self.client().await?;
        let (dial_host, dial_port) = (host.to_string(), port);
        // Dialled on the Tor runtime too, and bounded: Arti's `connect` has no
        // internal deadline, so a stuck rendezvous would otherwise hang a relay
        // slot forever.
        let started = tor_runtime()
            .spawn(async move { client.connect((dial_host.as_str(), dial_port)).await });
        match tokio::time::timeout(CONNECT_TIMEOUT, started).await {
            Err(_) => Err(relay_error(format!(
                "Tor connect to {host}:{port} timed out"
            ))),
            Ok(Err(join)) => Err(relay_error(format!("Tor connect panicked: {join}"))),
            Ok(Ok(Err(error))) => Err(relay_error(format!(
                "Tor connect to {host}:{port} failed: {error}"
            ))),
            Ok(Ok(Ok(stream))) => Ok(stream),
        }
    }
}

/// Where a relay URL points, and whether it is TLS.
struct RelayTarget {
    secure: bool,
    host: String,
    port: u16,
    /// Origin-form request target, query included.
    path: String,
    /// `Host` header: bare host, or `host:port` when the port is non-default.
    authority: String,
}

fn parse_relay_url(raw: &str) -> Result<RelayTarget, NostrError> {
    let parsed =
        url::Url::parse(raw).map_err(|error| relay_error(format!("bad relay URL: {error}")))?;
    let secure = match parsed.scheme() {
        "wss" => true,
        "ws" => false,
        other => return Err(relay_error(format!("not a relay scheme: {other}"))),
    };
    let host = parsed
        .host_str()
        .ok_or_else(|| relay_error("relay URL has no host"))?
        .to_string();
    let default_port = if secure { 443 } else { 80 };
    let port = parsed.port().unwrap_or(default_port);
    let mut path = parsed.path().to_string();
    if path.is_empty() {
        path.push('/');
    }
    if let Some(query) = parsed.query() {
        path.push('?');
        path.push_str(query);
    }
    let authority = if port == default_port {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    Ok(RelayTarget {
        secure,
        host,
        port,
        path,
        authority,
    })
}

/// A rustls config that verifies against the webpki root store.
///
/// Tor hides the client IP from the relay; it does not authenticate the relay.
/// That is still TLS's job, and certificate verification is never disabled here.
fn tls_config() -> Result<ClientConfig, NostrError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| relay_error(format!("TLS provider rejected the defaults: {error}")))
    .map(|builder| builder.with_root_certificates(roots).with_no_client_auth())
}

impl RelayConnector for TorRelayConnector {
    fn connect<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<RelayConnection, NostrError>> {
        Box::pin(async move {
            let target = parse_relay_url(url)?;
            let stream = self.dial(&target.host, target.port).await?;

            if !target.secure {
                // Plain `ws://` over Tor: the exit node sees the traffic. Every
                // relay in the default set is `wss://`, so this only happens for
                // an operator-supplied URL, and it is their call to make.
                let (sink, source) = ws::handshake(stream, &target.authority, &target.path).await?;
                return Ok((
                    Box::new(sink) as Box<dyn RelaySink>,
                    Box::new(source) as Box<dyn RelayStream>,
                ));
            }

            let server_name = ServerName::try_from(target.host.clone()).map_err(|_| {
                relay_error(format!("not a valid TLS server name: {}", target.host))
            })?;
            let tls = TlsConnector::from(Arc::new(tls_config()?))
                .connect(server_name, stream)
                .await
                .map_err(|error| {
                    relay_error(format!(
                        "TLS handshake with {} failed: {error}",
                        target.host
                    ))
                })?;

            let (sink, source) = ws::handshake(tls, &target.authority, &target.path).await?;
            Ok((
                Box::new(sink) as Box<dyn RelaySink>,
                Box::new(source) as Box<dyn RelayStream>,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_urls_parse_into_a_dialable_target() {
        let target = parse_relay_url("wss://relay.damus.io").expect("parsed");
        assert!(target.secure);
        assert_eq!(target.host, "relay.damus.io");
        assert_eq!(target.port, 443);
        assert_eq!(target.path, "/");
        assert_eq!(target.authority, "relay.damus.io");

        let target = parse_relay_url("ws://127.0.0.1:7000/nostr?x=1").expect("parsed");
        assert!(!target.secure);
        assert_eq!(target.port, 7000);
        assert_eq!(target.path, "/nostr?x=1");
        assert_eq!(
            target.authority, "127.0.0.1:7000",
            "a non-default port belongs in the Host header"
        );
    }

    #[test]
    fn only_relay_schemes_are_accepted() {
        assert!(parse_relay_url("https://relay.example").is_err());
        assert!(parse_relay_url("relay.example").is_err());
        assert!(parse_relay_url("").is_err());
    }

    #[test]
    fn the_tls_config_verifies_certificates() {
        // Construction is the assertion: `with_root_certificates` is the only
        // builder path taken, so there is no route here that skips verification.
        assert!(tls_config().is_ok());
    }
}
