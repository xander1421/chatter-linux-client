//! Posts from senders who are not (yet) in a private group's roster.
//!
//! A private group's roster is whatever the owner's last member list said.
//! A post from someone outside it is not delivered: a kicked member who kept
//! the mailbox keys must not keep posting to members who processed the kick.
//! But a new member's first posts can race the owner's member list that adds
//! them, so instead of dropping such a post at once it is held here — bounded
//! per group, for [`HOLD_TTL`] — and re-checked whenever the roster grows.
//! Whatever is still from a non-member when its time runs out is dropped.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::domain::UserId;
use crate::domain::anon_group::AnonGroupId;
use crate::proto::content_v1::Content;

/// Posts held per group; past it the oldest is dropped.
pub(crate) const HOLD_CAP: usize = 64;
/// How long a non-member's post waits for a roster that includes them.
pub(crate) const HOLD_TTL: Duration = Duration::from_secs(10 * 60);

pub(crate) struct HeldPost {
    pub(crate) at: Instant,
    pub(crate) sender: UserId,
    pub(crate) content: Content,
}

#[derive(Default)]
pub(crate) struct HeldPosts(Mutex<HashMap<AnonGroupId, VecDeque<HeldPost>>>);

impl HeldPosts {
    /// Hold `post`, dropping expired posts and, at the cap, the oldest.
    pub(crate) fn hold(&self, group: AnonGroupId, post: HeldPost, now: Instant) {
        let mut map = self.0.lock();
        let queue = map.entry(group).or_default();
        queue.retain(|p| now.saturating_duration_since(p.at) < HOLD_TTL);
        if now.saturating_duration_since(post.at) >= HOLD_TTL {
            return;
        }
        if queue.len() >= HOLD_CAP {
            queue.pop_front();
        }
        queue.push_back(post);
    }

    /// Every unexpired post held for `group`, oldest first; expired ones are
    /// dropped here.
    pub(crate) fn take(&self, group: &AnonGroupId, now: Instant) -> Vec<HeldPost> {
        self.0
            .lock()
            .remove(group)
            .map(|queue| {
                queue
                    .into_iter()
                    .filter(|p| now.saturating_duration_since(p.at) < HOLD_TTL)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Forget a group's held posts (it was left, or blocked).
    pub(crate) fn forget(&self, group: &AnonGroupId) {
        self.0.lock().remove(group);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(at: Instant) -> HeldPost {
        HeldPost {
            at,
            sender: UserId::new(),
            content: Content::default(),
        }
    }

    #[test]
    fn held_posts_are_bounded_and_expire() {
        let held = HeldPosts::default();
        let group = AnonGroupId::new();
        let t0 = Instant::now();
        for _ in 0..HOLD_CAP + 5 {
            held.hold(group, post(t0), t0);
        }
        assert_eq!(held.take(&group, t0).len(), HOLD_CAP, "capped per group");

        held.hold(group, post(t0), t0);
        assert!(
            held.take(&group, t0 + HOLD_TTL).is_empty(),
            "expired on take"
        );
        assert!(held.take(&group, t0).is_empty(), "take drains");
    }
}
