use hmac::{Hmac, Mac};
use rand::TryRngCore;
use sha2::Sha256;

use crate::error::SystemError;

use super::{AccountNotificationKeys, NotificationService};

impl NotificationService {
    pub(super) fn current_key(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
    ) -> Result<String, SystemError> {
        let Some(keys) = &self.account_keys else {
            #[cfg(test)]
            return Ok(notification_key(conversation_id));
            #[cfg(not(test))]
            return Err(SystemError::NotificationSend(
                "account key store unavailable".into(),
            ));
        };
        let owner = keys.owner().ok_or_else(|| {
            SystemError::NotificationSend("no signed-in notification account".into())
        })?;
        let secret = keys.secret(owner, true)?.ok_or_else(|| {
            SystemError::NotificationSend("notification account secret unavailable".into())
        })?;
        Ok(account_notification_key(owner, &secret, conversation_id))
    }

    /// Whether the opaque namespace in a tap belongs to the signed-in account.
    /// `None` means there is no usable account key yet; a cold-start tap may wait.
    pub fn tap_belongs_to_current_account(&self, key: &str) -> Option<bool> {
        if !is_opaque_notification_key(key) {
            return Some(false);
        }
        let keys = self.account_keys.as_ref()?;
        let owner = keys.owner()?;
        let secret = match keys.secret(owner, true) {
            Ok(Some(secret)) => secret,
            _ => return Some(false),
        };
        Some(key.starts_with(&account_namespace(owner, &secret)))
    }

    pub(crate) fn resolve_key(
        &self,
        key: &str,
        rows: impl IntoIterator<Item = (String, crate::domain::message::ConversationType)>,
    ) -> Option<crate::state::ActiveConversation> {
        if self.tap_belongs_to_current_account(key) != Some(true) {
            return None;
        }
        let keys = self.account_keys.as_ref()?;
        let owner = keys.owner()?;
        let secret = keys.secret(owner, false).ok()??;
        rows.into_iter().find_map(|(id, kind)| {
            let channel_id = id.parse().ok()?;
            (account_notification_key(owner, &secret, &channel_id) == key)
                .then(|| crate::state::ActiveConversation::from_routing_key(&kind, &id))
                .flatten()
        })
    }
}

impl AccountNotificationKeys {
    fn owner(&self) -> Option<crate::domain::UserId> {
        // Linked-device bring-up can have only the persisted owner. If both
        // sources are set during a switch, refuse an inconsistent account.
        let active = self.auth.get().current_user_id;
        let stored = self
            .settings
            .get(crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID)
            .ok()
            .flatten();
        match (active, stored) {
            (Some(a), Some(b)) if a != b => None,
            (Some(a), _) => Some(a),
            (None, b) => b,
        }
    }

    fn secret(
        &self,
        owner: crate::domain::UserId,
        create: bool,
    ) -> Result<Option<[u8; 32]>, SystemError> {
        let _guard = self
            .creation_lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let setting = format!("notification.tap_secret.{}", owner.0);
        if let Some(saved) = self
            .settings
            .get_string(&setting)
            .map_err(|e| SystemError::NotificationSend(e.to_string()))?
        {
            let bytes = hex::decode(saved)
                .map_err(|_| SystemError::NotificationSend("invalid notification secret".into()))?;
            let secret: [u8; 32] = bytes
                .try_into()
                .map_err(|_| SystemError::NotificationSend("invalid notification secret".into()))?;
            return Ok(Some(secret));
        }
        if !create {
            return Ok(None);
        }
        let mut secret = [0u8; 32];
        rand::rngs::OsRng.try_fill_bytes(&mut secret).map_err(|_| {
            SystemError::NotificationSend("notification entropy unavailable".into())
        })?;
        self.settings
            .set_string(&setting, &hex::encode(secret))
            .map_err(|e| SystemError::NotificationSend(e.to_string()))?;
        Ok(Some(secret))
    }
}

fn account_mac(
    owner: crate::domain::UserId,
    secret: &[u8; 32],
    label: &[u8],
    id: Option<&crate::domain::ids::ChannelId>,
) -> [u8; 32] {
    let mut mac =
        <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(secret).expect("fixed length HMAC key");
    mac.update(label);
    mac.update(owner.0.as_bytes());
    if let Some(id) = id {
        mac.update(id.to_string().as_bytes());
    }
    mac.finalize().into_bytes().into()
}

fn account_namespace(owner: crate::domain::UserId, secret: &[u8; 32]) -> String {
    hex::encode(&account_mac(owner, secret, b"chatter-notification-account-v1:", None)[..8])
}

fn account_notification_key(
    owner: crate::domain::UserId,
    secret: &[u8; 32],
    conversation_id: &crate::domain::ids::ChannelId,
) -> String {
    // 8-byte opaque account namespace + 16-byte conversation MAC. Android
    // can reject another account's tap before its sidebar rows are loaded.
    let mut key = account_namespace(owner, secret);
    key.push_str(&hex::encode(
        &account_mac(
            owner,
            secret,
            b"chatter-notification-conversation-v1:",
            Some(conversation_id),
        )[..16],
    ));
    key
}

/// Deterministic account fixture for existing sink and routing unit tests.
#[cfg(test)]
pub(super) fn notification_key(conversation_id: &crate::domain::ids::ChannelId) -> String {
    account_notification_key(
        crate::domain::UserId(uuid::Uuid::nil()),
        &[0x42; 32],
        conversation_id,
    )
}

/// Deterministic account fixture for existing routing unit tests.
#[cfg(test)]
pub(super) fn conversation_for_notification_key(
    key: &str,
    rows: impl IntoIterator<Item = (String, crate::domain::message::ConversationType)>,
) -> Option<crate::state::ActiveConversation> {
    if !is_opaque_notification_key(key) {
        return None;
    }
    rows.into_iter().find_map(|(id, kind)| {
        let channel_id = id.parse().ok()?;
        (notification_key(&channel_id) == key)
            .then(|| crate::state::ActiveConversation::from_routing_key(&kind, &id))
            .flatten()
    })
}

/// Intent extras may contain this key, never a raw conversation identifier.
pub fn is_opaque_notification_key(key: &str) -> bool {
    key.len() == 48
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
