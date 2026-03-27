# Recovery Architecture

## Credentials

| Credential | Entropy | Stored | Purpose |
|------------|---------|--------|---------|
| **Password** | User-chosen | Server (SHA256 hash) | Server auth, profile key derivation, username privacy |
| **PIN** | 6 digits (~20 bits) | Never stored | SVR secret derivation, registration lock, backup encryption |
| **Recovery Codes** | 8 codes × 8 chars (~40 bits each) | Server (Argon2id hash) | Last-resort account recovery, one-time use |

## What's Backed Up (SVR)

| Data | Size | Purpose |
|------|------|---------|
| Identity keypair | 64 bytes (32 priv + 32 pub) | Signal Protocol E2E encryption |
| Profile key | 32 bytes | Username privacy, profile encryption |

Encrypted with AES-256-GCM using PIN-derived SVR secret. Server stores opaque blob.

## Recovery Flows

### Flow 1: Same Device, Change Password
```
User: knows current password
Path: login → change_password(old, new)
Loss: None
```

### Flow 2: Same Device, Change PIN
```
User: knows current PIN
Path: login → change_pin(old_pin, new_pin, username)
       → re-encrypts SVR backup with new PIN
       → updates registration lock + SVR public key atomically
Loss: None
```

### Flow 3: New Device, Have Password + PIN
```
User: knows password + PIN
Path: login(password) → SVR restore(PIN) → full key recovery
Loss: None (except old group sender keys)
```

### Flow 4: New Device, Have Password Only (Forgot PIN)
```
User: knows password, forgot PIN
Path: login(password) → skip SVR restore → fresh identity
       → can use retry_svr_backup() later if PIN remembered
Loss: Old identity key, old E2E sessions
```

### Flow 5: Forgot Password, Have PIN + Same Device
```
User: forgot password, has PIN, still logged in on a device
Path: Already logged in → change_password() from settings
Loss: None
```

### Flow 6: Forgot Password, Have Recovery Code
```
User: forgot password, has recovery code
Path: use_recovery_code(username, code, new_password, new_pin)
       → server verifies Argon2id hash
       → resets password + registration lock + SVR public key
       → user logs in with new credentials
       → SVR backup created with new PIN
Loss: Old SVR backup (new PIN = new encryption key)
       Old identity if on new device (need to re-register keys)
```

### Flow 7: Forgot Password + PIN, Have Recovery Code
```
User: forgot both, has recovery code
Path: use_recovery_code(username, code, new_password, new_pin)
       → same as Flow 6
Loss: Same as Flow 6
```

### Flow 8: Lost Everything (No Recovery Code)
```
User: forgot password + PIN + lost recovery codes
Path: None — permanently locked out
Loss: Everything (by design — zero-knowledge model)
```

## API Surface

### Account Lifecycle
| Method | When | What it does |
|--------|------|-------------|
| `register(username, password, pin)` | Account creation | Creates account, generates keys, creates SVR backup, returns 8 recovery codes |
| `login(username, password, pin?)` | Sign in | Authenticates, optionally restores keys from SVR |
| `change_password(old, new)` | Settings | Updates server credential |
| `change_pin(old_pin, new_pin, username)` | Settings | Re-encrypts SVR backup, updates registration lock |
| `retry_svr_backup(pin, username, password)` | After failed backup | Creates SVR backup if registration backup failed |
| `use_recovery_code(username, code, new_password, new_pin)` | Account recovery | Resets all credentials with recovery code |

### Security Properties
- **Zero-knowledge**: Server never sees PIN, SVR secret, plaintext keys, or recovery codes
- **Rate limiting**: SVR fetch (10/24h), recovery codes (3/hour), all RPCs have middleware limits
- **One-time use**: Each recovery code invalidated after use
- **Atomic operations**: PIN change swaps backup + registration lock + SVR key in one transaction
- **Zeroization**: All sensitive intermediates zeroized after use on client

## Design Decisions

1. **No email-based reset** — Privacy-first. Email links account to external identity.
2. **No PIN recovery** — Zero-knowledge. If server could recover PIN, it could decrypt backups.
3. **No group message history recovery** — Sender keys are ephemeral per group membership. Signal has same limitation.
4. **Recovery codes shown once** — Never retrievable again. User must save them at registration.
5. **8 codes, not 1** — Redundancy. Losing one doesn't lock you out. Each is independent.
