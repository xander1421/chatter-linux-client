# Phase 96 — Channel Privacy Tiers

Support both full E2E and org-managed (server-visible) messaging on the same
server / client codebase. Per-channel `privacy_tier` replaces the binary
`channels.encrypted` flag. Org admins control what tiers their members can
create.

## Tier matrix

| Tier        | Content storage              | Sender seen by server     | Search / DLP / AI bots | Admin decrypt           |
|-------------|------------------------------|---------------------------|------------------------|-------------------------|
| `E2E`       | Ciphertext (sender key)      | Pseudonymous (author_id)  | No                     | Impossible              |
| `MANAGED`   | Plaintext                    | Yes                       | Yes                    | N/A (already plaintext) |
| `ESCROWED`  | Ciphertext + escrow envelope | Pseudonymous              | Escrow-only indexing   | Quorum unseal + audit   |

- **Consumer account (no org)**: always `E2E`. No UI toggle.
- **Enterprise org**: admin sets `default_channel_tier` + `allowed_tiers`.
  DMs remain `E2E`. Channels inside an org pick from the allowed set at
  creation. Channels cannot change tier after creation (new channel required).

## Wire format changes

### Proto (`pkg/proto/chat/v1/`)

Add to `channel/channel.proto`:

```proto
enum ChannelPrivacyTier {
  CHANNEL_PRIVACY_TIER_UNSPECIFIED = 0;
  CHANNEL_PRIVACY_TIER_E2E = 1;
  CHANNEL_PRIVACY_TIER_MANAGED = 2;
  CHANNEL_PRIVACY_TIER_ESCROWED = 3;
}

message ChannelConfig {
  // existing fields...
  ChannelPrivacyTier privacy_tier = N;
}

message OrgPrivacyPolicy {
  ChannelPrivacyTier default_channel_tier = 1;
  repeated ChannelPrivacyTier allowed_tiers = 2;
  bytes escrow_public_key = 3;    // X25519, 32 bytes
  uint32 escrow_quorum_m = 4;      // Shamir M-of-N threshold
  uint32 escrow_quorum_n = 5;
  uint32 retention_days = 6;
  repeated string dlp_patterns = 7;
}
```

Add to `message/message.proto` (SendMessageRequest):

```proto
message SendMessageRequest {
  // existing fields...
  // For ESCROWED tier only: sealed-sender ciphertext to the org escrow key.
  // Server stores in messages.escrow_payload for later admin decrypt.
  bytes escrow_payload = N;
}
```

New RPCs in `message/message.proto`:

```proto
rpc AdminDecryptMessage(AdminDecryptMessageRequest)
    returns (AdminDecryptMessageResponse);

message AdminDecryptMessageRequest {
  string message_id = 1;
  string reason = 2;  // required, logged to audit
  bytes quorum_unseal_proof = 3;  // M-of-N Shamir shares
}

message AdminDecryptMessageResponse {
  bytes plaintext = 1;
  string unsealed_by_admins = 2;  // comma-separated actor ids
}
```

## Server migrations

### 000041_channel_privacy_tiers.up.sql

```sql
CREATE TYPE channel_privacy_tier AS ENUM ('E2E', 'MANAGED', 'ESCROWED');

ALTER TABLE channels ADD COLUMN privacy_tier channel_privacy_tier;

-- Backfill from existing `encrypted` boolean.
UPDATE channels
SET privacy_tier = CASE WHEN encrypted THEN 'E2E'::channel_privacy_tier
                        ELSE 'MANAGED'::channel_privacy_tier END;

ALTER TABLE channels ALTER COLUMN privacy_tier SET NOT NULL;
ALTER TABLE channels ALTER COLUMN privacy_tier SET DEFAULT 'E2E';

CREATE INDEX idx_channels_privacy_tier ON channels(privacy_tier)
WHERE deleted_at IS NULL;
```

### 000042_escrow_payload.up.sql

```sql
-- Per-message escrow envelope for ESCROWED channels.
-- NULL for E2E + MANAGED messages.
ALTER TABLE messages ADD COLUMN escrow_payload JSONB;
```

### 000043_org_escrow.up.sql

```sql
ALTER TABLE organizations ADD COLUMN escrow_public_key BYTEA;  -- X25519 pk
ALTER TABLE organizations ADD COLUMN escrow_quorum_m INT;
ALTER TABLE organizations ADD COLUMN escrow_quorum_n INT;

-- Admin-decrypt audit rows go through existing org_audit_log.
-- Reserve an action string for them.
-- (no DDL change, just a convention: action = 'admin_decrypt_message')
```

## Server-side branch points

### `internal/services/message/handlers/send.go`

Branch on `channel.privacy_tier`:

- **E2E** — existing path. Reject if `req.Content` has plaintext body.
  Store `content_type = 'ENCRYPTED'`, `encrypted_data` JSONB.
- **MANAGED** — require plaintext body in `req.Content`. Run DLP against
  org's `dlp_patterns`. Store plaintext in `content` bytea, index for
  search, content_type = 'TEXT'.
- **ESCROWED** — existing encrypted path **plus** require
  `req.EscrowPayload`. Store in `messages.escrow_payload`. Reject if
  `channel.org_id` is null or org has no `escrow_public_key`.

### New `AdminDecryptMessage` handler

1. Require org admin role (SpiceDB: `organization:X#admin`).
2. Verify `quorum_unseal_proof` against Shamir threshold (`m`/`n` on org).
3. Fetch `messages.escrow_payload`.
4. Unseal with org escrow private key (retrieved via Vault using the proof).
5. Log to `org_audit_log` with `action='admin_decrypt_message'`,
   `target_type='message'`, `target_id=<msg_id>`, metadata includes reason.
6. Return plaintext.

### Search / AI / DLP gating

Every handler that reads message content must first check tier:

```go
if channel.PrivacyTier == types.PrivacyTierE2E {
    return nil, connect.NewError(connect.CodePermissionDenied,
        errors.New("search not available on E2E channels"))
}
```

### Vault integration

Each org with ESCROWED support has a Vault transit key at
`transit/keys/org-escrow-{org_id}` with Shamir-sealed master. Admin
actions: client submits enough unseal shares to Vault to derive the org
private key for a single decrypt operation. Server never caches the
key. Reuses existing Vault wiring (already used for certificates per
project memory).

## Client changes (Rust)

### Domain (`chatter-core/src/domain/channel.rs`)

```rust
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ChannelPrivacyTier {
    E2E,
    Managed,
    Escrowed,
}

pub struct ChannelConfig {
    // existing...
    pub privacy_tier: ChannelPrivacyTier,
}
```

Backwards-compat: parse old `encrypted: bool` into `E2E` / `Managed`.

### Send-path branch (`services/message/core.rs::send_message`)

Replace `(is_dm, require_encryption)` tuple with
`(is_dm, tier, escrow_pk_opt)`. Switch:

- DM: unchanged (always E2E via Double Ratchet).
- `E2E` channel: SKDM + sender-key encrypt. Current path.
- `Managed` channel: skip SKDM, send plaintext `DataMessage`. No key
  distribution.
- `Escrowed` channel: SKDM + sender-key encrypt AS TODAY, plus pairwise
  encrypt a copy of the plaintext to `escrow_pk` via sealed-sender, pass
  it as `SendMessageRequest.escrow_payload`.

### UI badges (Slint)

Add `ChannelData.privacy_tier: i32` field. Chat header renders:

- `E2E`: existing green lock.
- `MANAGED`: building icon + "Managed channel — readable by admins".
- `ESCROWED`: lock + amber eye icon + "Recoverable by admin quorum".

### First-send modal

On first send after joining a MANAGED or ESCROWED channel this session,
show a one-shot dialog explaining server visibility. Remember dismissal
in `settings_repo` keyed by `(channel_id, tier)`.

### Channel creation UI

When user is a member of an org: fetch org policy via
`ChatterClient::org_privacy_policy(org_id)`. Render a tier dropdown
restricted to `allowed_tiers`. Default to `default_channel_tier`.
Consumer (no org): tier dropdown hidden, always `E2E`.

## Phased delivery

| Phase | Scope                                                                        | Touches                                           |
|-------|------------------------------------------------------------------------------|---------------------------------------------------|
| 1     | Migrations 000041–000043. Proto enum + ChannelConfig field.                  | SQL, .proto, regen both servers & client bindings |
| 2     | Server send-path branch. Reject mismatched payloads. Rules-only, no escrow. | Go handlers                                       |
| 3     | Client domain + send-path branch. UI badges (no modal yet).                  | Rust core + Slint                                 |
| 4     | Org privacy policy: schema, RPC, admin UI, channel-create tier picker.       | Go + proto + Slint                                |
| 5     | Vault integration for org escrow key. `AdminDecryptMessage` RPC.             | Go + Vault                                        |
| 6     | DLP + full-text search on MANAGED. Gate on privacy_tier in every reader.     | Go search / DLP                                   |
| 7     | First-send modal + escrow session banner. Dismissal persistence.             | Slint                                             |
| 8     | Migration tool to backfill existing orgs: prompt admin to pick policy.       | CLI / admin UI                                    |

## Hard rules

- Tier never changes after channel creation. Period.
- DMs are always E2E. Not configurable.
- No silent-server-decrypt of E2E content ever.
- Admin decrypt of ESCROWED content always writes `org_audit_log` row.
- Server refuses SendMessage whose payload doesn't match the channel's
  tier contract. No mixed-tier rows.

## Rejected alternatives

- **Single key + server holds copy for admin**: hidden backdoor, breaks
  "E2E" claim. We have ESCROWED as the honest version.
- **Per-message tier flag**: analytics nightmare; retention policy can't
  apply evenly; users can't reason about one channel.
- **Downgrade tier**: can't decrypt existing ciphertext after E2E →
  Managed; leaves a confusing hybrid backlog.
