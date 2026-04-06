//! Recent emoji tracking with file persistence.
//!
//! Stores recently used emoji for the "Recent" tab in the emoji picker.
//! Persisted to `{data_dir}/recent_emoji.json`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Maximum number of recent emoji to store.
const MAX_RECENT: usize = 32;

/// Tracks recently used emoji with LRU ordering and file persistence.
pub struct RecentEmojiStore {
    recent: Mutex<Vec<String>>,
    path: PathBuf,
}

impl RecentEmojiStore {
    /// Load from disk or create empty. Never fails — returns empty on I/O errors.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("recent_emoji.json");
        let recent = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default();

        Self {
            recent: Mutex::new(recent),
            path,
        }
    }

    /// Record an emoji usage. Moves it to the front (MRU order), deduplicates, caps at MAX_RECENT.
    pub fn record(&self, emoji: &str) {
        let mut recent = self.recent.lock().unwrap();
        recent.retain(|e| e != emoji);
        recent.insert(0, emoji.to_string());
        recent.truncate(MAX_RECENT);

        // Best-effort persist — don't block on errors
        let _ = std::fs::write(&self.path, serde_json::to_string(&*recent).unwrap_or_default());
    }

    /// Get ordered list of recently used emoji (most recent first).
    pub fn get_recent(&self) -> Vec<String> {
        self.recent.lock().unwrap().clone()
    }
}
