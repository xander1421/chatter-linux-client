//! Pin the key-transparency log across time.
//!
//! # What was missing
//!
//! [`crate::crypto::verify_kt_proof`] checks that a peer's identity key is in
//! the tree whose root the server just signed. Every part of that is verified
//! — except *which tree*. Each proof was checked in isolation against a head
//! the server supplied in the same response, so a server free to sign heads is
//! free to serve a different tree per user, per device, or per request:
//!
//! - **Fork / split view.** Sign tree A for the victim and tree B for everyone
//!   else. Both verify. The victim's contact sees an identity key nobody else
//!   can observe, which is exactly the attack KT exists to make public.
//! - **Rollback.** Serve an older epoch whose leaf held a key the attacker
//!   controls. The signature is valid; it is simply the wrong point in time.
//!
//! `verify_consistency_proof` — the check that catches both — existed, was
//! correct, and had zero production callers. This module is the caller.
//!
//! # The rule
//!
//! Keep the highest tree head this device has ever accepted. For each new head:
//!
//! | relation to pinned | action |
//! |---|---|
//! | no pin yet | pin it (trust on first use, like the log itself) |
//! | same epoch, same root | accept |
//! | same epoch, **different root** | REJECT — signed fork at one epoch |
//! | older epoch | REJECT — rollback |
//! | newer epoch | fetch a consistency proof pinned→new, verify, then advance |
//!
//! # The binding that is easy to omit
//!
//! [`crate::crypto::verify_consistency_proof`] proves the *response* is
//! internally coherent: both heads signed, epochs ordered, proof roots equal to
//! the signed roots, old leaves reconstructing the old root. It knows nothing
//! about what THIS device previously trusted, so on its own a server can answer
//! with a perfectly valid proof between two epochs of a tree we never saw.
//!
//! [`advance_pinned_head`] therefore compares `old_root` against the pinned
//! root itself, and `new_root`/`new_epoch` against the head being offered.
//! Without those three comparisons the consistency proof is decoration.

use serde::{Deserialize, Serialize};

use crate::crypto::keytransparency::{verify_consistency_proof, KtError};
use std::future::Future;
use std::time::Duration;

use crate::error::NetworkError;
use crate::proto::chat_v1::{GetConsistencyProofResponse, SignedTreeHead};
use crate::storage::repositories::SettingsRepository;

/// Settings key holding [`PinnedTreeHead`].
pub const KT_PINNED_TREE_HEAD: &str = "kt.pinned_tree_head";

/// The highest KT tree head this device has accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedTreeHead {
    pub epoch: u64,
    pub root: [u8; 32],
}

/// What [`advance_pinned_head`] did, for logging and for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorOutcome {
    /// No pin existed; this head is now pinned. Trust on first use.
    FirstPin,
    /// Same epoch and same root as the pin.
    Unchanged,
    /// Newer epoch, consistency proof verified, pin advanced.
    Advanced,
}

fn root_of(sth: &SignedTreeHead) -> Result<[u8; 32], KtError> {
    sth.root_hash
        .as_slice()
        .try_into()
        .map_err(|_| KtError::Malformed("tree head root is not 32 bytes"))
}

/// Read the pinned head, if any.
pub fn pinned_head(settings: &SettingsRepository) -> Option<PinnedTreeHead> {
    settings.get::<PinnedTreeHead>(KT_PINNED_TREE_HEAD).ok().flatten()
}

/// What the caller must do about an offered tree head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorAction {
    /// Nothing to fetch; the head is already accounted for.
    Settled(AnchorOutcome),
    /// The head is newer than the pin. Fetch a consistency proof for this epoch
    /// range and hand it to [`accept_advance`].
    NeedsProof { old_epoch: u64, new_epoch: u64 },
}

/// Classify an offered tree head against the pinned one.
///
/// Deliberately synchronous and transport-free: the caller owns the network
/// call. An earlier version took an async callback, which forced a `RefCell`
/// around the client and made the resulting future `!Send`, breaking every
/// `tokio::spawn` downstream. Splitting the decision from the fetch is both
/// simpler and `Send`.
///
/// `Settled(FirstPin)` also WRITES the pin — first use is the one case with
/// nothing to prove against.
pub fn classify_head(
    settings: &SettingsRepository,
    sth: &SignedTreeHead,
) -> Result<AnchorAction, KtError> {
    let offered_root = root_of(sth)?;
    let offered_epoch = sth.epoch;

    // The server's epoch IS its log size (keytransparency/service.go `state`),
    // so an honest head has epoch == tree_size >= 1; epoch 0 is the empty log,
    // which is never signed. Refusing anything else, before the first pin, is
    // what makes every later advance need a consistency proof from the pinned
    // root: a head signed at epoch 0 over a non-empty tree used to be pinned,
    // and from a 0 pin the next head was re-pinned with no proof, so a fork
    // planted there could step onto the public log.
    if offered_epoch == 0 || offered_epoch != sth.tree_size {
        return Err(KtError::Malformed("tree head epoch is not its log size"));
    }

    let Some(pin) = pinned_head(settings) else {
        store(settings, offered_epoch, offered_root)?;
        return Ok(AnchorAction::Settled(AnchorOutcome::FirstPin));
    };

    if offered_epoch < pin.epoch {
        // Rollback: a validly signed head from before the one we accepted.
        // Nothing about the signature is wrong; the epoch is.
        return Err(KtError::ConsistencyEpochOrder);
    }

    if offered_epoch == pin.epoch {
        return if offered_root == pin.root {
            Ok(AnchorAction::Settled(AnchorOutcome::Unchanged))
        } else {
            // Two different signed roots at one epoch. The only way to produce
            // this is to sign two trees, so it is a fork by definition.
            Err(KtError::ConsistencyRootMismatch)
        };
    }

    Ok(AnchorAction::NeedsProof {
        old_epoch: pin.epoch,
        new_epoch: offered_epoch,
    })
}

/// Verify a fetched consistency proof and advance the pin.
///
/// Call only with the response for the range [`classify_head`] asked for.
pub fn accept_advance(
    settings: &SettingsRepository,
    sth: &SignedTreeHead,
    resp: &GetConsistencyProofResponse,
) -> Result<AnchorOutcome, KtError> {
    let offered_root = root_of(sth)?;
    let pin = pinned_head(settings).ok_or(KtError::Malformed("pin vanished mid-advance"))?;

    verify_consistency_proof(resp)?;
    bind_proof_to_pin(resp, pin, sth.epoch, offered_root)?;

    store(settings, sth.epoch, offered_root)?;
    Ok(AnchorOutcome::Advanced)
}

/// Serializes every read-check-write of the pin.
///
/// [`classify_head`] reads the pin and [`accept_advance`] writes it, with a
/// consistency-proof fetch in between. Two callers checking the same new head
/// concurrently (a prewarm and a profile-key send for one new friend, both
/// establishing a session) used to both see `NeedsProof` from the old pin; the
/// first advanced it, and the second's proof no longer started at the pin, so
/// it was REJECTED as a rewritten log and its session refused.
///
/// One lock for the process, not per database: a client process holds one
/// account, and the critical section is one proof fetch. A process hosting
/// many accounts (the test harness) only serializes those fetches.
static PIN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Proof that the caller holds [`PIN_LOCK`].
pub type PinGuard = tokio::sync::MutexGuard<'static, ()>;

/// Take [`PIN_LOCK`]. Hold it across fetching the head too when the head comes
/// from a fresh KT proof: a head fetched before another caller advanced the pin
/// would otherwise be older than the pin and be rejected as a rollback.
pub async fn lock_pin() -> PinGuard {
    PIN_LOCK.lock().await
}

/// Deadline for every RPC made while [`PIN_LOCK`] is held. The lock is
/// process-wide, so one response the server withholds (keepalive pings still
/// answered, so the transport never fails) would otherwise stall every session
/// establishment and every manifest fetch behind it. Ten seconds matches the
/// manifest fetch's own deadline in `token_manager`.
pub const PIN_RPC_DEADLINE: Duration = Duration::from_secs(10);

/// Run `rpc` under [`PIN_RPC_DEADLINE`]. A timeout is
/// [`NetworkError::Timeout`]: the caller fails that check (an error is never a
/// way past it) and returning drops its [`PinGuard`], freeing the lock.
pub async fn pinned_rpc<T, E>(rpc: impl Future<Output = Result<T, E>>) -> Result<T, E>
where
    E: From<NetworkError>,
{
    within(PIN_RPC_DEADLINE, rpc).await
}

async fn within<T, E>(deadline: Duration, rpc: impl Future<Output = Result<T, E>>) -> Result<T, E>
where
    E: From<NetworkError>,
{
    tokio::time::timeout(deadline, rpc)
        .await
        .unwrap_or_else(|_| Err(NetworkError::Timeout.into()))
}

/// [`classify_head`] then, for a newer head, [`accept_advance`] with the proof
/// `fetch` returns for the pinned->offered range (callers pass the anonymous
/// KT client). Any failure, fetch included, rejects: an error must not be a
/// way past the check. Verify the head's signature first; a first pin is
/// stored unverified.
///
/// `fetch` is `FnOnce` by value, so the future stays `Send` without the
/// `RefCell` an earlier callback design needed (see [`classify_head`]).
pub async fn hold_head<F, Fut>(
    settings: &SettingsRepository,
    sth: &SignedTreeHead,
    fetch: F,
) -> Result<AnchorOutcome, KtError>
where
    F: FnOnce(u64, u64) -> Fut,
    Fut: Future<Output = Result<GetConsistencyProofResponse, NetworkError>>,
{
    let pin = lock_pin().await;
    hold_head_locked(&pin, settings, sth, fetch).await
}

/// [`hold_head`] for a caller already holding [`PIN_LOCK`].
pub async fn hold_head_locked<F, Fut>(
    _pin: &PinGuard,
    settings: &SettingsRepository,
    sth: &SignedTreeHead,
    fetch: F,
) -> Result<AnchorOutcome, KtError>
where
    F: FnOnce(u64, u64) -> Fut,
    Fut: Future<Output = Result<GetConsistencyProofResponse, NetworkError>>,
{
    match classify_head(settings, sth)? {
        AnchorAction::Settled(outcome) => Ok(outcome),
        AnchorAction::NeedsProof { old_epoch, new_epoch } => {
            let resp = pinned_rpc(fetch(old_epoch, new_epoch))
                .await
                .map_err(|_| KtError::ConsistencyUnavailable)?;
            accept_advance(settings, sth, &resp)
        }
    }
}

/// Tie a verified consistency proof to THIS device's history.
///
/// [`verify_consistency_proof`] proves the response is internally coherent, and
/// nothing more — it has no idea what we previously trusted. Without the three
/// comparisons below, a server answers with a genuine, correctly signed proof
/// between two epochs of a tree this device never saw, and the whole check
/// becomes decoration. Kept separate from the transport so it can be tested
/// without minting signed tree heads.
fn bind_proof_to_pin(
    resp: &GetConsistencyProofResponse,
    pin: PinnedTreeHead,
    offered_epoch: u64,
    offered_root: [u8; 32],
) -> Result<(), KtError> {
    let old_sth = resp
        .old_signed_tree_head
        .as_ref()
        .ok_or(KtError::Malformed("missing old_signed_tree_head"))?;
    let new_sth = resp
        .new_signed_tree_head
        .as_ref()
        .ok_or(KtError::Malformed("missing new_signed_tree_head"))?;

    // The proof must START at the exact head we pinned...
    if old_sth.epoch != pin.epoch || root_of(old_sth)? != pin.root {
        return Err(KtError::ConsistencyRootMismatch);
    }
    // ...and END at the head actually being offered, so a server cannot prove
    // consistency to some third head and have us accept a different one.
    if new_sth.epoch != offered_epoch || root_of(new_sth)? != offered_root {
        return Err(KtError::ConsistencyRootMismatch);
    }
    Ok(())
}

fn store(settings: &SettingsRepository, epoch: u64, root: [u8; 32]) -> Result<(), KtError> {
    settings
        .set(KT_PINNED_TREE_HEAD, &PinnedTreeHead { epoch, root })
        .map_err(|_| KtError::Malformed("could not persist the pinned tree head"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn settings() -> (SettingsRepository, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(
            crate::storage::Database::open(&dir.path().join("kt.db"), "testpass").expect("open"),
        );
        db.run_migrations().expect("migrate");
        (SettingsRepository::new(db), dir)
    }

    fn sth(epoch: u64, root: [u8; 32]) -> SignedTreeHead {
        SignedTreeHead {
            epoch,
            tree_size: epoch,
            root_hash: root.to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn first_head_is_pinned_on_first_use() {
        let (s, _dir) = settings();
        let a = classify_head(&s, &sth(7, [1u8; 32])).unwrap();
        assert_eq!(a, AnchorAction::Settled(AnchorOutcome::FirstPin));
        assert_eq!(pinned_head(&s).unwrap(), PinnedTreeHead { epoch: 7, root: [1u8; 32] });
    }

    #[test]
    fn the_same_head_again_is_accepted_without_a_proof() {
        let (s, _dir) = settings();
        classify_head(&s, &sth(7, [1u8; 32])).unwrap();
        assert_eq!(
            classify_head(&s, &sth(7, [1u8; 32])).unwrap(),
            AnchorAction::Settled(AnchorOutcome::Unchanged)
        );
    }

    /// The split-view attack: two different roots signed at one epoch.
    #[test]
    fn a_second_root_at_the_same_epoch_is_a_fork_and_is_rejected() {
        let (s, _dir) = settings();
        classify_head(&s, &sth(7, [1u8; 32])).unwrap();

        assert_eq!(
            classify_head(&s, &sth(7, [2u8; 32])).unwrap_err(),
            KtError::ConsistencyRootMismatch
        );
        assert_eq!(
            pinned_head(&s).unwrap().root,
            [1u8; 32],
            "a rejected head must not overwrite the pin"
        );
    }

    /// The rollback attack: a validly signed but older head.
    #[test]
    fn an_older_epoch_is_rejected_as_a_rollback() {
        let (s, _dir) = settings();
        classify_head(&s, &sth(7, [1u8; 32])).unwrap();

        assert_eq!(
            classify_head(&s, &sth(6, [9u8; 32])).unwrap_err(),
            KtError::ConsistencyEpochOrder
        );
        assert_eq!(pinned_head(&s).unwrap().epoch, 7);
    }

    /// A newer head must not be silently accepted; it must demand a proof for
    /// exactly the pinned->offered range.
    #[test]
    fn a_newer_epoch_demands_a_proof_for_the_pinned_range() {
        let (s, _dir) = settings();
        classify_head(&s, &sth(7, [1u8; 32])).unwrap();

        assert_eq!(
            classify_head(&s, &sth(9, [2u8; 32])).unwrap(),
            AnchorAction::NeedsProof { old_epoch: 7, new_epoch: 9 }
        );
        assert_eq!(
            pinned_head(&s).unwrap().epoch,
            7,
            "classify must not advance the pin before the proof is checked"
        );
    }

    /// Pinned->offered proof (epochs 3->5) from the Go server's log code,
    /// signed with the development tree-head key, which `cargo test` pins.
    fn dev_consistency_vector() -> Option<GetConsistencyProofResponse> {
        use prost::Message;
        let dev_head = hex::decode("88ad412806054a7534e1b9deb42ef249bd7c92709f5bebf1d6be7b0e40926f72").unwrap();
        if crate::config::KT_TREE_HEAD_PUBLIC_KEY_BYTES[..] != dev_head[..] {
            return None;
        }
        let bytes = hex::decode("0ad001080310051a205ed9e9409cde85956181059d21619eb6d03ad1a702a28aab187b36f19e3ac559222093fc7ed082c0645ecda3730127828cbcc45715eb804f7ab9b3a6c88e20a29e962a202c1a31e263ce44dd488fe109624f5f2b6ce878abc94682443a1dfeaae31e101c2a20c3aeb3b4971908ef5a272c19ad3c48fbece6aae37ea81109d0b4dc6a43fbaf342a20d9ecb1f2efc4207cebe9e736aa3bc8c590ce5eb57e9512851026d3712b0f72492a204b1b46d1e688e4ec8c5308c5028bde466a0683e0a3b95784cb79c022fabdee371268080312205ed9e9409cde85956181059d21619eb6d03ad1a702a28aab187b36f19e3ac559180322406760e4ce59358342fd48591390ecb2367a172ade74a7ffece3a7170458bd9015c756e72a75daf44a010f62192e66d99fbd6bac8537deae41b3d946469a26d80a1a680805122093fc7ed082c0645ecda3730127828cbcc45715eb804f7ab9b3a6c88e20a29e9618052240ba4a10369857031171b4bd1b07f57d9fa9e83761553c9b0dfb021a9985d17ea313c2004a79b6dac3eac6eaa28b69bdc42c82c82074ecc85e4f8bb67a57bfca01").unwrap();
        Some(GetConsistencyProofResponse::decode(bytes.as_slice()).unwrap())
    }

    fn fetch_ok(
        resp: GetConsistencyProofResponse,
        asked: &mut Option<(u64, u64)>,
    ) -> impl FnOnce(u64, u64) -> std::future::Ready<Result<GetConsistencyProofResponse, NetworkError>> + '_ {
        move |old, new| {
            *asked = Some((old, new));
            std::future::ready(Ok(resp))
        }
    }

    #[tokio::test]
    async fn hold_head_pins_first_head_without_fetching() {
        let (s, _dir) = settings();
        // A fetch would fail the call: a first pin has nothing to prove against.
        let outcome = hold_head(&s, &sth(7, [1u8; 32]), |_, _| async {
            Err::<GetConsistencyProofResponse, _>(NetworkError::ConnectionFailed("no fetch".into()))
        })
        .await
        .unwrap();
        assert_eq!(outcome, AnchorOutcome::FirstPin);
        assert_eq!(pinned_head(&s).unwrap(), PinnedTreeHead { epoch: 7, root: [1u8; 32] });
    }

    #[tokio::test]
    async fn hold_head_advances_on_a_valid_consistency_proof() {
        let Some(resp) = dev_consistency_vector() else { return };
        let (s, _dir) = settings();
        let old = resp.old_signed_tree_head.clone().unwrap();
        let new = resp.new_signed_tree_head.clone().unwrap();
        classify_head(&s, &old).unwrap();
        let mut asked = None;
        let outcome = hold_head(&s, &new, fetch_ok(resp, &mut asked)).await.unwrap();
        assert_eq!(outcome, AnchorOutcome::Advanced);
        assert_eq!(asked, Some((3, 5)), "the proof is fetched for exactly pinned->offered");
        assert_eq!(pinned_head(&s).unwrap().epoch, 5);
    }

    /// Two concurrent checks of the same new head (V34: a new friend's prewarm
    /// and profile-key send each establishing a session). The second must see
    /// the advanced pin, not reject its own proof as a rewritten log.
    #[tokio::test]
    async fn concurrent_checks_of_one_new_head_both_pass() {
        let Some(resp) = dev_consistency_vector() else { return };
        let (s, _dir) = settings();
        let old = resp.old_signed_tree_head.clone().unwrap();
        let new = resp.new_signed_tree_head.clone().unwrap();
        classify_head(&s, &old).unwrap();

        // The interleaving the lock forbids: both classify against the old
        // pin, the first advances it, the second's proof no longer starts there.
        let needs = AnchorAction::NeedsProof { old_epoch: 3, new_epoch: 5 };
        assert_eq!(classify_head(&s, &new).unwrap(), needs);
        assert_eq!(classify_head(&s, &new).unwrap(), needs);
        assert_eq!(accept_advance(&s, &new, &resp).unwrap(), AnchorOutcome::Advanced);
        assert_eq!(accept_advance(&s, &new, &resp).unwrap_err(), KtError::ConsistencyRootMismatch);
        let (s, _dir) = settings();
        classify_head(&s, &old).unwrap();

        let fetch = |resp: GetConsistencyProofResponse| {
            move |_, _| async move {
                // Let the other check run between classify and accept.
                tokio::task::yield_now().await;
                Ok::<_, NetworkError>(resp)
            }
        };
        let (a, b) = tokio::join!(
            hold_head(&s, &new, fetch(resp.clone())),
            hold_head(&s, &new, fetch(resp)),
        );
        let mut outcomes = [a.unwrap(), b.unwrap()];
        outcomes.sort_by_key(|o| *o as u8);
        assert_eq!(outcomes, [AnchorOutcome::Unchanged, AnchorOutcome::Advanced]);
        assert_eq!(pinned_head(&s).unwrap().epoch, 5);
    }

    /// A newer head whose proof does not start at the root this device
    /// pinned: the log this client saw was not the one the server extended.
    #[tokio::test]
    async fn hold_head_rejects_a_fork_and_keeps_the_pin() {
        let Some(resp) = dev_consistency_vector() else { return };
        let (s, _dir) = settings();
        let new = resp.new_signed_tree_head.clone().unwrap();
        classify_head(&s, &sth(3, [9u8; 32])).unwrap();
        let mut asked = None;
        assert_eq!(
            hold_head(&s, &new, fetch_ok(resp, &mut asked)).await.unwrap_err(),
            KtError::ConsistencyRootMismatch
        );
        assert_eq!(pinned_head(&s).unwrap(), PinnedTreeHead { epoch: 3, root: [9u8; 32] });

        // A failed fetch is a rejection too, never a pass.
        assert_eq!(
            hold_head(&s, &new, |_, _| async {
                Err::<GetConsistencyProofResponse, _>(NetworkError::ConnectionFailed("down".into()))
            })
                .await
                .unwrap_err(),
            KtError::ConsistencyUnavailable
        );
        assert_eq!(pinned_head(&s).unwrap().epoch, 3);
    }

    /// F3: the server's epoch is its log size, so a head at epoch 0 or with
    /// epoch != tree_size was signed by no honest server. It is refused before
    /// it can become a pin, and nothing is stored.
    #[test]
    fn a_head_whose_epoch_is_not_its_log_size_is_never_pinned() {
        let (s, _dir) = settings();
        let mut zero = sth(0, [1u8; 32]);
        zero.tree_size = 1;
        assert_eq!(
            classify_head(&s, &zero).unwrap_err(),
            KtError::Malformed("tree head epoch is not its log size")
        );
        let mut skewed = sth(7, [1u8; 32]);
        skewed.tree_size = 9;
        assert!(classify_head(&s, &skewed).is_err());
        assert!(pinned_head(&s).is_none());
    }

    /// F3: a pin already at epoch 0 (from a build that stored one) no longer
    /// re-pins the next head for free. It needs a consistency proof bound to
    /// that root like any other pin, and a proof from anywhere else is refused.
    #[tokio::test]
    async fn an_epoch_zero_pin_needs_a_proof_like_any_other() {
        let (s, _dir) = settings();
        store(&s, 0, [9u8; 32]).unwrap();
        assert_eq!(
            classify_head(&s, &sth(5, [2u8; 32])).unwrap(),
            AnchorAction::NeedsProof { old_epoch: 0, new_epoch: 5 }
        );
        assert_eq!(
            hold_head(&s, &sth(5, [2u8; 32]), |_, _| async {
                Err::<GetConsistencyProofResponse, _>(NetworkError::InvalidRequest("old_epoch 0".into()))
            })
            .await
            .unwrap_err(),
            KtError::ConsistencyUnavailable
        );
        assert_eq!(pinned_head(&s).unwrap(), PinnedTreeHead { epoch: 0, root: [9u8; 32] });

        // A genuine, correctly signed proof that does not start at this pin.
        let Some(resp) = dev_consistency_vector() else { return };
        let new = resp.new_signed_tree_head.clone().unwrap();
        let mut asked = None;
        assert!(hold_head(&s, &new, fetch_ok(resp, &mut asked)).await.is_err());
        assert_eq!(pinned_head(&s).unwrap().epoch, 0);
    }

    /// A bad proof must leave the pin where it was.
    #[test]
    fn a_failed_advance_leaves_the_pin_untouched() {
        let (s, _dir) = settings();
        classify_head(&s, &sth(7, [1u8; 32])).unwrap();

        let bogus = GetConsistencyProofResponse::default();
        assert!(accept_advance(&s, &sth(8, [2u8; 32]), &bogus).is_err());
        assert_eq!(pinned_head(&s).unwrap().epoch, 7);
    }

    /// The binding this module exists to add, tested directly: a consistency
    /// proof that is internally valid but does not start at OUR pinned root.
    /// Testing `bind_proof_to_pin` rather than the whole path keeps this from
    /// needing a real signing key, and keeps it failing for the right reason.
    #[test]
    fn a_proof_that_does_not_start_at_our_pin_is_rejected() {
        let pin = PinnedTreeHead { epoch: 7, root: [1u8; 32] };
        let resp = GetConsistencyProofResponse {
            // Correctly signed, correctly ordered — but of a tree we never saw.
            old_signed_tree_head: Some(sth(7, [0xAAu8; 32])),
            new_signed_tree_head: Some(sth(8, [2u8; 32])),
            ..Default::default()
        };
        assert_eq!(
            bind_proof_to_pin(&resp, pin, 8, [2u8; 32]).unwrap_err(),
            KtError::ConsistencyRootMismatch
        );
    }

    /// A proof to a DIFFERENT new head than the one being offered. Without the
    /// second comparison the server proves consistency to head X and then has
    /// us pin head Y.
    #[test]
    fn a_proof_that_does_not_end_at_the_offered_head_is_rejected() {
        let pin = PinnedTreeHead { epoch: 7, root: [1u8; 32] };
        let resp = GetConsistencyProofResponse {
            old_signed_tree_head: Some(sth(7, [1u8; 32])),
            new_signed_tree_head: Some(sth(8, [0xBBu8; 32])),
            ..Default::default()
        };
        assert_eq!(
            bind_proof_to_pin(&resp, pin, 8, [2u8; 32]).unwrap_err(),
            KtError::ConsistencyRootMismatch
        );
    }

    /// Same roots, wrong epoch labels — the epoch half of both comparisons.
    #[test]
    fn a_proof_with_mismatched_epochs_is_rejected() {
        let pin = PinnedTreeHead { epoch: 7, root: [1u8; 32] };
        let resp = GetConsistencyProofResponse {
            old_signed_tree_head: Some(sth(6, [1u8; 32])),
            new_signed_tree_head: Some(sth(8, [2u8; 32])),
            ..Default::default()
        };
        assert_eq!(
            bind_proof_to_pin(&resp, pin, 8, [2u8; 32]).unwrap_err(),
            KtError::ConsistencyRootMismatch
        );
    }

    #[test]
    fn a_proof_from_our_pin_to_the_offered_head_is_accepted() {
        let pin = PinnedTreeHead { epoch: 7, root: [1u8; 32] };
        let resp = GetConsistencyProofResponse {
            old_signed_tree_head: Some(sth(7, [1u8; 32])),
            new_signed_tree_head: Some(sth(8, [2u8; 32])),
            ..Default::default()
        };
        assert!(bind_proof_to_pin(&resp, pin, 8, [2u8; 32]).is_ok());
    }

    /// A withheld response fails the check at the deadline instead of holding
    /// the process-wide pin lock forever.
    #[tokio::test]
    async fn a_withheld_response_times_out() {
        let r: Result<(), NetworkError> =
            within(Duration::from_millis(10), std::future::pending()).await;
        assert!(matches!(r, Err(NetworkError::Timeout)));
    }
}
