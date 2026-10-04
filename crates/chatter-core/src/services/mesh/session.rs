//! Noise XX sessions, keyed by peer.
//!
//! `chatter_mesh_noise` gives us a handshake state machine and a transport
//! state; it deliberately owns no map from peer to state, because that map is
//! session *policy* — when to start one, what to do when two devices start at
//! once, when to throw one away. That policy lives here.
//!
//! # Why this exists at all
//!
//! [`RouterTransport::can_deliver_securely`] must mean "an established Noise
//! session", not "a live link". Without a per-peer session map there is nothing
//! to answer that question with, and the router's first branch — send and
//! retain nothing — would be taken on the strength of a link binding that a
//! replayed announce can forge.
//!
//! [`RouterTransport::can_deliver_securely`]: chatter_mesh_store::RouterTransport::can_deliver_securely

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chatter_mesh::ids::PeerId;
use chatter_mesh_noise::identity::MeshIdentity;
use chatter_mesh_noise::session::{HandshakeRole, NoiseHandshake, NoiseSession};
use chatter_mesh_noise::{NoiseError, fingerprint_of};

/// Length of the opening XX message: one 32-byte ephemeral, no payload.
///
/// Used only to recognise a peer *restarting* a handshake. The two later
/// messages are 96 and 64 bytes, so the discrimination is unambiguous for the
/// empty payloads we send. A wrong guess here costs a handshake round trip, not
/// correctness: the handshake state machine rejects anything it cannot read.
const OPENING_MESSAGE_LEN: usize = 32;

/// A session unused this long is renegotiated on its next use (bitchat
/// `NoiseSecurityConstants.sessionTimeout`, the idle half of
/// `needsRenegotiation()`; the other half is the nonce budget).
pub const IDLE_REKEY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// What a handshake message produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HandshakeStep {
    /// The next handshake message to put on the wire, if the exchange
    /// continues.
    pub reply: Option<Vec<u8>>,
    /// The peer's Noise static public key, set on the step that completes the
    /// handshake. This is the point at which the peer becomes addressable
    /// offline — it is the key courier envelopes are sealed to.
    pub established: Option<[u8; 32]>,
}

/// Per-peer Noise state.
#[derive(Default)]
pub struct SessionManager {
    handshakes: BTreeMap<PeerId, (NoiseHandshake, HandshakeRole)>,
    sessions: BTreeMap<PeerId, NoiseSession>,
    /// Per session: when it last carried a message, and whether a use found
    /// it idle past [`IDLE_REKEY_AFTER`] (a rekey is then due).
    activity: BTreeMap<PeerId, (Instant, bool)>,
}

impl SessionManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether an encrypted delivery to this peer can complete right now.
    #[must_use]
    pub fn has_session(&self, peer: &PeerId) -> bool {
        self.sessions.contains_key(peer)
    }

    /// Whether a handshake is in flight — a send will stall rather than fail.
    #[must_use]
    pub fn is_handshaking(&self, peer: &PeerId) -> bool {
        self.handshakes.contains_key(peer)
    }

    /// The peer's Noise static key, once a session exists.
    #[must_use]
    pub fn remote_static(&self, peer: &PeerId) -> Option<[u8; 32]> {
        self.sessions.get(peer).map(NoiseSession::remote_static)
    }

    /// The peer's fingerprint, once a session exists.
    #[must_use]
    pub fn fingerprint(&self, peer: &PeerId) -> Option<String> {
        self.sessions
            .get(peer)
            .map(|session| fingerprint_of(&session.remote_static()))
    }

    /// Every peer we hold an established session with.
    #[must_use]
    pub fn established_peers(&self) -> Vec<PeerId> {
        self.sessions.keys().cloned().collect()
    }

    /// Begin an outbound handshake, returning the opening message.
    ///
    /// Restarts any handshake already in flight for this peer: a stalled
    /// handshake that is never retried is a peer that can never be messaged
    /// securely, and the far side treats a fresh opening message as a restart
    /// for the same reason.
    pub fn initiate(
        &mut self,
        peer: &PeerId,
        identity: &MeshIdentity,
    ) -> Result<Vec<u8>, NoiseError> {
        let mut handshake = NoiseHandshake::new(HandshakeRole::Initiator, identity)?;
        let message = handshake.write_message(&[])?;
        self.handshakes
            .insert(peer.clone(), (handshake, HandshakeRole::Initiator));
        Ok(message)
    }

    /// Feed an inbound handshake message.
    pub fn on_handshake(
        &mut self,
        peer: &PeerId,
        local_peer: &PeerId,
        identity: &MeshIdentity,
        message: &[u8],
    ) -> Result<HandshakeStep, NoiseError> {
        let is_opening = message.len() == OPENING_MESSAGE_LEN;

        // A peer that restarts — wiped, or simply lost its state — opens a new
        // handshake while we still hold a session. Its half-finished predecessor
        // is worthless and is dropped now, so the new exchange starts from a
        // fresh responder state rather than failing to read out of turn.
        //
        // The established *session*, however, stays until the restart actually
        // completes and authenticates (below). An opening XX message is 32 bytes
        // of ephemeral key with nothing bound to it, and the sender ID that
        // carried it is whatever the sender typed, so tearing a session down on
        // receipt of one hands any device in radio range a way to sever every
        // private conversation we hold, one forged frame at a time. Deferring the
        // teardown to the completing message costs the honest restart nothing:
        // the replacement lands one round trip later either way.
        if is_opening {
            if matches!(
                self.handshakes.get(peer),
                Some((_, HandshakeRole::Initiator))
            ) && local_peer < peer
            {
                // Simultaneous initiation. Both sides hold an initiator state and
                // neither can read the other's opening message, so one has to
                // yield. The rule has to be symmetric and computable from data
                // both sides already have: the lower peer ID keeps its initiator
                // role, the higher yields and becomes responder. Without this,
                // both sides sit on dead handshakes until something else evicts
                // them.
                return Ok(HandshakeStep::default());
            }
            // Either we just yielded the initiator role or this is a restart, and
            // in both cases whatever we held is unreadable from here. Drop it so
            // the exchange begins from a fresh responder state.
            self.handshakes.remove(peer);
        }

        let (mut handshake, role) = match self.handshakes.remove(peer) {
            Some(existing) => existing,
            None => (
                NoiseHandshake::new(HandshakeRole::Responder, identity)?,
                HandshakeRole::Responder,
            ),
        };

        handshake.read_message(message)?;

        if handshake.is_complete() {
            let remote = self.establish(peer, handshake)?;
            return Ok(HandshakeStep {
                reply: None,
                established: Some(remote),
            });
        }

        let reply = handshake.write_message(&[])?;

        if handshake.is_complete() {
            let remote = self.establish(peer, handshake)?;
            return Ok(HandshakeStep {
                reply: Some(reply),
                established: Some(remote),
            });
        }

        self.handshakes.insert(peer.clone(), (handshake, role));
        Ok(HandshakeStep {
            reply: Some(reply),
            established: None,
        })
    }

    /// Turn a completed handshake into the session for `peer` — but only if the
    /// key it authenticated is actually that peer's.
    ///
    /// XX authenticates the remote *static key*, and the mesh peer ID is
    /// `SHA256(static key)[..8]` (`MeshIdentity::peer_id`, and our own ID is built
    /// that way). So at the moment a handshake completes we hold cryptographic
    /// proof of who the far side is, and the sender ID that its packets claimed is
    /// still just a claim. Keying the session on the claim rather than the proof
    /// is a session hijack: an attacker runs a completely valid handshake with
    /// their own static key while every frame claims the victim's ID, and from
    /// then on every private message we address to the victim is encrypted to the
    /// attacker and reads as delivered.
    ///
    /// Refusing is safe: an honest peer's ID always derives from its own key, so
    /// this rejects nothing a real device does. It is the same rule the offline
    /// path already applies to courier envelopes, where the sender is *derived*
    /// from the authenticated key rather than read off the packet.
    fn establish(
        &mut self,
        peer: &PeerId,
        handshake: NoiseHandshake,
    ) -> Result<[u8; 32], NoiseError> {
        let session = handshake.into_session()?;
        let remote = session.remote_static();
        if PeerId::from_noise_key(&remote) != *peer {
            // Drop everything for this peer: the handshake state is spent, and
            // leaving it around would let the impersonator retry against a
            // half-consumed exchange.
            self.handshakes.remove(peer);
            return Err(NoiseError::WrongState(
                "handshake authenticated a key that does not derive the claimed peer ID",
            ));
        }
        self.sessions.insert(peer.clone(), session);
        self.activity.insert(peer.clone(), (Instant::now(), false));
        Ok(remote)
    }

    /// Record a use of `peer`'s session at `now`, flagging a rekey when the
    /// session had sat idle past [`IDLE_REKEY_AFTER`].
    fn touch(&mut self, peer: &PeerId, now: Instant) {
        let (last, idle) = self.activity.entry(peer.clone()).or_insert((now, false));
        if now.saturating_duration_since(*last) > IDLE_REKEY_AFTER {
            *idle = true;
        }
        *last = now;
    }

    /// Encrypt for an established session.
    pub fn encrypt(&mut self, peer: &PeerId, plaintext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        self.encrypt_at(peer, plaintext, Instant::now())
    }

    fn encrypt_at(&mut self, peer: &PeerId, plaintext: &[u8], now: Instant) -> Result<Vec<u8>, NoiseError> {
        let sealed = self
            .sessions
            .get_mut(peer)
            .ok_or(NoiseError::WrongState("no session for peer"))?
            .encrypt(plaintext)?;
        self.touch(peer, now);
        Ok(sealed)
    }

    /// Decrypt for an established session.
    pub fn decrypt(&mut self, peer: &PeerId, ciphertext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let opened = self
            .sessions
            .get_mut(peer)
            .ok_or(NoiseError::WrongState("no session for peer"))?
            .decrypt(ciphertext)?;
        self.touch(peer, Instant::now());
        Ok(opened)
    }

    /// Whether to open a renegotiation with this peer now: its session has
    /// spent 90% of its nonce budget, or was used after sitting idle past
    /// [`IDLE_REKEY_AFTER`] (both halves of bitchat `needsRenegotiation()`),
    /// and no handshake is already replacing it.
    ///
    /// The handshake check matters: [`initiate`](Self::initiate) restarts a
    /// handshake in flight, so re-opening on every send past the threshold would
    /// never let one complete. The old session keeps carrying traffic until the
    /// new handshake completes and [`establish`](Self::establish) swaps it out.
    #[must_use]
    pub fn needs_rekey(&self, peer: &PeerId) -> bool {
        !self.is_handshaking(peer)
            && self
                .sessions
                .get(peer)
                .is_some_and(|session| {
                    session.needs_rekey() || self.activity.get(peer).is_some_and(|(_, idle)| *idle)
                })
    }

    /// Set a session's sent-message count, so tests can reach the rekey
    /// threshold. Requires mesh-noise's `testing` hook.
    #[cfg(all(test, feature = "testing"))]
    pub(crate) fn set_messages_sent_for_testing(&mut self, peer: &PeerId, count: u64) {
        if let Some(session) = self.sessions.get_mut(peer) {
            session.set_messages_sent_for_testing(count);
        }
    }

    /// Forget everything about one peer.
    pub fn forget(&mut self, peer: &PeerId) {
        self.handshakes.remove(peer);
        self.sessions.remove(peer);
        self.activity.remove(peer);
    }

    /// Forget every session. Used by the panic wipe and on shutdown.
    pub fn clear(&mut self) {
        self.handshakes.clear();
        self.sessions.clear();
        self.activity.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core_06::OsRng;

    fn identity() -> MeshIdentity {
        MeshIdentity::generate(&mut OsRng)
    }

    /// Drive a full XX exchange between two managers.
    fn complete(
        alice: &mut SessionManager,
        alice_id: &MeshIdentity,
        bob: &mut SessionManager,
        bob_id: &MeshIdentity,
    ) -> (PeerId, PeerId) {
        let alice_peer = alice_id.peer_id();
        let bob_peer = bob_id.peer_id();

        let m1 = alice
            .initiate(&bob_peer, alice_id)
            .expect("opening message");
        let step = bob
            .on_handshake(&alice_peer, &bob_peer, bob_id, &m1)
            .expect("bob reads m1");
        let m2 = step.reply.expect("bob replies with m2");
        assert!(step.established.is_none());

        let step = alice
            .on_handshake(&bob_peer, &alice_peer, alice_id, &m2)
            .expect("alice reads m2");
        let m3 = step.reply.expect("alice replies with m3");
        assert!(step.established.is_some(), "alice completes on m2");

        let step = bob
            .on_handshake(&alice_peer, &bob_peer, bob_id, &m3)
            .expect("bob reads m3");
        assert!(step.reply.is_none());
        assert!(step.established.is_some(), "bob completes on m3");

        (alice_peer, bob_peer)
    }

    #[test]
    fn a_full_handshake_establishes_a_session_on_both_sides() {
        let (alice_id, bob_id) = (identity(), identity());
        let (mut alice, mut bob) = (SessionManager::new(), SessionManager::new());
        let (alice_peer, bob_peer) = complete(&mut alice, &alice_id, &mut bob, &bob_id);

        assert!(alice.has_session(&bob_peer));
        assert!(bob.has_session(&alice_peer));
        assert_eq!(alice.remote_static(&bob_peer), Some(bob_id.static_public()));
        assert_eq!(bob.remote_static(&alice_peer), Some(alice_id.static_public()));
        assert_eq!(alice.fingerprint(&bob_peer).as_deref(), Some(bob_id.fingerprint().as_str()));
    }

    #[test]
    fn messages_round_trip_after_the_handshake() {
        let (alice_id, bob_id) = (identity(), identity());
        let (mut alice, mut bob) = (SessionManager::new(), SessionManager::new());
        let (alice_peer, bob_peer) = complete(&mut alice, &alice_id, &mut bob, &bob_id);

        let sealed = alice.encrypt(&bob_peer, b"ping").expect("encrypt");
        assert_ne!(sealed, b"ping");
        let opened = bob.decrypt(&alice_peer, &sealed).expect("decrypt");
        assert_eq!(opened, b"ping");
    }

    #[test]
    fn no_session_means_no_encryption_rather_than_plaintext() {
        let mut manager = SessionManager::new();
        let peer = PeerId::from_str("abcdef0123456789");
        assert!(!manager.has_session(&peer));
        assert!(manager.encrypt(&peer, b"secret").is_err());
        assert!(manager.decrypt(&peer, b"secret").is_err());
    }

    #[test]
    fn simultaneous_initiation_resolves_to_exactly_one_initiator() {
        // Both sides open at once; the lower peer ID must keep its initiator
        // role and the higher must yield, or both sit on dead handshakes.
        let (a_id, b_id) = (identity(), identity());
        let (a_peer, b_peer) = (a_id.peer_id(), b_id.peer_id());
        let (low_id, low_peer, high_id, high_peer) = if a_peer < b_peer {
            (&a_id, a_peer, &b_id, b_peer)
        } else {
            (&b_id, b_peer, &a_id, a_peer)
        };

        let mut low = SessionManager::new();
        let mut high = SessionManager::new();
        let low_open = low.initiate(&high_peer, low_id).expect("low opens");
        let high_open = high.initiate(&low_peer, high_id).expect("high opens");

        // The low side sees the high side's opening message and ignores it.
        let step = low
            .on_handshake(&high_peer, &low_peer, low_id, &high_open)
            .expect("low handles the collision");
        assert_eq!(step, HandshakeStep::default(), "low keeps its initiator role");

        // The high side yields and answers as responder.
        let step = high
            .on_handshake(&low_peer, &high_peer, high_id, &low_open)
            .expect("high yields");
        let m2 = step.reply.expect("high replies as responder");

        let step = low
            .on_handshake(&high_peer, &low_peer, low_id, &m2)
            .expect("low reads m2");
        let m3 = step.reply.expect("low replies with m3");
        assert!(step.established.is_some());
        let step = high
            .on_handshake(&low_peer, &high_peer, high_id, &m3)
            .expect("high reads m3");
        assert!(step.established.is_some());

        assert!(low.has_session(&high_peer));
        assert!(high.has_session(&low_peer));
    }

    #[test]
    fn a_peer_restart_replaces_the_stale_session_once_it_completes() {
        let (alice_id, bob_id) = (identity(), identity());
        let (mut alice, mut bob) = (SessionManager::new(), SessionManager::new());
        let (alice_peer, bob_peer) = complete(&mut alice, &alice_id, &mut bob, &bob_id);
        assert!(bob.has_session(&alice_peer));

        // Alice restarts with fresh state and opens again.
        let mut restarted = SessionManager::new();
        let m1 = restarted
            .initiate(&bob_peer, &alice_id)
            .expect("opening message");
        let step = bob
            .on_handshake(&alice_peer, &bob_peer, &bob_id, &m1)
            .expect("bob accepts the restart");
        let m2 = step.reply.expect("bob replies with m2");
        // The old session is still there: nothing has been authenticated yet.
        assert!(bob.has_session(&alice_peer));

        let m3 = restarted
            .on_handshake(&bob_peer, &alice_peer, &alice_id, &m2)
            .expect("alice reads m2")
            .reply
            .expect("alice replies with m3");
        let step = bob
            .on_handshake(&alice_peer, &bob_peer, &bob_id, &m3)
            .expect("bob completes the restart");
        assert!(step.established.is_some());

        // And now the replacement is in force: the *new* session decrypts.
        let sealed = restarted.encrypt(&bob_peer, b"back").expect("encrypt");
        assert_eq!(
            bob.decrypt(&alice_peer, &sealed).expect("decrypt"),
            b"back",
            "the completed restart must have replaced the stale session"
        );
    }

    #[test]
    fn a_bare_opening_message_from_a_stranger_cannot_tear_down_a_session() {
        // The opening XX message carries no proof of anything — it is 32 bytes
        // of ephemeral key, and the sender ID on the packet that wrapped it is
        // whatever the sender typed. Dropping an established session on receipt
        // of one lets any device in radio range sever every private conversation
        // we hold, one forged 32-byte frame at a time.
        let (alice_id, bob_id) = (identity(), identity());
        let (mut alice, mut bob) = (SessionManager::new(), SessionManager::new());
        let (alice_peer, bob_peer) = complete(&mut alice, &alice_id, &mut bob, &bob_id);

        // A stranger opens a handshake while claiming to be Alice.
        let stranger_id = identity();
        let mut stranger = SessionManager::new();
        let forged = stranger
            .initiate(&bob_peer, &stranger_id)
            .expect("opening message");
        let _ = bob.on_handshake(&alice_peer, &bob_peer, &bob_id, &forged);

        assert!(
            bob.has_session(&alice_peer),
            "an unauthenticated opening message must not evict a live session"
        );
        let sealed = alice.encrypt(&bob_peer, b"still here").expect("encrypt");
        assert_eq!(
            bob.decrypt(&alice_peer, &sealed).expect("decrypt"),
            b"still here",
            "and Alice's traffic must still open"
        );
    }

    #[test]
    fn a_handshake_that_authenticates_to_another_peer_id_establishes_nothing() {
        // XX authenticates the remote *static key*, and the mesh peer ID is
        // SHA256 of exactly that key. So once the handshake completes there is
        // no excuse for keying the session on the sender ID the packet claimed:
        // the authenticated key says who this really is. Without the check, an
        // attacker completes a perfectly valid handshake under the victim's peer
        // ID and every subsequent DM to the victim is encrypted to the attacker.
        let victim_id = identity();
        let victim_peer = victim_id.peer_id();

        let local_id = identity();
        let local_peer = local_id.peer_id();
        let mut local = SessionManager::new();

        let attacker_id = identity();
        let attacker_peer = attacker_id.peer_id();
        let mut attacker = SessionManager::new();

        // Every frame the attacker sends claims to come from the victim.
        let m1 = attacker.initiate(&local_peer, &attacker_id).expect("m1");
        let m2 = local
            .on_handshake(&victim_peer, &local_peer, &local_id, &m1)
            .expect("local replies to the opening message")
            .reply
            .expect("m2");
        let m3 = attacker
            .on_handshake(&local_peer, &attacker_peer, &attacker_id, &m2)
            .expect("attacker reads m2")
            .reply
            .expect("m3");

        let result = local.on_handshake(&victim_peer, &local_peer, &local_id, &m3);

        assert!(
            result.is_err(),
            "a handshake whose authenticated static key does not derive the \
             claimed peer ID must fail rather than establish"
        );
        assert!(
            !local.has_session(&victim_peer),
            "and no session may be keyed on the impersonated ID"
        );
    }

    #[cfg(feature = "testing")]
    #[test]
    fn a_rekey_is_due_at_the_threshold_until_a_handshake_replaces_the_session() {
        use chatter_mesh_noise::session::REKEY_MESSAGE_THRESHOLD;

        let (alice_id, bob_id) = (identity(), identity());
        let (mut alice, mut bob) = (SessionManager::new(), SessionManager::new());
        let (alice_peer, bob_peer) = complete(&mut alice, &alice_id, &mut bob, &bob_id);
        assert!(!alice.needs_rekey(&bob_peer));

        alice.set_messages_sent_for_testing(&bob_peer, REKEY_MESSAGE_THRESHOLD);
        assert!(alice.needs_rekey(&bob_peer));

        // Opening the renegotiation suppresses the signal, so the next send does
        // not restart it, and the old session keeps working meanwhile.
        let m1 = alice.initiate(&bob_peer, &alice_id).expect("m1");
        assert!(!alice.needs_rekey(&bob_peer));
        let sealed = alice.encrypt(&bob_peer, b"mid-rekey").expect("old session");
        assert_eq!(bob.decrypt(&alice_peer, &sealed).expect("opens"), b"mid-rekey");

        let m2 = bob
            .on_handshake(&alice_peer, &bob_peer, &bob_id, &m1)
            .expect("bob reads m1")
            .reply
            .expect("m2");
        let m3 = alice
            .on_handshake(&bob_peer, &alice_peer, &alice_id, &m2)
            .expect("alice reads m2")
            .reply
            .expect("m3");
        bob.on_handshake(&alice_peer, &bob_peer, &bob_id, &m3)
            .expect("bob completes");

        // A fresh session: the count is back to zero, and both sides agree.
        assert!(!alice.needs_rekey(&bob_peer));
        let sealed = alice.encrypt(&bob_peer, b"rekeyed").expect("new session");
        assert_eq!(bob.decrypt(&alice_peer, &sealed).expect("opens"), b"rekeyed");
    }

    /// bitchat's idle half: a session first used again after more than
    /// 24 h asks for a rekey on that use, and a completed renegotiation
    /// clears it. A use inside the window does not.
    #[test]
    fn a_session_used_after_24h_idle_is_rekeyed() {
        let (alice_id, bob_id) = (identity(), identity());
        let (mut alice, mut bob) = (SessionManager::new(), SessionManager::new());
        let (alice_peer, bob_peer) = complete(&mut alice, &alice_id, &mut bob, &bob_id);
        let start = alice.activity[&bob_peer].0;

        alice.encrypt_at(&bob_peer, b"soon", start + IDLE_REKEY_AFTER).expect("in window");
        assert!(!alice.needs_rekey(&bob_peer), "exactly 24 h is not idle past it");

        let later = start + IDLE_REKEY_AFTER * 2 + Duration::from_secs(1);
        let sealed = alice.encrypt_at(&bob_peer, b"after a day", later).expect("still usable");
        assert_eq!(bob.decrypt(&alice_peer, &sealed).expect("opens"), b"after a day");
        assert!(alice.needs_rekey(&bob_peer));

        let m1 = alice.initiate(&bob_peer, &alice_id).expect("m1");
        assert!(!alice.needs_rekey(&bob_peer), "one renegotiation at a time");
        let m2 = bob.on_handshake(&alice_peer, &bob_peer, &bob_id, &m1).expect("m1").reply.expect("m2");
        let m3 = alice.on_handshake(&bob_peer, &alice_peer, &alice_id, &m2).expect("m2").reply.expect("m3");
        bob.on_handshake(&alice_peer, &bob_peer, &bob_id, &m3).expect("bob completes");
        assert!(!alice.needs_rekey(&bob_peer), "the new session starts fresh");
    }

    #[test]
    fn forgetting_a_peer_drops_both_handshake_and_session() {
        let (alice_id, bob_id) = (identity(), identity());
        let (mut alice, mut bob) = (SessionManager::new(), SessionManager::new());
        let (_, bob_peer) = complete(&mut alice, &alice_id, &mut bob, &bob_id);

        assert!(alice.has_session(&bob_peer));
        alice.forget(&bob_peer);
        assert!(!alice.has_session(&bob_peer));
        assert!(!alice.is_handshaking(&bob_peer));
        assert!(alice.established_peers().is_empty());
    }

    #[test]
    fn clear_removes_every_session() {
        let (alice_id, bob_id) = (identity(), identity());
        let (mut alice, mut bob) = (SessionManager::new(), SessionManager::new());
        complete(&mut alice, &alice_id, &mut bob, &bob_id);
        alice.clear();
        assert!(alice.established_peers().is_empty());
    }
}
