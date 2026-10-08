//! Picker history façade: the signed-in account's recent emoji and GIFs.
//!
//! Stored in the encrypted settings table under a key that carries the
//! account id, so two accounts on one installation keep separate history
//! and nothing is read while signed out. Sign-out leaves the rows in place
//! for the same account's next sign-in. The shaping (MRU, dedupe, cap) is
//! `ui::picker_history`'s.
//!
//! A pick is recorded for the account it was made under: the caller takes a
//! [`PickerHistoryScope`] at the pick, and the write happens only while that
//! owner and session are still current, under that owner's key. The
//! read-modify-write is serialized, so two picks cannot drop each other.

use std::sync::Mutex;

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::ChatterClient;
use crate::domain::UserId;
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::ui::picker_history::{self, RecentGif};

const EMOJI: &str = "emoji";
const GIFS: &str = "gifs";

/// The account and session a pick was made under, captured once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PickerHistoryScope {
    pub owner: UserId,
    pub session_revision: u64,
}

impl PickerHistoryScope {
    fn key(&self, what: &str) -> String {
        format!("ui.picker_history.{what}.{}", self.owner.0)
    }
}

// One writer at a time: a reaction and an insertion landing together must
// not read the same list and each write back without the other's pick.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

impl ChatterClient {
    /// Who a pick made now belongs to, or `None` while signed out.
    pub fn picker_history_scope(&self) -> Option<PickerHistoryScope> {
        let auth = self.auth_state().get();
        Some(PickerHistoryScope {
            owner: auth.current_user_id?,
            session_revision: auth.session_revision,
        })
    }

    fn picker_scope_is_current(&self, scope: PickerHistoryScope) -> bool {
        let auth = self.auth_state().get();
        auth.current_user_id == Some(scope.owner) && auth.session_revision == scope.session_revision
    }

    fn read_picker_history<T: DeserializeOwned>(&self, key: &str) -> Vec<T> {
        SettingsRepository::new(self.services.database.clone())
            .get::<Vec<T>>(key)
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    /// Serialized read-modify-write under the scope's own key, skipped
    /// (and logged without content) once that account or session is gone.
    fn update_picker_history<T: Serialize + DeserializeOwned>(
        &self,
        scope: PickerHistoryScope,
        what: &str,
        change: impl FnOnce(&mut Vec<T>),
    ) {
        let _writer = WRITE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.picker_scope_is_current(scope) {
            tracing::debug!("picker history pick dropped: account or session changed");
            return;
        }
        let key = scope.key(what);
        let mut recent: Vec<T> = self.read_picker_history(&key);
        change(&mut recent);
        if SettingsRepository::new(self.services.database.clone())
            .set(&key, &recent)
            .is_err()
        {
            tracing::warn!("picker history not saved");
        }
    }

    /// Emoji the signed-in account has inserted or reacted with, newest first.
    pub fn recent_emoji(&self) -> Vec<String> {
        self.picker_history_scope()
            .map(|scope| self.read_picker_history(&scope.key(EMOJI)))
            .unwrap_or_default()
    }

    /// Remember an accepted emoji pick for the account it was made under.
    pub fn record_recent_emoji(&self, scope: PickerHistoryScope, emoji: &str) {
        self.update_picker_history(scope, EMOJI, |recent| {
            picker_history::remember_emoji(recent, emoji)
        });
    }

    /// GIFs the signed-in account has sent, newest first.
    pub fn recent_gifs(&self) -> Vec<RecentGif> {
        self.picker_history_scope()
            .map(|scope| self.read_picker_history(&scope.key(GIFS)))
            .unwrap_or_default()
    }

    /// Remember a GIF the composer accepted for sending, for the account it
    /// was picked under. Recorded at the validated pick, so a failed upload
    /// can simply be picked again from Recent.
    pub fn record_recent_gif(&self, scope: PickerHistoryScope, gif: RecentGif) {
        self.update_picker_history(scope, GIFS, |recent| {
            picker_history::remember_gif(recent, gif)
        });
    }
}
