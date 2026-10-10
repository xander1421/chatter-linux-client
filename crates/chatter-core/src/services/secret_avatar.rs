//! Secret-chat avatars as encrypted attachment pointers.
//!
//! A secret-chat control (request, accept, profile update, group invite,
//! join ack, profile share, member list) carries the sender's avatar as an
//! [`AttachmentPointer`] to an encrypted blob uploaded over Tor, never as
//! inline bytes: a request is sealed to a device inbox whose body cap is
//! 8 KiB, and a 100 KB avatar inline made every secret-chat start fail.
//!
//! The sender uploads each distinct avatar once and reuses the pointer (cached
//! in settings by the avatar's SHA-256). The receiver fetches the blob over
//! Tor, bounded by the size the pointer claims, and checks MAC, digest and
//! plaintext hash before it trusts a byte. An avatar it already holds (same
//! plaintext hash) is not fetched again.

use sha2::{Digest, Sha256};

use crate::crypto::attachment_crypto::{AttachmentCrypto, KEY_SET_LENGTH};
use crate::domain::anon_dm_init::MAX_AVATAR_SIZE;
use crate::proto::content_v1::AttachmentPointer;
use crate::services::attachment::{
    AttachmentError, AttachmentService, attachments_dir, ensure_attachments_dir,
};
use crate::storage::repositories::SettingsRepository;

/// Settings key prefix: `<prefix><hex sha256(avatar)>` holds the encoded
/// pointer of that avatar's upload.
const POINTER_CACHE_PREFIX: &str = "secret_chat.avatar_pointer.";

/// A fetch slower than this gives up; the profile simply has no avatar yet.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

fn plaintext_hash(avatar: &[u8]) -> String {
    hex::encode(Sha256::digest(avatar))
}

/// The pointer for an uploaded, encrypted avatar.
fn pointer(
    cdn_key: String,
    key: &[u8; KEY_SET_LENGTH],
    digest: &[u8; 32],
    avatar: &[u8],
) -> AttachmentPointer {
    AttachmentPointer {
        cdn_key,
        encryption_key: key.to_vec(),
        digest: digest.to_vec(),
        plaintext_size: avatar.len() as u64,
        plaintext_hash: plaintext_hash(avatar),
        ..Default::default()
    }
}

/// The pointer for `avatar`: the cached one when this avatar was uploaded
/// before, else a fresh upload over Tor. `None` (logged) when the avatar is
/// empty, too large, or the upload fails: the control goes without one.
pub(crate) async fn pointer_for(
    settings: &SettingsRepository,
    attachments: &AttachmentService,
    avatar: &[u8],
) -> Option<AttachmentPointer> {
    use prost::Message;

    if avatar.is_empty() || avatar.len() > MAX_AVATAR_SIZE {
        return None;
    }
    let cache_key = format!("{POINTER_CACHE_PREFIX}{}", plaintext_hash(avatar));
    if let Some(cached) = settings
        .get_bytes(&cache_key)
        .ok()
        .flatten()
        .and_then(|b| AttachmentPointer::decode(b.as_slice()).ok())
    {
        return Some(cached);
    }
    match upload(&attachments.for_secret_chat(), avatar).await {
        Ok(pointer) => {
            if let Err(e) = settings.set_bytes(&cache_key, &pointer.encode_to_vec()) {
                tracing::warn!(error = %e, "Could not cache the secret-chat avatar pointer");
            }
            Some(pointer)
        }
        Err(e) => {
            tracing::warn!(error = %e, "Secret-chat avatar upload failed; sending without it");
            None
        }
    }
}

async fn upload(
    attachments: &AttachmentService,
    avatar: &[u8],
) -> Result<AttachmentPointer, AttachmentError> {
    let net = |e: crate::error::NetworkError| AttachmentError::NetworkError(e.to_string());
    let encrypted = AttachmentCrypto::encrypt(avatar)
        .map_err(|e| AttachmentError::EncryptionError(e.to_string()))?;
    let mut files = attachments.file_client()?;
    let slot = files
        .get_attachment_upload_url(encrypted.ciphertext.len() as u64)
        .await
        .map_err(net)?;
    let (status, _) = attachments
        .cdn_http_put(
            &slot.upload_url,
            encrypted.ciphertext.clone(),
            "application/octet-stream",
        )
        .await?;
    if !(200..300).contains(&status) {
        return Err(AttachmentError::NetworkError(format!(
            "avatar upload returned status {status}"
        )));
    }
    files
        .confirm_attachment_upload(&slot.cdn_key)
        .await
        .map_err(net)?;
    Ok(pointer(
        slot.cdn_key,
        &encrypted.key,
        &encrypted.digest,
        avatar,
    ))
}

/// The avatar a received pointer names. `current` is the avatar already held
/// for this peer: returned as is, without a fetch, when the pointer names the
/// same image. `Ok(None)` when there is no usable pointer or the blob fails a
/// check (callers treat it as "avatar unchanged"); `Err` when the fetch itself
/// failed, which is worth retrying ([`retrying`]).
pub(crate) async fn resolve(
    attachments: &AttachmentService,
    pointer: Option<&AttachmentPointer>,
    current: Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>, AttachmentError> {
    let Some(pointer) = pointer else {
        return Ok(None);
    };
    if let Some(current) = current
        && !pointer.plaintext_hash.is_empty()
        && plaintext_hash(&current) == pointer.plaintext_hash
    {
        return Ok(Some(current));
    }
    let Some(bound) = fetch_bound(pointer) else {
        return Ok(None);
    };
    let ciphertext = fetch(&attachments.for_secret_chat(), pointer, bound).await?;
    Ok(open(pointer, &ciphertext))
}

/// [`resolve`] once, for a caller that will not retry: a failed fetch is
/// logged and reads as "no avatar".
pub(crate) async fn resolve_once(
    attachments: &AttachmentService,
    pointer: Option<&AttachmentPointer>,
    current: Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    resolve(attachments, pointer, current)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Secret-chat avatar fetch failed");
            None
        })
}

/// Waits before each fetch attempt of a peer's avatar. Tor circuits to the
/// storage onion fail in bursts (RC107: the S25 dropped its peer's avatar for
/// good after one failed fetch), so a failed fetch is tried again later rather
/// than dropped. Retries live in memory only: a restart forgets them, and the
/// peer's next profile update brings the pointer again.
pub(crate) const RETRY_DELAYS: [std::time::Duration; 5] = [
    std::time::Duration::ZERO,
    std::time::Duration::from_secs(30),
    std::time::Duration::from_secs(120),
    std::time::Duration::from_secs(600),
    std::time::Duration::from_secs(1800),
];

/// Run `attempt` (a [`resolve`]) after each of `delays` until it does not
/// fail. Its answer, or the last fetch error when every attempt failed.
pub(crate) async fn retrying<F, Fut>(
    delays: &[std::time::Duration],
    mut attempt: F,
) -> Result<Option<Vec<u8>>, AttachmentError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Option<Vec<u8>>, AttachmentError>>,
{
    let mut last = Ok(None);
    for delay in delays {
        tokio::time::sleep(*delay).await;
        last = attempt().await;
        match &last {
            Ok(_) => return last,
            Err(e) => tracing::warn!(error = %e, "Secret-chat avatar fetch failed; will retry"),
        }
    }
    last
}

/// The most ciphertext a well-formed pointer can name: the padded plaintext
/// plus IV, a CBC block and the MAC. `None` for a pointer no avatar fits.
fn fetch_bound(pointer: &AttachmentPointer) -> Option<u64> {
    let size = usize::try_from(pointer.plaintext_size).ok()?;
    if size == 0 || size > MAX_AVATAR_SIZE || pointer.cdn_key.is_empty() {
        return None;
    }
    Some((AttachmentCrypto::padded_size(size) + 64) as u64)
}

async fn fetch(
    attachments: &AttachmentService,
    pointer: &AttachmentPointer,
    bound: u64,
) -> Result<Vec<u8>, AttachmentError> {
    use tokio::io::AsyncWriteExt;

    let file_err = |_| AttachmentError::FileError("avatar staging file failed".into());
    let url = attachments
        .file_client()?
        .get_attachment_download_url(&pointer.cdn_key)
        .await
        .map_err(|e| AttachmentError::NetworkError(e.to_string()))?
        .download_url;
    ensure_attachments_dir().map_err(file_err)?;
    let staging = tempfile::NamedTempFile::new_in(attachments_dir()).map_err(file_err)?;
    let mut file = tokio::fs::File::from_std(staging.reopen().map_err(file_err)?);
    tokio::time::timeout(
        FETCH_TIMEOUT,
        attachments.cdn_http_get_to_file(&url, &mut file, bound),
    )
    .await
    .map_err(|_| AttachmentError::NetworkError("avatar fetch timed out".into()))??;
    file.flush().await.map_err(file_err)?;
    drop(file);
    std::fs::read(staging.path()).map_err(file_err)
}

/// Decrypt a fetched avatar blob: MAC and digest under the pointer's key, the
/// claimed size, then the claimed plaintext hash.
fn open(pointer: &AttachmentPointer, ciphertext: &[u8]) -> Option<Vec<u8>> {
    let size = usize::try_from(pointer.plaintext_size).ok()?;
    if size > MAX_AVATAR_SIZE {
        return None;
    }
    let key: [u8; KEY_SET_LENGTH] = pointer.encryption_key.as_slice().try_into().ok()?;
    let digest: [u8; 32] = pointer.digest.as_slice().try_into().ok()?;
    let avatar = AttachmentCrypto::decrypt(ciphertext, &key, &digest, size).ok()?;
    (plaintext_hash(&avatar) == pointer.plaintext_hash).then_some(avatar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::content_v1::{
        AnonDmAccept, AnonDmProfileUpdate, AnonDmRequest, AnonGroupInvite, AnonGroupJoinAck,
        AnonGroupMemberInfo, AnonGroupMemberList, AnonGroupProfileShare, Content, content,
    };
    use prost::Message;

    /// What a 512x512 avatar costs on disk: incompressible bytes the size of
    /// a photo JPEG at that resolution (the S9 report saw 121 KB).
    fn realistic_avatar() -> Vec<u8> {
        use rand::RngCore;
        let mut avatar = vec![0u8; 128 * 1024];
        rand::rng().fill_bytes(&mut avatar);
        avatar
    }

    /// The pointer a real upload of `avatar` yields, with the longest CDN key
    /// the proto allows.
    fn uploaded(avatar: &[u8]) -> (AttachmentPointer, Vec<u8>) {
        let encrypted = AttachmentCrypto::encrypt(avatar).unwrap();
        (
            pointer("k".repeat(64), &encrypted.key, &encrypted.digest, avatar),
            encrypted.ciphertext.clone(),
        )
    }

    #[test]
    fn receiver_opens_the_pointer_it_was_sent() {
        let avatar = realistic_avatar();
        let (pointer, ciphertext) = uploaded(&avatar);
        assert!(ciphertext.len() as u64 <= fetch_bound(&pointer).unwrap());
        assert_eq!(
            open(&pointer, &ciphertext).as_deref(),
            Some(avatar.as_slice())
        );

        // A blob that is not the one named: the MAC/digest refuse it.
        let (other, other_ciphertext) = uploaded(&realistic_avatar());
        assert_eq!(open(&pointer, &other_ciphertext), None);
        // Right blob, lying hash: refused after decryption.
        let lying = AttachmentPointer {
            plaintext_hash: other.plaintext_hash,
            ..pointer.clone()
        };
        assert_eq!(open(&lying, &ciphertext), None);
        // A pointer claiming more than an avatar may be is never fetched.
        let huge = AttachmentPointer {
            plaintext_size: MAX_AVATAR_SIZE as u64 + 1,
            ..pointer
        };
        assert_eq!(fetch_bound(&huge), None);
    }

    #[tokio::test]
    async fn receiver_keeps_an_avatar_it_already_holds_without_fetching() {
        let avatar = realistic_avatar();
        let (pointer, _) = uploaded(&avatar);
        // A service with no network: a fetch would fail, so getting the bytes
        // back proves no fetch happened.
        let dir = tempfile::tempdir().unwrap();
        let db = crate::storage::Database::open(&dir.path().join("t.db"), "test").unwrap();
        let attachments = AttachmentService::new_local(std::sync::Arc::new(
            crate::storage::repositories::AttachmentRepository::new(std::sync::Arc::new(db)),
        ));
        let got = resolve(&attachments, Some(&pointer), Some(avatar.clone())).await;
        assert_eq!(got.unwrap().as_deref(), Some(avatar.as_slice()));
        // A different held avatar would need the fetch, which fails here, and
        // says so (retryable) instead of reading as "no avatar".
        assert!(
            resolve(&attachments, Some(&pointer), Some(vec![1, 2, 3]))
                .await
                .is_err()
        );
        assert_eq!(
            resolve(&attachments, None, Some(avatar)).await.unwrap(),
            None
        );
    }

    /// A fetch that fails (flaky Tor circuit) is tried again; the first answer
    /// that is not a fetch failure ends the retries.
    #[tokio::test]
    async fn failed_fetch_is_retried_until_it_answers() {
        use std::time::Duration;
        let tries = std::cell::Cell::new(0);
        let attempt = || {
            tries.set(tries.get() + 1);
            let n = tries.get();
            async move {
                if n < 3 {
                    Err(AttachmentError::NetworkError(
                        "hidden service circuit".into(),
                    ))
                } else {
                    Ok(Some(vec![n as u8]))
                }
            }
        };
        let got = retrying(&[Duration::ZERO; 5], attempt).await.unwrap();
        assert_eq!((got, tries.get()), (Some(vec![3]), 3));

        // Every attempt failing gives up after the last delay.
        let tries = std::cell::Cell::new(0);
        let failing = || {
            tries.set(tries.get() + 1);
            async { Err(AttachmentError::NetworkError("down".into())) }
        };
        assert!(retrying(&[Duration::ZERO; 2], failing).await.is_err());
        assert_eq!(tries.get(), 2);

        // An unusable pointer is an answer, not a failure: no retry.
        let tries = std::cell::Cell::new(0);
        let unusable = || {
            tries.set(tries.get() + 1);
            async { Ok(None) }
        };
        assert_eq!(
            retrying(&[Duration::ZERO; 5], unusable).await.unwrap(),
            None
        );
        assert_eq!(tries.get(), 1);
    }

    /// Every secret-chat control that names an avatar, built with a realistic
    /// avatar's pointer and every other field at its proto maximum, stays far
    /// under the server's 8192-byte sealed body cap
    /// (`internal/services/message/adapters/signal_crypto.go` MaxSealedBodySize).
    /// The budget leaves room for the 160-byte padding bucket, the Signal
    /// message and the sealed-sender envelope with its sender certificate.
    #[test]
    fn secret_chat_controls_fit_the_sealed_cap() {
        const SEALED_CAP: usize = 8192;
        const CONTENT_BUDGET: usize = 2048;

        let (avatar, _) = uploaded(&realistic_avatar());
        let avatar = Some(avatar);
        let key = Some(vec![7u8; 32]);
        let name = Some(vec![9u8; 300]);
        let id16 = vec![1u8; 16];
        let pk32 = vec![2u8; 32];
        let wrap = |c: content::Content| Content {
            content: Some(c),
            timestamp: u64::MAX,
            ..Default::default()
        };
        let member = AnonGroupMemberInfo {
            user_id: id16.clone(),
            mailbox_pk: pk32.clone(),
            role: 2,
            profile_key: key.clone(),
            encrypted_display_name: name.clone(),
            avatar: avatar.clone(),
            admission_timestamp: u64::MAX,
            device_id: 127,
        };
        let controls = [
            (
                "AnonDmRequest",
                wrap(content::Content::AnonDmRequest(AnonDmRequest {
                    conversation_id: id16.clone(),
                    initiator_mailbox_pk: pk32.clone(),
                    intro_message: "x".repeat(500),
                    timestamp: u64::MAX,
                    profile_key: key.clone(),
                    encrypted_display_name: name.clone(),
                    avatar: avatar.clone(),
                })),
            ),
            (
                "AnonDmAccept",
                wrap(content::Content::AnonDmAccept(AnonDmAccept {
                    conversation_id: id16.clone(),
                    responder_mailbox_pk: pk32.clone(),
                    timestamp: u64::MAX,
                    profile_key: key.clone(),
                    encrypted_display_name: name.clone(),
                    avatar: avatar.clone(),
                })),
            ),
            (
                "AnonDmProfileUpdate",
                wrap(content::Content::AnonDmProfileUpdate(AnonDmProfileUpdate {
                    conversation_id: id16.clone(),
                    avatar_url: None,
                    bio: Some("b".repeat(512)),
                    timestamp: u64::MAX,
                    avatar: avatar.clone(),
                    profile_key: key.clone(),
                    encrypted_display_name: name.clone(),
                })),
            ),
            (
                "AnonGroupInvite",
                wrap(content::Content::AnonGroupInvite(AnonGroupInvite {
                    group_id: id16.clone(),
                    group_name: "g".repeat(1024),
                    inviter_mailbox_pk: pk32.clone(),
                    timestamp: u64::MAX,
                    profile_key: key.clone(),
                    encrypted_display_name: name.clone(),
                    avatar: avatar.clone(),
                })),
            ),
            (
                "AnonGroupJoinAck",
                wrap(content::Content::AnonGroupJoinAck(AnonGroupJoinAck {
                    group_id: id16.clone(),
                    joiner_mailbox_pk: pk32.clone(),
                    timestamp: u64::MAX,
                    profile_key: key.clone(),
                    encrypted_display_name: name.clone(),
                    avatar: avatar.clone(),
                })),
            ),
            (
                "AnonGroupProfileShare",
                wrap(content::Content::AnonGroupProfileShare(
                    AnonGroupProfileShare {
                        group_id: id16.clone(),
                        timestamp: u64::MAX,
                        profile_key: key.clone(),
                        encrypted_display_name: name.clone(),
                        avatar: avatar.clone(),
                    },
                )),
            ),
        ];
        for (name, control) in controls {
            let size = control.encoded_len();
            assert!(
                size <= CONTENT_BUDGET && size + 160 < SEALED_CAP / 2,
                "{name} is {size} bytes, over the {CONTENT_BUDGET}-byte budget"
            );
        }

        // A member list rides the members' anonymous mailboxes (1 MiB cap),
        // not a device inbox, and grows with the roster; what matters is that
        // a member's avatar costs a pointer, not the image.
        let list = |n| {
            wrap(content::Content::AnonGroupMemberList(AnonGroupMemberList {
                group_id: id16.clone(),
                members: vec![member.clone(); n],
                timestamp: u64::MAX,
            }))
            .encoded_len()
        };
        let per_member = list(2) - list(1);
        assert!(per_member <= 1024, "a roster entry is {per_member} bytes");
        assert!(list(1) <= CONTENT_BUDGET);
    }
}
