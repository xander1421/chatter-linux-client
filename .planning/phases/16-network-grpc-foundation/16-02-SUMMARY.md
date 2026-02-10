# Summary: GrpcClientPool, AuthInterceptor, ConnectionStateMachine

**Plan:** 16-02-PLAN.md
**Status:** Complete
**Duration:** ~5 min

## What Was Built

### Task 1: AuthInterceptor (7531e71)

Thread-safe authentication interceptor for gRPC requests supporting multiple auth modes:

**AuthMode Variants:**
- `Bearer(String)` - Device token → `Authorization: Bearer {token}`
- `Basic { username, password }` - Signal-style → `Authorization: Basic base64({user}:{pass})`
- `SealedSender { access_key }` - Anonymous → `Unidentified-Access-Key: base64({16-byte-key})`
- `None` - No authentication header

**Key Features:**
- Uses `parking_lot::RwLock` for sync access (tonic Interceptor is sync)
- `Arc<RwLock<AuthMode>>` allows sharing across cloned interceptors
- Methods: `set_bearer()`, `set_basic()`, `set_sealed_sender()`, `clear()`

### Task 2: GrpcClientPool (a8d00f9)

Connection pool managing a shared tonic Channel with production-ready HTTP/2 settings:

**Endpoint Configuration:**
- `connect_timeout(10s)` - Initial connection timeout
- `timeout(30s)` - Request timeout
- `tcp_keepalive(300s)` - TCP keepalive interval
- `http2_keep_alive_interval(30s)` - HTTP/2 ping interval
- `keep_alive_timeout(10s)` - HTTP/2 ping timeout
- `keep_alive_while_idle(true)` - Ping even when idle
- `concurrency_limit(256)` - Max concurrent streams

**API:**
- `new(server_url)` - Create pool with URL validation
- `connect()` - Establish HTTP/2 connection (async)
- `disconnect()` - Drop channel
- `channel()` / `try_channel()` - Get cloneable channel
- `auth()` / `auth_clone()` - Access auth interceptor

### Task 3: ConnectionStateMachine (59c38bf)

State machine for connection lifecycle with event broadcasting:

**States:**
- `Disconnected` - Not connected
- `Connecting` - Initial connection attempt
- `Connected` - Successfully connected
- `Reconnecting { attempt }` - Retry in progress
- `Failed { reason }` - Terminal state (AuthExpired, DeviceDelinked, MaxRetriesExceeded)

**Events (via tokio::sync::broadcast):**
- `StateChanged { old, new }` - Every transition
- `Online` - Connected from Connecting/Reconnecting
- `Offline` - Lost connection from Connected
- `AuthRequired` - Failed with AuthExpired/DeviceDelinked

**Helper Methods:**
- `is_connected()`, `is_connecting()`, `is_failed()`
- Clone shares state via `Arc<RwLock<ConnectionState>>`

## Tests Added

| Module | Tests | Coverage |
|--------|-------|----------|
| interceptors | 9 | All auth modes, header injection |
| client_pool | 8 | URL validation, connection errors, panic on not-connected |
| connection | 14 | All transitions, event emission, clone behavior |

## Files Modified

| File | Changes |
|------|---------|
| src/network/grpc/interceptors.rs | AuthMode, AuthInterceptor implementation |
| src/network/grpc/client_pool.rs | GrpcClientPool with HTTP/2 config |
| src/network/connection.rs | ConnectionStateMachine, events |
| Cargo.toml | Added parking_lot 0.12 |

## Decisions Made

| Decision | Rationale |
|----------|-----------|
| parking_lot::RwLock over std | Faster, non-poisoning, sync (tonic needs sync) |
| broadcast channel capacity 16 | Small events, consumers should keep up |
| Panic on channel() if not connected | Explicit contract, use try_channel() for safety |
| Clone shares state | Multi-component access to same connection state |

## Verification

```
cargo check --lib           # OK
cargo test --lib interceptors  # 9/9 passed
cargo test --lib client_pool   # 8/8 passed
cargo test --lib connection    # 17/17 passed (including recovery tests)
```

## Integration Notes

To create a gRPC client with auth:

```rust
let pool = GrpcClientPool::new("http://localhost:50051")?;
pool.connect().await?;

// Set auth
pool.auth().set_bearer(device_token);

// Create service client with interceptor
let channel = pool.channel();
let auth = pool.auth_clone();
let client = MyServiceClient::with_interceptor(channel, auth);
```

## Next Steps

Phase 16 foundation complete. Ready for Phase 17: Network Auth & User Clients.
