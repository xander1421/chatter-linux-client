//! Picker history: the emoji and GIFs an account has picked, newest first.
//!
//! Pure shaping only. Persistence is the client's (`client::picker_history`),
//! in the encrypted settings table under a per-account key, so one
//! installation's accounts never see each other's picks.

use serde::{Deserialize, Serialize};

/// How many picks each list keeps.
pub const MAX_RECENT: usize = 32;

/// A GIF the account has sent, with what the picker needs to show and
/// resend it. Provider metadata only; never the uploaded attachment.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentGif {
    pub id: String,
    pub url: String,
    pub preview_url: String,
    pub width: u32,
    pub height: u32,
}

/// Move the entry `is_same` matches to the front, or insert `item` there,
/// then cap the list. MRU, unique.
pub fn remember<T>(recent: &mut Vec<T>, item: T, is_same: impl Fn(&T) -> bool) {
    recent.retain(|existing| !is_same(existing));
    recent.insert(0, item);
    recent.truncate(MAX_RECENT);
}

pub fn remember_emoji(recent: &mut Vec<String>, emoji: &str) {
    if emoji.is_empty() {
        return;
    }
    remember(recent, emoji.to_string(), |existing| existing == emoji);
}

pub fn remember_gif(recent: &mut Vec<RecentGif>, gif: RecentGif) {
    if gif.id.is_empty() || gif.url.is_empty() {
        return;
    }
    let id = gif.id.clone();
    remember(recent, gif, |existing| existing.id == id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mru_unique_and_capped() {
        let mut recent = Vec::new();
        remember_emoji(&mut recent, "😀");
        remember_emoji(&mut recent, "🎉");
        remember_emoji(&mut recent, "😀");
        assert_eq!(recent, ["😀", "🎉"]);
        remember_emoji(&mut recent, "");
        assert_eq!(recent.len(), 2);
        for i in 0..40 {
            remember_emoji(&mut recent, &format!("e{i}"));
        }
        assert_eq!(recent.len(), MAX_RECENT);
        assert_eq!(recent[0], "e39");
        assert!(!recent.iter().any(|e| e == "😀"));
    }

    #[test]
    fn gifs_dedupe_by_id() {
        let gif = |id: &str, url: &str| RecentGif {
            id: id.into(),
            url: url.into(),
            ..Default::default()
        };
        let mut recent = Vec::new();
        remember_gif(&mut recent, gif("a", "u1"));
        remember_gif(&mut recent, gif("b", "u2"));
        remember_gif(&mut recent, gif("a", "u3"));
        assert_eq!(recent.len(), 2);
        assert_eq!((recent[0].id.as_str(), recent[0].url.as_str()), ("a", "u3"));
        remember_gif(&mut recent, gif("", "u"));
        remember_gif(&mut recent, gif("c", ""));
        assert_eq!(recent.len(), 2);
    }
}
