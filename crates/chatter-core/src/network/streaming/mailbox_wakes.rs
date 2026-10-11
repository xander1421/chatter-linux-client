//! Process-local random capabilities. Never persisted with account settings or logged.
//! Re-registration on every ownership-proven poll repairs a server/cache restart.
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

struct Entry {
    topic: String,
    seen: Instant,
}
#[derive(Default)]
struct TopicRegistry(Mutex<HashMap<Vec<u8>, Entry>>);

impl TopicRegistry {
    fn topic(&self, public_key: &[u8]) -> String {
        let mut topics = self.0.lock().unwrap();
        let entry = topics.entry(public_key.to_vec()).or_insert_with(|| {
            use rand::TryRngCore;
            let mut bytes = [0u8; 16];
            rand::rngs::OsRng
                .try_fill_bytes(&mut bytes)
                .expect("OS CSPRNG unavailable");
            Entry {
                topic: format!("chatter_{}", hex::encode(bytes)),
                seen: Instant::now(),
            }
        });
        entry.seen = Instant::now();
        entry.topic.clone()
    }

    fn active(&self) -> Vec<String> {
        let mut topics = self.0.lock().unwrap();
        topics.retain(|_, entry| entry.seen.elapsed() < Duration::from_secs(600));
        topics.values().map(|entry| entry.topic.clone()).collect()
    }
}
fn registry() -> &'static TopicRegistry {
    static TOPICS: OnceLock<TopicRegistry> = OnceLock::new();
    TOPICS.get_or_init(Default::default)
}
pub(crate) fn topic(public_key: &[u8]) -> String {
    registry().topic(public_key)
}
pub(crate) fn active() -> Vec<String> {
    registry().active()
}
pub(crate) fn clear() {
    registry().0.lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    #[test]
    fn capabilities_are_random_per_mailbox_and_stable_for_registration() {
        let registry = super::TopicRegistry::default();
        let a = registry.topic(&[1; 32]);
        assert_eq!(a, registry.topic(&[1; 32]));
        assert_ne!(a, registry.topic(&[2; 32]));
        assert_eq!(a.len(), 40);
        assert!(!a.contains(&hex::encode([1; 32])));
        assert!(registry.active().contains(&a));
    }

    #[test]
    fn abandoned_mailbox_subscription_expires_and_new_registration_rotates() {
        let registry = super::TopicRegistry::default();
        let old = registry.topic(&[1; 32]);
        registry
            .0
            .lock()
            .unwrap()
            .get_mut(&vec![1; 32])
            .unwrap()
            .seen = std::time::Instant::now() - std::time::Duration::from_secs(601);
        assert!(registry.active().is_empty());
        assert_ne!(registry.topic(&[1; 32]), old);
    }
}
