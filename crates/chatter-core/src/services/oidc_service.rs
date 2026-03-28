//! OIDC authentication service.
//!
//! Implements OpenID Connect Authorization Code Flow with PKCE for desktop
//! SSO login. Handles provider discovery, browser-based authentication,
//! token exchange, storage, and refresh.
//!
//! # Security
//!
//! - PKCE required (prevents authorization code interception)
//! - State parameter for CSRF protection
//! - Localhost-only callback (127.0.0.1)
//! - Tokens stored in SQLCipher-encrypted settings
//! - Public client (no client secret — desktop app can't keep secrets)

use std::io::{BufRead, BufReader, Write as IoWrite};
use std::net::TcpListener;
use std::sync::Arc;

use openidconnect::{
    AuthorizationCode, ClientId, CsrfToken, EndpointMaybeSet, EndpointNotSet, EndpointSet,
    IssuerUrl, Nonce, OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl,
    Scope,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata, CoreTokenResponse},
};
use tokio::sync::Mutex;
use url::Url;

use crate::storage::repositories::settings_repo::{SettingsRepository, keys};

/// OIDC client type alias that allows endpoints to be set.
type OidcClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// OIDC authentication errors.
#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    /// Provider discovery failed.
    #[error("OIDC discovery failed: {0}")]
    DiscoveryFailed(String),

    /// Authorization flow failed.
    #[error("OIDC auth flow failed: {0}")]
    AuthFlowFailed(String),

    /// Token exchange failed.
    #[error("OIDC token exchange failed: {0}")]
    TokenExchangeFailed(String),

    /// Token refresh failed.
    #[error("OIDC token refresh failed: {0}")]
    RefreshFailed(String),

    /// No stored tokens available.
    #[error("no OIDC tokens stored")]
    NoTokens,

    /// Tokens have expired and cannot be refreshed.
    #[error("OIDC tokens expired")]
    TokensExpired,

    /// Storage error.
    #[error("storage error: {0}")]
    Storage(#[from] crate::error::StorageError),

    /// Callback server error.
    #[error("callback server error: {0}")]
    CallbackError(String),
}

/// Tokens returned from OIDC flow.
#[derive(Debug, Clone)]
pub struct OidcTokens {
    /// Access token (JWT) for API authentication.
    pub access_token: String,
    /// Refresh token for obtaining new access tokens.
    pub refresh_token: Option<String>,
    /// ID token (JWT) containing user claims.
    pub id_token: Option<String>,
    /// When the access token expires (Unix timestamp seconds).
    pub expires_at: Option<u64>,
}

/// Pending authorization flow state.
struct PendingAuth {
    pkce_verifier: PkceCodeVerifier,
    csrf_token: CsrfToken,
    nonce: Nonce,
    /// Kept alive to prevent TOCTOU port races — another process cannot steal
    /// the port between `start_auth_flow()` and `wait_for_callback()`.
    listener: TcpListener,
    redirect_port: u16,
}

/// OIDC authentication service.
///
/// Manages the OIDC Authorization Code Flow with PKCE for desktop SSO.
pub struct OidcService {
    settings_repo: Arc<SettingsRepository>,
    client: Mutex<Option<OidcClient>>,
    pending_auth: Mutex<Option<PendingAuth>>,
    refresh_guard: Mutex<()>,
}

impl OidcService {
    /// Create a new OIDC service.
    pub fn new(settings_repo: Arc<SettingsRepository>) -> Self {
        Self {
            settings_repo,
            client: Mutex::new(None),
            pending_auth: Mutex::new(None),
            refresh_guard: Mutex::new(()),
        }
    }

    /// Discover OIDC provider and configure client.
    ///
    /// Fetches provider metadata from `{issuer_url}/.well-known/openid-configuration`
    /// and creates the OIDC client.
    pub async fn discover(
        &self,
        issuer_url: &str,
        client_id: &str,
    ) -> Result<(), OidcError> {
        let issuer = IssuerUrl::new(issuer_url.to_string())
            .map_err(|e| OidcError::DiscoveryFailed(format!("invalid issuer URL: {}", e)))?;

        // Use async discovery
        let http_client = reqwest::Client::new();
        let metadata = CoreProviderMetadata::discover_async(issuer, &http_client)
            .await
            .map_err(|e| OidcError::DiscoveryFailed(e.to_string()))?;

        // Build client (public client — no secret)
        let oidc_client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(client_id.to_string()),
            None, // No client secret for public clients
        );

        // Store client
        *self.client.lock().await = Some(oidc_client.into());

        // Persist config for session restore
        self.settings_repo.set_string(keys::OIDC_ISSUER_URL, issuer_url)?;
        self.settings_repo.set_string(keys::OIDC_CLIENT_ID, client_id)?;

        tracing::info!(issuer = %issuer_url, client_id = %client_id, "OIDC provider discovered");
        Ok(())
    }

    /// Start the authorization flow.
    pub async fn start_auth_flow(&self) -> Result<(Url, u16), OidcError> {
        let client_guard = self.client.lock().await;
        let client = client_guard
            .as_ref()
            .ok_or_else(|| OidcError::AuthFlowFailed("OIDC client not configured".into()))?;

        // Bind localhost callback server on random port
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| OidcError::CallbackError(format!("failed to bind callback server: {}", e)))?;
        let port = listener.local_addr()
            .map_err(|e| OidcError::CallbackError(format!("failed to get port: {}", e)))?
            .port();

        listener.set_nonblocking(true)
            .map_err(|e| OidcError::CallbackError(format!("failed to set non-blocking: {}", e)))?;

        let redirect_url = format!("http://127.0.0.1:{}/oauth2callback", port);
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let csrf_token = CsrfToken::new_random();
        let nonce = Nonce::new_random();

        // Build authorization URL
        let (auth_url, csrf_state, nonce_out) = client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                move || csrf_token.clone(),
                move || nonce.clone(),
            )
            
            .add_scope(Scope::new("openid".to_string()))
            .add_scope(Scope::new("profile".to_string()))
            .add_scope(Scope::new("email".to_string()))
            .set_pkce_challenge(pkce_challenge)
            .set_redirect_uri(std::borrow::Cow::Owned(
                RedirectUrl::new(redirect_url)
                    .map_err(|e| OidcError::AuthFlowFailed(format!("invalid redirect URL: {}", e)))?,
            ))
            .url();

        *self.pending_auth.lock().await = Some(PendingAuth {
            pkce_verifier,
            csrf_token: csrf_state,
            nonce: nonce_out,
            listener,
            redirect_port: port,
        });

        Ok((auth_url.into(), port))
    }

    /// Wait for the OIDC callback and exchange the authorization code for tokens.
    pub async fn wait_for_callback(&self) -> Result<OidcTokens, OidcError> {
        let pending = self.pending_auth.lock().await.take()
            .ok_or_else(|| OidcError::AuthFlowFailed("no pending auth flow".into()))?;

        let PendingAuth { pkce_verifier, csrf_token, nonce, listener, redirect_port } = pending;
        let expected_state = csrf_token.secret().clone();

        let (code, state) = tokio::task::spawn_blocking(move || {
            let timeout = std::time::Duration::from_secs(300);
            listener.set_nonblocking(true).ok();
            let start = std::time::Instant::now();
            loop {
                match listener.accept() {
                    Ok((mut stream, _addr)) => {
                        stream.set_nonblocking(false).ok();
                        let mut reader = BufReader::new(&stream);
                        let mut request_line = String::new();
                        reader.read_line(&mut request_line).ok();
                        let path = request_line.split_whitespace().nth(1).unwrap_or("/");
                        let url = Url::parse(&format!("http://127.0.0.1{}", path)).unwrap();
                        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
                        let code = params.get("code").cloned().unwrap_or_default();
                        let state = params.get("state").cloned().unwrap_or_default();
                        let response_body = "<html><body><h2>Authentication successful</h2></body></html>";
                        let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", response_body.len(), response_body);
                        let _ = stream.write_all(response.as_bytes());
                        return Ok((code, state));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if start.elapsed() > timeout { return Err("timeout"); }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Err(_) => { return Err("accept error"); }
                }
            }
        }).await.unwrap().map_err(|e| OidcError::CallbackError(e.to_string()))?;

        if state != expected_state {
            return Err(OidcError::AuthFlowFailed("CSRF state mismatch".into()));
        }

        self.exchange_code(&code, pkce_verifier, nonce, redirect_port).await
    }

    /// Exchange authorization code for tokens.
    ///
    /// Verifies the ID token nonce claim matches `expected_nonce` per
    /// OIDC Core spec §3.1.3.7 to prevent ID token replay attacks.
    async fn exchange_code(
        &self,
        code: &str,
        pkce_verifier: PkceCodeVerifier,
        expected_nonce: Nonce,
        redirect_port: u16,
    ) -> Result<OidcTokens, OidcError> {
        let client_guard = self.client.lock().await;
        let client = client_guard
            .as_ref()
            .ok_or_else(|| OidcError::TokenExchangeFailed("OIDC client not configured".into()))?;

        let redirect_url = RedirectUrl::new(format!("http://127.0.0.1:{}/oauth2callback", redirect_port))
            .map_err(|e| OidcError::TokenExchangeFailed(format!("invalid redirect URL: {}", e)))?;

        let http_client = reqwest::Client::new();
        let token_response: CoreTokenResponse = client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .map_err(|e| OidcError::TokenExchangeFailed(format!("config error: {}", e)))?
            .set_pkce_verifier(pkce_verifier)
            .set_redirect_uri(std::borrow::Cow::Owned(redirect_url))
            .request_async(&http_client)
            .await
            .map_err(|e| OidcError::TokenExchangeFailed(e.to_string()))?;

        // Verify ID token nonce claim matches our stored value (OIDC Core §3.1.3.7).
        if let Some(id_token_jwt) = token_response.extra_fields().id_token() {
            let id_token_str = id_token_jwt.to_string();
            let parts: Vec<&str> = id_token_str.split('.').collect();
            if parts.len() == 3 {
                if let Ok(payload) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]) {
                    if let Ok(claims) = serde_json::from_slice::<serde_json::Value>(&payload) {
                        let token_nonce = claims.get("nonce").and_then(|v| v.as_str());
                        match token_nonce {
                            Some(n) if n == expected_nonce.secret() => { /* nonce matches */ }
                            Some(_) => {
                                return Err(OidcError::TokenExchangeFailed(
                                    "ID token nonce mismatch — possible replay".into(),
                                ));
                            }
                            None => {
                                return Err(OidcError::TokenExchangeFailed(
                                    "ID token missing nonce claim".into(),
                                ));
                            }
                        }
                    }
                }
            }
        }

        let access_token = token_response.access_token().secret().clone();
        let refresh_token = token_response.refresh_token().map(|t| t.secret().clone());
        let id_token = token_response.extra_fields().id_token().map(|t| t.to_string());
        let expires_at = token_response.expires_in().map(|d| {
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + d.as_secs()
        });

        let tokens = OidcTokens { access_token, refresh_token, id_token, expires_at };
        self.store_tokens(&tokens)?;
        Ok(tokens)
    }

    /// Refresh the access token using the stored refresh token.
    pub async fn refresh_tokens(&self) -> Result<OidcTokens, OidcError> {
        // Serialize concurrent refresh attempts — only one caller exchanges at a time.
        let _refresh_lock = self.refresh_guard.lock().await;

        // Re-check after acquiring lock: another caller may have already refreshed.
        if !self.needs_refresh() {
            return self.get_access_token().map(|access_token| {
                OidcTokens {
                    access_token,
                    refresh_token: self.settings_repo.get_string(keys::OIDC_REFRESH_TOKEN).ok().flatten(),
                    id_token: self.settings_repo.get_string(keys::OIDC_ID_TOKEN).ok().flatten(),
                    expires_at: self.settings_repo.get::<u64>(keys::OIDC_EXPIRES_AT).ok().flatten(),
                }
            });
        }

        let client_guard = self.client.lock().await;
        let client = client_guard
            .as_ref()
            .ok_or_else(|| OidcError::RefreshFailed("OIDC client not configured".into()))?;

        let rt_str = self.settings_repo.get_string(keys::OIDC_REFRESH_TOKEN)
            .map_err(OidcError::Storage)?.ok_or(OidcError::NoTokens)?;

        let http_client = reqwest::Client::new();
        let token_response: CoreTokenResponse = client
            .exchange_refresh_token(&openidconnect::RefreshToken::new(rt_str))
            .map_err(|e| OidcError::RefreshFailed(format!("config error: {}", e)))?
            .request_async(&http_client)
            .await
            .map_err(|e| OidcError::RefreshFailed(e.to_string()))?;

        let access_token = token_response.access_token().secret().clone();
        let refresh_token = token_response.refresh_token().map(|t| t.secret().clone());
        let id_token = token_response.extra_fields().id_token().map(|t| t.to_string());
        let expires_at = token_response.expires_in().map(|d| {
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + d.as_secs()
        });

        let tokens = OidcTokens { access_token, refresh_token, id_token, expires_at };
        self.store_tokens(&tokens)?;
        Ok(tokens)
    }

    pub fn get_access_token(&self) -> Result<String, OidcError> {
        let token = self.settings_repo.get_string(keys::OIDC_ACCESS_TOKEN).map_err(OidcError::Storage)?.ok_or(OidcError::NoTokens)?;
        if let Ok(Some(exp)) = self.settings_repo.get::<u64>(keys::OIDC_EXPIRES_AT) {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            if now >= exp { return Err(OidcError::TokensExpired); }
        }
        Ok(token)
    }

    pub fn is_oidc_session(&self) -> bool { self.settings_repo.get::<bool>(keys::OIDC_SESSION).ok().flatten().unwrap_or(false) }
    pub fn needs_refresh(&self) -> bool {
        if let Ok(Some(exp)) = self.settings_repo.get::<u64>(keys::OIDC_EXPIRES_AT) {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            now + 300 >= exp
        } else { false }
    }

    /// Extract display-only claims (sub, email, name) from the stored ID token.
    ///
    /// # Safety — Display Only
    ///
    /// This performs a raw base64 decode of the JWT payload **without signature
    /// verification** against the provider's JWKS. The returned claims MUST NOT
    /// be used for authorization decisions. The server validates the access token
    /// for all API calls; these claims are only for showing the logged-in user's
    /// name/email in the local UI.
    pub fn parse_id_token_display_claims(&self) -> Option<(String, Option<String>, Option<String>)> {
        let id_token = self.settings_repo.get_string(keys::OIDC_ID_TOKEN).ok()??;
        let parts: Vec<&str> = id_token.split('.').collect();
        if parts.len() != 3 { return None; }
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
        let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
        let sub = claims.get("sub")?.as_str()?.to_string();
        let email = claims.get("email").and_then(|v| v.as_str()).map(String::from);
        let name = claims.get("name").or_else(|| claims.get("preferred_username")).and_then(|v| v.as_str()).map(String::from);
        Some((sub, email, name))
    }

    fn store_tokens(&self, tokens: &OidcTokens) -> Result<(), OidcError> {
        self.settings_repo.set_string(keys::OIDC_ACCESS_TOKEN, &tokens.access_token)?;
        self.settings_repo.set(keys::OIDC_SESSION, &true)?;
        if let Some(ref rt) = tokens.refresh_token { self.settings_repo.set_string(keys::OIDC_REFRESH_TOKEN, rt)?; }
        if let Some(ref idt) = tokens.id_token { self.settings_repo.set_string(keys::OIDC_ID_TOKEN, idt)?; }
        if let Some(exp) = tokens.expires_at { self.settings_repo.set(keys::OIDC_EXPIRES_AT, &exp)?; }
        Ok(())
    }

    pub fn clear_tokens(&self) -> Result<(), OidcError> {
        let _ = self.settings_repo.delete(keys::OIDC_SESSION);
        let _ = self.settings_repo.delete(keys::OIDC_ACCESS_TOKEN);
        let _ = self.settings_repo.delete(keys::OIDC_REFRESH_TOKEN);
        let _ = self.settings_repo.delete(keys::OIDC_ID_TOKEN);
        let _ = self.settings_repo.delete(keys::OIDC_EXPIRES_AT);
        Ok(())
    }
}

use base64::Engine;
