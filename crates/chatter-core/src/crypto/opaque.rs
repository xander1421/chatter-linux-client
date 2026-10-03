//! OPAQUE (RFC 9807) client: password authentication in which the server never
//! receives the password or anything password-equivalent.
//!
//! Ciphersuite: OPRF ristretto255-SHA512, OPAQUE-3DH over ristretto255 with
//! SHA-512, and an Argon2id key-stretching function ([`ChatterKsf`]) that runs
//! only here, on the client. Application context [`OPAQUE_CONTEXT`]. Client and
//! server identities are left at their RFC defaults (the two public keys).
//!
//! The server half is `internal/services/auth/opaque` (Go,
//! `github.com/bytemare/opaque` v0.18.0). Interop is pinned from both ends by
//! one transcript, recorded with this module's suite against that server over a
//! live pipe: [`tests::cross_impl_transcript_matches_go_server`] here and
//! `TestCrossImplementationTranscriptFromRustClient` there. The RFC 9807
//! Appendix C.1.1 vector pins this library to the spec
//! ([`tests::rfc9807_vector_c11_client`]).
//!
//! # What a server breach costs
//!
//! The server stores, per account, a RegistrationRecord and a credential
//! identifier. To test a password guess against a stolen record an attacker
//! needs the OPRF key (derived from the server's secret seed) AND one Argon2id
//! at [`ksf_params`] per guess. Without the seed, offline guessing is
//! impossible; with it, every guess still costs the stretch.
//!
//! Every function that runs the KSF blocks for one Argon2id (~0.1 s desktop,
//! longer on a phone): call the `_async` variants from async code.

use opaque_ke::ciphersuite::CipherSuite;
use opaque_ke::errors::{InternalError, ProtocolError};
use opaque_ke::generic_array::{ArrayLength, GenericArray};
use opaque_ke::ksf::Ksf;
use opaque_ke::rand::{CryptoRng, RngCore};
use opaque_ke::{
    ClientLogin, ClientLoginFinishParameters, ClientRegistration,
    ClientRegistrationFinishParameters, CredentialResponse, Identifiers, RegistrationResponse,
    Ristretto255, TripleDh,
};
use zeroize::Zeroizing;

/// RFC 9807 §6.1 application context, authenticated in the 3DH transcript.
/// Must equal the server's `opaque.Context`.
pub const OPAQUE_CONTEXT: &[u8] = b"chatter-opaque-v1";

/// Argon2id work factor for the OPAQUE key-stretching function.
///
/// `m = 64 MiB`, `t = 4`, `p = 4`, salt 16 zero bytes, output 64 bytes (the
/// OPRF output length). RFC 9807 §4.3 suggests Argon2id with a zero salt; the
/// input is the OPRF output, already unique per account and password, so a
/// salt would add nothing. The memory/time are the project's existing login
/// stretch (measured ~108 ms on a Ryzen 7 5800X, release, lanes sequential;
/// roughly 0.5-0.8 s estimated on a mid-range phone, not measured), below
/// RFC 9807's 2 GiB suggestion, which no phone this ships on can run.
///
/// Changing any of these numbers makes every stored record unopenable: a clean
/// break that locks out every account.
pub fn ksf_params() -> argon2::Params {
    argon2::Params::new(65536, 4, 4, None).expect("valid Argon2 parameters")
}

/// The Argon2id KSF. Its `Default` IS the production configuration, so no
/// call site can run OPAQUE with opaque-ke's own (weaker) Argon2 default.
#[derive(Default)]
pub struct ChatterKsf;

impl Ksf for ChatterKsf {
    fn hash<L: ArrayLength<u8>>(
        &self,
        input: GenericArray<u8, L>,
    ) -> Result<GenericArray<u8, L>, InternalError> {
        let mut out = GenericArray::<u8, L>::default();
        argon2::Argon2::new(
            argon2::Algorithm::Argon2id,
            argon2::Version::V0x13,
            ksf_params(),
        )
        .hash_password_into(&input, &[0u8; 16], &mut out)
        .map_err(|_| InternalError::KsfError)?;
        Ok(out)
    }
}

/// The production ciphersuite.
pub struct ChatterSuite;

impl CipherSuite for ChatterSuite {
    type OprfCs = Ristretto255;
    type KeyExchange = TripleDh<Ristretto255, sha2::Sha512>;
    type Ksf = ChatterKsf;
}

/// OPAQUE client failures.
#[derive(Debug, thiserror::Error)]
pub enum OpaqueError {
    /// The server's KE2 did not open: the password is wrong, the account does
    /// not exist (fake record), or the server is not the one the record was
    /// registered with. Deliberately one case — they are indistinguishable.
    #[error("invalid login")]
    InvalidLogin,
    /// A malformed server message or a local protocol failure.
    #[error("OPAQUE protocol error: {0}")]
    Protocol(String),
}

fn protocol(e: ProtocolError) -> OpaqueError {
    match e {
        ProtocolError::InvalidLoginError => OpaqueError::InvalidLogin,
        other => OpaqueError::Protocol(other.to_string()),
    }
}

/// Client registration in progress: send `request`, keep `state`.
pub struct RegistrationStart {
    pub(crate) state: ClientRegistration<ChatterSuite>,
    /// RegistrationRequest (32 bytes).
    pub request: Vec<u8>,
}

/// Client login in progress: send `ke1`, keep `state`.
pub struct LoginStart {
    pub(crate) state: ClientLogin<ChatterSuite>,
    /// KE1 (96 bytes).
    pub ke1: Vec<u8>,
}

/// A completed client login.
pub struct LoginFinish {
    /// KE3 (64 bytes): the proof the server checks.
    pub ke3: Vec<u8>,
    /// RFC 9807 session_key, shared with the server. Unused by chatter today:
    /// the session that follows is authenticated by the device token.
    pub session_key: Zeroizing<Vec<u8>>,
    /// RFC 9807 export_key: a client-only secret, stable across logins for one
    /// registration.
    pub export_key: Zeroizing<Vec<u8>>,
}

fn registration_start_with<R: RngCore + CryptoRng>(
    rng: &mut R,
    password: &[u8],
) -> Result<RegistrationStart, OpaqueError> {
    let r = ClientRegistration::<ChatterSuite>::start(rng, password).map_err(protocol)?;
    Ok(RegistrationStart {
        request: r.message.serialize().to_vec(),
        state: r.state,
    })
}

fn registration_finish_with<R: RngCore + CryptoRng>(
    rng: &mut R,
    start: RegistrationStart,
    password: &[u8],
    response: &[u8],
) -> Result<Vec<u8>, OpaqueError> {
    let response = RegistrationResponse::<ChatterSuite>::deserialize(response).map_err(protocol)?;
    let r = start
        .state
        .finish(
            rng,
            password,
            response,
            ClientRegistrationFinishParameters::default(),
        )
        .map_err(protocol)?;
    Ok(r.message.serialize().to_vec())
}

fn login_start_with<R: RngCore + CryptoRng>(
    rng: &mut R,
    password: &[u8],
) -> Result<LoginStart, OpaqueError> {
    let r = ClientLogin::<ChatterSuite>::start(rng, password).map_err(protocol)?;
    Ok(LoginStart {
        ke1: r.message.serialize().to_vec(),
        state: r.state,
    })
}

fn login_finish_with<R: RngCore + CryptoRng>(
    rng: &mut R,
    start: LoginStart,
    password: &[u8],
    ke2: &[u8],
) -> Result<LoginFinish, OpaqueError> {
    let ke2 = CredentialResponse::<ChatterSuite>::deserialize(ke2).map_err(protocol)?;
    let r = start
        .state
        .finish(
            rng,
            password,
            ke2,
            ClientLoginFinishParameters::new(Some(OPAQUE_CONTEXT), Identifiers::default(), None),
        )
        .map_err(protocol)?;
    Ok(LoginFinish {
        ke3: r.message.serialize().to_vec(),
        session_key: Zeroizing::new(r.session_key.to_vec()),
        export_key: Zeroizing::new(r.export_key.to_vec()),
    })
}

/// Begin a registration (sign-up, password change, recovery). Cheap: blinds
/// the password, no KSF.
pub fn registration_start(password: &str) -> Result<RegistrationStart, OpaqueError> {
    registration_start_with(&mut rand_core_06::OsRng, password.as_bytes())
}

/// Finish a registration against the server's RegistrationResponse, returning
/// the RegistrationRecord (192 bytes) to upload. Runs the KSF (blocking).
pub fn registration_finish(
    start: RegistrationStart,
    password: &str,
    response: &[u8],
) -> Result<Vec<u8>, OpaqueError> {
    registration_finish_with(
        &mut rand_core_06::OsRng,
        start,
        password.as_bytes(),
        response,
    )
}

/// Begin a login or password proof. Cheap: no KSF.
pub fn login_start(password: &str) -> Result<LoginStart, OpaqueError> {
    login_start_with(&mut rand_core_06::OsRng, password.as_bytes())
}

/// Answer the server's KE2. Runs the KSF (blocking). A wrong password (or an
/// unknown account) is [`OpaqueError::InvalidLogin`] — there is then nothing
/// to send.
pub fn login_finish(
    start: LoginStart,
    password: &str,
    ke2: &[u8],
) -> Result<LoginFinish, OpaqueError> {
    login_finish_with(&mut rand_core_06::OsRng, start, password.as_bytes(), ke2)
}

/// [`registration_finish`] on the blocking pool, so the Argon2id does not
/// stall the async worker (see `svr::derive_svr_secret_async`).
pub async fn registration_finish_async(
    start: RegistrationStart,
    password: &str,
    response: Vec<u8>,
) -> Result<Vec<u8>, OpaqueError> {
    let password = Zeroizing::new(password.to_owned());
    tokio::task::spawn_blocking(move || registration_finish(start, &password, &response))
        .await
        .map_err(|e| OpaqueError::Protocol(format!("OPAQUE task failed: {e}")))?
}

/// [`login_finish`] on the blocking pool.
pub async fn login_finish_async(
    start: LoginStart,
    password: &str,
    ke2: Vec<u8>,
) -> Result<LoginFinish, OpaqueError> {
    let password = Zeroizing::new(password.to_owned());
    tokio::task::spawn_blocking(move || login_finish(start, &password, &ke2))
        .await
        .map_err(|e| OpaqueError::Protocol(format!("OPAQUE task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use opaque_ke::ksf::Identity;

    /// Hands out pre-queued byte chunks in order; a request of another size is
    /// a test bug. opaque-ke draws, per call: blind (64 bytes, reduced wide),
    /// then envelope nonce (registration finish) or key-share seed + nonce
    /// (login start).
    struct ScriptRng(Vec<Vec<u8>>);
    impl RngCore for ScriptRng {
        fn next_u32(&mut self) -> u32 {
            unreachable!()
        }
        fn next_u64(&mut self) -> u64 {
            unreachable!()
        }
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            let chunk = self.0.remove(0);
            assert_eq!(chunk.len(), dest.len(), "scripted rng chunk size");
            dest.copy_from_slice(&chunk);
        }
        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), opaque_ke::rand::Error> {
            self.fill_bytes(dest);
            Ok(())
        }
    }
    impl CryptoRng for ScriptRng {}

    fn h(s: &str) -> Vec<u8> {
        hex::decode(s).unwrap()
    }
    /// A 32-byte scalar as the 64 bytes `Scalar::random` reduces.
    fn wide(s: &str) -> Vec<u8> {
        let mut v = h(s);
        v.resize(64, 0);
        v
    }

    // RFC 9807 Appendix C.1.1 inputs (shared by the golden transcript below).
    const BLIND_REG: &str = "76cfbfe758db884bebb33582331ba9f159720ca8784a2a070a265d9c2d6abe01";
    const ENVELOPE_NONCE: &str = "ac13171b2f17bc2c74997f0fce1e1f35bec6b91fe2e12dbd323d23ba7a38dfec";
    const BLIND_LOGIN: &str = "6ecc102d2e7a7cf49617aad7bbe188556792d4acd60a1a8a8d2b65d4b0790308";
    const CLIENT_KEYSHARE_SEED: &str =
        "82850a697b42a505f5b68fcdafce8c31f0af2b581f063cf1091933541936304b";
    const CLIENT_NONCE: &str = "da7e07376d6d6f034cfa9bb537d11b8c6b4238c334333d1f0aebb380cae6a6cc";

    fn scripted() -> ScriptRng {
        ScriptRng(vec![
            wide(BLIND_REG),
            h(ENVELOPE_NONCE),
            wide(BLIND_LOGIN),
            h(CLIENT_KEYSHARE_SEED),
            h(CLIENT_NONCE),
        ])
    }

    struct VectorSuite;
    impl CipherSuite for VectorSuite {
        type OprfCs = Ristretto255;
        type KeyExchange = TripleDh<Ristretto255, sha2::Sha512>;
        type Ksf = Identity;
    }

    /// RFC 9807 Appendix C.1.1 (OPAQUE-3DH Real Test Vector 1), client side:
    /// every client output byte-for-byte, given the server's messages from the
    /// RFC. KSF Identity and context "OPAQUE-POC" as the vector specifies.
    #[test]
    fn rfc9807_vector_c11_client() {
        let mut rng = scripted();
        let password = h("436f7272656374486f72736542617474657279537461706c65");

        let reg = ClientRegistration::<VectorSuite>::start(&mut rng, &password).unwrap();
        assert_eq!(
            hex::encode(reg.message.serialize()),
            "5059ff249eb1551b7ce4991f3336205bde44a105a032e747d21bf382e75f7a71"
        );
        let resp = RegistrationResponse::<VectorSuite>::deserialize(&h(
            "7408a268083e03abc7097fc05b587834539065e86fb0c7b6342fcf5e01e5b019b2fe7af9f48cc502d016729d2fe25cdd433f2c4bc904660b2a382c9b79df1a78",
        ))
        .unwrap();
        let fin = reg
            .state
            .finish(
                &mut rng,
                &password,
                resp,
                ClientRegistrationFinishParameters::default(),
            )
            .unwrap();
        assert_eq!(
            hex::encode(fin.message.serialize()),
            "76a845464c68a5d2f7e442436bb1424953b17d3e2e289ccbaccafb57ac5c36751ac5844383c7708077dea41cbefe2fa15724f449e535dd7dd562e66f5ecfb95864eadddec9db5874959905117dad40a4524111849799281fefe3c51fa82785c5ac13171b2f17bc2c74997f0fce1e1f35bec6b91fe2e12dbd323d23ba7a38dfec634b0f5b96109c198a8027da51854c35bee90d1e1c781806d07d49b76de6a28b8d9e9b6c93b9f8b64d16dddd9c5bfb5fea48ee8fd2f75012a8b308605cdd8ba5"
        );
        assert_eq!(
            hex::encode(&fin.export_key),
            "1ef15b4fa99e8a852412450ab78713aad30d21fa6966c9b8c9fb3262a970dc62950d4dd4ed62598229b1b72794fc0335199d9f7fcc6eaedde92cc04870e63f16"
        );

        let login = ClientLogin::<VectorSuite>::start(&mut rng, &password).unwrap();
        assert_eq!(
            hex::encode(login.message.serialize()),
            "c4dedb0ba6ed5d965d6f250fbe554cd45cba5dfcce3ce836e4aee778aa3cd44dda7e07376d6d6f034cfa9bb537d11b8c6b4238c334333d1f0aebb380cae6a6cc6e29bee50701498605b2c085d7b241ca15ba5c32027dd21ba420b94ce60da326"
        );
        let ke2 = CredentialResponse::<VectorSuite>::deserialize(&h(
            "7e308140890bcde30cbcea28b01ea1ecfbd077cff62c4def8efa075aabcbb47138fe59af0df2c79f57b8780278f5ae47355fe1f817119041951c80f612fdfc6dd6ec60bcdb26dc455ddf3e718f1020490c192d70dfc7e403981179d8073d1146a4f9aa1ced4e4cd984c657eb3b54ced3848326f70331953d91b02535af44d9fedc80188ca46743c52786e0382f95ad85c08f6afcd1ccfbff95e2bdeb015b166c6b20b92f832cc6df01e0b86a7efd92c1c804ff865781fa93f2f20b446c8371b671cd9960ecef2fe0d0f7494986fa3d8b2bb01963537e60efb13981e138e3d4a1c4f62198a9d6fa9170c42c3c71f1971b29eb1d5d0bd733e40816c91f7912cc4a660c48dae03e57aaa38f3d0cffcfc21852ebc8b405d15bd6744945ba1a93438a162b6111699d98a16bb55b7bdddfe0fc5608b23da246e7bd73b47369169c5c90",
        ))
        .unwrap();
        let context = h("4f50415155452d504f43");
        let done = login
            .state
            .finish(
                &mut rng,
                &password,
                ke2,
                ClientLoginFinishParameters::new(Some(&context), Identifiers::default(), None),
            )
            .unwrap();
        assert_eq!(
            hex::encode(done.message.serialize()),
            "4455df4f810ac31a6748835888564b536e6da5d9944dfea9e34defb9575fe5e2661ef61d2ae3929bcf57e53d464113d364365eb7d1a57b629707ca48da18e442"
        );
        assert_eq!(
            hex::encode(&done.session_key),
            "42afde6f5aca0cfa5c163763fbad55e73a41db6b41bc87b8e7b62214a8eedc6731fa3cb857d657ab9b3764b89a84e91ebcb4785166fbb02cedfcbdfda215b96f"
        );
    }

    // Transcript recorded with ChatterSuite (Argon2id KSF, chatter context)
    // against the Go server (github.com/bytemare/opaque v0.18.0) over a live
    // pipe; the Go side replays it in internal/services/auth/opaque
    // (TestCrossImplementationTranscriptFromRustClient). Server inputs: RFC
    // C.1.1's oprf_seed / server_private_key / masking_nonce / server nonce
    // and key-share seed, credential_identifier "chatter-golden-credential-id-001".
    const GOLDEN_PASSWORD: &str = "correct horse battery staple";
    const GOLDEN_REG_REQUEST: &str =
        "b0d6fdba2c5b399d52e75063804783d70c1c487b4fb6204ac71b00713f60736a";
    const GOLDEN_REG_RESPONSE: &str = "8447aacacc019f83e4e367bd2d38ef8ea5c6eb5619b174e3d274aad0653bb054b2fe7af9f48cc502d016729d2fe25cdd433f2c4bc904660b2a382c9b79df1a78";
    const GOLDEN_UPLOAD: &str = "d2099be4088d0e64b5eda0fa5fc644acaea89ba88884d0b30372acb461b50862475b009200c7930d6a6e259c6878f08797a38f798804ee2fe6d1709b114416b6726d6be0f1c6109c6c5e21dec869b962ba6581f5db9c8fec9162065bf9d75d46ac13171b2f17bc2c74997f0fce1e1f35bec6b91fe2e12dbd323d23ba7a38dfeca9e83f3497f9caed298f11d448144ac33dd3250584a2b1d33be2a6b1f518a838f21a2a8f4c07620150c7790189148714291d009801b22089e5d163d15ef0eda6";
    const GOLDEN_KE1: &str = "281d6f0bd1fca73beca075aa6d87724a65c191e7b657d25c1b4cc392898ee526da7e07376d6d6f034cfa9bb537d11b8c6b4238c334333d1f0aebb380cae6a6cc6e29bee50701498605b2c085d7b241ca15ba5c32027dd21ba420b94ce60da326";
    const GOLDEN_KE2: &str = "8c15ef4d982e31b9def8627355896a94143cc9b08e3a13e358c8acfb14c8974538fe59af0df2c79f57b8780278f5ae47355fe1f817119041951c80f612fdfc6d69b8f0e48c92229737cefdcc2658142aead13d27fccc4ff0a50a347bf57bfb777a18d500559743db58f0581362cbda9382a1fcf856713663eb38f784caed25f2e6fb309a8b61905a3e37fd918c54a5b3d8c6912734cd6e06f5e807688871f944f5c223972f47cfab7b536c3a77d341676f73f3f6b5986f46d6df9e848e8f740f71cd9960ecef2fe0d0f7494986fa3d8b2bb01963537e60efb13981e138e3d4a1c4f62198a9d6fa9170c42c3c71f1971b29eb1d5d0bd733e40816c91f7912cc4a90b0e0206dcd31e5e03198f4be226538876f4a3bf08dae76550cfd25b4bac7365af668b2b0c0f76af0a8a63198400a04840ef78270667142d7e6b1a5951d7bbd";
    const GOLDEN_KE3: &str = "6752311a583b580d33e1a279a09d2cd6c2bcc0fa511bbab2df83dcc64acb1f8e8eb178b67b825ece8483aa1ee1fc40ee8c220c71e1dd54ddbe6332220518d6de";
    const GOLDEN_SESSION_KEY: &str = "76a036b065b22be20f5f82e5089c992ea5fe91d60e04a5fa54a2f0de7fcf1d01aaad39dbc8a75c1c6744113c431c79839620a78e9b2758f771eca1cb5ae3340b";
    const GOLDEN_EXPORT_KEY: &str = "d6ee47c27f95a40d8ec08cc0ea5f39d0dc6c8b752d12f5fb3092bffdaf1370b566710c95e19a97d4add4db01fab16f6d2f95f9791aabe3c0013315ce17ba1f5c";

    /// Production suite, byte-for-byte against the Go server's responses.
    /// Two production-strength Argon2id runs (~0.2 s).
    #[test]
    fn cross_impl_transcript_matches_go_server() {
        let mut rng = scripted();
        let pw = GOLDEN_PASSWORD.as_bytes();

        let reg = registration_start_with(&mut rng, pw).unwrap();
        assert_eq!(hex::encode(&reg.request), GOLDEN_REG_REQUEST);
        let upload = registration_finish_with(&mut rng, reg, pw, &h(GOLDEN_REG_RESPONSE)).unwrap();
        assert_eq!(hex::encode(&upload), GOLDEN_UPLOAD);

        let login = login_start_with(&mut rng, pw).unwrap();
        assert_eq!(hex::encode(&login.ke1), GOLDEN_KE1);
        let done = login_finish_with(&mut rng, login, pw, &h(GOLDEN_KE2)).unwrap();
        assert_eq!(hex::encode(&done.ke3), GOLDEN_KE3);
        assert_eq!(hex::encode(&*done.session_key), GOLDEN_SESSION_KEY);
        assert_eq!(hex::encode(&*done.export_key), GOLDEN_EXPORT_KEY);
    }

    /// The same Go KE2 with a wrong password does not open: nothing to send.
    #[test]
    fn wrong_password_cannot_answer_go_ke2() {
        let mut rng = ScriptRng(vec![
            wide(BLIND_LOGIN),
            h(CLIENT_KEYSHARE_SEED),
            h(CLIENT_NONCE),
        ]);
        let login = login_start_with(&mut rng, b"correct horse battery stapler").unwrap();
        let mut rng = rand_core_06::OsRng;
        assert!(matches!(
            login_finish_with(
                &mut rng,
                login,
                b"correct horse battery stapler",
                &h(GOLDEN_KE2)
            ),
            Err(OpaqueError::InvalidLogin)
        ));
    }

    /// A server KE2 under a different application context is refused (the
    /// context is in the authenticated transcript).
    #[test]
    fn foreign_context_rejected() {
        let mut rng = ScriptRng(vec![
            wide(BLIND_LOGIN),
            h(CLIENT_KEYSHARE_SEED),
            h(CLIENT_NONCE),
        ]);
        let login =
            ClientLogin::<ChatterSuite>::start(&mut rng, GOLDEN_PASSWORD.as_bytes()).unwrap();
        let ke2 = CredentialResponse::<ChatterSuite>::deserialize(&h(GOLDEN_KE2)).unwrap();
        let r = login.state.finish(
            &mut rand_core_06::OsRng,
            GOLDEN_PASSWORD.as_bytes(),
            ke2,
            ClientLoginFinishParameters::new(Some(b"OPAQUE-POC"), Identifiers::default(), None),
        );
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn async_variants_round_trip_shapes() {
        let reg = registration_start("pw").unwrap();
        assert_eq!(reg.request.len(), 32);
        let login = login_start("pw").unwrap();
        assert_eq!(login.ke1.len(), 96);
        // A response that is not a valid group element is a protocol error,
        // not a panic in the blocking task.
        assert!(matches!(
            registration_finish_async(reg, "pw", vec![0xff; 64]).await,
            Err(OpaqueError::Protocol(_))
        ));
    }
}
