# AuthService

**File**: `src/services/auth_service.rs`

Authentication flow orchestration - login, register, logout.

## Purpose

AuthService orchestrates the full authentication lifecycle:
- Registration with identity key generation
- Login with session establishment
- Logout with cleanup

## Construction

```rust
// From AppContext
let auth_service = ctx.auth_service();

// Internal construction (in AppContext)
AuthService::new(
    identity_manager,
    prekey_manager,
    user_repo,
    settings_repo,
    key_store,
    auth_interceptor,
)
```

## Methods

### Register

```rust
/// Register new account with Signal Protocol keys
pub async fn register(
    &self,
    auth_client: &mut AuthClient,
    username: &str,
    password: &str,
    display_name: &str,
    device_info: &str,
) -> Result<RegisterResponse, ServiceError>
```

**What it does**:
1. Generates identity key pair
2. Generates signed prekey
3. Generates Kyber prekey (post-quantum)
4. Generates batch of one-time prekeys
5. Hashes password (Argon2)
6. Sends registration request
7. Stores identity locally
8. Sets auth token in interceptor
9. Stores user settings

### Login

```rust
/// Login with existing credentials
pub async fn login(
    &self,
    auth_client: &mut AuthClient,
    identifier: &str,  // username or email
    password: &str,
    device_info: &str,
) -> Result<LoginResponse, ServiceError>
```

**What it does**:
1. Sends login request with password hash
2. Receives auth token and user info
3. Sets token in AuthInterceptor
4. Loads/creates local identity
5. Uploads prekeys if needed
6. Stores user settings

### Logout

```rust
/// Logout and cleanup
pub async fn logout(&self) -> Result<(), ServiceError>
```

**What it does**:
1. Clears auth token from interceptor
2. Clears sensitive settings
3. (Optional) Revokes device on server

## Usage Examples

### Register (from UI)

```rust
// In register.rs
let auth_service = ctx.auth_service();
let mut auth_client = ctx.auth_client();

match auth_service.register(
    &mut auth_client,
    &username,
    &password,
    &display_name,
    "Linux Desktop",
).await {
    Ok(response) => {
        // Update auth state
        auth_state.set_logged_in(response.user_id, response.device_id);
        // Navigate to main view
    }
    Err(e) => {
        // Show error in UI
    }
}
```

### Login (from UI)

```rust
// In login.rs
let auth_service = ctx.auth_service();
let mut auth_client = ctx.auth_client();

match auth_service.login(
    &mut auth_client,
    &identifier,
    &password,
    "Linux Desktop",
).await {
    Ok(response) => {
        // Update auth state
        auth_state.set_logged_in(response.user_id, response.device_id);
        // Start inbox polling
        ctx.init_device_inbox_provider();
    }
    Err(e) => {
        // Show error in UI
    }
}
```

## Registration Flow

```
User fills registration form
    │
    ▼
AuthService::register()
    │
    ├─→ IdentityKeyManager::generate_identity()
    │   (Ed25519 key pair)
    │
    ├─→ PreKeyManager::generate_prekey_bundle()
    │   - Signed prekey (X25519)
    │   - Kyber prekey (ML-KEM 768)
    │   - 100 one-time prekeys
    │
    ├─→ Hash password (Argon2id)
    │
    ├─→ AuthClient::register()
    │   (gRPC to server)
    │
    ├─→ Store identity in KeyStore
    │
    ├─→ Set token in AuthInterceptor
    │
    └─→ Store user settings
```

## Login Flow

```
User enters credentials
    │
    ▼
AuthService::login()
    │
    ├─→ AuthClient::login()
    │   (gRPC to server)
    │
    ├─→ Set token in AuthInterceptor
    │
    ├─→ Load identity from KeyStore
    │   (or generate if first login on device)
    │
    ├─→ Check prekey count, upload if low
    │
    └─→ Store user settings
```

## Dependencies

- `IdentityKeyManager` - Identity key generation
- `PreKeyManager` - PreKey bundle generation
- `UserRepository` - User storage
- `SettingsRepository` - Settings storage
- `KeyStore` - Signal key storage
- `AuthInterceptor` - JWT token injection

## Security Notes

1. **Password hashing**: Done client-side with Argon2id before sending
2. **Identity keys**: Generated locally, never leave device
3. **Auth tokens**: Stored in memory via AuthInterceptor, cleared on logout
4. **PreKeys**: Uploaded to server, private keys stay local
