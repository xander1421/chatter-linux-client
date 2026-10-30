use super::*;

/// An unanswered resend used to hold the channel's sync checkpoint
/// forever ("message ingestion must be retried" on every sync). The gate
/// must report enough failures to give up within a bounded schedule.
#[test]
fn decrypt_failure_gate_reaches_give_up_on_a_bounded_schedule() {
    let mut gate = DecryptFailureGate::new();
    let key = (ChannelId::new(), UserId::new(), 1, [7u8; 32]);
    let mut now = std::time::Instant::now();
    let start = now;
    let mut attempts = 0;
    while attempts < RESEND_GIVE_UP_ATTEMPTS {
        attempts = record_decrypt_failure(&mut gate, key, now);
        // The next attempt is allowed once the gate's deadline passes.
        now = gate[&key].0;
    }
    assert_eq!(attempts, RESEND_GIVE_UP_ATTEMPTS);
    assert!(now - start <= std::time::Duration::from_secs(30 * 60));
}

#[test]
fn a_new_edit_is_not_a_duplicate_of_the_original() {
    let mut original = Message::new_text(
        ChannelId::new(),
        ConversationType::Channel,
        None,
        "original".into(),
        1_000_000,
    );
    let edit = chat_v1::Message {
        edited: true,
        edited_at: 2_000_000,
        ..Default::default()
    };
    assert!(is_newer_edit(&edit, &original));
    original.edited_at = Some(chrono::DateTime::from_timestamp_nanos(2_000_000));
    assert!(!is_newer_edit(&edit, &original));
    let older = chat_v1::Message {
        edited_at: 1_000_000,
        ..edit.clone()
    };
    assert!(!is_newer_edit(&older, &original));
    assert!(!is_newer_edit(&chat_v1::Message::default(), &original));
}
