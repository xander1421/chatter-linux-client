# Research: Rename public_key to svr_public_key

## Overview

Replace the legacy `public_key` field with `svr_public_key` (Secure Value Recovery) across the full stack. This prepares for PIN-based key recovery functionality.

**Scope**: ~25 files across Go backend + Rust client
**Migration Strategy**: Destructive (development app)
**Proto Field Number**: Keep `= 3` (wire-compatible rename)

---

## Phase 1: Backend Proto (2 files)

### `/home/alex/Documents/chatter/pkg/proto/chat/v1/auth/auth.proto`
- Line 64-66: `LoginRequest.public_key` → `svr_public_key`
- Line 102-104: `RegisterRequest.public_key` → `svr_public_key`
- Line 247: `CompletePasswordResetRequest.new_public_key` → `new_svr_public_key`
- Update comments: "SVR (Secure Value Recovery) public key for PIN-based key recovery"

### `/home/alex/Documents/chatter/pkg/proto/chat/v1/common/common.proto`
- Lines 66-69: `User.public_key` → `svr_public_key`

**After**: Run `buf generate` to regenerate Go code

---

## Phase 2: Backend Migration (2 new files)

### Create `/home/alex/Documents/chatter/scripts/migrations/000028_rename_public_key_to_svr_public_key.up.sql`
```sql
ALTER TABLE users RENAME COLUMN public_key TO svr_public_key;
DROP INDEX IF EXISTS idx_users_public_key;
CREATE UNIQUE INDEX idx_users_svr_public_key ON users(svr_public_key) WHERE deleted_at IS NULL;
```

### Create `/home/alex/Documents/chatter/scripts/migrations/000028_rename_public_key_to_svr_public_key.down.sql`
```sql
ALTER TABLE users RENAME COLUMN svr_public_key TO public_key;
DROP INDEX IF EXISTS idx_users_svr_public_key;
CREATE UNIQUE INDEX idx_users_public_key ON users(public_key) WHERE deleted_at IS NULL;
```

---

## Phase 3: Backend Types (1 file)

### `/home/alex/Documents/chatter/internal/services/auth/types/types.go`
- Line 82: `ErrPublicKeyExists` → `ErrSvrPublicKeyExists` ("SVR public key already exists")
- Line 97: `PublicKey []byte` → `SvrPublicKey []byte` (json tag: `"svr_public_key"`)
- Line 165: `UpdateUserPublicKey` → `UpdateUserSvrPublicKey`
- Line 199: `GetUserIdentityPublicKey` → `GetUserSvrPublicKey`

---

## Phase 4: Backend Repository (2 files)

### `/home/alex/Documents/chatter/internal/services/auth/repository/postgres/repository.go`
- Line 64: SQL `public_key` → `svr_public_key`
- Line 72: `user.PublicKey` → `user.SvrPublicKey`
- Line 85: Constraint `idx_users_public_key` → `idx_users_svr_public_key`
- Line 86: Error `ErrPublicKeyExists` → `ErrSvrPublicKeyExists`
- Lines 242, 278: SELECT column rename
- Lines 254, 290: Scan field rename
- Lines 367-385: Method `UpdateUserPublicKey` → `UpdateUserSvrPublicKey`

### `/home/alex/Documents/chatter/internal/services/auth/repository/redis/repository.go`
- Update method wrapper names

---

## Phase 5: Backend Service (3 files)

### `/home/alex/Documents/chatter/internal/services/auth/service/user.go`
- Line 840: Logging field `has_public_key` → `has_svr_public_key`
- Line 854: Validation check `.GetPublicKey()` → `.GetSvrPublicKey()`
- Line 858-859: Error message update
- Lines 1246-1275: Method rename + field references

### `/home/alex/Documents/chatter/internal/services/auth/service/password.go`
- Lines 126-130: `newPublicKey` → `newSvrPublicKey`, `.GetNewPublicKey()` → `.GetNewSvrPublicKey()`

### `/home/alex/Documents/chatter/internal/services/auth/service/batch_registration.go`
- Line 38: `PublicKey` → `SvrPublicKey` (json tag update)

---

## Phase 6: Backend Handlers (3 files)

### `/home/alex/Documents/chatter/internal/services/auth/handlers/register.go`
- Line 26: `.GetPublicKey()` → `.GetSvrPublicKey()`
- Line 43: Validation call update

### `/home/alex/Documents/chatter/internal/services/auth/handlers/validation.go`
- Update comments in `validateCryptographicKey` to reference SVR

### `/home/alex/Documents/chatter/internal/services/auth/handlers/convert.go`
- Line 61: `ErrPublicKeyExists` → `ErrSvrPublicKeyExists`
- Error message: "SVR public key already in use"

---

## Phase 7: Backend Temporal (4 files)

### `/home/alex/Documents/chatter/internal/temporal/workflows/auth/batch_registration_workflow.go`
- Line 54: `PublicKey` → `SvrPublicKey`

### `/home/alex/Documents/chatter/internal/temporal/workflows/auth/user_creation_workflow.go`
- Input struct: `PublicKey` → `SvrPublicKey`

### `/home/alex/Documents/chatter/internal/temporal/activities/auth/activities.go`
- Line 44: `CreateAuthUserActivityInput.PublicKey` → `SvrPublicKey`
- Line 63: Field assignment
- Lines 405-415: `NewPublicKey` → `NewSvrPublicKey`

### `/home/alex/Documents/chatter/internal/temporal/activities/auth/batch_activities.go`
- Field references and error messages

---

## Phase 8: Backend Mocks (regenerate)

```bash
cd /home/alex/Documents/chatter
go generate ./internal/services/auth/...
```

---

## Phase 9: Rust Client Domain (1 file)

### `/home/alex/Documents/chatter/linux-client/src/domain/user.rs`
- Line 46-47: Comment + field `public_key` → `svr_public_key`
- Line 56: Constructor parameter
- Line 61: Field assignment
- Tests: Update field references

---

## Phase 10: Rust Client Migration (2 files)

### Create `/home/alex/Documents/chatter/linux-client/src/storage/migrations/v018_rename_svr_public_key.rs`
```rust
pub struct V018RenameSvrPublicKey;

impl Migration for V018RenameSvrPublicKey {
    fn version(&self) -> u32 { 18 }
    fn description(&self) -> &'static str { "Rename public_key to svr_public_key" }
    fn up(&self) -> &'static str {
        "ALTER TABLE users RENAME COLUMN public_key TO svr_public_key;"
    }
    fn down(&self) -> Option<&'static str> {
        Some("ALTER TABLE users RENAME COLUMN svr_public_key TO public_key;")
    }
}
```

### `/home/alex/Documents/chatter/linux-client/src/storage/migrations/mod.rs`
- Add `mod v018_rename_svr_public_key;`
- Register in migrations list

---

## Phase 11: Rust Client Repository (1 file)

### `/home/alex/Documents/chatter/linux-client/src/storage/repositories/user_repo.rs`
- Lines 34, 39: INSERT column + param
- Lines 57, 62: INSERT OR IGNORE
- Lines 74, 84: SELECT + scan
- Lines 101, 111: SELECT + scan
- Lines 128, 132: UPDATE + param

---

## Phase 12: Rust Client Services (2 files)

### `/home/alex/Documents/chatter/linux-client/src/services/auth_service.rs`
- Line 219: User struct `public_key` → `svr_public_key`
- Lines 400-406: Register method

### `/home/alex/Documents/chatter/linux-client/src/network/grpc/auth_client.rs`
- Line 75: Request field (login)
- Line 185: Request field (register)

---

## Execution Order

1. Backend Proto → `buf generate`
2. Backend Migration files
3. Backend Types
4. Backend Repository
5. Backend Service
6. Backend Handlers
7. Backend Temporal
8. Backend Mocks → `go generate`
9. **Test Backend**: `go test ./...`
10. Rust Client Proto → `cargo build` (regenerates)
11. Rust Client Domain
12. Rust Client Migration
13. Rust Client Repository
14. Rust Client Services
15. **Test Client**: `cargo test`

---

## Verification

### Backend
```bash
cd /home/alex/Documents/chatter
buf generate
go build ./...
go test ./internal/services/auth/...
```

### Client
```bash
cd /home/alex/Documents/chatter/linux-client
cargo build
cargo test
```

### Integration
1. Run backend with new migration
2. Register new user via client
3. Verify `svr_public_key` stored in both PostgreSQL and SQLite

---

## Background: Why SVR?

The `public_key` field is currently legacy - it duplicates the Signal Protocol identity key that's already in `prekey_bundle.identity_key`. Repurposing it as `svr_public_key` enables:

1. **Registration Lock** - Prevent account takeover even if password is known
2. **Identity Key Recovery** - Recover encrypted keys on new device via PIN
3. **Multi-Device Setup** - PIN retrieves keys from secure enclave
4. **Encrypted Backup Restoration** - PIN-derived key decrypts backups

This is more important than username link keys because it's core security infrastructure, not just UX polish.
