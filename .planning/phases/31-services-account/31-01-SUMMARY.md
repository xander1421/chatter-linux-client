# Summary: AccountService Implementation

## What Was Built

Implemented `AccountService` for secure account management operations with password change and account deletion flows.

## Key Implementation Details

### AccountError Enum
- `InvalidPassword` - Current password verification failed
- `NotLoggedIn` - Operation requires authenticated session
- `DeletionFailed(String)` - Account deletion failed with reason
- `Network(NetworkError)` - Underlying network error (via From trait)
- `Storage(StorageError)` - Underlying storage error (via From trait)

### AccountService Struct
Dependencies following established service pattern:
- `Arc<SettingsRepository>` - For login state check (CURRENT_USER_ID)
- `AuthInterceptor` - For clearing auth state on deletion

### change_password Method
1. Verifies user is logged in via `is_logged_in()` helper
2. Hashes both passwords with SHA256 (server expects pre-hashed, matching AuthService pattern)
3. Calls `auth_client.change_password(current_hash, new_hash)`
4. Maps `NetworkError::Unauthenticated` to `AccountError::InvalidPassword`

### delete_account Method
1. Verifies user is logged in
2. Hashes password with SHA256
3. Calls `auth_client.delete_account(password_hash)` returning event stream
4. Consumes `AccountDeletionEvent` stream:
   - `Started`, `DataRemovalInProgress`, `SpicedbCleanupInProgress` → progress, continue
   - `Completed` → clear auth interceptor, delete CURRENT_USER_ID and CURRENT_DEVICE_ID
   - `Failed` → return `DeletionFailed` with error message
5. Handles stream ending without completion as error

## Decisions Made

- **Private is_logged_in helper**: Unlike AuthService's public method, AccountService's helper is private since external callers should use AuthService for login state queries
- **Error mapping for password validation**: Map Unauthenticated to InvalidPassword for both operations (password change and delete confirmation)
- **Stream consumption pattern**: Match AuthService's register_and_wait pattern using tokio::pin! and while let

## Commits

| Hash | Type | Description |
|------|------|-------------|
| b0e5a87 | feat | implement AccountService for password change and account deletion |

## Verification

- [x] `cargo check --lib` passes without errors
- [x] AccountService has change_password and delete_account methods
- [x] Error types properly map network errors (Unauthenticated → InvalidPassword)
- [x] Local state cleanup on deletion (interceptor.clear(), delete settings keys)

## Files Changed

- `src/services/account_service.rs` - Full implementation replacing stub
