//! Interactive Noise `XX` sessions: the three-message handshake and the
//! transport state it splits into.
//!
//! Ported from `Noise/NoiseSession.swift`, `Noise/SecureNoiseSession.swift` and
//! `Noise/NoiseSecurityConstants.swift`. The handshake itself is `snow`'s —
//! bitchat validates its hand-rolled implementation against the canonical
//! specification vectors, so a spec-conformant library interoperates with it by
//! construction. What is ported here is the *wiring*: the protocol name, the
//! empty prologue bitchat uses for interactive sessions (unlike the seals in
//! [`crate::seal`], which are domain-separated), the received-key validation,
//! and the nonce budget.
//!
//! Sans-IO and clock-free. bitchat's `needsRenegotiation()` also fires on a
//! 24-hour idle timeout; that half needs a clock and therefore belongs to the
//! caller. [`NoiseSession::needs_rekey`] reports the message-count half, and
//! nothing here ever rekeys on its own — the caller decides.

// The pinned-ephemeral constructors below are gated on `feature = "vectors"`,
// which the manifest does not (yet) declare. Cargo's check-cfg would flag the
// unknown feature name; the gate is deliberate, so silence it here rather than
// weaken the gate to something that would ship test hooks in release builds.
#![allow(unexpected_cfgs)]

use snow::error::StateProblem;
use snow::params::NoiseParams;
use snow::{Builder, HandshakeState, TransportState};

use crate::NOISE_XX_PROTOCOL;
use crate::error::NoiseError;
use crate::identity::{MeshIdentity, fingerprint_of};
use crate::seal::validate_public_key;

/// Largest single Noise message, from the specification and from
/// `NoiseSecurityConstants.maxMessageSize`. Anything larger is refused before it
/// reaches the cipher, so a hostile length cannot drive an allocation.
pub const MAX_MESSAGE_SIZE: usize = 65535;

/// ChaCha20-Poly1305 authentication tag length.
pub(crate) const TAG_LEN: usize = 16;

/// Worst-case bytes a handshake message adds on top of its payload: one 32-byte
/// ephemeral public key, one 32-byte static public key with its 16-byte tag, and
/// the payload's own tag. `XX` message two and the `X` seal message both hit
/// this exactly; every other message is smaller.
pub(crate) const MAX_HANDSHAKE_OVERHEAD: usize = 32 + 32 + TAG_LEN + TAG_LEN;

/// Hard nonce budget for one session, from
/// `NoiseSecurityConstants.maxMessagesPerSession`. Encryption fails outright at
/// this point rather than reusing a nonce.
pub const MAX_MESSAGES_PER_SESSION: u64 = 1_000_000_000;

/// Advisory rekey threshold: 90% of the budget, matching
/// `SecureNoiseSession.needsRenegotiation()`. Reaching it is a signal, not an
/// error — the session keeps working until [`MAX_MESSAGES_PER_SESSION`].
pub const REKEY_MESSAGE_THRESHOLD: u64 = MAX_MESSAGES_PER_SESSION / 10 * 9;

/// Which side of the handshake we are. The initiator sends `XX` message one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HandshakeRole {
    /// Sends the first message.
    Initiator,
    /// Reads the first message.
    Responder,
}

/// An in-progress Noise `XX` handshake.
///
/// Drive it by alternating [`write_message`](Self::write_message) and
/// [`read_message`](Self::read_message) in pattern order; the underlying state
/// machine rejects an out-of-turn call rather than silently reordering. Once
/// [`is_complete`](Self::is_complete) is true, convert with
/// [`into_session`](Self::into_session).
pub struct NoiseHandshake {
    state: HandshakeState,
}

impl NoiseHandshake {
    /// Starts a Noise `XX` handshake with our long-term static key.
    ///
    /// The prologue is empty, matching `NoiseSession.swift`, which constructs
    /// its `XX` states without one. Do not add domain separation here without
    /// changing bitchat too: the prologue is hashed into the transcript, so a
    /// mismatch fails the handshake against every real peer.
    pub fn new(role: HandshakeRole, identity: &MeshIdentity) -> Result<Self, NoiseError> {
        Self::build(
            NOISE_XX_PROTOCOL,
            role,
            Some(identity.static_secret()),
            None,
            None,
            &[],
        )
    }

    /// Shared construction path for every handshake this crate builds.
    fn build(
        protocol: &str,
        role: HandshakeRole,
        local_static: Option<&[u8; 32]>,
        remote_static: Option<&[u8; 32]>,
        fixed_ephemeral: Option<&[u8; 32]>,
        prologue: &[u8],
    ) -> Result<Self, NoiseError> {
        let params: NoiseParams = protocol
            .parse()
            .map_err(|_| NoiseError::Setup(format!("unsupported protocol name {protocol}")))?;

        let mut builder = Builder::new(params);
        if let Some(key) = local_static {
            builder = builder.local_private_key(key).map_err(setup_error)?;
        }
        if let Some(key) = remote_static {
            validate_public_key(key)?;
            builder = builder.remote_public_key(key).map_err(setup_error)?;
        }
        if let Some(ephemeral) = fixed_ephemeral {
            builder = builder.fixed_ephemeral_key_for_testing_only(ephemeral);
        }
        // Always set the prologue, including when it is empty: Noise mixes it
        // into the transcript hash unconditionally, so "empty" is not "absent".
        builder = builder.prologue(prologue).map_err(setup_error)?;

        let state = match role {
            HandshakeRole::Initiator => builder.build_initiator(),
            HandshakeRole::Responder => builder.build_responder(),
        }
        .map_err(setup_error)?;

        Ok(Self { state })
    }

    /// Writes the next handshake message, carrying `payload`.
    ///
    /// In `XX` the first message's payload is *not* encrypted — no key exists
    /// yet — so callers must not put anything secret in it.
    pub fn write_message(&mut self, payload: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if payload.len() > MAX_MESSAGE_SIZE.saturating_sub(MAX_HANDSHAKE_OVERHEAD) {
            return Err(NoiseError::Handshake);
        }
        let mut buffer = vec![0u8; payload.len() + MAX_HANDSHAKE_OVERHEAD];
        let written = self
            .state
            .write_message(payload, &mut buffer)
            .map_err(handshake_error)?;
        buffer.truncate(written);
        Ok(buffer)
    }

    /// Reads the next handshake message and returns its payload.
    ///
    /// Every cryptographic rejection collapses to [`NoiseError::Handshake`]; the
    /// caller is never told whether a tag mismatched or the transcript diverged.
    pub fn read_message(&mut self, message: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if message.len() > MAX_MESSAGE_SIZE {
            return Err(NoiseError::Handshake);
        }
        let mut buffer = vec![0u8; message.len()];
        let written = self
            .state
            .read_message(message, &mut buffer)
            .map_err(handshake_error)?;
        buffer.truncate(written);

        // `snow` accepts whatever point arrives; bitchat rejects degenerate ones
        // at exactly this spot (`NoiseHandshakeState.validatePublicKey` on the
        // `s` token), so we do the same as soon as the key is revealed.
        if let Some(remote) = self.remote_static() {
            validate_public_key(&remote)?;
        }

        Ok(buffer)
    }

    /// True once every pattern message has been sent or received.
    pub fn is_complete(&self) -> bool {
        self.state.is_handshake_finished()
    }

    /// The peer's static public key, once the pattern has revealed it. In `XX`
    /// that is after message two for the initiator, after message three for the
    /// responder.
    pub fn remote_static(&self) -> Option<[u8; 32]> {
        self.state
            .get_remote_static()
            .and_then(|key| <[u8; 32]>::try_from(key).ok())
    }

    /// The final handshake hash, suitable as a channel binding.
    ///
    /// `None` until the handshake completes: a mid-handshake hash is not a
    /// binding value and returning one invites it to be used as though it were.
    pub fn handshake_hash(&self) -> Option<[u8; 32]> {
        if !self.is_complete() {
            return None;
        }
        <[u8; 32]>::try_from(self.state.get_handshake_hash()).ok()
    }

    /// Splits the completed handshake into a transport session.
    pub fn into_session(self) -> Result<NoiseSession, NoiseError> {
        if !self.is_complete() {
            return Err(NoiseError::WrongState("handshake not finished"));
        }
        let remote_static = self
            .remote_static()
            .ok_or(NoiseError::MissingRemoteStatic)?;
        let transport = self
            .state
            .into_transport_mode()
            .map_err(|_| NoiseError::WrongState("handshake not finished"))?;
        Ok(NoiseSession {
            transport,
            remote_static,
            messages_sent: 0,
        })
    }
}

#[cfg(any(test, feature = "vectors"))]
impl NoiseHandshake {
    /// Starts a Noise `XX` handshake with a pinned ephemeral key.
    ///
    /// Test scaffolding only. The specification vectors fix both ephemerals so
    /// the ciphertexts are reproducible; nothing outside a test may call this,
    /// because a predictable ephemeral destroys the handshake's secrecy.
    pub fn new_with_fixed_ephemeral(
        role: HandshakeRole,
        identity: &MeshIdentity,
        ephemeral_secret: &[u8; 32],
    ) -> Result<Self, NoiseError> {
        Self::build(
            NOISE_XX_PROTOCOL,
            role,
            Some(identity.static_secret()),
            None,
            Some(ephemeral_secret),
            &[],
        )
    }

    /// Fully parameterised constructor for driving the official vector file,
    /// whose entries carry their own protocol name, prologue and fixed keys.
    ///
    /// Test scaffolding only, for the same reason as
    /// [`new_with_fixed_ephemeral`](Self::new_with_fixed_ephemeral). Patterns
    /// that never establish a remote static key cannot be converted with
    /// [`into_session`](Self::into_session).
    pub fn for_vectors(
        protocol: &str,
        role: HandshakeRole,
        local_static: Option<&[u8; 32]>,
        remote_static: Option<&[u8; 32]>,
        fixed_ephemeral: Option<&[u8; 32]>,
        prologue: &[u8],
    ) -> Result<Self, NoiseError> {
        Self::build(
            protocol,
            role,
            local_static,
            remote_static,
            fixed_ephemeral,
            prologue,
        )
    }
}

impl core::fmt::Debug for NoiseHandshake {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NoiseHandshake")
            .field("initiator", &self.state.is_initiator())
            .field("complete", &self.is_complete())
            .finish_non_exhaustive()
    }
}

/// A live Noise transport session with a peer.
///
/// Holds the split cipher states plus the sent-message counter that drives
/// bitchat's rekey policy. Sending and receiving use independent nonce
/// sequences, so only the sending side is counted — that is the one whose nonce
/// space we can exhaust.
pub struct NoiseSession {
    transport: TransportState,
    remote_static: [u8; 32],
    messages_sent: u64,
}

impl NoiseSession {
    /// Encrypts one transport message.
    ///
    /// Fails with [`NoiseError::SessionExhausted`] once the nonce budget is
    /// spent, rather than reusing a nonce — a nonce repeat under ChaCha20 is a
    /// total break of both messages involved.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if self.messages_sent >= MAX_MESSAGES_PER_SESSION {
            return Err(NoiseError::SessionExhausted);
        }
        if plaintext.len() > MAX_MESSAGE_SIZE - TAG_LEN {
            return Err(NoiseError::Encrypt);
        }
        let mut buffer = vec![0u8; plaintext.len() + TAG_LEN];
        let written = self
            .transport
            .write_message(plaintext, &mut buffer)
            .map_err(|err| match err {
                snow::Error::State(StateProblem::Exhausted) => NoiseError::SessionExhausted,
                _ => NoiseError::Encrypt,
            })?;
        buffer.truncate(written);
        self.messages_sent = self.messages_sent.saturating_add(1);
        Ok(buffer)
    }

    /// Decrypts one transport message.
    ///
    /// Every failure is [`NoiseError::Decrypt`]. A caller — and therefore an
    /// attacker probing us — learns only that it did not work, never whether the
    /// tag mismatched, the key was wrong, or the length was implausible.
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if ciphertext.len() < TAG_LEN || ciphertext.len() > MAX_MESSAGE_SIZE {
            return Err(NoiseError::Decrypt);
        }
        let mut buffer = vec![0u8; ciphertext.len()];
        let written = self
            .transport
            .read_message(ciphertext, &mut buffer)
            .map_err(|_| NoiseError::Decrypt)?;
        buffer.truncate(written);
        Ok(buffer)
    }

    /// The peer's authenticated static public key.
    pub fn remote_static(&self) -> [u8; 32] {
        self.remote_static
    }

    /// The peer's fingerprint — SHA-256 of [`remote_static`](Self::remote_static),
    /// lowercase hex. This is what a user verifies out of band.
    pub fn remote_fingerprint(&self) -> String {
        fingerprint_of(&self.remote_static)
    }

    /// Whether the session has spent 90% of its message budget and should be
    /// renegotiated. Advisory: nothing here acts on it.
    pub fn needs_rekey(&self) -> bool {
        self.messages_sent >= REKEY_MESSAGE_THRESHOLD
    }

    /// Messages encrypted so far on this session.
    pub fn messages_sent(&self) -> u64 {
        self.messages_sent
    }

    /// Mirrors `SecureNoiseSession.setMessageCountForTesting`. Reaching the real
    /// threshold takes 900 million encryptions, so the policy is untestable
    /// without it.
    #[cfg(test)]
    pub(crate) fn set_messages_sent_for_testing(&mut self, count: u64) {
        self.messages_sent = count;
    }
}

impl core::fmt::Debug for NoiseSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NoiseSession")
            .field("remote_fingerprint", &self.remote_fingerprint())
            .field("messages_sent", &self.messages_sent)
            .finish_non_exhaustive()
    }
}

/// Setup failures may name the stage — none of this is secret, it is all
/// determined by our own configuration.
fn setup_error(err: snow::Error) -> NoiseError {
    NoiseError::Setup(format!("{err:?}"))
}

/// Handshake failures never name a cause. Only sequencing mistakes, which are
/// our own bug and carry no secret, are distinguished.
fn handshake_error(err: snow::Error) -> NoiseError {
    match err {
        snow::Error::State(StateProblem::NotTurnToWrite) => {
            NoiseError::WrongState("not our turn to write")
        }
        snow::Error::State(StateProblem::NotTurnToRead) => {
            NoiseError::WrongState("not our turn to read")
        }
        snow::Error::State(StateProblem::HandshakeAlreadyFinished) => {
            NoiseError::WrongState("handshake already finished")
        }
        _ => NoiseError::Handshake,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alice() -> MeshIdentity {
        MeshIdentity::from_keys([1u8; 32], [11u8; 32])
    }

    fn bob() -> MeshIdentity {
        MeshIdentity::from_keys([2u8; 32], [22u8; 32])
    }

    /// Runs the three `XX` messages and returns both sides' sessions.
    fn handshake(a: &MeshIdentity, b: &MeshIdentity) -> (NoiseSession, NoiseSession) {
        let mut initiator = NoiseHandshake::new(HandshakeRole::Initiator, a).unwrap();
        let mut responder = NoiseHandshake::new(HandshakeRole::Responder, b).unwrap();

        let msg1 = initiator.write_message(b"hello").unwrap();
        assert_eq!(responder.read_message(&msg1).unwrap().as_slice(), b"hello");

        let msg2 = responder.write_message(b"there").unwrap();
        assert_eq!(initiator.read_message(&msg2).unwrap().as_slice(), b"there");

        let msg3 = initiator.write_message(b"done").unwrap();
        assert_eq!(responder.read_message(&msg3).unwrap().as_slice(), b"done");

        assert!(initiator.is_complete());
        assert!(responder.is_complete());

        (
            initiator.into_session().unwrap(),
            responder.into_session().unwrap(),
        )
    }

    #[test]
    fn xx_handshake_completes_and_carries_traffic_both_ways() {
        let (a, b) = (alice(), bob());
        let (mut initiator, mut responder) = handshake(&a, &b);

        let to_bob = initiator.encrypt(b"ping from alice").unwrap();
        assert_eq!(
            responder.decrypt(&to_bob).unwrap().as_slice(),
            b"ping from alice"
        );

        let to_alice = responder.encrypt(b"pong from bob").unwrap();
        assert_eq!(
            initiator.decrypt(&to_alice).unwrap().as_slice(),
            b"pong from bob"
        );

        // Sequences of messages in one direction must keep working: the nonce
        // advances independently on each side.
        for i in 0..8u8 {
            let payload = vec![i; 64];
            let sealed = initiator.encrypt(&payload).unwrap();
            assert_eq!(responder.decrypt(&sealed).unwrap(), payload);
        }
        assert_eq!(initiator.messages_sent(), 9);
        assert_eq!(responder.messages_sent(), 1);
    }

    #[test]
    fn empty_payloads_round_trip() {
        let (a, b) = (alice(), bob());
        let (mut initiator, mut responder) = handshake(&a, &b);
        let sealed = initiator.encrypt(&[]).unwrap();
        assert_eq!(sealed.len(), TAG_LEN);
        assert!(responder.decrypt(&sealed).unwrap().is_empty());
    }

    #[test]
    fn each_side_learns_the_others_static_key_and_fingerprint() {
        let (a, b) = (alice(), bob());
        let (initiator, responder) = handshake(&a, &b);

        assert_eq!(initiator.remote_static(), b.static_public());
        assert_eq!(responder.remote_static(), a.static_public());
        assert_eq!(initiator.remote_fingerprint(), b.fingerprint());
        assert_eq!(responder.remote_fingerprint(), a.fingerprint());
    }

    #[test]
    fn both_sides_derive_the_same_handshake_hash() {
        let (a, b) = (alice(), bob());
        let mut initiator = NoiseHandshake::new(HandshakeRole::Initiator, &a).unwrap();
        let mut responder = NoiseHandshake::new(HandshakeRole::Responder, &b).unwrap();

        assert!(initiator.handshake_hash().is_none());

        let msg1 = initiator.write_message(&[]).unwrap();
        responder.read_message(&msg1).unwrap();
        let msg2 = responder.write_message(&[]).unwrap();
        initiator.read_message(&msg2).unwrap();
        let msg3 = initiator.write_message(&[]).unwrap();
        responder.read_message(&msg3).unwrap();

        let a_hash = initiator.handshake_hash().unwrap();
        let b_hash = responder.handshake_hash().unwrap();
        assert_eq!(a_hash, b_hash);
        assert_ne!(a_hash, [0u8; 32]);
    }

    #[test]
    fn remote_static_is_unknown_until_the_pattern_reveals_it() {
        let (a, b) = (alice(), bob());
        let mut initiator = NoiseHandshake::new(HandshakeRole::Initiator, &a).unwrap();
        let mut responder = NoiseHandshake::new(HandshakeRole::Responder, &b).unwrap();

        assert!(initiator.remote_static().is_none());
        assert!(responder.remote_static().is_none());

        let msg1 = initiator.write_message(&[]).unwrap();
        responder.read_message(&msg1).unwrap();
        // XX message one carries no static key.
        assert!(responder.remote_static().is_none());

        let msg2 = responder.write_message(&[]).unwrap();
        initiator.read_message(&msg2).unwrap();
        assert_eq!(initiator.remote_static(), Some(b.static_public()));

        let msg3 = initiator.write_message(&[]).unwrap();
        responder.read_message(&msg3).unwrap();
        assert_eq!(responder.remote_static(), Some(a.static_public()));
    }

    #[test]
    fn message_one_is_ephemeral_plus_cleartext_payload() {
        let a = alice();
        let mut initiator = NoiseHandshake::new(HandshakeRole::Initiator, &a).unwrap();
        let msg1 = initiator.write_message(b"payload").unwrap();
        // e (32) + payload, unencrypted: no key exists yet in XX.
        assert_eq!(msg1.len(), 32 + b"payload".len());
        assert!(msg1.ends_with(b"payload"));
    }

    #[test]
    fn a_tampered_transport_message_is_rejected() {
        let (a, b) = (alice(), bob());
        let (mut initiator, mut responder) = handshake(&a, &b);

        let mut sealed = initiator.encrypt(b"authentic").unwrap();
        sealed[0] ^= 0x01;
        assert_eq!(responder.decrypt(&sealed), Err(NoiseError::Decrypt));

        let mut sealed = initiator.encrypt(b"authentic").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x80;
        assert_eq!(responder.decrypt(&sealed), Err(NoiseError::Decrypt));
    }

    #[test]
    fn a_truncated_transport_message_is_rejected_without_panicking() {
        let (a, b) = (alice(), bob());
        let (mut initiator, mut responder) = handshake(&a, &b);
        let sealed = initiator.encrypt(b"authentic").unwrap();

        for len in 0..sealed.len() {
            assert_eq!(responder.decrypt(&sealed[..len]), Err(NoiseError::Decrypt));
        }
    }

    #[test]
    fn a_message_from_a_stranger_is_rejected() {
        let (a, b) = (alice(), bob());
        let (_, mut responder) = handshake(&a, &b);

        let mallory = MeshIdentity::from_keys([3u8; 32], [33u8; 32]);
        let (mut stranger, _) = handshake(&mallory, &bob());
        let sealed = stranger.encrypt(b"not from alice").unwrap();

        assert_eq!(responder.decrypt(&sealed), Err(NoiseError::Decrypt));
    }

    #[test]
    fn writing_out_of_turn_is_a_state_error_not_a_handshake_failure() {
        let a = alice();
        let mut initiator = NoiseHandshake::new(HandshakeRole::Initiator, &a).unwrap();
        initiator.write_message(&[]).unwrap();

        // It is the responder's turn now.
        assert_eq!(
            initiator.write_message(&[]),
            Err(NoiseError::WrongState("not our turn to write"))
        );
    }

    #[test]
    fn reading_out_of_turn_is_a_state_error() {
        let (a, b) = (alice(), bob());
        let mut initiator = NoiseHandshake::new(HandshakeRole::Initiator, &a).unwrap();
        let mut responder = NoiseHandshake::new(HandshakeRole::Responder, &b).unwrap();
        let msg1 = initiator.write_message(&[]).unwrap();
        responder.read_message(&msg1).unwrap();

        assert_eq!(
            responder.read_message(&msg1),
            Err(NoiseError::WrongState("not our turn to read"))
        );
    }

    #[test]
    fn garbage_in_place_of_a_handshake_message_fails_opaquely() {
        let b = bob();
        let mut responder = NoiseHandshake::new(HandshakeRole::Responder, &b).unwrap();
        assert_eq!(responder.read_message(&[]), Err(NoiseError::Handshake));
        assert_eq!(
            responder.read_message(&[0u8; 16]),
            Err(NoiseError::Handshake)
        );

        // A well-formed message one, then a garbage message three: the tag on
        // the encrypted static key fails, and the caller is told nothing more.
        let mut responder = NoiseHandshake::new(HandshakeRole::Responder, &b).unwrap();
        let a = alice();
        let mut initiator = NoiseHandshake::new(HandshakeRole::Initiator, &a).unwrap();
        let msg1 = initiator.write_message(&[]).unwrap();
        responder.read_message(&msg1).unwrap();
        responder.write_message(&[]).unwrap();
        assert_eq!(
            responder.read_message(&[7u8; 64]),
            Err(NoiseError::Handshake)
        );
    }

    #[test]
    fn an_oversized_handshake_message_is_refused_before_allocating() {
        let a = alice();
        let mut initiator = NoiseHandshake::new(HandshakeRole::Initiator, &a).unwrap();
        let huge = vec![0u8; MAX_MESSAGE_SIZE + 1];
        assert_eq!(initiator.write_message(&huge), Err(NoiseError::Handshake));
        assert_eq!(initiator.read_message(&huge), Err(NoiseError::Handshake));
    }

    #[test]
    fn a_handshake_cannot_become_a_session_before_it_finishes() {
        let a = alice();
        let mut initiator = NoiseHandshake::new(HandshakeRole::Initiator, &a).unwrap();
        assert!(matches!(
            NoiseHandshake::new(HandshakeRole::Initiator, &a)
                .unwrap()
                .into_session(),
            Err(NoiseError::WrongState(_))
        ));
        initiator.write_message(&[]).unwrap();
        assert!(matches!(
            initiator.into_session(),
            Err(NoiseError::WrongState(_))
        ));
    }

    #[test]
    fn rekey_is_advised_at_ninety_percent_of_the_budget() {
        assert_eq!(REKEY_MESSAGE_THRESHOLD, 900_000_000);

        let (a, b) = (alice(), bob());
        let (mut initiator, _) = handshake(&a, &b);
        assert!(!initiator.needs_rekey());

        initiator.set_messages_sent_for_testing(REKEY_MESSAGE_THRESHOLD - 1);
        assert!(!initiator.needs_rekey());

        initiator.set_messages_sent_for_testing(REKEY_MESSAGE_THRESHOLD);
        assert!(initiator.needs_rekey());
        assert_eq!(initiator.messages_sent(), REKEY_MESSAGE_THRESHOLD);

        // Advisory only: the session must still work past the threshold.
        assert!(initiator.encrypt(b"still fine").is_ok());
    }

    #[test]
    fn the_session_refuses_to_spend_more_than_its_nonce_budget() {
        let (a, b) = (alice(), bob());
        let (mut initiator, _) = handshake(&a, &b);

        initiator.set_messages_sent_for_testing(MAX_MESSAGES_PER_SESSION - 1);
        assert!(initiator.encrypt(b"last one").is_ok());
        assert_eq!(
            initiator.encrypt(b"one too many"),
            Err(NoiseError::SessionExhausted)
        );
    }

    #[test]
    fn an_oversized_plaintext_is_refused() {
        let (a, b) = (alice(), bob());
        let (mut initiator, _) = handshake(&a, &b);
        let huge = vec![0u8; MAX_MESSAGE_SIZE];
        assert_eq!(initiator.encrypt(&huge), Err(NoiseError::Encrypt));
        assert_eq!(initiator.messages_sent(), 0);
    }

    #[test]
    fn pinning_the_ephemeral_makes_the_handshake_reproducible() {
        let a = alice();
        let ephemeral = [42u8; 32];

        let mut first =
            NoiseHandshake::new_with_fixed_ephemeral(HandshakeRole::Initiator, &a, &ephemeral)
                .unwrap();
        let mut second =
            NoiseHandshake::new_with_fixed_ephemeral(HandshakeRole::Initiator, &a, &ephemeral)
                .unwrap();

        assert_eq!(
            first.write_message(b"same").unwrap(),
            second.write_message(b"same").unwrap()
        );

        // And an unpinned handshake must not be reproducible.
        let mut random_a = NoiseHandshake::new(HandshakeRole::Initiator, &a).unwrap();
        let mut random_b = NoiseHandshake::new(HandshakeRole::Initiator, &a).unwrap();
        assert_ne!(
            random_a.write_message(b"same").unwrap(),
            random_b.write_message(b"same").unwrap()
        );
    }

    #[test]
    fn the_vector_constructor_accepts_a_protocol_name_and_prologue() {
        let a = alice();
        let b = bob();

        let mut initiator = NoiseHandshake::for_vectors(
            NOISE_XX_PROTOCOL,
            HandshakeRole::Initiator,
            Some(a.static_secret()),
            None,
            Some(&[9u8; 32]),
            b"John Galt",
        )
        .unwrap();
        let mut responder = NoiseHandshake::for_vectors(
            NOISE_XX_PROTOCOL,
            HandshakeRole::Responder,
            Some(b.static_secret()),
            None,
            Some(&[8u8; 32]),
            b"John Galt",
        )
        .unwrap();

        let msg1 = initiator.write_message(b"Ludwig").unwrap();
        assert_eq!(responder.read_message(&msg1).unwrap().as_slice(), b"Ludwig");
        let msg2 = responder.write_message(b"von").unwrap();
        assert_eq!(initiator.read_message(&msg2).unwrap().as_slice(), b"von");
        let msg3 = initiator.write_message(b"Mises").unwrap();
        assert_eq!(responder.read_message(&msg3).unwrap().as_slice(), b"Mises");

        assert_eq!(initiator.handshake_hash(), responder.handshake_hash());
    }

    #[test]
    fn a_mismatched_prologue_breaks_the_handshake() {
        let a = alice();
        let b = bob();
        let mut initiator = NoiseHandshake::for_vectors(
            NOISE_XX_PROTOCOL,
            HandshakeRole::Initiator,
            Some(a.static_secret()),
            None,
            None,
            b"one prologue",
        )
        .unwrap();
        let mut responder = NoiseHandshake::for_vectors(
            NOISE_XX_PROTOCOL,
            HandshakeRole::Responder,
            Some(b.static_secret()),
            None,
            None,
            b"another prologue",
        )
        .unwrap();

        // Message one has no key yet, so it reads fine; the divergence shows up
        // the moment authentication starts, on message two.
        let msg1 = initiator.write_message(&[]).unwrap();
        responder.read_message(&msg1).unwrap();
        let msg2 = responder.write_message(&[]).unwrap();
        assert_eq!(initiator.read_message(&msg2), Err(NoiseError::Handshake));
    }

    #[test]
    fn an_unknown_protocol_name_is_a_setup_error() {
        let a = alice();
        assert!(matches!(
            NoiseHandshake::for_vectors(
                "Noise_XX_25519_ChaChaPoly_MD5",
                HandshakeRole::Initiator,
                Some(a.static_secret()),
                None,
                None,
                &[],
            ),
            Err(NoiseError::Setup(_))
        ));
    }

    #[test]
    fn debug_output_carries_no_key_material() {
        let (a, b) = (alice(), bob());
        let handshake_state = NoiseHandshake::new(HandshakeRole::Initiator, &a).unwrap();
        assert!(format!("{handshake_state:?}").contains("NoiseHandshake"));

        let (initiator, _) = handshake(&a, &b);
        let rendered = format!("{initiator:?}");
        assert!(rendered.contains(&b.fingerprint()));
        assert!(!rendered.contains("0101010101"));
    }
}
