use super::*;

use chatter_mesh::ids::PeerId;

use crate::domain::message::MessageContent;
use crate::services::mesh::mapping::{self, InboundMessage};

fn peer(id: &str) -> PeerId {
    PeerId::from_str(id)
}

/// The defect this directory exists to fix.
///
/// A mesh DM's messages are ordinary rows and always survived a restart, but
/// the sidebar entry pointing at them did not — the conversation UUID is
/// derived one-way from the peer ID, so it cannot be recovered from the
/// stored messages. The history was on disk and unreachable, which reads as
/// lost data.
#[test]
fn a_stored_mesh_file_reads_back_the_way_the_ui_resolves_it() {
    // The UI does not read the attachment out of the message content. It takes
    // the ids from `Message::attachments` and resolves each one through the
    // attachment repository — `resolve_attachment` in the display converter —
    // and renders a card from the metadata it gets back. Anything that breaks
    // either half of that produces a bubble with a timestamp and no file in
    // it, while storage, decryption and the digest are all perfectly fine.
    //
    // So this asserts the shape the UI actually consumes: ids on the row, and
    // a repository row behind each id carrying the name and size a card needs.
    let timeline = temp_timeline();
    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("clip.bin");
    let bytes = vec![7u8; 4_096];
    std::fs::write(&source, &bytes).expect("write");

    let attachment = timeline
        .store_file_streamed(Some("clip.bin"), Some("application/octet-stream"), &source)
        .expect("stored");

    let peer = PeerId::from_str("dddddddddddddddd");
    let conversation = MeshConversation::Room;
    let message = crate::services::mesh::mapping::inbound_file_message(
        &conversation,
        "WIRE-FILE",
        &peer,
        attachment,
        1_700_000_000_000,
    );
    timeline
        .record(&MeshMessage {
            message,
            conversation: conversation.clone(),
            sender: peer.clone(),
            nickname: "anon".to_string(),
            is_local: false,
            sender_verified: true,
        })
        .expect("record");

    let history = timeline.history(&conversation, None, 50).expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].attachments,
        vec![attachment],
        "the UI resolves ids from this field, not from the message content"
    );

    // And the id has to lead somewhere: an unresolvable id renders as nothing.
    let stored = timeline
        .attachments
        .get(&attachment)
        .expect("query")
        .expect("the row the card is built from must exist");
    assert_eq!(stored.metadata.name, "clip.bin");
    assert_eq!(
        stored.metadata.size, 4_096,
        "the card shows the plaintext size, not the ciphertext size"
    );
}

#[test]
fn two_mesh_messages_in_the_same_millisecond_are_both_stored() {
    // The mesh identifies a message by its wire ID, and nothing else. The
    // server path also treats `(sender, sent_at)` as an identity, via a
    // unique index — and `sent_at` here is milliseconds widened to
    // nanoseconds, so under that rule any two mesh messages from one sender
    // to one conversation inside the same millisecond were the same row and
    // the second was dropped by `INSERT OR IGNORE`. Silently: the mesh has no
    // server echo, so that row was the only copy of the message.
    //
    // Two distinct wire IDs, one timestamp, deliberately.
    let timeline = temp_timeline();
    let peer = PeerId::from_str("cccccccccccccccc");
    let conversation = MeshConversation::Dm(peer.clone());
    let at_ms = 1_700_000_000_123_u64;

    for wire_id in ["WIRE-ONE", "WIRE-TWO"] {
        let message = crate::services::mesh::mapping::outbound_message(
            &conversation,
            wire_id,
            &peer,
            "hello",
            at_ms,
        );
        let stored = timeline
            .record(&MeshMessage {
                message,
                conversation: conversation.clone(),
                sender: peer.clone(),
                nickname: "anon".to_string(),
                is_local: true,
                sender_verified: true,
            })
            .expect("record");
        assert!(stored, "{wire_id} is a distinct message and must be stored");
    }

    let history = timeline.history(&conversation, None, 50).expect("history");
    assert_eq!(
        history.len(),
        2,
        "two wire IDs are two messages, however close together they were sent"
    );
}

#[test]
fn a_room_sender_is_still_named_after_a_restart() {
    // The S9 report: bitchat's public message rendered as "Unknown peer"
    // because only a peer still in range could be named, and the stored row
    // keeps nothing but a hash of the peer ID.
    use crate::storage::Database;

    let dir = tempfile::tempdir().expect("temp dir");
    let db = Arc::new(Database::open(&dir.path().join("names.db"), "testpass").expect("open"));
    db.run_migrations().expect("migrations");
    let messages = Arc::new(MessageRepository::new(db.clone()));
    let directory = Arc::new(MeshConversationRepository::new(db.clone()));
    let deliveries = Arc::new(MeshDeliveryRepository::new(db.clone()));
    let attachments = Arc::new(AttachmentRepository::new(db.clone()));
    let sender = peer("4f74eb3eb7441921");
    let event = inbound(
        "w1",
        MeshConversation::Room,
        &sender,
        "anon7431",
        "hi",
        1_000,
    );
    let sender_id = event.message.sender_id.clone().expect("sender");

    MeshTimeline::new(
        messages.clone(),
        directory.clone(),
        deliveries.clone(),
        attachments.clone(),
    )
    .record(&event)
    .expect("record");

    let restarted = MeshTimeline::new(messages, directory, deliveries, attachments);
    assert_eq!(
        restarted.remembered_sender_name(&sender_id).as_deref(),
        Some("anon7431")
    );
}

#[test]
fn a_dm_conversation_survives_a_restart() {
    use crate::storage::Database;

    let dir = tempfile::tempdir().expect("temp dir");
    let db = Arc::new(Database::open(&dir.path().join("restart.db"), "testpass").expect("open"));
    db.run_migrations().expect("migrations");

    let messages = Arc::new(MessageRepository::new(db.clone()));
    let directory = Arc::new(MeshConversationRepository::new(db.clone()));
    let deliveries = Arc::new(MeshDeliveryRepository::new(db.clone()));
    let attachments = Arc::new(AttachmentRepository::new(db.clone()));
    let conversation = MeshConversation::Dm(peer("00112233445566778899aabbccddeeff"));

    {
        let timeline = MeshTimeline::new(
            messages.clone(),
            directory.clone(),
            deliveries.clone(),
            attachments.clone(),
        );
        timeline.register(&conversation, Some("alice"));
        assert!(
            timeline
                .conversation_for(&conversation.conversation_id())
                .is_some(),
            "the DM should be open in the session that created it"
        );
    }

    // A second timeline over the same database is what a restart looks like.
    let restarted = MeshTimeline::new(messages, directory, deliveries, attachments);
    let recovered = restarted
        .conversation_for(&conversation.conversation_id())
        .expect("the DM must still be reachable after a restart");
    assert_eq!(recovered, conversation);

    let listed = restarted.conversations().expect("conversations");
    assert!(
        listed.iter().any(|c| c.name == "alice"),
        "the peer's name should come back too, not just the row"
    );
}

#[test]
fn a_delivery_state_survives_a_restart_and_never_reads_as_sent() {
    // The failure this pins: mesh delivery used to live only on the event
    // stream, so a message that was *never delivered* came back after a
    // reopen rendered as an ordinary sent message — the converter turns an
    // empty status on an own message into "sent". A mesh surface claiming an
    // arrival nobody confirmed is the one thing it must never do.
    use crate::storage::Database;

    let dir = tempfile::tempdir().expect("temp dir");
    let db = Arc::new(Database::open(&dir.path().join("delivery.db"), "testpass").expect("open"));
    db.run_migrations().expect("migrations");

    let messages = Arc::new(MessageRepository::new(db.clone()));
    let directory = Arc::new(MeshConversationRepository::new(db.clone()));
    let deliveries = Arc::new(MeshDeliveryRepository::new(db.clone()));
    let attachments = Arc::new(AttachmentRepository::new(db.clone()));
    let conversation = MeshConversation::Dm(peer("00112233445566778899aabbccddeeff"));

    {
        let timeline = MeshTimeline::new(
            messages.clone(),
            directory.clone(),
            deliveries.clone(),
            attachments.clone(),
        );
        let message = MeshMessage {
            message: crate::services::mesh::mapping::outbound_message(
                &conversation,
                "WIRE-1",
                &peer("00112233445566778899aabbccddeeff"),
                "did this arrive?",
                1_700_000_000_000,
            ),
            conversation: conversation.clone(),
            sender: peer("00112233445566778899aabbccddeeff"),
            nickname: String::new(),
            is_local: true,
            sender_verified: false,
        };
        timeline.record(&message).expect("record");
        timeline.record_delivery("WIRE-1", MeshDelivery::Dropped);
    }

    let restarted = MeshTimeline::new(messages, directory, deliveries, attachments);
    let history = restarted.history(&conversation, None, 10).expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].delivery_status,
        MeshDelivery::Dropped.as_str(),
        "a message the mesh dropped must still say so after a restart"
    );
    assert_ne!(history[0].delivery_status, "sent");
}

/// The case a naive panic wipe misses.
///
/// Leaving a cell keeps its history by design, so a wipe that walks the open
/// sidebar spares everything said in every cell the user ever left — while
/// telling them it worked.
#[test]
fn a_wipe_destroys_history_from_cells_that_were_already_left() {
    let timeline = temp_timeline();

    timeline.note_geohash("dr5rsj7");
    let cell = MeshConversation::Geohash("dr5rsj7".to_string());
    timeline
        .record(&inbound(
            "wire-cell-1",
            cell.clone(),
            &peer("aabbccddeeff00112233445566778899"),
            "stranger",
            "in the square",
            1_700_000_000_000,
        ))
        .expect("record");
    timeline.forget_geohash("dr5rsj7");

    // Gone from the sidebar, still on disk — that is the design.
    assert!(timeline.conversation_for(&cell.conversation_id()).is_none());
    assert_eq!(
        timeline.history(&cell, None, 50).expect("history").len(),
        1,
        "leaving a cell must keep its history, or this test proves nothing"
    );

    timeline.wipe().expect("wipe");

    assert!(
        timeline
            .history(&cell, None, 50)
            .expect("history")
            .is_empty(),
        "a panic wipe must reach history the sidebar can no longer see"
    );
}

/// A wipe leaves the room present but empty: it is not something you join,
/// so a running radio is always in it.
#[test]
fn a_wipe_destroys_the_bytes_of_a_received_file_not_only_its_row() {
    // A mesh file's bytes live outside the database. Deleting the row
    // crypto-shreds them — the per-file key is only in that row — but a panic
    // wipe is not only about contents: a megabyte left on disk is proof that
    // something was received, which is exactly what the user pressed this
    // button to destroy. So the file has to go too.
    let timeline = temp_timeline();
    let room = MeshConversation::Room;

    let attachment = timeline
        .store_file(
            Some("secret.bin"),
            Some("application/octet-stream"),
            b"payload",
        )
        .expect("stores");
    let path = crate::services::attachment::attachment_file_path(&attachment);
    assert!(path.exists(), "the fixture must really have written a file");

    // A message that names it, so the wipe can find it.
    let sender = peer("00112233445566778899aabbccddeeff");
    let message = MeshMessage {
        message: crate::services::mesh::mapping::outbound_file_message(
            &room,
            "WIRE-FILE",
            &sender,
            attachment,
            1_700_000_000_000,
        ),
        conversation: room.clone(),
        sender,
        nickname: String::new(),
        is_local: true,
        sender_verified: false,
    };
    timeline.record(&message).expect("record");

    timeline.wipe().expect("wipe");

    assert!(
        !path.exists(),
        "the file must be gone, not merely unreadable"
    );
    assert!(
        timeline
            .history(&room, None, 10)
            .expect("history")
            .is_empty(),
        "and the message with it"
    );
}

#[test]
fn a_wipe_destroys_partial_chunked_transfers_too() {
    // The most exposed thing the wipe has to reach. A finished attachment is
    // encrypted under a key held only in its row, so deleting the row alone
    // would at least crypto-shred it — a *partial* transfer has no row and is
    // plaintext on disk, so nothing but an unlink destroys it.
    let timeline = temp_timeline();
    super::super::file_stream::ensure_transfers_dir().expect("spool dir");
    let leftover = super::super::file_stream::transfers_dir().join("deadbeefdeadbeef.part");
    std::fs::write(&leftover, b"half of somebody's file").expect("write");

    timeline.wipe().expect("wipe");

    assert!(
        !leftover.exists(),
        "a partial transfer is unencrypted proof that something was received"
    );
}

#[test]
fn a_wipe_empties_everything_and_keeps_only_the_empty_room() {
    let timeline = temp_timeline();
    let room = MeshConversation::Room;
    let dm = MeshConversation::Dm(peer("00112233445566778899aabbccddeeff"));

    timeline
        .record(&inbound(
            "wire-room-1",
            room.clone(),
            &peer("aabbccddeeff00112233445566778899"),
            "stranger",
            "hello room",
            1_700_000_000_000,
        ))
        .expect("record");
    timeline
        .record(&inbound(
            "wire-dm-1",
            dm.clone(),
            &peer("00112233445566778899aabbccddeeff"),
            "alice",
            "hello dm",
            1_700_000_000_001,
        ))
        .expect("record");
    timeline.note_geohash("u4pruy");

    timeline.wipe().expect("wipe");

    assert!(
        timeline
            .history(&room, None, 50)
            .expect("history")
            .is_empty()
    );
    assert!(timeline.history(&dm, None, 50).expect("history").is_empty());
    assert!(
        timeline.conversation_for(&dm.conversation_id()).is_none(),
        "the DM must be gone from the directory too"
    );

    let listed = timeline.conversations().expect("conversations");
    assert_eq!(listed.len(), 1, "only the room should remain");
}

/// Leaving a cell forgets it durably, rather than only for this session.
#[test]
fn a_left_cell_stays_left_across_a_restart() {
    use crate::storage::Database;

    let dir = tempfile::tempdir().expect("temp dir");
    let db = Arc::new(Database::open(&dir.path().join("left.db"), "testpass").expect("open"));
    db.run_migrations().expect("migrations");

    let messages = Arc::new(MessageRepository::new(db.clone()));
    let directory = Arc::new(MeshConversationRepository::new(db.clone()));
    let deliveries = Arc::new(MeshDeliveryRepository::new(db.clone()));
    let attachments = Arc::new(AttachmentRepository::new(db.clone()));

    {
        let timeline = MeshTimeline::new(
            messages.clone(),
            directory.clone(),
            deliveries.clone(),
            attachments.clone(),
        );
        timeline.note_geohash("dr5rsj7");
        timeline.forget_geohash("dr5rsj7");
    }

    let restarted = MeshTimeline::new(messages, directory, deliveries, attachments);
    let cell = MeshConversation::Geohash("dr5rsj7".to_string());
    assert!(
        restarted
            .conversation_for(&cell.conversation_id())
            .is_none(),
        "a cell that was left must not come back on restart"
    );
}

/// One inbound message, as a runtime would surface it.
fn inbound(
    wire_id: &str,
    conversation: MeshConversation,
    sender: &PeerId,
    nickname: &str,
    content: &str,
    timestamp_ms: u64,
) -> MeshMessage {
    let decoded = InboundMessage {
        wire_id: wire_id.to_string(),
        conversation: conversation.clone(),
        sender: sender.clone(),
        nickname: nickname.to_string(),
        content: content.to_string(),
        timestamp_ms,
    };
    MeshMessage {
        message: decoded.to_domain(),
        conversation,
        sender: sender.clone(),
        nickname: nickname.to_string(),
        is_local: false,
        sender_verified: false,
    }
}

/// One message this device authored.
fn outbound(
    wire_id: &str,
    conversation: MeshConversation,
    local: &PeerId,
    content: &str,
    timestamp_ms: u64,
) -> MeshMessage {
    MeshMessage {
        message: mapping::outbound_message(&conversation, wire_id, local, content, timestamp_ms),
        conversation,
        sender: local.clone(),
        nickname: "me".to_string(),
        is_local: true,
        sender_verified: true,
    }
}

fn texts(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn an_inbound_message_is_stored_and_reads_back() {
    let timeline = temp_timeline();
    let sender = peer("abcdef0123456789");
    let message = inbound(
        "MSG-1",
        MeshConversation::Room,
        &sender,
        "wanderer",
        "hello room",
        1_700_000_000_000,
    );

    assert!(timeline.record(&message).expect("stored"));

    let history = timeline
        .history(&MeshConversation::Room, None, 50)
        .expect("history");
    assert_eq!(texts(&history), vec!["hello room"]);
    assert_eq!(history[0].conversation_type, ConversationType::Mesh);
    assert_eq!(
        history[0].sender_id,
        Some(mapping::peer_user_id(&sender)),
        "a mesh sender keeps its derived id; None would mean sealed sender"
    );
    assert!(!history[0].is_read, "someone else's message arrives unread");
}

#[test]
fn the_same_message_arriving_three_ways_is_stored_once() {
    // The mesh delivers redundantly on purpose: over the radio, from a
    // courier, and from a relay drop. Dedup by message ID is what makes that
    // redundancy free.
    let timeline = temp_timeline();
    let sender = peer("abcdef0123456789");
    let message = inbound(
        "MSG-DUP",
        MeshConversation::Dm(sender.clone()),
        &sender,
        "wanderer",
        "are you there",
        1_700_000_000_000,
    );

    assert!(timeline.record(&message).expect("first"));
    assert!(
        !timeline.record(&message).expect("second"),
        "already stored"
    );
    assert!(!timeline.record(&message).expect("third"), "still stored");

    let history = timeline
        .history(&MeshConversation::Dm(sender.clone()), None, 50)
        .expect("history");
    assert_eq!(history.len(), 1, "one row, not three bubbles");
}

#[test]
fn a_sent_message_is_readable_immediately_and_counts_as_read() {
    let timeline = temp_timeline();
    let local = peer("0123456789abcdef");
    let sent = outbound(
        "MSG-OUT",
        MeshConversation::Room,
        &local,
        "anyone out there",
        1_700_000_000_000,
    );

    assert!(timeline.record(&sent).expect("stored"));

    let history = timeline
        .history(&MeshConversation::Room, None, 50)
        .expect("history");
    assert_eq!(texts(&history), vec!["anyone out there"]);
    assert!(
        history[0].is_read,
        "our own message must not badge the conversation we just typed in"
    );

    let room = timeline
        .conversations()
        .expect("conversations")
        .into_iter()
        .find(|c| c.conversation_type == ConversationType::Mesh)
        .expect("the room is there");
    assert_eq!(room.unread_count, 0);
    assert!(room.last_message_at.is_some());
}

#[test]
fn the_room_a_cell_and_a_dm_all_surface_with_the_right_type() {
    let timeline = temp_timeline();
    let sender = peer("abcdef0123456789");
    timeline.note_geohash("u4pruy");
    timeline
        .record(&inbound(
            "MSG-DM",
            MeshConversation::Dm(sender.clone()),
            &sender,
            "wanderer",
            "hi",
            1_700_000_000_000,
        ))
        .expect("stored");

    let conversations = timeline.conversations().expect("conversations");
    assert_eq!(conversations.len(), 3, "room, cell, dm");

    let room = conversations
        .iter()
        .find(|c| c.name == "#mesh")
        .expect("the public room");
    assert_eq!(room.conversation_type, ConversationType::Mesh);
    assert_eq!(
        room.id,
        MeshConversation::Room.conversation_id().to_string()
    );

    let cell = conversations
        .iter()
        .find(|c| c.name == "#u4pruy")
        .expect("the joined cell");
    assert_eq!(cell.conversation_type, ConversationType::Mesh);
    assert!(
        cell.last_message_at.is_none(),
        "a joined cell with no traffic is still a conversation"
    );

    let dm = conversations
        .iter()
        .find(|c| c.conversation_type == ConversationType::MeshDm)
        .expect("the dm");
    assert_eq!(dm.name, "wanderer", "named by what the peer calls itself");
    assert_eq!(dm.unread_count, 1);

    // Every one of them answers the serverless predicate, so anything keyed
    // on "the server knows about this conversation" skips all three.
    assert!(conversations.iter().all(|c| c.conversation_type.is_mesh()));
}

#[test]
fn a_mesh_conversation_is_never_given_server_side_identifiers() {
    let timeline = temp_timeline();
    let sender = peer("abcdef0123456789");
    timeline
        .record(&inbound(
            "MSG-1",
            MeshConversation::Dm(sender.clone()),
            &sender,
            "wanderer",
            "hi",
            1_700_000_000_000,
        ))
        .expect("stored");

    for conversation in timeline.conversations().expect("conversations") {
        assert!(
            conversation.owner_id.is_none(),
            "no server account owns a mesh conversation"
        );
        assert!(
            conversation.my_role.is_empty(),
            "roles are a server-side authorization concept"
        );
        assert!(!conversation.archived);
        assert_eq!(conversation.status, STATUS_ACTIVE);
    }
}

#[test]
fn a_dm_is_never_labelled_with_our_own_nickname() {
    // The outbound mirror carries *our* nickname. Naming the conversation
    // from it would label every DM after ourselves.
    let timeline = temp_timeline();
    let remote = peer("abcdef0123456789");
    let local = peer("0123456789abcdef");
    // `outbound` carries our own nickname, "me", exactly as the send path does.
    let sent = outbound(
        "MSG-OUT",
        MeshConversation::Dm(remote.clone()),
        &local,
        "first contact",
        1_700_000_000_000,
    );
    timeline.record(&sent).expect("stored");

    let dm = timeline
        .conversations()
        .expect("conversations")
        .into_iter()
        .find(|c| c.conversation_type == ConversationType::MeshDm)
        .expect("the dm");
    assert_eq!(
        dm.name,
        remote.as_str(),
        "the peer, until they name themselves"
    );

    // Their reply names it.
    timeline
        .record(&inbound(
            "MSG-IN",
            MeshConversation::Dm(remote.clone()),
            &remote,
            "wanderer",
            "hello",
            1_700_000_000_001,
        ))
        .expect("stored");
    let dm = timeline
        .conversations()
        .expect("conversations")
        .into_iter()
        .find(|c| c.conversation_type == ConversationType::MeshDm)
        .expect("the dm");
    assert_eq!(dm.name, "wanderer");
}

#[test]
fn leaving_a_cell_takes_it_off_the_sidebar_and_leaves_its_history_alone() {
    let timeline = temp_timeline();
    let sender = peer("abcdef0123456789");
    let cell = MeshConversation::Geohash("u4pruy".to_string());
    timeline.note_geohash("u4pruy");
    timeline
        .record(&inbound(
            "MSG-GEO",
            cell.clone(),
            &sender,
            "wanderer",
            "anyone in this cell",
            1_700_000_000_000,
        ))
        .expect("stored");

    timeline.forget_geohash("u4pruy");
    let conversations = timeline.conversations().expect("conversations");
    assert!(
        !conversations.iter().any(|c| c.name == "#u4pruy"),
        "a cell you left is not a conversation you are in"
    );

    let history = timeline.history(&cell, None, 50).expect("history");
    assert_eq!(
        texts(&history),
        vec!["anyone in this cell"],
        "leaving a cell is not deleting what was said in it"
    );
}

#[test]
fn a_sidebar_row_maps_back_to_something_sendable() {
    let timeline = temp_timeline();
    let sender = peer("abcdef0123456789");
    timeline
        .record(&inbound(
            "MSG-1",
            MeshConversation::Dm(sender.clone()),
            &sender,
            "wanderer",
            "hi",
            1_700_000_000_000,
        ))
        .expect("stored");

    let dm = timeline
        .conversations()
        .expect("conversations")
        .into_iter()
        .find(|c| c.conversation_type == ConversationType::MeshDm)
        .expect("the dm");
    let id: ChannelId = dm.id.parse().expect("the row id is a uuid");

    assert_eq!(
        timeline.conversation_for(&id),
        Some(MeshConversation::Dm(sender)),
        "clicking the row has to yield something a send can address"
    );
    assert!(
        timeline.conversation_for(&ChannelId::new()).is_none(),
        "an id from some other conversation type is not ours to answer for"
    );
}

#[test]
fn reading_a_conversation_clears_its_unread_count() {
    let timeline = temp_timeline();
    let sender = peer("abcdef0123456789");
    let dm = MeshConversation::Dm(sender.clone());
    for (index, wire_id) in ["MSG-1", "MSG-2"].iter().enumerate() {
        timeline
            .record(&inbound(
                wire_id,
                dm.clone(),
                &sender,
                "wanderer",
                "hi",
                1_700_000_000_000 + index as u64,
            ))
            .expect("stored");
    }

    let unread = timeline
        .conversations()
        .expect("conversations")
        .into_iter()
        .find(|c| c.conversation_type == ConversationType::MeshDm)
        .expect("the dm")
        .unread_count;
    assert_eq!(unread, 2);

    timeline.mark_read(&dm).expect("mark read");
    let read = timeline
        .conversations()
        .expect("conversations")
        .into_iter()
        .find(|c| c.conversation_type == ConversationType::MeshDm)
        .expect("the dm")
        .unread_count;
    assert_eq!(read, 0);
}

#[test]
fn conversations_are_ordered_most_recent_first_with_silent_ones_last() {
    let timeline = temp_timeline();
    let sender = peer("abcdef0123456789");
    timeline.note_geohash("u4pruy");
    timeline
        .record(&inbound(
            "MSG-OLD",
            MeshConversation::Room,
            &sender,
            "wanderer",
            "old",
            1_700_000_000_000,
        ))
        .expect("stored");
    timeline
        .record(&inbound(
            "MSG-NEW",
            MeshConversation::Dm(sender.clone()),
            &sender,
            "wanderer",
            "new",
            1_700_000_100_000,
        ))
        .expect("stored");

    let names: Vec<String> = timeline
        .conversations()
        .expect("conversations")
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, vec!["wanderer", "#mesh", "#u4pruy"]);
}

#[test]
fn a_message_still_reaches_subscribers_and_the_store_through_the_fan_out() {
    let events = MeshEvents::new(8, temp_timeline());
    let mut subscriber = events.subscribe();
    let sender = peer("abcdef0123456789");

    events.send(MeshEvent::Message(Box::new(inbound(
        "MSG-1",
        MeshConversation::Room,
        &sender,
        "wanderer",
        "hello room",
        1_700_000_000_000,
    ))));

    match subscriber.try_recv() {
        Ok(MeshEvent::Message(message)) => {
            assert_eq!(mapping::text_of(&message.message), Some("hello room"));
        }
        other => panic!("expected the message, got {other:?}"),
    }
}

#[test]
fn joining_and_leaving_a_cell_through_the_fan_out_moves_the_sidebar() {
    let timeline = temp_timeline();
    let events = MeshEvents::new(8, Arc::clone(&timeline));

    events.send(MeshEvent::GeohashJoined("u4pruy".to_string()));
    assert!(
        timeline
            .conversations()
            .expect("conversations")
            .iter()
            .any(|c| c.name == "#u4pruy")
    );

    events.send(MeshEvent::GeohashLeft("u4pruy".to_string()));
    assert!(
        !timeline
            .conversations()
            .expect("conversations")
            .iter()
            .any(|c| c.name == "#u4pruy")
    );
}
