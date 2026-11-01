use super::*;

impl SignalService {
    /// Key Transparency gate (Phase 2).
    ///
    /// Before trusting the identity key inside a peer's prekey bundle, fetch the
    /// server's signed KT inclusion proof for that peer and verify that the
    /// bundle's identity key is exactly the one committed in the append-only KT
    /// log, under the client-pinned tree-head public key
    /// (`config::KT_TREE_HEAD_PUBLIC_KEY_BYTES`).
    ///
    /// No-proof policy (deliberate, LOUD, not silent):
    /// * peer HAS a leaf, key matches  -> Ok (verified).
    /// * peer HAS a leaf, key differs   -> REJECT (F9 forged/MITM identity key).
    /// * peer has NO leaf (NotFound)    -> REJECT with
    ///   [`CryptoError::NotInTransparencyLog`]. The server logs every identity
    ///   key in the transaction that stores it, so "no leaf" is never honest
    ///   for a peer with a bundle; accepting it would let a server skip KT for
    ///   any peer it wants to MITM.
    /// * any other transport/server error -> REJECT (an error is not evidence of
    ///   an absent leaf; a malicious server must not be able to error its way
    ///   past the check).
    /// Hold the KT log to one history over time.
    ///
    /// `verify_kt_proof` verifies a peer's key against the head in the SAME
    /// response, so a server that signs heads can serve a different tree per
    /// user or per request and every proof still verifies. This compares the
    /// offered head against the highest one this device ever accepted and, when
    /// it is newer, requires a consistency proof from the pinned head to it.
    ///
    /// Failures REJECT the session rather than warn. The server can rebuild
    /// state at any epoch by replaying its log (`Service.replay`), so it has no honest
    /// reason to fail — and a server able to suppress this check by erroring
    /// could fork the log undetectably, which is the same reasoning that
    /// already makes a transport error on `GetProof` a reject rather than a
    /// "no leaf".
    ///
    /// `fetch_consistency` is the channel's own consistency-proof RPC, so the
    /// anonymous path never falls back to an identified request.
    async fn check_tree_head_consistency<F, Fut>(
        &self,
        pin: &kt_anchor::PinGuard,
        fetch_consistency: F,
        recipient_id: &str,
        proof: &crate::proto::chat_v1::GetProofResponse,
    ) -> Result<(), ServiceError>
    where
        F: FnOnce(u64, u64) -> Fut,
        Fut: std::future::Future<
                Output = Result<
                    crate::proto::chat_v1::GetConsistencyProofResponse,
                    crate::error::NetworkError,
                >,
            >,
    {
        let Some(sth) = proof.signed_tree_head.as_ref() else {
            return Err(ServiceError::Validation(
                "KT proof has no signed tree head".to_string(),
            ));
        };

        match kt_anchor::hold_head_locked(pin, &self.settings_repo, sth, fetch_consistency).await {
            Ok(kt_anchor::AnchorOutcome::FirstPin) => {
                tracing::info!(
                    epoch = sth.epoch,
                    "KT: pinned the transparency log's tree head for the first time"
                );
                Ok(())
            }
            Ok(kt_anchor::AnchorOutcome::Advanced) => {
                tracing::info!(
                    new_epoch = sth.epoch,
                    "KT: advanced the pinned tree head; log verified append-only"
                );
                Ok(())
            }
            Ok(kt_anchor::AnchorOutcome::Unchanged) => Ok(()),
            Err(e) => {
                e.trace_rejection();
                tracing::error!(
                    recipient = %recipient_id,
                    offered_epoch = sth.epoch,
                    error = %e,
                    "KT REJECT: could not establish continuity with the pinned tree head"
                );
                Err(ServiceError::Validation(format!(
                    "Key Transparency consistency check failed for {recipient_id}: {e}"
                )))
            }
        }
    }

    pub(super) async fn verify_peer_key_transparency(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        bundle: &libsignal_protocol::PreKeyBundle,
    ) -> Result<(), ServiceError> {
        let identity_key = bundle
            .identity_key()
            .map_err(|e| ServiceError::Validation(format!("bundle missing identity key: {e}")))?
            .serialize();
        self.verify_identity_key_transparency(auth_client, recipient_id, &identity_key)
            .await
    }

    /// The KT gate on a bare identity key: `Ok(())` iff `identity_key` is
    /// `user_id`'s key in the pinned, append-only transparency log. Used for
    /// prekey bundles and for the authors of signed channel membership changes.
    pub async fn verify_identity_key_transparency(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
        identity_key: &[u8],
    ) -> Result<(), ServiceError> {
        // Held from the proof fetch through the pin update, so this head is
        // never older than a pin a concurrent check just advanced
        // (`kt_anchor::lock_pin`).
        let pin = kt_anchor::lock_pin().await;
        let fetched =
            kt_anchor::pinned_rpc(auth_client.get_key_transparency_proof(recipient_id)).await;
        self.judge_kt_proof(
            &pin,
            "KT",
            recipient_id,
            identity_key,
            fetched,
            |old, new| auth_client.get_consistency_proof(old, new),
        )
        .await
    }

    /// Return the identity key from one proof only after the same inclusion
    /// and pinned-head checks used for prekey bundles have accepted it.
    pub async fn verified_identity_key_from_kt(
        &self,
        auth_client: &mut AuthClient,
        recipient_id: &str,
    ) -> Result<Vec<u8>, ServiceError> {
        let pin = kt_anchor::lock_pin().await;
        let fetched =
            kt_anchor::pinned_rpc(auth_client.get_key_transparency_proof(recipient_id)).await;
        let identity_key = fetched
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .map(|proof| proof.identity_key.clone())
            .unwrap_or_default();
        self.judge_kt_proof(
            &pin,
            "KT",
            recipient_id,
            &identity_key,
            fetched,
            |old, new| auth_client.get_consistency_proof(old, new),
        )
        .await?;
        Ok(identity_key)
    }

    /// The verdict both KT gates (identified and anonymous) reach once the
    /// proof fetch has returned. `path` labels the log lines.
    async fn judge_kt_proof<F, Fut>(
        &self,
        pin: &kt_anchor::PinGuard,
        path: &'static str,
        recipient_id: &str,
        identity_key: &[u8],
        fetched: Result<
            Option<crate::proto::chat_v1::GetProofResponse>,
            crate::error::NetworkError,
        >,
        fetch_consistency: F,
    ) -> Result<(), ServiceError>
    where
        F: FnOnce(u64, u64) -> Fut,
        Fut: std::future::Future<
                Output = Result<
                    crate::proto::chat_v1::GetConsistencyProofResponse,
                    crate::error::NetworkError,
                >,
            >,
    {
        let proof = match fetched {
            Ok(Some(proof)) => proof,
            Ok(None) => {
                tracing::error!(
                    recipient = %recipient_id,
                    "{path} REJECT: server says the peer has no Key Transparency entry — every \
                     stored identity key is logged, so this is the server skipping KT; refusing \
                     session"
                );
                return Err(ServiceError::Crypto(CryptoError::NotInTransparencyLog {
                    peer_id: recipient_id.to_string(),
                }));
            }
            Err(e) => {
                tracing::error!(
                    recipient = %recipient_id,
                    error = %e,
                    "{path}: proof fetch failed (transport/server error or deadline) — refusing \
                     session (an error is not evidence of an absent leaf)"
                );
                return Err(ServiceError::Network(e));
            }
        };

        match crate::crypto::verify_kt_proof(recipient_id, identity_key, &proof) {
            Ok(()) => {
                let epoch = proof
                    .signed_tree_head
                    .as_ref()
                    .map(|s| s.epoch)
                    .unwrap_or_default();
                tracing::debug!(
                    recipient = %recipient_id,
                    epoch,
                    "{path}: peer identity key verified against the signed transparency log"
                );
                // The inclusion proof says the key is in the tree the server
                // just signed. It says nothing about WHICH tree, so on its own
                // it cannot see a fork or a rollback. Pin the head across time
                // as well.
                self.check_tree_head_consistency(pin, fetch_consistency, recipient_id, &proof)
                    .await
            }
            // `Malformed` means the proof doesn't even have the shape this
            // verifier expects (e.g. missing prefix_root) — that's a
            // protocol-version mismatch (client speaks KT v2, two-tree/log-sealed;
            // this server still speaks v1, single-tree), not evidence of a forged
            // key. Every OTHER variant is a real cryptographic verification
            // failure against a structurally-valid v2 proof and must keep
            // hard-rejecting even here.
            #[cfg(feature = "dev-trust-root")]
            Err(KtError::Malformed(reason)) => {
                tracing::warn!(
                    recipient = %recipient_id,
                    reason,
                    "{path} DOWNGRADE (dev-trust-root, protocol version mismatch): server's KT \
                     proof doesn't match the v2 shape this client verifies (server is still on \
                     v1) — trusting the bundle identity key WITHOUT a KT proof. NEVER hit \
                     outside a dev-trust-root build."
                );
                Ok(())
            }
            Err(e) => {
                e.trace_rejection();
                tracing::error!(
                    recipient = %recipient_id,
                    error = %e,
                    "{path} REJECT: refusing session — bundle identity key is NOT the key \
                     committed in the Key Transparency log (possible forged/MITM key, F9)"
                );
                Err(ServiceError::Validation(format!(
                    "Key Transparency verification failed for {recipient_id}: {e}"
                )))
            }
        }
    }
    /// Key Transparency gate for the ANONYMOUS-DM path (Phase 3a).
    ///
    /// The anonymous analog of `verify_peer_key_transparency`. Same semantics
    /// (verify the bundle's identity key is the one committed in the signed KT
    /// log under the pinned tree-head key; REJECT a mismatch; REJECT a
    /// transport/server error and `NotFound`), but it fetches the
    /// proof over the isolated ANONYMOUS channel via `AnonymousKtClient` — no
    /// authenticated identity — because the anon path has none.
    ///
    /// This closes the anon-path MITM hole: before Phase 3a the anon path
    /// trusted peer identity keys blindly (no KT call at all). Its tree head
    /// is held to this device's pin exactly like the identified path's.
    pub(super) async fn verify_peer_key_transparency_anon(
        &self,
        anon_kt_client: &mut crate::network::grpc::AnonymousKtClient,
        recipient_id: &str,
        bundle: &libsignal_protocol::PreKeyBundle,
    ) -> Result<(), ServiceError> {
        let identity_key = bundle
            .identity_key()
            .map_err(|e| ServiceError::Validation(format!("bundle missing identity key: {e}")))?
            .serialize();
        // Same pin discipline as the identified path (`kt_anchor::lock_pin`).
        let pin = kt_anchor::lock_pin().await;
        let fetched = kt_anchor::pinned_rpc(anon_kt_client.get_proof(recipient_id)).await;
        // The consistency proof comes over the same anonymous channel: without
        // it a server could fork the log for anonymous lookups alone.
        self.judge_kt_proof(
            &pin,
            "KT(anon)",
            recipient_id,
            &identity_key,
            fetched,
            |old, new| anon_kt_client.get_consistency_proof(old, new),
        )
        .await
    }
}

#[cfg(test)]
mod kt_pin_tests {
    use super::*;
    use crate::proto::chat_v1::{GetConsistencyProofResponse, GetProofResponse, SignedTreeHead};

    /// The anchor both KT paths (identified and `verify_peer_key_transparency_anon`)
    /// run after the inclusion proof: a head that forks from the pin is refused
    /// without asking the server anything.
    #[tokio::test]
    async fn a_head_forked_from_the_pin_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            crate::storage::Database::open(&dir.path().join("kt.db"), "testpass").unwrap(),
        );
        db.run_migrations().unwrap();
        let settings = Arc::new(crate::storage::repositories::SettingsRepository::new(
            db.clone(),
        ));
        let svc = SignalService::new(
            Arc::new(crate::storage::KeyStore::new(db)),
            settings.clone(),
        );

        let head = |root: u8| GetProofResponse {
            signed_tree_head: Some(SignedTreeHead {
                epoch: 5,
                tree_size: 5,
                root_hash: vec![root; 32],
                ..Default::default()
            }),
            ..Default::default()
        };
        fn no_fetch(
            _: u64,
            _: u64,
        ) -> std::future::Ready<Result<GetConsistencyProofResponse, crate::error::NetworkError>>
        {
            panic!("a fork needs no consistency proof")
        }
        let pin = kt_anchor::lock_pin().await;
        svc.check_tree_head_consistency(&pin, no_fetch, "bob", &head(1))
            .await
            .unwrap();
        let err = svc
            .check_tree_head_consistency(&pin, no_fetch, "bob", &head(2))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("consistency check failed"),
            "{err}"
        );
        assert_eq!(
            kt_anchor::pinned_head(&settings).unwrap().unwrap().root,
            [1u8; 32]
        );
    }

    /// Both KT gates hand their fetch result to `judge_kt_proof`. A server
    /// answering NotFound must get a refused session, not a TOFU downgrade.
    #[tokio::test]
    async fn not_found_refuses_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            crate::storage::Database::open(&dir.path().join("kt.db"), "testpass").unwrap(),
        );
        db.run_migrations().unwrap();
        let settings = Arc::new(crate::storage::repositories::SettingsRepository::new(
            db.clone(),
        ));
        let svc = SignalService::new(
            Arc::new(crate::storage::KeyStore::new(db)),
            settings.clone(),
        );
        fn no_fetch(
            _: u64,
            _: u64,
        ) -> std::future::Ready<Result<GetConsistencyProofResponse, crate::error::NetworkError>>
        {
            panic!("no proof, nothing to hold")
        }

        let pin = kt_anchor::lock_pin().await;
        for path in ["KT", "KT(anon)"] {
            let err = svc
                .judge_kt_proof(&pin, path, "bob", &[5u8; 33], Ok(None), no_fetch)
                .await
                .unwrap_err();
            assert!(
                matches!(&err, ServiceError::Crypto(CryptoError::NotInTransparencyLog { peer_id }) if peer_id == "bob"),
                "{path}: {err}"
            );
        }
        assert!(kt_anchor::pinned_head(&settings).unwrap().is_none());
    }
}
