# Phase 16 Research: Network gRPC Foundation

## Executive Summary

This phase implements the gRPC network layer foundation for the Chatter native client. Research covers tonic client patterns, Signal-Desktop's network architecture, and best practices for connection management, authentication, and streaming.

**Key Findings:**
1. tonic's Channel is already pooled - cloning is cheap, no custom pooling needed
2. Signal uses dual-socket architecture (authenticated + unauthenticated channels)
3. Interceptors should be layered: sync for metadata, tower middleware for cross-cutting
4. Fibonacci backoff with jitter is optimal for reconnection
5. HTTP/2 keepalive is critical to prevent proxy timeouts

---

## Ecosystem & Libraries

### Core Stack (Already in Cargo.toml)

| Crate | Version | Purpose | Notes |
|-------|---------|---------|-------|
| tonic | 0.12 | gRPC client framework | `tls`, `tls-roots` features enabled |
| prost | 0.13 | Protocol buffer codegen | Already generating types |
| prost-types | 0.13 | Well-known types | Timestamps, Duration, etc. |
| tokio | 1.x | Async runtime | Full features enabled |
| tokio-stream | 0.1 | Stream utilities | For streaming RPCs |

### Recommended Additions

| Crate | Version | Purpose | Required? |
|-------|---------|---------|-----------|
| tower | 0.5 | Middleware layer | Yes - for retry/timeout layers |
| tower-http | 0.6 | HTTP-specific middleware | Optional - for tracing |
| tonic-health | 0.12 | Health checking | Optional - for monitoring |
| tracing | 0.1 | Already present | For request logging |
| backoff | 0.4 | Exponential backoff | Or implement Fibonacci ourselves |

**NOT Needed:**
- `mobc-tonic`: Custom pooling unnecessary - tonic Channel handles it
- `tonic-async-interceptor`: Sync interceptors sufficient for auth token injection
- `tower-retry`: Simpler to implement retry at service level for gRPC-specific codes

---

## Architecture Patterns

### Pattern 1: Channel as Connection Pool

**Key Insight:** tonic's `Channel` is already a connection pool. Cloning is essentially free.

```rust
// Good: Clone channel for each client - shares underlying HTTP/2 connection
pub struct GrpcClientPool {
    channel: Channel,
    auth_token: Arc<RwLock<Option<String>>>,
}

impl GrpcClientPool {
    pub fn auth_client(&self) -> AuthServiceClient<InterceptedService<Channel, AuthInterceptor>> {
        AuthServiceClient::with_interceptor(
            self.channel.clone(),  // Cheap clone - just Arc reference
            AuthInterceptor::new(self.auth_token.clone()),
        )
    }

    pub fn message_client(&self) -> MessageServiceClient<InterceptedService<Channel, AuthInterceptor>> {
        MessageServiceClient::with_interceptor(
            self.channel.clone(),
            AuthInterceptor::new(self.auth_token.clone()),
        )
    }
}
```

**Anti-Pattern:** Creating new Channel per request or per client type.

### Pattern 2: Layered Interceptors

**Three Layers:**

1. **Sync Interceptor** - Auth token injection (fast, per-request):
```rust
#[derive(Clone)]
pub struct AuthInterceptor {
    token: Arc<RwLock<Option<String>>>,
}

impl Interceptor for AuthInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        if let Some(token) = self.token.read().as_ref() {
            let value = format!("Bearer {}", token)
                .parse()
                .map_err(|_| Status::internal("invalid token format"))?;
            request.metadata_mut().insert("authorization", value);
        }
        Ok(request)
    }
}
```

2. **Request Metadata** - User-agent, trace IDs:
```rust
fn metadata_interceptor(mut req: Request<()>) -> Result<Request<()>, Status> {
    req.metadata_mut().insert("x-client-version", "1.0.0".parse().unwrap());
    req.metadata_mut().insert("x-device-id", device_id().parse().unwrap());
    Ok(req)
}
```

3. **Tower Middleware** - Timeout, tracing (wrap channel):
```rust
let channel = Channel::from_static("https://api.chatter.example")
    .connect()
    .await?;

let service = ServiceBuilder::new()
    .layer(TimeoutLayer::new(Duration::from_secs(30)))
    .layer(TraceLayer::new_for_grpc())
    .service(channel);
```

### Pattern 3: Connection State Machine

Signal-inspired state machine with events:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting { attempt: u32 },
    Failed { reason: FailReason },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailReason {
    AuthExpired,
    NetworkError,
    ServerError,
    DeviceDelinked,
}

pub struct ConnectionStateMachine {
    state: Arc<RwLock<ConnectionState>>,
    events: broadcast::Sender<ConnectionEvent>,
}

pub enum ConnectionEvent {
    StateChanged(ConnectionState),
    Online,
    Offline,
    AuthRequired,
}
```

### Pattern 4: Fibonacci Backoff with Jitter

Signal's proven pattern:

```rust
pub struct FibonacciBackoff {
    timeouts: &'static [Duration],
    index: usize,
    jitter: Duration,
}

impl FibonacciBackoff {
    pub const STANDARD: &'static [Duration] = &[
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(3),
        Duration::from_secs(5),
        Duration::from_secs(8),
        Duration::from_secs(13),
        Duration::from_secs(21),
        Duration::from_secs(34),
        Duration::from_secs(55),
    ];

    pub const EXTENDED: &'static [Duration] = &[
        // ... STANDARD ...
        Duration::from_secs(89),
        Duration::from_secs(144),
        Duration::from_secs(233),
        Duration::from_secs(377),
        Duration::from_secs(610),   // ~10 min
    ];

    pub fn next(&mut self) -> Duration {
        let base = self.timeouts.get(self.index)
            .copied()
            .unwrap_or(*self.timeouts.last().unwrap());

        if self.index < self.timeouts.len() - 1 {
            self.index += 1;
        }

        // Add jitter (0 to jitter_max)
        let jitter = rand::thread_rng()
            .gen_range(Duration::ZERO..self.jitter);

        base + jitter
    }

    pub fn reset(&mut self, timeouts: &'static [Duration]) {
        self.timeouts = timeouts;
        self.index = 0;
    }
}
```

### Pattern 5: Error Classification

**gRPC Status Code → Action:**

| Code | Name | Action |
|------|------|--------|
| 0 | OK | Success |
| 1 | Cancelled | Don't retry |
| 2 | Unknown | Retry with backoff |
| 3 | InvalidArgument | Don't retry (client bug) |
| 4 | DeadlineExceeded | Retry with backoff |
| 5 | NotFound | Don't retry |
| 7 | PermissionDenied | Don't retry, emit auth error |
| 8 | ResourceExhausted | Retry after rate limit wait |
| 13 | Internal | Retry with backoff |
| 14 | Unavailable | Retry with backoff (transient) |
| 16 | Unauthenticated | Emit auth error, don't retry |

```rust
impl GrpcError {
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.code(),
            Code::Unknown | Code::DeadlineExceeded |
            Code::ResourceExhausted | Code::Internal | Code::Unavailable
        )
    }

    pub fn is_auth_error(&self) -> bool {
        matches!(
            self.code(),
            Code::PermissionDenied | Code::Unauthenticated
        )
    }

    pub fn should_reconnect(&self) -> bool {
        matches!(
            self.code(),
            Code::Unavailable | Code::Internal
        )
    }
}
```

---

## Endpoint Configuration

### Production Settings

```rust
pub fn create_endpoint(server_url: &str) -> Result<Endpoint> {
    let endpoint = Endpoint::from_shared(server_url.to_string())?
        // Connection establishment
        .connect_timeout(Duration::from_secs(10))

        // Per-request timeout (doesn't set gRPC deadline header)
        .timeout(Duration::from_secs(30))

        // TCP-level keepalive
        .tcp_keepalive(Some(Duration::from_secs(300)))

        // HTTP/2 keepalive (prevents proxy timeouts)
        .http2_keep_alive_interval(Duration::from_secs(30))
        .http2_keep_alive_timeout(Duration::from_secs(10))
        .http2_keep_alive_while_idle(true)

        // Concurrency limits
        .concurrency_limit(256)

        // TLS with system roots
        .tls_config(ClientTlsConfig::new().with_webpki_roots())?;

    Ok(endpoint)
}
```

### HTTP/2 Keepalive Rationale

- **Interval 30s**: Send PING frame every 30 seconds when idle
- **Timeout 10s**: Expect PONG within 10 seconds
- **While Idle**: Keep pinging even with no active streams
- **Why**: Proxies/load balancers often have 60-120s idle timeouts; 30s interval prevents silent disconnects

---

## Streaming Patterns

### Server Streaming (Message/Notification Streams)

```rust
pub struct StreamManager {
    streams: HashMap<StreamId, JoinHandle<()>>,
    shutdown: broadcast::Sender<()>,
}

impl StreamManager {
    pub async fn start_message_stream(
        &mut self,
        client: MessageServiceClient<...>,
        channel_id: ChannelId,
    ) -> Result<()> {
        let mut shutdown_rx = self.shutdown.subscribe();

        let handle = tokio::spawn(async move {
            loop {
                // Connect and get stream
                let stream = match client.stream_messages(request).await {
                    Ok(response) => response.into_inner(),
                    Err(e) => {
                        tracing::warn!("stream error: {}", e);
                        tokio::time::sleep(backoff.next()).await;
                        continue;
                    }
                };

                // Process stream
                loop {
                    tokio::select! {
                        _ = shutdown_rx.recv() => return,

                        msg = stream.message() => {
                            match msg {
                                Ok(Some(message)) => {
                                    // Process message
                                },
                                Ok(None) => {
                                    // Stream ended, reconnect
                                    break;
                                },
                                Err(e) => {
                                    tracing::warn!("stream error: {}", e);
                                    break;
                                }
                            }
                        }
                    }
                }

                // Backoff before reconnect
                tokio::time::sleep(backoff.next()).await;
            }
        });

        self.streams.insert(stream_id, handle);
        Ok(())
    }
}
```

### Backpressure Management

```rust
// Bounded channel for natural backpressure
let (tx, rx) = tokio::sync::mpsc::channel::<Message>(100);

// Consumer controls pace - if slow, producer blocks
while let Some(msg) = stream.message().await? {
    // This will block if channel full
    tx.send(msg).await?;
}
```

---

## Authentication Flow

### Signal-Style Device Auth

The Chatter server uses Signal-style device authentication:

```
Authorization: Basic base64({UUID}.{deviceId}:authToken)
```

```rust
pub struct DeviceCredentials {
    pub user_id: String,      // ACI UUID
    pub device_id: u32,       // 1 for primary, 2+ for linked
    pub auth_token: String,   // Random 32-byte token (base64)
}

impl DeviceCredentials {
    pub fn to_basic_auth(&self) -> String {
        let username = format!("{}.{}", self.user_id, self.device_id);
        let credentials = format!("{}:{}", username, self.auth_token);
        format!("Basic {}", base64::encode(credentials))
    }
}
```

### Unauthenticated Access (Sealed Sender)

For sealed sender messages, use UAK instead of auth:

```rust
pub enum AuthMode {
    /// Normal authenticated request
    Authenticated(DeviceCredentials),

    /// Sealed sender with UAK
    SealedSender { access_key: [u8; 16] },

    /// Group send with ZK token
    GroupSend { token: Vec<u8> },

    /// Public endpoint (no auth)
    Public,
}

impl AuthMode {
    pub fn apply_to_request(&self, request: &mut Request<()>) -> Result<(), Status> {
        match self {
            AuthMode::Authenticated(creds) => {
                request.metadata_mut().insert(
                    "authorization",
                    creds.to_basic_auth().parse().unwrap(),
                );
            }
            AuthMode::SealedSender { access_key } => {
                request.metadata_mut().insert(
                    "unidentified-access-key",
                    base64::encode(access_key).parse().unwrap(),
                );
            }
            AuthMode::GroupSend { token } => {
                request.metadata_mut().insert(
                    "group-send-token",
                    base64::encode(token).parse().unwrap(),
                );
            }
            AuthMode::Public => {}
        }
        Ok(())
    }
}
```

---

## Common Pitfalls & Solutions

### Pitfall 1: Creating New Channels Per Request

**Wrong:**
```rust
async fn call_api() {
    let channel = Channel::from_static("http://...").connect().await?;
    let client = MyServiceClient::new(channel);
    client.method().await
}
```

**Right:**
```rust
// Channel created once, stored in Arc
let pool = Arc::new(GrpcClientPool::new(endpoint).await?);

// Clone for each call (cheap)
let client = pool.my_service_client();
client.method().await
```

### Pitfall 2: Ignoring HTTP/2 Keepalive

**Problem:** Connection silently dies after proxy timeout (60-120s typical).

**Solution:** Configure HTTP/2 keepalive:
```rust
.http2_keep_alive_interval(Duration::from_secs(30))
.http2_keep_alive_timeout(Duration::from_secs(10))
.http2_keep_alive_while_idle(true)
```

### Pitfall 3: Retrying Non-Retryable Errors

**Wrong:**
```rust
loop {
    match client.call().await {
        Err(e) => {
            sleep(backoff).await;
            continue;  // Retries InvalidArgument!
        }
    }
}
```

**Right:**
```rust
loop {
    match client.call().await {
        Err(status) if status.code().is_retryable() => {
            sleep(backoff.next()).await;
            continue;
        }
        result => return result,
    }
}
```

### Pitfall 4: Not Handling Stream Reconnection

**Problem:** Server streaming ends on disconnect; client stops receiving.

**Solution:** Wrap stream consumption in reconnection loop (see Streaming Patterns above).

### Pitfall 5: Blocking in Interceptors

**Wrong:**
```rust
fn call(&mut self, req: Request<()>) -> Result<Request<()>, Status> {
    let token = block_on(fetch_token_async())?;  // Blocks tokio!
    // ...
}
```

**Right:**
```rust
// Pre-fetch token and store in Arc<RwLock<...>>
// Interceptor only reads from cache
fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
    if let Some(token) = self.token.read().as_ref() {
        req.metadata_mut().insert("authorization", token.parse()?);
    }
    Ok(req)
}
```

---

## Implementation Recommendations

### Phase 16 Scope

1. **GrpcClientPool** (`client_pool.rs`)
   - Create endpoint with production settings
   - Connect channel (lazy or eager)
   - Provide typed client factories with interceptors
   - Thread-safe auth token storage

2. **AuthInterceptor** (`interceptors.rs`)
   - Inject authorization header
   - Support both Bearer and Basic auth modes (device tokens)
   - Support UAK/group-send-token for sealed sender

3. **ConnectionStateMachine** (`connection.rs`)
   - State enum with transitions
   - Event broadcasting
   - Online/offline detection

4. **FibonacciBackoff** (`recovery.rs`)
   - Standard and extended sequences
   - Jitter for thundering herd prevention
   - Reset on success/network change

### Deferred to Later Phases

- **Phase 17-19**: Individual service clients (auth, message, channel, etc.)
- **Phase 20**: StreamManager for real-time streams
- **Phase 21**: Full recovery/retry logic, offline queue
- **Phase 22**: Validation pipeline (orderer, deduplicator)

---

## References

### Official Documentation
- [tonic::transport::Channel](https://docs.rs/tonic/latest/tonic/transport/struct.Channel.html)
- [tonic::service::Interceptor](https://docs.rs/tonic/latest/tonic/service/trait.Interceptor.html)
- [gRPC Status Codes](https://grpc.io/docs/guides/status-codes/)
- [gRPC Retry Policy](https://grpc.io/docs/guides/retry/)

### Signal Implementation
- `/ts/textsecure/SocketManager.preload.ts` - Connection management
- `/ts/util/BackOff.std.ts` - Fibonacci backoff
- `/ts/textsecure/WebAPI.preload.ts` - Auth header injection

### Chatter Backend Reference
- `internal/middleware/auth.go` - Auth interceptor patterns
- Proto files: `chat.v1.tonic.rs` - Generated client stubs

---

## Summary

**Standard Stack:**
- tonic 0.12 for gRPC clients
- tower for middleware layering
- Channel cloning for connection reuse
- Sync interceptors for auth injection

**Architecture Patterns:**
1. Single Channel, multiple cloned clients
2. Layered interceptors (auth → metadata → tower middleware)
3. Fibonacci backoff with 5s jitter
4. State machine with broadcast events
5. Classify errors by gRPC status code

**What NOT to Do:**
- Don't implement custom connection pooling
- Don't use mobc-tonic (Channel already pools)
- Don't retry InvalidArgument/NotFound/PermissionDenied
- Don't block in interceptors

**Next Steps:**
Run `/gsd:plan-phase 16` to create the implementation plan.
