//! Full call identities shared by Android notification and action delivery.

pub(super) fn tag(call_id: &str) -> String {
    format!("chatter.call:{call_id}")
}

/// A stable request code. Full call/action categories also participate in
/// PendingIntent identity, so a hash collision cannot retarget another call.
pub(super) fn request_code(call_id: &str) -> i32 {
    // FNV-1a → positive i32. Stable across show/cancel for the same call_id.
    let mut h: u32 = 2166136261;
    for b in call_id.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(16777619);
    }
    (h & 0x7fff_ffff) as i32
}

pub(super) fn category(call_id: &str, action: &str) -> String {
    // Call ids are authenticated UUIDs; delimiters cannot occur in them.
    format!("com.chatter.client.call.{call_id}:{action}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn colliding_request_codes_cannot_replace_or_cancel_other_call() {
        let a = "2165ee0a-3f69-8e4c-8d5d-80b00d8a46f5";
        let b = "345cffa2-e803-f012-23c8-54256d598597";
        assert_eq!(request_code(a), request_code(b));

        // Android NotificationManager keys by (tag, id), including cancel.
        let mut notifications = HashMap::new();
        notifications.insert((tag(a), request_code(a)), a);
        notifications.insert((tag(b), request_code(b)), b);
        assert_eq!(notifications.len(), 2);
        notifications.remove(&(tag(a), request_code(a)));
        assert_eq!(notifications.values().copied().collect::<Vec<_>>(), [b]);

        // Intent extras are ignored by PendingIntent identity. Even with
        // colliding request codes, the category preserves each call/action.
        for action in ["open", "accept", "com.chatter.client.CALL_DECLINE"] {
            assert_ne!(category(a, action), category(b, action));
        }
        assert_ne!(category(a, "open"), category(a, "accept"));
        assert_ne!(category(b, "open"), category(b, "accept"));
    }
}
