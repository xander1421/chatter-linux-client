//! Process-local, single-use notification capabilities. Never persisted or logged.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const ACCEPT: &str = "com.chatter.client.CALL_ACCEPT";
pub const OPEN: &str = "com.chatter.client.CALL_OPEN";
pub const FULL_SCREEN: &str = "com.chatter.client.CALL_FULL_SCREEN";
pub const DECLINE: &str = "com.chatter.client.CALL_DECLINE";
const MAX_CALLS: usize = 128;

struct Entry {
    call_id: String,
    expires: Instant,
    tokens: [Option<String>; 4],
}
#[derive(Default)]
pub struct Consent {
    entries: VecDeque<Entry>,
}
impl Consent {
    pub const fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }
    pub fn issue(&mut self, call_id: &str, now: Instant, lifetime: Duration) -> [String; 4] {
        self.prune(now);
        self.revoke(call_id);
        while self.entries.len() >= MAX_CALLS {
            self.entries.pop_front();
        }
        // rand's thread RNG is cryptographically secure and seeded from OS entropy.
        let tokens = std::array::from_fn(|_| {
            rand::random::<[u8; 32]>()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        });
        self.entries.push_back(Entry {
            call_id: call_id.to_owned(),
            expires: now + lifetime,
            tokens: tokens.clone().map(Some),
        });
        tokens
    }
    pub fn consume(&mut self, call_id: &str, action: &str, token: &str, now: Instant) -> bool {
        self.prune(now);
        let index = match action {
            ACCEPT => 0,
            OPEN => 1,
            FULL_SCREEN => 2,
            DECLINE => 3,
            _ => return false,
        };
        let Some(entry) = self.entries.iter_mut().find(|e| e.call_id == call_id) else {
            return false;
        };
        if token.is_empty() || entry.tokens[index].as_deref() != Some(token) {
            return false;
        }
        entry.tokens[index] = None;
        // Accept/Decline retire every sibling action. Body and full-screen each
        // get their own one-shot proof so auto-display does not consume a tap.
        if index == 0 || index == 3 {
            self.revoke(call_id);
        }
        true
    }
    pub fn revoke(&mut self, call_id: &str) {
        self.entries.retain(|e| e.call_id != call_id);
    }
    fn prune(&mut self, now: Instant) {
        self.entries.retain(|e| now < e.expires);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn valid_actions_are_bound_and_single_use() {
        let mut consent = Consent::new();
        let now = Instant::now();
        let a = consent.issue("a", now, Duration::from_secs(60));
        let b = consent.issue("b", now, Duration::from_secs(60));
        assert!(!consent.consume("", ACCEPT, &a[0], now));
        assert!(!consent.consume("a", ACCEPT, "", now));
        assert!(!consent.consume("a", ACCEPT, "forged", now));
        assert!(!consent.consume("a", "wrong", &a[0], now));
        assert!(!consent.consume("a", OPEN, &a[0], now));
        assert!(!consent.consume("b", ACCEPT, &a[0], now));
        assert!(consent.consume("a", FULL_SCREEN, &a[2], now));
        assert!(!consent.consume("a", FULL_SCREEN, &a[2], now));
        assert!(consent.consume("a", OPEN, &a[1], now));
        assert!(!consent.consume("a", OPEN, &a[1], now));
        assert!(consent.consume("a", ACCEPT, &a[0], now));
        assert!(!consent.consume("a", ACCEPT, &a[0], now));
        assert!(!consent.consume("a", DECLINE, &a[3], now));
        assert!(consent.consume("b", DECLINE, &b[3], now));
        assert!(!consent.consume("b", ACCEPT, &b[0], now));
    }
    #[test]
    fn replacement_cancellation_expiry_and_process_loss_fail_closed() {
        let mut consent = Consent::new();
        let now = Instant::now();
        let old = consent.issue("a", now, Duration::from_secs(60));
        let current = consent.issue("a", now, Duration::from_secs(60));
        assert!(!consent.consume("a", ACCEPT, &old[0], now));
        assert!(consent.consume("a", OPEN, &current[1], now));
        consent.revoke("a");
        assert!(!consent.consume("a", ACCEPT, &current[0], now));
        let expired = consent.issue("a", now, Duration::from_secs(60));
        assert!(!consent.consume("a", ACCEPT, &expired[0], now + Duration::from_secs(60)));
        let mut restarted = Consent::new();
        assert!(!restarted.consume("a", ACCEPT, &expired[0], now));
    }
    #[test]
    fn cleanup_is_bounded_without_a_background_task() {
        let mut consent = Consent::new();
        let now = Instant::now();
        let first = consent.issue("first", now, Duration::from_secs(60));
        for i in 0..MAX_CALLS * 2 {
            consent.issue(&i.to_string(), now, Duration::from_secs(60));
        }
        assert_eq!(consent.entries.len(), MAX_CALLS);
        assert!(!consent.consume("first", ACCEPT, &first[0], now));
        consent.prune(now + Duration::from_secs(60));
        assert!(consent.entries.is_empty());
    }
}
