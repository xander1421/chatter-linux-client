//! One-shot PQOPK pool seeding with a bounded rate-limit retry.
//!
//! Registration and device linking both seed the server's post-quantum
//! one-time-prekey pool through `RefreshPreKeys` right after their bundle
//! upload. The server rate-limits that RPC per IP (0.1 RPS, burst 6 —
//! `internal/middleware/auth.go` in the server repo), so a burst of
//! registrations from one address (NAT'd office, test harness) can throttle
//! the seed call. The seed is deliberately best-effort — a linked device must
//! never be stranded over it — but giving up on the FIRST 429 leaves the
//! account handshaking against the single last-resort PQSPK until the
//! replenisher's next pass: one reused PQ secret across every session in the
//! meantime (OPEN-ITEMS §34). A short, bounded retry that respects the
//! limiter's refill closes that window without changing the best-effort
//! contract.

use std::time::Duration;

use crate::error::NetworkError;
use crate::network::grpc::auth_client::AuthClient;
use crate::proto::signal_pre_key_bundle::KyberPreKeyRecord;

/// Wait long enough for the server's token bucket (0.1 RPS) to refill at
/// least one slot, with headroom for competing callers behind the same IP.
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(12), Duration::from_secs(24)];

/// Upload `kyber_otpks` via `RefreshPreKeys`, retrying only when the server
/// answers rate-limited. Any other error — and exhaustion of the retry
/// budget — is returned for the caller to log; seeding stays best-effort.
pub(crate) async fn seed_pqopk_pool(
    client: &mut AuthClient,
    kyber_otpks: Vec<KyberPreKeyRecord>,
) -> Result<(), NetworkError> {
    let mut attempt = 0;
    loop {
        match client
            .refresh_pre_keys_with_kyber_otpk(Vec::new(), kyber_otpks.clone())
            .await
        {
            Ok(()) => return Ok(()),
            Err(NetworkError::RateLimited) if attempt < RETRY_DELAYS.len() => {
                tracing::warn!(
                    attempt = attempt + 1,
                    retry_in_s = RETRY_DELAYS[attempt].as_secs(),
                    "PQOPK seed rate-limited — waiting for the limiter to refill"
                );
                tokio::time::sleep(RETRY_DELAYS[attempt]).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}
