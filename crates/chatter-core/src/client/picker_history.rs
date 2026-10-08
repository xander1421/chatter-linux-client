//! Picker history façade: the signed-in account's recent emoji and GIFs.
//!
//! Stored in the encrypted settings table under a key that carries the
//! account id, so two accounts on one installation keep separate history
//! and nothing is read while signed out. Sign-out leaves the rows in place
//! for the same account's next sign-in. The shaping (MRU, dedupe, cap) is
//! `ui::picker_history`'s.

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::ChatterClient;
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::ui::picker_history::{self, RecentGif};

const EMOJI: &str = "emoji";
const GIFS: &str = "gifs";

impl ChatterClient {
    /// The settings key for `what`, or `None` while no account is signed in.
    fn picker_history_key(&self, what: &str) -> Option<String> {
        let user = self.auth_state().get().current_user_id?;
        Some(format!("ui.picker_history.{what}.{}", user.0))
    }

    fn picker_history<T: DeserializeOwned>(&self, what: &str) -> Vec<T> {
        let Some(key) = self.picker_history_key(what) else {
            return Vec::new();
        };
        SettingsRepository::new(self.services.database.clone())
            .get::<Vec<T>>(&key)
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    fn store_picker_history<T: Serialize>(&self, what: &str, recent: &[T]) {
        let Some(key) = self.picker_history_key(what) else {
            return;
        };
        if let Err(error) =
            SettingsRepository::new(self.services.database.clone()).set(&key, &recent)
        {
            tracing::warn!(%error, "picker history not saved");
        }
    }

    /// Emoji the account has inserted or reacted with, newest first.
    pub fn recent_emoji(&self) -> Vec<String> {
        self.picker_history(EMOJI)
    }

    /// Remember an accepted emoji pick for the signed-in account.
    pub fn record_recent_emoji(&self, emoji: &str) {
        let mut recent = self.recent_emoji();
        picker_history::remember_emoji(&mut recent, emoji);
        self.store_picker_history(EMOJI, &recent);
    }

    /// GIFs the account has sent, newest first.
    pub fn recent_gifs(&self) -> Vec<RecentGif> {
        self.picker_history(GIFS)
    }

    /// Remember a GIF the composer accepted for sending. Recorded at the
    /// pick, once the send is validated, so a failed upload can be repicked.
    pub fn record_recent_gif(&self, gif: RecentGif) {
        let mut recent = self.recent_gifs();
        picker_history::remember_gif(&mut recent, gif);
        self.store_picker_history(GIFS, &recent);
    }
}
