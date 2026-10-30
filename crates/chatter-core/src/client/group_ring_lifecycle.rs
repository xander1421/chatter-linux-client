//! Cancellation of a pending channel ring uses the server's call identity.
//! Participant departure is insufficient: other members may still be active.

pub(super) fn snapshot_ends_ring(ringing_call: &str, active_call: Option<&str>) -> bool {
    active_call != Some(ringing_call)
}

pub(super) fn event_ends_ring(
    ringing_call: &str,
    ringing_channel: &str,
    event_call: &str,
    event_channel: &str,
    ended: bool,
) -> bool {
    ended && ringing_call == event_call && ringing_channel == event_channel
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_predicate_rejects_absent_or_replaced_call() {
        // Policy comparisons only; this does not schedule the async watcher.
        assert!(!snapshot_ends_ring("first", Some("first")));
        assert!(snapshot_ends_ring("first", None));
        // A new call in the same channel cannot keep the old ring alive.
        assert!(snapshot_ends_ring("first", Some("second")));
        assert!(!snapshot_ends_ring("second", Some("second")));
    }

    #[test]
    fn only_matching_authoritative_end_cancels_a_ring() {
        assert!(event_ends_ring("call", "room", "call", "room", true));
        // Late/reordered end of an older call cannot cancel the new ring.
        assert!(!event_ends_ring("new", "room", "old", "room", true));
        assert!(!event_ends_ring("call", "room", "call", "other", true));
        // A creator leaving a room with remaining participants is not an end.
        assert!(!event_ends_ring("call", "room", "call", "room", false));
    }
}
