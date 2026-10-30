//! Picker history façade: the signed-in account's recent emoji and GIFs.
//!
//! Stored in the encrypted settings table under a key that carries the
//! account id, so two accounts on one installation keep separate history
//! and nothing is read while signed out. Sign-out leaves the rows in place
//! for the same account's next sign-in. The shaping (MRU, dedupe, cap) is
//! `ui::picker_history`'s.
//!
//! A pick is recorded for the account it was made under, and history is read
//! for the account it was asked for: the caller takes a [`PickerHistoryScope`]
//! once, and both the write and the read happen under that owner's key only
//! while that owner and session are still current — checked again after the
//! database round trip, so a sign-out or account switch in the middle yields
//! nothing rather than the other account's rows. Reads and read-modify-writes
//! share one lock, so two picks cannot drop each other and a read never sees
//! a pick half-applied.

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

// One reader or writer at a time: a reaction and an insertion landing
// together must not read the same list and each write back without the
// other's pick, and a listing must not read between the two.
static HISTORY_LOCK: Mutex<()> = Mutex::new(());

impl ChatterClient {
    /// Who a pick made now belongs to, or `None` while signed out.
    pub fn picker_history_scope(&self) -> Option<PickerHistoryScope> {
        let auth = self.auth_state().get();
        Some(PickerHistoryScope {
            owner: auth.current_user_id?,
            session_revision: auth.session_revision,
        })
    }

    /// Whether the account and session a scope was captured under are still
    /// the signed-in ones. The UI asks this once more right before it
    /// publishes rows read for that scope.
    pub fn picker_scope_is_current(&self, scope: PickerHistoryScope) -> bool {
        let auth = self.auth_state().get();
        auth.current_user_id == Some(scope.owner) && auth.session_revision == scope.session_revision
    }

    /// The scope's rows, or none once that account or session is no longer
    /// current — checked before and after the database read, under the same
    /// lock the writers hold.
    fn read_scoped_picker_history<T: DeserializeOwned>(
        &self,
        scope: PickerHistoryScope,
        what: &str,
    ) -> Vec<T> {
        let _reader = HISTORY_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.picker_scope_is_current(scope) {
            return Vec::new();
        }
        let rows = self.read_picker_history(&scope.key(what));
        if self.picker_scope_is_current(scope) {
            rows
        } else {
            Vec::new()
        }
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
        let _writer = HISTORY_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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

    /// Emoji the account in `scope` has inserted or reacted with, newest
    /// first; none once that account or session is gone.
    pub fn recent_emoji(&self, scope: PickerHistoryScope) -> Vec<String> {
        self.read_scoped_picker_history(scope, EMOJI)
    }

    /// Remember an accepted emoji pick for the account it was made under.
    pub fn record_recent_emoji(&self, scope: PickerHistoryScope, emoji: &str) {
        self.update_picker_history(scope, EMOJI, |recent| {
            picker_history::remember_emoji(recent, emoji)
        });
    }

    /// GIFs the account in `scope` has sent, newest first; none once that
    /// account or session is gone.
    pub fn recent_gifs(&self, scope: PickerHistoryScope) -> Vec<RecentGif> {
        self.read_scoped_picker_history(scope, GIFS)
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
