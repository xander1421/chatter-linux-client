//! Split-view detection by contact gossip (docs/KT-LIBSIGNAL.md, server repo).
//!
//! Each pairwise `Content` carries the sender's pinned head for its route.
//! The receiver proves that head and its own pin are prefixes of one signed
//! head. ContactMonitoring has no auditor, so this is the only check that two
//! devices were shown the same log.

use chatter_proto::content_v1::{Content, KtHeadGossip};
use libsignal_keytrans::{Signature, TreeHead};

use super::*;
use crate::storage::peer_verification::KtRoute;

/// The pin whose head travels on, and is checked against, this route.
pub(crate) fn scope(route: KtRoute) -> PinScope {
    match route {
        KtRoute::Normal => PinScope::Account,
        KtRoute::AnonymousTor => PinScope::Anonymous,
    }
}

/// Add this device's pinned head for `route` to an outgoing `Content`.
/// Bytes that are not a `Content`, or a route with no pin yet, pass unchanged.
pub(crate) fn attach(settings: &SettingsRepository, route: KtRoute, plaintext: Vec<u8>) -> Vec<u8> {
    let Ok(mut content) = Content::decode(plaintext.as_slice()) else {
        return plaintext;
    };
    match load_head(settings, scope(route).head()) {
        Ok(Some(pinned)) => {
            content.kt_head = Some(to_gossip(&pinned.head));
            content.encode_to_vec()
        }
        Ok(None) => plaintext,
        Err(error) => {
            tracing::warn!(%error, "Could not read the transparency head to share");
            plaintext
        }
    }
}

/// The head carried by a decrypted `Content`, if any.
pub(crate) fn received(plaintext: &[u8]) -> Option<KtHeadGossip> {
    Content::decode(plaintext).ok()?.kt_head
}

fn to_gossip(head: &LastTreeHead) -> KtHeadGossip {
    let LastTreeHead(tree_head, root) = head;
    KtHeadGossip {
        tree_size: tree_head.tree_size,
        timestamp: tree_head.timestamp,
        signature: tree_head
            .signatures
            .first()
            .map(|signature| signature.signature.clone())
            .unwrap_or_default(),
        root: root.to_vec(),
    }
}

fn from_gossip(gossip: &KtHeadGossip) -> Result<LastTreeHead, KtError> {
    let root = gossip
        .root
        .as_slice()
        .try_into()
        .map_err(|_| KtError::Invalid("invalid shared tree root"))?;
    if gossip.tree_size == 0 || gossip.tree_size > (1 << 62) {
        return Err(KtError::Invalid("invalid shared tree size"));
    }
    signed_time(gossip.timestamp)?;
    Ok(LastTreeHead(
        TreeHead {
            tree_size: gossip.tree_size,
            timestamp: gossip.timestamp,
            signatures: vec![Signature {
                auditor_public_key: Vec::new(),
                signature: gossip.signature.clone(),
            }],
        },
        root,
    ))
}

/// Libsignal checks the operator signature only inside its proof verifiers.
/// A zero-key Monitor response whose inclusion proof is the head's own root,
/// with that head as the equal-size baseline, leaves only that check.
fn check_signature(verifier: &KeyTransparency, head: &LastTreeHead) -> Result<(), KtError> {
    let response = MonitorResponse {
        tree_head: Some(FullTreeHead {
            tree_head: Some(head.0.clone()),
            ..Default::default()
        }),
        proofs: Vec::new(),
        inclusion: vec![head.1.to_vec()],
    };
    verifier.verify_monitor(
        &MonitorRequest::default(),
        &response,
        MonitorContext {
            last_tree_head: None,
            last_distinguished_tree_head: head,
            data: HashMap::new(),
        },
        signed_time(head.0.timestamp)?,
    )?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// This device's pin and the peer head are prefixes of one signed head.
    Consistent,
    /// The operator signed two heads that cannot belong to one log.
    Split,
    /// The peer sent a head without a valid operator signature.
    Invalid,
}

/// Check a contact's head against this device's pin on `scope`'s route.
/// Transport failures and unclassified proof errors stay errors so the caller
/// retries; only positive evidence of two histories is `Split`.
pub(crate) async fn reconcile(
    client: &mut impl KtTransport,
    settings: &SettingsRepository,
    scope: PinScope,
    peer: &KtHeadGossip,
    deadline: Duration,
) -> Result<Verdict, KtError> {
    let Ok(peer) = from_gossip(peer) else {
        return Ok(Verdict::Invalid);
    };
    let verifier = verifier()?;
    with_deadline(deadline, async {
        loop {
            match reconcile_at(client, settings, scope, &peer, verifier, SystemTime::now()).await {
                Err(KtError::PinChanged) => tokio::task::yield_now().await,
                result => return result,
            }
        }
    })
    .await
}

async fn reconcile_at(
    client: &mut impl KtTransport,
    settings: &SettingsRepository,
    scope: PinScope,
    peer: &LastTreeHead,
    verifier: &KeyTransparency,
    now: SystemTime,
) -> Result<Verdict, KtError> {
    if check_signature(verifier, peer).is_err() {
        return Ok(Verdict::Invalid);
    }
    let pin = Pin::load_at(client, settings, scope, verifier, now).await?;
    for known in [pin.last_head(), Some(&pin.distinguished.head)].into_iter().flatten() {
        if known == peer {
            return Ok(Verdict::Consistent);
        }
        if known.0.tree_size == peer.0.tree_size {
            // Both heads carry a valid operator signature for one size.
            return Ok(Verdict::Split);
        }
    }
    let baseline = pin.last_head().unwrap_or(&pin.distinguished.head);
    let request = KtMonitorRequest {
        keys: Vec::new(),
        consistency: Some(KtConsistency {
            last: Some(baseline.0.tree_size),
            distinguished: Some(peer.0.tree_size),
        }),
    };
    let response = match client.monitor(request.clone()).await {
        // The operator signed this size and now claims its log is smaller.
        Err(NetworkError::InvalidRequest(_)) => return Ok(Verdict::Split),
        result => result?,
    };
    let request = MonitorRequest::decode(request.encode_to_vec().as_slice())?;
    let response = MonitorResponse::decode(response.encode_to_vec().as_slice())?;
    let update = match verifier.verify_monitor(
        &request,
        &response,
        MonitorContext {
            last_tree_head: Some(baseline),
            last_distinguished_tree_head: peer,
            data: HashMap::new(),
        },
        now,
    ) {
        Ok(update) => update,
        Err(error) if history_unverified(&error) => return Ok(Verdict::Split),
        Err(error) => return Err(error.into()),
    };
    // Keep the joint head: later proofs must extend it, so the operator cannot
    // return to a branch the peer head is not on.
    pin.save_head(LastTreeHead(update.tree_head, update.tree_root))?;
    Ok(Verdict::Consistent)
}

impl Pin<'_> {
    fn save_head(&self, head: LastTreeHead) -> Result<(), KtError> {
        let mut conn = self.settings.database().connection()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StorageError::from)?;
        check_setting(&tx, self.scope.head(), encoded_head(self.last.as_ref()))?;
        check_setting(&tx, self.scope.distinguished(), Some(&self.distinguished.encoded))?;
        if self.last_head() != Some(&head) {
            SettingsRepository::set_with_connection(&tx, self.scope.head(), &encode_head(head, self.now))?;
        }
        tx.commit().map_err(StorageError::from)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
