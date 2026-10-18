//! Friend DMs — one conversation per friend, two transports.
//!
//! Chatter's DM model mirrors Signal's 1:1 messaging: there is no
//! server-side DM channel. The local conversation is keyed by the peer's
//! `UserId` (`ActiveConversation::ServerDm { peer_id }`, local storage key
//! `ChannelId::from(peer_id)`, `ConversationType::ServerDm`) whichever
//! transport carried a message, so switching modes never hides history.
//!
//! The transport is picked per send by Shield mode — in
//! [`MessageService::send_friend_dm_content`], and for a user's post (which
//! normal mode delivers through the replaying `dm_outbox`) in
//! [`MessageService::send_server_dm`] / `retry_server_dm`:
//!
//! - **Normal**: sealed sender, streaming. A message is Signal-Protocol-
//!   encrypted once per recipient device and deposited directly into that
//!   device's inbox via `SendSealedMessage`, authorized with the recipient's
//!   Unidentified Access Key rather than our own credentials — so the server
//!   learns the recipient (and the timing) but never the sender. Instant.
//! - **Shield**: the NDSS'21 ephemeral-mailbox transport (`dm.rs`: two
//!   encryption layers, mailbox addresses, bootstrap handshake), over the
//!   onion pool, received by polling. Recipient and timing are hidden too —
//!   the statistical-disclosure signal sealed sender alone leaks (Martiny et
//!   al., NDSS'21 §III-IV).

use prost::Message as ProstMessage;
use rand::TryRngCore;

use crate::domain::ids::{AttachmentId, ChannelId, UserId};
use crate::domain::message::{ConversationType, Message, MessageContent, MixedContent};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::message_client::MessageClient;
use crate::proto::chat_v1::{
    SealedDelivery, SealedDeviceMessage, SealedSenderMessage, SendSealedMessageResponse,
};
use crate::proto::content_v1::AttachmentPointer;
use crate::services::current_user::CurrentUserProvider;

use super::{DmSendResult, MessageError, MessageService};

// Mirrors internal/services/message/adapters/signal_crypto.go. Signal encryption
// and sealed-sender wrapping add bytes; neither compresses the encoded Content.
const MAX_SEALED_BODY_BYTES: usize = crate::crypto::signal::sealed_sender::MAX_SEALED_BODY_BYTES;

pub(super) fn validate_sealed_plaintext_size(encoded_len: usize) -> Result<(), MessageError> {
    // Cheap lower-bound refusal. The bounded actor also checks every actual
    // envelope against this cap before committing any ratchet state.
    if encoded_len > MAX_SEALED_BODY_BYTES {
        return Err(MessageError::InvalidContent(format!(
            "message content too large for sealed delivery: {encoded_len} bytes (max {MAX_SEALED_BODY_BYTES})"
        )));
    }
    Ok(())
}

mod delivery;
mod deposit;
mod posts;
mod profile;
mod recovery;

pub(super) use deposit::{DepositOutcome, record_deposit_error};

#[cfg(test)]
mod tests {
    use super::deposit::pick_sealed_access_key;

    /// One response names every device the batch reached, all under the one
    /// server timestamp the server queued them with.
    #[test]
    fn a_batch_response_is_one_deposit_per_delivered_device() {
        let response = crate::proto::chat_v1::SendSealedMessageResponse {
            success: true,
            delivered_to_devices: vec![1, 3],
            server_timestamp: 42,
            ..Default::default()
        };
        let outcome = super::deposit::DepositOutcome::delivered(&response);
        assert_eq!(outcome.delivered, 2);
        assert_eq!(outcome.deposits, vec![(1, 42), (3, 42)]);
        assert!(outcome.error.is_none());
    }

    #[test]
    fn decryption_error_reports_stop_after_five_per_message() {
        use super::recovery::{DEM_RETRY_CAP, DEM_RETRY_LIMIT, count_dem_retry};
        use crate::domain::ids::UserId;
        let mut retries = std::collections::HashMap::new();
        let (peer, ts) = (UserId::new(), 1_700_000_000_000);
        for _ in 0..DEM_RETRY_LIMIT {
            assert!(count_dem_retry(&mut retries, (peer, 2, ts)));
        }
        assert!(
            !count_dem_retry(&mut retries, (peer, 2, ts)),
            "sixth report for one message"
        );
        assert!(
            count_dem_retry(&mut retries, (peer, 3, ts)),
            "another device's message"
        );
        assert!(
            count_dem_retry(&mut retries, (peer, 2, ts + 1)),
            "another message"
        );

        for i in 0..DEM_RETRY_CAP as u64 {
            count_dem_retry(&mut retries, (peer, 9, i));
        }
        assert!(retries.len() <= DEM_RETRY_CAP, "bounded");
    }

    #[tokio::test]
    async fn retry_while_retries_until_success_and_stops_when_unwanted() {
        use super::MessageError;
        use super::profile::{Backoff, retry_while};
        use std::cell::Cell;
        let quick = Backoff {
            attempts: 5,
            first: std::time::Duration::from_millis(1),
            max: std::time::Duration::from_millis(2),
        };
        let fail = || MessageError::DmError("transient".into());

        // Two transient failures, then delivered.
        let calls = Cell::new(0);
        let ok = retry_while(
            quick,
            || {
                calls.set(calls.get() + 1);
                let n = calls.get();
                async move { if n < 3 { Err(fail()) } else { Ok(()) } }
            },
            || true,
        )
        .await;
        assert!(ok.is_ok());
        assert_eq!(calls.get(), 3);

        // Bounded: gives up after `attempts`.
        let calls = Cell::new(0);
        assert!(
            retry_while(
                quick,
                || {
                    calls.set(calls.get() + 1);
                    async { Err(fail()) }
                },
                || true
            )
            .await
            .is_err()
        );
        assert_eq!(calls.get(), 5);

        // No longer a friend: no further attempt.
        let calls = Cell::new(0);
        assert!(
            retry_while(
                quick,
                || {
                    calls.set(calls.get() + 1);
                    async { Err(fail()) }
                },
                || false
            )
            .await
            .is_err()
        );
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test]
    async fn oversized_content_refuses_before_encryption_boundary() {
        use super::MessageError;
        use crate::services::gateway::submission::{mark_submission_started, track_submission};
        use prost::Message as _;
        for text in ["A".repeat(1024 * 1024), "界".repeat(3000)] {
            let content = crate::services::build_data_message_content(&text, vec![], 1);
            let (result, started, row) = track_submission(
                async {
                    super::validate_sealed_plaintext_size(content.encoded_len())?;
                    mark_submission_started()?;
                    Ok::<_, MessageError>(())
                },
                None,
            )
            .await;
            assert!(matches!(result, Err(MessageError::InvalidContent(_))));
            assert!(!started, "definitely oversized content reached encryption");
            assert!(row.is_none());
        }
        let valid = crate::services::build_data_message_content(&"U".repeat(6000), vec![], 1);
        assert!(super::validate_sealed_plaintext_size(valid.encoded_len()).is_ok());
        assert!(super::validate_sealed_plaintext_size(super::MAX_SEALED_BODY_BYTES).is_ok());
        assert!(super::validate_sealed_plaintext_size(super::MAX_SEALED_BODY_BYTES + 1).is_err());
    }

    #[test]
    fn deposit_failure_keeps_typed_retry_and_permanent_causes() {
        use super::MessageError;
        use super::deposit::{DepositOutcome, record_deposit_error};
        use crate::error::NetworkError;
        for cause in [
            NetworkError::RateLimited,
            NetworkError::Timeout,
            NetworkError::MismatchedDevices {
                missing: vec![2],
                extra: vec![],
                stale: vec![],
            },
        ] {
            let result = DepositOutcome {
                error: Some(MessageError::Network(cause)),
                ..Default::default()
            };
            assert_eq!(result.error.unwrap().permanent_send_failure_reason(), None);
        }
        let mut cause = None;
        record_deposit_error(
            &mut cause,
            MessageError::Network(NetworkError::PermissionDenied),
        );
        assert_eq!(
            cause.as_ref().unwrap().permanent_send_failure_reason(),
            Some("Not delivered")
        );
        record_deposit_error(&mut cause, MessageError::Network(NetworkError::Unavailable));
        record_deposit_error(
            &mut cause,
            MessageError::Network(NetworkError::InvalidRequest("too large".into())),
        );
        assert_eq!(cause.unwrap().permanent_send_failure_reason(), None);
    }

    #[test]
    fn sealed_access_key_comes_from_the_profile_key_we_hold() {
        let pk = [7u8; 32];
        let want = crate::crypto::derive_uak(&pk);
        // Held key wins even when the recipient is unrestricted.
        assert_eq!(pick_sealed_access_key(Some(&pk), false), Some(want));
        assert_eq!(pick_sealed_access_key(Some(&pk), true), Some(want));
        // Unrestricted and no key: any key is accepted, so seal anyway.
        assert!(pick_sealed_access_key(None, true).is_some());
        // Neither: no key to present — the caller delivers identified.
        assert_eq!(pick_sealed_access_key(None, false), None);
    }
}
