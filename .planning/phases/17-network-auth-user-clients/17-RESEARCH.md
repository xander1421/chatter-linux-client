# Phase 17: Network Auth & User Clients - Research

**Researched:** 2026-02-03
**Domain:** tonic gRPC service clients for authentication and user management
**Confidence:** HIGH

<research_summary>
## Summary

Researched gRPC client implementation patterns for Chatter's authentication and user services using tonic in Rust. The architecture leverages Phase 16's GrpcClientPool and AuthInterceptor foundations, wrapping the generated tonic clients with ergonomic Rust APIs.

Key findings:
1. Tonic-generated clients (`AuthServiceClient`, `UserServiceClient`) are created from a shared `Channel` and use interceptors for authentication
2. Signal-style device authentication uses Basic auth with format `{uuid}.{device_id}:{token}` (already implemented in AuthInterceptor)
3. Server streaming responses (Register, DeleteAccount) require special handling with tokio streams
4. Error handling should map gRPC status codes to domain-specific errors using the `tonic::Status` API

**Primary recommendation:** Implement thin wrapper clients around tonic-generated code that handle credential management, map errors to domain types, and provide ergonomic async APIs. Use the existing AuthInterceptor for all authentication modes.
</research_summary>

<standard_stack>
## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| tonic | 0.14.x | gRPC client/server framework | De facto Rust gRPC, async/await native |
| prost | 0.13.x | Protocol buffer serialization | Standard pairing with tonic |
| tokio | 1.x | Async runtime | Required by tonic, de facto Rust async |
| parking_lot | 0.12.x | Sync primitives | Already used in Phase 16 interceptors |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| base64 | 0.22.x | Credential encoding | Signal Basic auth encoding |
| uuid | 1.x | Request ID generation | Idempotency keys |
| thiserror | 2.x | Error types | Domain error definitions |
| tokio-stream | 0.1.x | Stream utilities | Server streaming responses |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| tonic | grpc (C++ bindings) | tonic is pure Rust, better async |
| parking_lot | std::sync | parking_lot faster, no poisoning |
| thiserror | anyhow | thiserror gives typed errors for matching |

**Already in Cargo.toml:**
```toml
# From Phase 16
tonic = { version = "0.14", features = ["transport", "tls"] }
prost = "0.13"
prost-types = "0.13"
tokio = { version = "1", features = ["full"] }
parking_lot = "0.12"
base64 = "0.22"
```
</standard_stack>

<architecture_patterns>
## Architecture Patterns

### Recommended Module Structure
```
src/network/grpc/
├── mod.rs              # Re-exports
├── client_pool.rs      # GrpcClientPool (Phase 16) ✓
├── interceptors.rs     # AuthInterceptor (Phase 16) ✓
├── auth_client.rs      # AuthClient wrapper
├── user_client.rs      # UserClient wrapper
├── error.rs            # gRPC error types (shared)
└── ...                 # Other clients (Phase 18+)
```

### Pattern 1: Service Client Wrapper
**What:** Thin wrapper around generated tonic client that holds channel + interceptor
**When to use:** Every gRPC service client
**Example:**
```rust
// Source: tonic generated client pattern + interceptor docs
use std::sync::Arc;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::proto::chat_v1::auth_service_client::AuthServiceClient;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::error::NetworkError;

pub struct AuthClient {
    inner: AuthServiceClient<InterceptedService<Channel, AuthInterceptor>>,
}

impl AuthClient {
    /// Create a new AuthClient from pool components.
    pub fn new(channel: Channel, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: AuthServiceClient::with_interceptor(channel, interceptor),
        }
    }

    /// Login with username and password.
    pub async fn login(
        &mut self,
        username: &str,
        password: &[u8],
        device_info: &str,
    ) -> Result<AuthResponse, NetworkError> {
        let request = LoginRequest {
            username: username.to_string(),
            password_hash: password.to_vec(),
            // ... other fields
        };

        let response = self.inner.login(request)
            .await
            .map_err(NetworkError::from)?;

        Ok(response.into_inner())
    }
}
```

### Pattern 2: Streaming Response Handling
**What:** Convert server streaming responses to collected results or async iterators
**When to use:** Register, DeleteAccount, UpdateUserProfile, etc.
**Example:**
```rust
// Source: tonic streaming + tokio-stream patterns
use tokio_stream::StreamExt;

impl AuthClient {
    /// Register a new user, streaming progress events.
    ///
    /// Returns a stream of AuthEvent for UI progress display.
    pub async fn register(
        &mut self,
        request: RegisterRequest,
    ) -> Result<impl Stream<Item = Result<AuthEvent, NetworkError>>, NetworkError> {
        let response = self.inner.register(request)
            .await
            .map_err(NetworkError::from)?;

        Ok(response.into_inner().map(|r| r.map_err(NetworkError::from)))
    }

    /// Register and wait for completion.
    ///
    /// Consumes all events and returns final session on success.
    pub async fn register_and_wait(
        &mut self,
        request: RegisterRequest,
    ) -> Result<Session, NetworkError> {
        let stream = self.register(request).await?;
        tokio::pin!(stream);

        let mut session = None;
        while let Some(event) = stream.next().await {
            let event = event?;
            if let Some(auth_event::Payload::SessionCreated(s)) = event.payload {
                session = Some(s);
            }
            if let Some(auth_event::Payload::RegistrationError(e)) = event.payload {
                return Err(NetworkError::ServerError(e.message));
            }
        }

        session.ok_or(NetworkError::NoSession)
    }
}
```

### Pattern 3: Credential Management After Login
**What:** Store credentials and update interceptor after successful auth
**When to use:** After login/register success
**Example:**
```rust
// Source: Signal Desktop pattern from User.dom.ts
impl AuthClient {
    /// Login and configure authentication for subsequent requests.
    ///
    /// On success, sets Basic auth credentials on the interceptor.
    pub async fn login_and_configure(
        &mut self,
        pool: &GrpcClientPool,
        username: &str,
        password: &[u8],
        device_info: &str,
        device_auth_token: &[u8],
    ) -> Result<AuthResponse, NetworkError> {
        let response = self.login(username, password, device_info).await?;

        // Extract session info
        let session = response.session.as_ref()
            .ok_or(NetworkError::NoSession)?;

        // Configure interceptor with Signal-style Basic auth
        // Format: {uuid}.{device_id}:{device_auth_token}
        let user_id = response.user_profile.as_ref()
            .map(|p| p.user_id.as_str())
            .ok_or(NetworkError::NoProfile)?;
        let device_id = session.device_id;

        let auth_username = format!("{}.{}", user_id, device_id);
        let auth_password = base64::engine::general_purpose::STANDARD
            .encode(device_auth_token);

        pool.auth().set_basic(auth_username, auth_password);

        Ok(response)
    }
}
```

### Pattern 4: Error Mapping
**What:** Convert tonic::Status to domain NetworkError
**When to use:** All gRPC calls
**Example:**
```rust
// Source: tonic Status docs + tonic-richer-error patterns
use tonic::{Code, Status};

impl From<Status> for NetworkError {
    fn from(status: Status) -> Self {
        match status.code() {
            Code::Unauthenticated => NetworkError::Unauthenticated(status.message().to_string()),
            Code::PermissionDenied => NetworkError::PermissionDenied(status.message().to_string()),
            Code::NotFound => NetworkError::NotFound(status.message().to_string()),
            Code::InvalidArgument => NetworkError::InvalidArgument(status.message().to_string()),
            Code::AlreadyExists => NetworkError::AlreadyExists(status.message().to_string()),
            Code::ResourceExhausted => NetworkError::RateLimited(status.message().to_string()),
            Code::Unavailable => NetworkError::Unavailable(status.message().to_string()),
            Code::DeadlineExceeded => NetworkError::Timeout(status.message().to_string()),
            Code::Internal => NetworkError::Internal(status.message().to_string()),
            _ => NetworkError::Unknown(status.code() as i32, status.message().to_string()),
        }
    }
}
```

### Anti-Patterns to Avoid
- **Creating channel per request:** Use shared Channel from GrpcClientPool, it's cheap to clone
- **Ignoring streaming errors:** Server streams can fail mid-stream, handle each item's Result
- **Storing credentials in client:** Store in secure storage (keyring), not in memory struct
- **Blocking on async in interceptors:** AuthInterceptor must be sync (tonic limitation), use RwLock not async mutex
</architecture_patterns>

<dont_hand_roll>
## Don't Hand-Roll

Problems that have existing solutions:

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| gRPC client code | Custom HTTP/2 client | tonic-generated clients | Handles all proto encoding, streaming, compression |
| Basic auth encoding | Manual string formatting | `base64::engine::general_purpose::STANDARD.encode()` | Correct padding, URL-safe variants available |
| Request IDs | Custom ID generation | `uuid::Uuid::new_v4().to_string()` | Guaranteed uniqueness, standard format |
| Stream processing | Manual async iteration | `tokio_stream::StreamExt` | Efficient, composable, handles backpressure |
| Error retry logic | Custom retry loops | Phase 21 retry module (later) | Proper backoff, jitter, cancellation |

**Key insight:** Tonic's generated code handles all the hard parts of gRPC (framing, HTTP/2 streams, deadlines, compression). The wrapper clients just need to provide ergonomic APIs and credential management.
</dont_hand_roll>

<common_pitfalls>
## Common Pitfalls

### Pitfall 1: Forgetting to Clone Channel Before Creating Client
**What goes wrong:** Channel consumed, can't create other clients
**Why it happens:** Channel is Clone but looks like it might not be
**How to avoid:** Always `channel.clone()` when passing to client constructors
**Warning signs:** Compile error "value moved"

### Pitfall 2: Not Handling Registration Stream Completion
**What goes wrong:** Client hangs or misses final session
**Why it happens:** Stream not fully consumed, or error event ignored
**How to avoid:** Consume stream until `SessionCreated` or `RegistrationError` received
**Warning signs:** Login works but register hangs

### Pitfall 3: Using Wrong Auth Mode After Login
**What goes wrong:** Subsequent requests fail with Unauthenticated
**Why it happens:** Interceptor still in Bearer mode instead of Basic
**How to avoid:** Call `pool.auth().set_basic()` after successful login
**Warning signs:** First request works, all subsequent fail

### Pitfall 4: Blocking in Interceptor
**What goes wrong:** Deadlock or poor performance
**Why it happens:** Using async operations or blocking mutex in sync interceptor
**How to avoid:** Use `parking_lot::RwLock` (already done), keep interceptor logic simple
**Warning signs:** Requests hang, or intermittent timeouts

### Pitfall 5: Not Generating Request IDs for Idempotent Operations
**What goes wrong:** Duplicate operations on retry
**Why it happens:** Missing request_id field
**How to avoid:** Always generate UUID for any mutating operation
**Warning signs:** Duplicate registrations, double message sends
</common_pitfalls>

<code_examples>
## Code Examples

Verified patterns from tonic docs and existing codebase:

### Creating Client from Pool
```rust
// Source: Phase 16 GrpcClientPool pattern
use crate::network::grpc::{GrpcClientPool, AuthClient, UserClient};

// Pool manages connection and interceptor
let pool = GrpcClientPool::new("http://localhost:50051")?;
pool.connect().await?;

// Create clients from pool
let auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
let user_client = UserClient::new(pool.channel(), pool.auth_clone());
```

### Full Login Flow
```rust
// Source: Signal Desktop User.dom.ts + tonic patterns
use crate::proto::chat_v1::{LoginRequest, Session};

let request = LoginRequest {
    username: "alice".to_string(),
    password_hash: password_bytes.to_vec(),
    public_key: identity_key.to_vec(),
    device_info: "linux-desktop".to_string(),
    request_id: uuid::Uuid::new_v4().to_string(),
    device_auth_token: device_token.to_vec(),
    unidentified_access_key: uak.to_vec(),
    device_id: 0, // 0 = new device
};

let response = auth_client.login(request).await?;

// Store session details securely
let session = response.session.expect("login returns session");
keyring.store_device_token(
    &response.user_profile.as_ref().unwrap().user_id,
    session.device_id,
    &session.device_auth_token,
)?;

// Configure auth for subsequent requests
pool.auth().set_basic(
    format!("{}.{}", user_id, session.device_id),
    base64::encode(&session.device_auth_token),
);
```

### Registration with Progress Events
```rust
// Source: tonic streaming patterns
use tokio_stream::StreamExt;
use crate::proto::chat_v1::{AuthEvent, auth_event};

let stream = auth_client.register(register_request).await?;
tokio::pin!(stream);

while let Some(result) = stream.next().await {
    let event = result?;

    match event.status() {
        auth_event::RegistrationStatus::AccountCreating => {
            ui.show_progress("Creating account...");
        }
        auth_event::RegistrationStatus::AuthzRelationsInitialized => {
            ui.show_progress("Setting up permissions...");
        }
        auth_event::RegistrationStatus::RegistrationCompleted => {
            if let Some(auth_event::Payload::SessionCreated(session)) = event.payload {
                return Ok(session);
            }
        }
        auth_event::RegistrationStatus::RegistrationFailed => {
            if let Some(auth_event::Payload::RegistrationError(error)) = event.payload {
                return Err(NetworkError::RegistrationFailed(error.message));
            }
        }
        _ => {}
    }
}
```

### UserClient Friend Operations
```rust
// Source: user.proto definitions
use crate::proto::chat_v1::{FriendRequest, FriendResponse, FriendStatus};

impl UserClient {
    pub async fn send_friend_request(
        &mut self,
        user_id: &str,
        message: Option<&str>,
    ) -> Result<impl Stream<Item = Result<UserEvent, NetworkError>>, NetworkError> {
        let request = FriendRequest {
            user_id: user_id.to_string(),
            message: message.unwrap_or("").to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self.inner.send_friend_request(request)
            .await
            .map_err(NetworkError::from)?;

        Ok(response.into_inner().map(|r| r.map_err(NetworkError::from)))
    }

    pub async fn list_friends(
        &mut self,
        limit: i32,
        offset: i32,
    ) -> Result<Vec<UserProfile>, NetworkError> {
        let request = ListFriendsRequest { limit, offset };

        let response = self.inner.list_friends(request)
            .await
            .map_err(NetworkError::from)?;

        Ok(response.into_inner().friends)
    }
}
```
</code_examples>

<sota_updates>
## State of the Art (2025-2026)

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| tonic 0.10 | tonic 0.14 | 2025 | New codec API, better interceptors |
| Manual retry | tonic-retry / tower | 2024+ | Use tower Service layers for retry |
| Sync interceptors only | tonic-async-interceptor | 2024 | Async interceptors possible but complex |

**New tools/patterns to consider:**
- **tonic-richer-error:** Standard Google error details extraction
- **tower layers:** Stack retry, timeout, rate limiting as Service layers

**Deprecated/outdated:**
- **tonic 0.6-0.10:** Older API, different codec patterns
- **Manual channel management:** Use Endpoint/Channel abstraction
</sota_updates>

<open_questions>
## Open Questions

Things that couldn't be fully resolved:

1. **Sealed Sender Certificate Caching**
   - What we know: Certificate valid for ~7 days, should be cached
   - What's unclear: Where to store (memory? SQLite? keyring?)
   - Recommendation: Store in SQLite with expiry check on load (Phase 30+)

2. **Multi-Device Prekey Fetching**
   - What we know: GetPreKeyBundle supports device_id="*" for all devices
   - What's unclear: How to handle mixed success (some devices fail)
   - Recommendation: Return partial results with error list (handle in service layer)
</open_questions>

<sources>
## Sources

### Primary (HIGH confidence)
- [tonic docs.rs](https://docs.rs/tonic/latest/tonic/) - client, interceptor, streaming APIs
- Chatter auth.proto - Service definitions, message types
- Chatter user.proto - Service definitions, message types
- Phase 16 implementation - GrpcClientPool, AuthInterceptor patterns

### Secondary (MEDIUM confidence)
- [tonic GitHub](https://github.com/hyperium/tonic) - Current 0.14.x branch
- Signal Desktop `User.dom.ts` - Credential storage patterns
- Signal Desktop `WebAPI.preload.ts` - HTTP client patterns

### Tertiary (LOW confidence - needs validation)
- [tonic-richer-error](https://docs.rs/tonic-richer-error) - Not currently needed, evaluate later
</sources>

<metadata>
## Metadata

**Research scope:**
- Core technology: tonic gRPC client implementation
- Ecosystem: tokio-stream, base64, uuid
- Patterns: Wrapper clients, streaming, credential management
- Pitfalls: Channel cloning, stream handling, auth state

**Confidence breakdown:**
- Standard stack: HIGH - tonic already in use, well documented
- Architecture: HIGH - follows Phase 16 patterns, verified with tonic docs
- Pitfalls: HIGH - derived from tonic docs and common Rust patterns
- Code examples: HIGH - based on existing proto definitions and tonic patterns

**Research date:** 2026-02-03
**Valid until:** 2026-03-03 (30 days - tonic ecosystem stable)
</metadata>

---

*Phase: 17-network-auth-user-clients*
*Research completed: 2026-02-03*
*Ready for planning: yes*
