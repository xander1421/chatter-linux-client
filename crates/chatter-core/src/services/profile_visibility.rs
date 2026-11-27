//! Who can see our photo and name (Settings › Privacy).
//!
//! Signal-style: the server holds our profile only as ciphertext under our
//! profile key, so visibility is enforced by WHO RECEIVES THAT KEY, never by
//! the server. Every place that hands the key to someone else asks
//! [`shareable_profile_key`] (friends over DM, channel co-members in the SKDM,
//! a friend request) or [`anon_shareable_profile_key`] (anon DM / anon group).
//!
//! Restricting the setting cannot take the key back from people who already
//! hold it, so it rotates the key (`client::profile_visibility`); the local
//! half of that — which key is current, what is pending — lives here.
//!
//! The same key also derives our Unidentified-Access-Key, which is what lets
//! a friend send to us sealed. So accepted friends ALWAYS hold the current
//! key (owner decision 2026-11-27), "nobody" included: there the server
//! profile is uploaded empty ([`server_profile_plaintext`]), so the key
//! decrypts nothing and friends see only the username.

use crate::domain::ids::UserId;
use crate::domain::user::FriendshipStatus;
use crate::error::StorageError;
use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
use crate::storage::repositories::user_repo::UserRepository;

/// Ordered least to most restrictive, so `new > old` means "restricting".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProfileVisibility {
    Everyone,
    Contacts,
    Nobody,
}

impl ProfileVisibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Everyone => "everyone",
            Self::Contacts => "contacts",
            Self::Nobody => "nobody",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "everyone" => Some(Self::Everyone),
            "contacts" => Some(Self::Contacts),
            "nobody" => Some(Self::Nobody),
            _ => None,
        }
    }

    /// The stored setting; everyone when never set (today's behaviour).
    pub fn load(settings: &SettingsRepository) -> Self {
        settings
            .get_string(keys::PROFILE_VISIBILITY)
            .ok()
            .flatten()
            .and_then(|s| Self::parse(&s))
            .unwrap_or(Self::Everyone)
    }

    pub fn store(self, settings: &SettingsRepository) -> Result<(), StorageError> {
        settings.set_string(keys::PROFILE_VISIBILITY, self.as_str())
    }

    pub fn to_proto(self) -> chatter_proto::content_v1::ProfileVisibility {
        use chatter_proto::content_v1::ProfileVisibility as P;
        match self {
            Self::Everyone => P::Everyone,
            Self::Contacts => P::Contacts,
            Self::Nobody => P::Nobody,
        }
    }

    pub fn from_proto(p: chatter_proto::content_v1::ProfileVisibility) -> Option<Self> {
        use chatter_proto::content_v1::ProfileVisibility as P;
        match p {
            P::Everyone => Some(Self::Everyone),
            P::Contacts => Some(Self::Contacts),
            P::Nobody => Some(Self::Nobody),
            P::Unspecified => None,
        }
    }
}

/// Our current profile key, if well-formed.
pub fn own_profile_key(settings: &SettingsRepository) -> Option<[u8; 32]> {
    settings
        .get_bytes(keys::PROFILE_KEY)
        .ok()
        .flatten()
        .and_then(|k| <[u8; 32]>::try_from(k.as_slice()).ok())
}

/// A committed rotation still owes someone the new key: share nothing until
/// the server profile is re-encrypted under it.
pub fn rotation_pending(settings: &SettingsRepository) -> bool {
    settings
        .get::<bool>(keys::PROFILE_KEY_ROTATION_PENDING)
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// Our profile key for `recipient`, or `None` when the setting says they may
/// not hold it: never to ourselves (our devices get it by `ProfileKeySync`),
/// never to someone we blocked, never mid-rotation; then everyone / accepted
/// friends only. Accepted friends get it under "nobody" too: they need it to
/// reach us sealed, and the profile it opens is empty then. Fails closed: a
/// lookup error shares nothing.
pub fn shareable_profile_key(
    settings: &SettingsRepository,
    users: &UserRepository,
    recipient: &UserId,
) -> Option<[u8; 32]> {
    let us = settings.current_user_id().ok()?;
    if *recipient == us
        || rotation_pending(settings)
        || users.is_blocked(&us, recipient).unwrap_or(true)
    {
        return None;
    }
    let allowed = match ProfileVisibility::load(settings) {
        ProfileVisibility::Everyone => true,
        ProfileVisibility::Contacts | ProfileVisibility::Nobody => {
            is_accepted_friend(users, &us, recipient)
        }
    };
    if allowed {
        own_profile_key(settings)
    } else {
        None
    }
}

fn is_accepted_friend(users: &UserRepository, us: &UserId, peer: &UserId) -> bool {
    [(*us, *peer), (*peer, *us)].iter().any(|(a, b)| {
        matches!(
            users.get_friendship(a, b),
            Ok(Some(f)) if f.status == FriendshipStatus::Accepted
        )
    })
}

/// Who must be sent our new key after a rotation finishes: every accepted
/// friend we have not blocked, whatever the visibility, so each can derive
/// our new UAK and keep reaching us sealed. Fails closed to nobody.
pub fn key_rotation_recipients(settings: &SettingsRepository, users: &UserRepository) -> Vec<UserId> {
    let Ok(us) = settings.current_user_id() else {
        return Vec::new();
    };
    let mut peers: Vec<UserId> = users
        .list_friends(&us)
        .unwrap_or_default()
        .into_iter()
        .filter(|f| f.status == FriendshipStatus::Accepted)
        .map(|f| if f.requester_id == us { f.addressee_id } else { f.requester_id })
        .filter(|peer| *peer != us && !users.is_blocked(&us, peer).unwrap_or(true))
        .collect();
    peers.sort_by_key(|p| p.to_string());
    peers.dedup();
    peers
}

/// The plaintext our server profile carries under `visibility`: the real
/// name, status and avatar, except under "nobody", where every field goes up
/// empty (and the avatar is cleared) so the key our friends hold opens
/// nothing. Our own devices keep the plaintext locally.
pub fn server_profile_plaintext(
    visibility: ProfileVisibility,
    name: Option<String>,
    status: Option<String>,
    avatar: Option<Vec<u8>>,
) -> (Option<String>, Option<String>, Option<Vec<u8>>) {
    if visibility == ProfileVisibility::Nobody {
        (Some(String::new()), Some(String::new()), Some(Vec::new()))
    } else {
        (name, status, avatar)
    }
}

/// Our profile key for an anon DM / anon group peer. Those peers are
/// strangers by design, so only "everyone" shares with them.
pub fn anon_shareable_profile_key(settings: &SettingsRepository) -> Option<[u8; 32]> {
    // no per-peer friend check — an anon peer is not addressed as a
    // contact; add one if anon conversations with friends must see the profile.
    if rotation_pending(settings)
        || ProfileVisibility::load(settings) != ProfileVisibility::Everyone
    {
        return None;
    }
    own_profile_key(settings)
}

/// The fresh key for a rotation: the uncommitted one from an interrupted
/// attempt if there is one (its SVR backup may already have landed), else a
/// new random key, persisted before anything uses it.
pub fn next_profile_key(settings: &SettingsRepository) -> Result<[u8; 32], StorageError> {
    if let Some(next) = settings
        .get_bytes(keys::PROFILE_KEY_NEXT)?
        .and_then(|k| <[u8; 32]>::try_from(k.as_slice()).ok())
    {
        return Ok(next);
    }
    let next = crate::crypto::auth_crypto::generate_profile_key();
    settings.set_bytes(keys::PROFILE_KEY_NEXT, &next)?;
    Ok(next)
}

/// Make the pending key current, under `visibility`. The pending flag goes
/// down FIRST, so a crash anywhere after it is finished by
/// [`finish_interrupted_commit`] rather than leaving a switched key nobody
/// was told about.
pub fn commit_rotation(
    settings: &SettingsRepository,
    visibility: ProfileVisibility,
) -> Result<(), StorageError> {
    settings.set(keys::PROFILE_KEY_ROTATION_PENDING, &true)?;
    visibility.store(settings)?;
    finish_interrupted_commit(settings)
}

/// Complete a commit a crash cut short: a still-present next key becomes the
/// current one. A no-op when there is none.
pub fn finish_interrupted_commit(settings: &SettingsRepository) -> Result<(), StorageError> {
    if let Some(next) = settings.get_bytes(keys::PROFILE_KEY_NEXT)? {
        settings.set_bytes(keys::PROFILE_KEY, &next)?;
        settings.delete(keys::PROFILE_KEY_NEXT)?;
    }
    Ok(())
}

pub fn clear_rotation_pending(settings: &SettingsRepository) -> Result<(), StorageError> {
    settings
        .delete(keys::PROFILE_KEY_ROTATION_PENDING)
        .map(|_| ())
}

/// Adopt a `ProfileKeySync` from another of our devices (the caller has
/// checked it came from our own user). Returns whether our key changed.
pub fn apply_sync(
    settings: &SettingsRepository,
    sync: &chatter_proto::content_v1::ProfileKeySync,
) -> Result<bool, StorageError> {
    let Ok(key) = <[u8; 32]>::try_from(sync.profile_key.as_slice()) else {
        tracing::warn!(
            len = sync.profile_key.len(),
            "ProfileKeySync with a malformed key dropped"
        );
        return Ok(false);
    };
    if let Some(v) = ProfileVisibility::from_proto(sync.visibility()) {
        v.store(settings)?;
    }
    let changed = own_profile_key(settings) != Some(key);
    if changed {
        settings.set_bytes(keys::PROFILE_KEY, &key)?;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::domain::user::Friendship;
    use crate::storage::Database;

    struct T {
        settings: SettingsRepository,
        users: UserRepository,
        me: UserId,
    }

    fn setup() -> T {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("pv.db"), "testpass").unwrap());
        db.run_migrations().unwrap();
        crate::test_tempdir::hold(dir);
        let t = T {
            settings: SettingsRepository::new(db.clone()),
            users: UserRepository::new(db),
            me: UserId::new(),
        };
        t.settings
            .set(keys::CURRENT_USER_ID, &t.me.to_string())
            .unwrap();
        t.settings.set_bytes(keys::PROFILE_KEY, &[7; 32]).unwrap();
        t.users.ensure_user_exists(&t.me).unwrap();
        t
    }

    fn peer(t: &T, status: Option<FriendshipStatus>) -> UserId {
        let id = UserId::new();
        t.users.ensure_user_exists(&id).unwrap();
        if let Some(status) = status {
            t.users
                .insert_friendship(&Friendship {
                    requester_id: t.me,
                    addressee_id: id,
                    status,
                    created_at: chrono::Utc::now(),
                })
                .unwrap();
        }
        id
    }

    #[test]
    fn contacts_shares_with_incoming_friend_but_still_honors_our_block() {
        let t = setup();
        let friend = peer(&t, None);
        t.users
            .insert_friendship(&Friendship {
                requester_id: friend,
                addressee_id: t.me,
                status: FriendshipStatus::Accepted,
                created_at: chrono::Utc::now(),
            })
            .unwrap();
        ProfileVisibility::Contacts.store(&t.settings).unwrap();
        assert!(shareable_profile_key(&t.settings, &t.users, &friend).is_some());

        t.users
            .insert_friendship(&Friendship {
                requester_id: t.me,
                addressee_id: friend,
                status: FriendshipStatus::Blocked,
                created_at: chrono::Utc::now(),
            })
            .unwrap();
        assert!(shareable_profile_key(&t.settings, &t.users, &friend).is_none());
    }

    #[test]
    fn key_goes_only_where_the_setting_allows() {
        let t = setup();
        let friend = peer(&t, Some(FriendshipStatus::Accepted));
        let stranger = peer(&t, None); // e.g. a channel co-member
        let blocked = peer(&t, Some(FriendshipStatus::Blocked));
        let share = |to: &UserId| shareable_profile_key(&t.settings, &t.users, to).is_some();

        assert_eq!(
            ProfileVisibility::load(&t.settings),
            ProfileVisibility::Everyone,
            "default"
        );
        assert!(share(&friend) && share(&stranger));
        assert!(!share(&blocked), "a blocked user never gets it");
        assert!(!share(&t.me), "our own devices get it by ProfileKeySync");
        assert!(anon_shareable_profile_key(&t.settings).is_some());

        ProfileVisibility::Contacts.store(&t.settings).unwrap();
        assert!(share(&friend));
        assert!(!share(&stranger) && !share(&blocked));
        assert!(anon_shareable_profile_key(&t.settings).is_none());

        // "Nobody" still hands friends the delivery key (their UAK for us);
        // the profile it opens is empty (`server_profile_plaintext`).
        ProfileVisibility::Nobody.store(&t.settings).unwrap();
        assert!(share(&friend));
        assert!(!share(&stranger) && !share(&blocked));
    }

    /// After any rotation every accepted, non-blocked friend is owed the new
    /// key, whatever the setting: Everyone→Contacts and Everyone→Nobody both
    /// keep friends deliverable (their UAK is derived from what we send, and
    /// the server UAK moves to the same key).
    #[test]
    fn rotation_pushes_the_new_key_to_every_friend() {
        let t = setup();
        let friend = peer(&t, Some(FriendshipStatus::Accepted));
        let incoming = peer(&t, None);
        t.users
            .insert_friendship(&Friendship {
                requester_id: incoming,
                addressee_id: t.me,
                status: FriendshipStatus::Accepted,
                created_at: chrono::Utc::now(),
            })
            .unwrap();
        let _stranger = peer(&t, None);
        let _pending = peer(&t, Some(FriendshipStatus::Pending));
        let _blocked = peer(&t, Some(FriendshipStatus::Blocked));
        let mut expected = vec![friend, incoming];
        expected.sort_by_key(|p| p.to_string());

        for visibility in [ProfileVisibility::Contacts, ProfileVisibility::Nobody] {
            let next = next_profile_key(&t.settings).unwrap();
            commit_rotation(&t.settings, visibility).unwrap();
            clear_rotation_pending(&t.settings).unwrap();
            assert_eq!(key_rotation_recipients(&t.settings, &t.users), expected);
            for to in &expected {
                // The handshake carries exactly the key the server UAK moved to.
                assert_eq!(shareable_profile_key(&t.settings, &t.users, to), Some(next));
                assert_eq!(
                    crate::crypto::derive_uak(&shareable_profile_key(&t.settings, &t.users, to).unwrap()),
                    crate::crypto::derive_uak(&own_profile_key(&t.settings).unwrap()),
                );
            }
        }
    }

    /// Everyone→Nobody uploads an empty profile, so friends (who still hold
    /// the key) see only the username; Nobody→Contacts uploads it again.
    #[test]
    fn nobody_uploads_an_empty_profile_and_contacts_restores_it() {
        let real = || (Some("Ada".to_string()), Some("hi".to_string()), Some(vec![1, 2, 3]));
        let (n, s, a) = real();
        assert_eq!(
            server_profile_plaintext(ProfileVisibility::Nobody, n, s, a),
            (Some(String::new()), Some(String::new()), Some(Vec::new()))
        );
        for visibility in [ProfileVisibility::Contacts, ProfileVisibility::Everyone] {
            let (n, s, a) = real();
            assert_eq!(server_profile_plaintext(visibility, n, s, a), real());
        }
    }

    #[test]
    fn rotation_switches_the_key_and_shares_nothing_until_finished() {
        let t = setup();
        let friend = peer(&t, Some(FriendshipStatus::Accepted));

        // A retry reuses the uncommitted key its backup may already hold.
        let next = next_profile_key(&t.settings).unwrap();
        assert_eq!(next_profile_key(&t.settings).unwrap(), next);
        assert_ne!(next, [7; 32]);
        assert_eq!(
            own_profile_key(&t.settings),
            Some([7; 32]),
            "not current before commit"
        );

        commit_rotation(&t.settings, ProfileVisibility::Contacts).unwrap();
        assert_eq!(own_profile_key(&t.settings), Some(next));
        assert_eq!(
            ProfileVisibility::load(&t.settings),
            ProfileVisibility::Contacts
        );
        assert!(rotation_pending(&t.settings));
        assert!(
            shareable_profile_key(&t.settings, &t.users, &friend).is_none(),
            "nothing mid-rotation"
        );

        clear_rotation_pending(&t.settings).unwrap();
        assert_eq!(
            shareable_profile_key(&t.settings, &t.users, &friend),
            Some(next)
        );
        assert_ne!(
            next_profile_key(&t.settings).unwrap(),
            next,
            "the next rotation gets a fresh key"
        );
    }

    #[test]
    fn a_commit_cut_short_is_finished_on_resume() {
        let t = setup();
        let next = next_profile_key(&t.settings).unwrap();
        // Crash right after the flag went up, before the key switched.
        t.settings
            .set(keys::PROFILE_KEY_ROTATION_PENDING, &true)
            .unwrap();
        finish_interrupted_commit(&t.settings).unwrap();
        assert_eq!(own_profile_key(&t.settings), Some(next));
        assert!(
            t.settings
                .get_bytes(keys::PROFILE_KEY_NEXT)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn own_device_sync_adopts_key_and_setting() {
        use chatter_proto::content_v1::ProfileKeySync;
        let t = setup();
        let mut sync = ProfileKeySync {
            profile_key: vec![8; 32],
            ..Default::default()
        };
        sync.set_visibility(ProfileVisibility::Nobody.to_proto());
        assert!(apply_sync(&t.settings, &sync).unwrap());
        assert!(
            !apply_sync(&t.settings, &sync).unwrap(),
            "same key is no change"
        );
        assert_eq!(own_profile_key(&t.settings), Some([8; 32]));
        assert_eq!(
            ProfileVisibility::load(&t.settings),
            ProfileVisibility::Nobody
        );

        let bad = ProfileKeySync {
            profile_key: vec![1; 5],
            ..Default::default()
        };
        assert!(!apply_sync(&t.settings, &bad).unwrap());
        assert_eq!(
            own_profile_key(&t.settings),
            Some([8; 32]),
            "a malformed key is ignored"
        );
    }
}
