//! Split-view detection by contact gossip (docs/KT-LIBSIGNAL.md, server repo).
//!
//! Each pairwise `Content` carries the sender's pinned head for the route it
//! was sent on. The receiver later proves that head and its own pin are
//! prefixes of one signed head. ContactMonitoring has no auditor, so this is
//! the only check that two devices were shown the same log.

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

/// The route the sender attached `head` for. It is checked only there.
pub(crate) fn route(head: &KtHeadGossip) -> KtRoute {
    if head.anonymous {
        KtRoute::AnonymousTor
    } else {
        KtRoute::Normal
    }
}

/// Add this device's pinned head for `route` to an outgoing `Content`.
/// Bytes that are not a `Content`, or a route with no pin yet, pass unchanged.
pub(crate) fn attach(settings: &SettingsRepository, route: KtRoute, plaintext: Vec<u8>) -> Vec<u8> {
    let Ok(mut content) = Content::decode(plaintext.as_slice()) else {
        return plaintext;
    };
    let pinned = match load_head(settings, scope(route).head()) {
        Ok(Some(pinned)) => pinned,
        Ok(None) => return plaintext,
        Err(error) => {
            tracing::warn!(%error, "Could not read the transparency head to share");
            return plaintext;
        }
    };
    let mut head = to_gossip(&pinned.head);
    head.anonymous = route == KtRoute::AnonymousTor;
    // A padded message either has padding or landed on a bucket exactly.
    let padded = !content.padding.is_empty() || BUCKETS.contains(&plaintext.len());
    content.kt_head = Some(head);
    if !padded {
        return content.encode_to_vec();
    }
    // Re-pad with the head included: the same bucket when it fits, the next
    // one when it does not. Past the largest bucket the message goes without
    // a head rather than leave its bucket.
    crate::services::message::MessageService::pad_content_to_bucket(&mut content);
    let sent = content.encode_to_vec();
    if BUCKETS.contains(&sent.len()) {
        sent
    } else {
        plaintext
    }
}

/// `MessageService::pad_content_to_bucket`'s sizes.
const BUCKETS: [usize; 4] = [256, 512, 1024, 4096];

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
        anonymous: false,
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
    /// Signed evidence of two logs: two heads of one size with different
    /// roots, or a later-signed head smaller than the peer's.
    Split,
    /// The peer sent a head without a valid operator signature.
    Invalid,
    /// The check failed between two routine requests on the same connection
    /// that were answered and verified.
    Withheld,
    /// Nothing was learned: a routine request failed. A long streak while
    /// identified requests succeed raises a warning (`storage::peer_kt_heads`).
    Undecided,
    /// No check was sent: this pin monitors no label to blend in, or holds no
    /// data for it yet. Counts toward nothing.
    Skipped,
}

/// Check a contact's head against this device's pin on `scope`'s route. Each
/// request has its own `step` deadline.
pub(crate) async fn reconcile(
    client: &mut AnonymousKtClient,
    settings: &SettingsRepository,
    scope: PinScope,
    peer: &KtHeadGossip,
    own_label: &str,
    step: Duration,
) -> Result<Verdict, KtError> {
    let Ok(peer) = from_gossip(peer) else {
        return Ok(Verdict::Invalid);
    };
    let verifier = verifier()?;
    loop {
        match reconcile_at(
            client,
            settings,
            scope,
            &peer,
            own_label.as_bytes(),
            verifier,
            SystemTime::now(),
            step,
        )
        .await
        {
            Err(KtError::PinChanged) => tokio::task::yield_now().await,
            result => return result,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn reconcile_at(
    client: &mut impl KtTransport,
    settings: &SettingsRepository,
    scope: PinScope,
    peer: &LastTreeHead,
    own_label: &[u8],
    verifier: &KeyTransparency,
    now: SystemTime,
    step: Duration,
) -> Result<Verdict, KtError> {
    if check_signature(verifier, peer).is_err() {
        return Ok(Verdict::Invalid);
    }
    let pin = match with_deadline(step, Pin::load_at(client, settings, scope, verifier, now)).await {
        Ok(pin) => pin,
        Err(KtError::Storage(error)) => return Err(error.into()),
        Err(KtError::PinChanged) => return Err(KtError::PinChanged),
        Err(_) => return Ok(Verdict::Undecided),
    };
    for known in [pin.last_head(), Some(&pin.distinguished.head)].into_iter().flatten() {
        if known == peer {
            return Ok(Verdict::Consistent);
        }
        if known.0.tree_size == peer.0.tree_size {
            // Both heads carry a valid operator signature for one size.
            return Ok(Verdict::Split);
        }
    }
    // The check has the shape of routine contact monitoring: one label this
    // pin already monitors, chosen at random and never our own.
    let Some(label) = cover_label(&pin, own_label)? else {
        return Ok(Verdict::Skipped);
    };
    let observations = pin.observations(&label)?;
    let Some(data) = observations.data.clone() else {
        return Ok(Verdict::Skipped);
    };
    let baseline = pin.last_head().unwrap_or(&pin.distinguished.head).clone();
    let checkpoint = pin.distinguished.head.clone();
    // A routine request first: it opens the connection, so connection or
    // circuit setup never falls inside the check's deadline.
    if pin
        .monitor_with(client, &label, &data, &baseline, &checkpoint, step)
        .await
        .is_err()
    {
        return Ok(Verdict::Undecided);
    }
    if let Ok((head, data)) = pin
        .monitor_with(client, &label, &data, &baseline, peer, step)
        .await
    {
        // Keep the joint head: later proofs must extend it, so the operator
        // cannot return to a branch the peer head is not on.
        pin.save(head, observations, Some(data), |_| Ok(()))?;
        return Ok(Verdict::Consistent);
    }
    // The check failed. It was withheld only if the same connection still
    // answers a routine request afterwards. An error code is not evidence; a
    // signed head is.
    let Ok((head, _)) = pin
        .monitor_with(client, &label, &data, &baseline, &checkpoint, step)
        .await
    else {
        return Ok(Verdict::Undecided);
    };
    let rollback = head.0.tree_size < peer.0.tree_size && head.0.timestamp > peer.0.timestamp;
    Ok(if rollback {
        Verdict::Split
    } else {
        Verdict::Withheld
    })
}

/// A random label with stored observations on this pin, excluding our own
/// account and the checkpoint label.
fn cover_label(pin: &Pin<'_>, own_label: &[u8]) -> Result<Option<Vec<u8>>, KtError> {
    use rand::Rng;
    let prefix = pin.scope.label_key("label", b"");
    let conn = pin.settings.database().read_connection()?;
    let mut query = conn
        .prepare("SELECT key FROM settings WHERE key LIKE ?1 || '%'")
        .map_err(StorageError::from)?;
    let labels: Vec<Vec<u8>> = query
        .query_map([&prefix], |row| row.get::<_, String>(0))
        .map_err(StorageError::from)?
        .filter_map(|key| {
            key.ok()?
                .strip_prefix(prefix.as_str())
                .and_then(|label| hex::decode(label).ok())
        })
        .filter(|label| label.as_slice() != own_label && label.as_slice() != DISTINGUISHED)
        .collect();
    if labels.is_empty() {
        return Ok(None);
    }
    let pick = rand::rng().random_range(0..labels.len());
    Ok(labels.into_iter().nth(pick))
}

impl Pin<'_> {
    /// Monitor one observed label with `baseline` and `second` as the two
    /// client-held heads the response must extend.
    async fn monitor_with(
        &self,
        client: &mut impl KtTransport,
        label: &[u8],
        data: &MonitoringData,
        baseline: &LastTreeHead,
        second: &LastTreeHead,
        step: Duration,
    ) -> Result<(LastTreeHead, MonitoringData), KtError> {
        let request = KtMonitorRequest {
            keys: vec![KtMonitorKey {
                search_key: label.to_vec(),
                entry_position: data.latest_log_position(),
                commitment_index: data.index.to_vec(),
            }],
            consistency: Some(KtConsistency {
                last: Some(baseline.0.tree_size),
                distinguished: Some(second.0.tree_size),
            }),
        };
        let response = tokio::time::timeout(step, client.monitor(request.clone()))
            .await
            .map_err(|_| NetworkError::Timeout)??;
        let request = MonitorRequest::decode(request.encode_to_vec().as_slice())?;
        let response = MonitorResponse::decode(response.encode_to_vec().as_slice())?;
        let mut update = self.verifier.verify_monitor(
            &request,
            &response,
            MonitorContext {
                last_tree_head: Some(baseline),
                last_distinguished_tree_head: second,
                data: HashMap::from([(label.to_vec(), data.clone())]),
            },
            self.now,
        )?;
        let data = update
            .monitoring_data
            .remove(label)
            .unwrap_or_else(|| data.clone());
        Ok((LastTreeHead(update.tree_head, update.tree_root), data))
    }
}

#[cfg(test)]
mod tests;
