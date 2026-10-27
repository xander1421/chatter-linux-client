//! Channel call events: the Call rows a group call leaves in its channel.
//!
//! A 1:1 call needs no message: each side files its own row from its own
//! orchestrator events (`call_history_recorder`). A channel call has members
//! who never join, so the caller posts STARTED and the last participant to
//! leave posts ENDED, end-to-end encrypted over the per-device sealed control
//! fan-out (`content_v1::CallEvent`). Every device files one row per call:
//! ENDED updates STARTED when present, or files a row if STARTED was missed.

use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::domain::message::{CallMessage, CallMessageType, Message, MessageContent};
use crate::events::DomainEvent;
use crate::proto::content_v1::{CallEvent, Content, call_event, content};

use super::{MessageError, MessageService};

/// Which end of a channel call an event reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelCallPhase {
    Started,
    Ended,
}

impl ChannelCallPhase {
    fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Ended => "ended",
        }
    }
}

/// The row id for one phase of one call in one channel.
fn row_id(channel: &ChannelId, call: uuid::Uuid, phase: ChannelCallPhase) -> MessageId {
    MessageId::from_uuid(uuid::Uuid::new_v5(
        &call,
        format!("{channel}:{}", phase.label()).as_bytes(),
    ))
}

impl MessageService {
    /// File or update the Call row for `phase` of a channel call, at most once, and
    /// show it. `sender` is who reported it: us, or a member of `channel` —
    /// anyone else's event is dropped (fail closed: no roster row, no row).
    /// An ENDED update keeps the STARTED row's caller and position.
    pub fn file_channel_call_event(
        &self,
        channel: &ChannelId,
        sender: &UserId,
        call_id: &str,
        phase: ChannelCallPhase,
        duration_secs: u32,
        sent_at: u64,
    ) -> Result<Option<Message>, MessageError> {
        let call: uuid::Uuid = call_id
            .parse()
            .map_err(|_| MessageError::InvalidContent("call id is not a UUID".into()))?;
        if *sender != self.current_user_id()?
            && self.channel_repo.get_membership(channel, sender)?.is_none()
        {
            tracing::warn!(%sender, %channel, "Call event from a non-member dropped");
            return Ok(None);
        }

        let started_id = row_id(channel, call, ChannelCallPhase::Started);
        let ended_id = row_id(channel, call, ChannelCallPhase::Ended);
        let started = self.message_repo.get(&started_id)?;
        if phase == ChannelCallPhase::Started {
            if started.is_some() || self.message_repo.get(&ended_id)?.is_some() {
                return Ok(None);
            }
        } else if let Some(mut row) = started {
            let MessageContent::Call(ref mut call_message) = row.content else {
                return Ok(None);
            };
            if call_message.call_type == CallMessageType::Ended {
                return Ok(None);
            }
            let at = crate::util::time::TimestampNanos::from_nanos(sent_at)
                .to_datetime()
                .unwrap_or_else(chrono::Utc::now);
            call_message.call_type = CallMessageType::Ended;
            call_message.ended_at = Some(at);
            call_message.duration = (duration_secs > 0)
                .then(|| chrono::Duration::seconds(i64::from(duration_secs)));
            self.message_repo.update(&row)?;
            self.event_bus.emit_domain(DomainEvent::MessageSnapshotChanged {
                message: row.clone(),
            });
            return Ok(Some(row));
        } else if self.message_repo.get(&ended_id)?.is_some() {
            return Ok(None);
        }
        let at = crate::util::time::TimestampNanos::from_nanos(sent_at)
            .to_datetime()
            .unwrap_or_else(chrono::Utc::now);

        let mut row = Message::new_text(
            *channel,
            self.conversation_type(channel),
            Some(*sender),
            String::new(),
            sent_at,
        );
        row.id = row_id(channel, call, phase);
        // History, not unread: a call record never badges (F73).
        row.is_read = true;
        row.content = MessageContent::Call(CallMessage {
            call_id: call_id.to_string(),
            call_type: match phase {
                ChannelCallPhase::Started => CallMessageType::Started,
                ChannelCallPhase::Ended => CallMessageType::Ended,
            },
            initiator_id: *sender,
            initiator_name: String::new(),
            started_at: (phase == ChannelCallPhase::Started).then_some(at),
            ended_at: (phase == ChannelCallPhase::Ended).then_some(at),
            duration: (duration_secs > 0)
                .then(|| chrono::Duration::seconds(i64::from(duration_secs))),
            participant_count: 0,
        });

        if !self.store_message_local_once(&row)? {
            return Ok(None);
        }
        self.event_bus.emit_domain(DomainEvent::MessageReceived {
            message: row.clone(),
        });
        Ok(Some(row))
    }

    /// Post `phase` of our channel call to every member device, end-to-end
    /// encrypted, and file our own row for it.
    pub async fn post_channel_call_event(
        &self,
        channel: &ChannelId,
        call_id: &str,
        phase: ChannelCallPhase,
        duration_secs: u32,
        is_video: bool,
    ) -> Result<(), MessageError> {
        let me = self.current_user_id()?;
        let now = crate::util::time::TimestampNanos::now().as_u64();
        self.file_channel_call_event(channel, &me, call_id, phase, duration_secs, now)?;
        self.send_control(
            channel,
            call_event_content(channel, call_id, phase, duration_secs, is_video, now),
            None,
        )
        .await
    }
}

/// The wire form of one channel call event.
pub(crate) fn call_event_content(
    channel: &ChannelId,
    call_id: &str,
    phase: ChannelCallPhase,
    duration_secs: u32,
    is_video: bool,
    timestamp: u64,
) -> Content {
    Content {
        timestamp,
        content: Some(content::Content::CallEvent(CallEvent {
            call_id: call_id.to_string(),
            channel_id: channel.to_string(),
            r#type: match phase {
                ChannelCallPhase::Started => call_event::Type::Started,
                ChannelCallPhase::Ended => call_event::Type::Ended,
            } as i32,
            duration_secs: if phase == ChannelCallPhase::Ended {
                duration_secs
            } else {
                0
            },
            is_video,
            timestamp,
        })),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::channel::{
        Channel, ChannelType, MemberRole, Membership, NotificationPreference,
    };
    use crate::inbox::InboxEvent;
    use crate::storage::repositories::settings_repo::keys;
    use std::sync::Arc;

    struct Fixture {
        client: Arc<crate::client::ChatterClient>,
        _dir: tempfile::TempDir,
        me: UserId,
        carol: UserId,
        channel: ChannelId,
    }

    /// We and Carol are members of `channel`; nobody else is.
    fn fixture() -> Fixture {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = crate::client::ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let (me, carol, channel) = (UserId::new(), UserId::new(), ChannelId::new());
        let svc = client.messages();
        svc.settings_repo
            .set(keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
        svc.channel_repo
            .insert(&Channel::new(channel, "g".into(), ChannelType::Private, me))
            .unwrap();
        for user_id in [me, carol] {
            svc.channel_repo
                .insert_membership(&Membership {
                    channel_id: channel,
                    user_id,
                    role: MemberRole::Member,
                    joined_at: chrono::Utc::now(),
                    last_read_message_id: None,
                    is_muted: false,
                    muted_until: None,
                    notification_preference: NotificationPreference::default(),
                })
                .unwrap();
        }
        Fixture {
            client,
            _dir: dir,
            me,
            carol,
            channel,
        }
    }

    fn call_of(m: &Message) -> &CallMessage {
        match &m.content {
            MessageContent::Call(c) => c,
            other => panic!("not a call row: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_members_call_updates_one_row_when_it_ends() {
        let f = fixture();
        let svc = f.client.messages();
        let call = uuid::Uuid::new_v4().to_string();

        let started = svc
            .file_channel_call_event(
                &f.channel,
                &f.carol,
                &call,
                ChannelCallPhase::Started,
                0,
                10,
            )
            .unwrap()
            .expect("first STARTED files a row");
        assert_eq!(started.conversation_id, f.channel);
        assert_eq!(call_of(&started).call_type, CallMessageType::Started);
        assert_eq!(call_of(&started).initiator_id, f.carol);
        assert!(started.is_read, "a call record never badges");
        // A resend, or the same event via another path, files nothing.
        assert!(
            svc.file_channel_call_event(
                &f.channel,
                &f.carol,
                &call,
                ChannelCallPhase::Started,
                0,
                11
            )
            .unwrap()
            .is_none()
        );

        // We were the last out: our ENDED keeps Carol as the caller.
        let ended = svc
            .file_channel_call_event(&f.channel, &f.me, &call, ChannelCallPhase::Ended, 95, 20)
            .unwrap()
            .expect("ENDED updates the STARTED row");
        assert_eq!(call_of(&ended).call_type, CallMessageType::Ended);
        assert_eq!(call_of(&ended).initiator_id, f.carol);
        assert_eq!(ended.id, started.id);
        assert_eq!(ended.sent_at, started.sent_at);
        assert_eq!(ended.sender_id, started.sender_id);
        assert_eq!(call_of(&ended).started_at, call_of(&started).started_at);
        assert!(call_of(&ended).ended_at.is_some());
        assert_eq!(
            call_of(&ended).duration,
            Some(chrono::Duration::seconds(95))
        );
        assert_eq!(
            svc.message_repo.get(&started.id).unwrap().unwrap().content,
            ended.content
        );
        assert!(svc
            .message_repo
            .get(&row_id(
                &f.channel,
                call.parse().unwrap(),
                ChannelCallPhase::Ended
            ))
            .unwrap()
            .is_none());
        // Carol's ENDED for the same call, arriving later, changes nothing.
        assert!(
            svc.file_channel_call_event(
                &f.channel,
                &f.carol,
                &call,
                ChannelCallPhase::Ended,
                90,
                21
            )
            .unwrap()
            .is_none()
        );
        assert!(svc
            .file_channel_call_event(&f.channel, &f.carol, &call, ChannelCallPhase::Started, 0, 22)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn ended_without_started_files_one_row_and_late_started_is_ignored() {
        let f = fixture();
        let svc = f.client.messages();
        let call = uuid::Uuid::new_v4().to_string();
        let ended = svc
            .file_channel_call_event(&f.channel, &f.carol, &call, ChannelCallPhase::Ended, 37, 20)
            .unwrap()
            .expect("ENDED files a row when STARTED was missed");
        assert_eq!(call_of(&ended).call_type, CallMessageType::Ended);
        assert!(call_of(&ended).started_at.is_none());
        assert!(svc
            .file_channel_call_event(&f.channel, &f.carol, &call, ChannelCallPhase::Started, 0, 10)
            .unwrap()
            .is_none());
        assert!(svc
            .file_channel_call_event(&f.channel, &f.carol, &call, ChannelCallPhase::Ended, 37, 20)
            .unwrap()
            .is_none());
        assert!(svc
            .message_repo
            .get(&row_id(
                &f.channel,
                call.parse().unwrap(),
                ChannelCallPhase::Started
            ))
            .unwrap()
            .is_none());
        assert_eq!(
            svc.message_repo.get(&ended.id).unwrap().unwrap().content,
            ended.content
        );
    }

    #[tokio::test]
    async fn a_non_members_call_event_is_dropped() {
        let f = fixture();
        let svc = f.client.messages();
        let outsider = UserId::new();
        let call = uuid::Uuid::new_v4().to_string();
        for phase in [ChannelCallPhase::Started, ChannelCallPhase::Ended] {
            assert!(
                svc.file_channel_call_event(&f.channel, &outsider, &call, phase, 5, 10)
                    .unwrap()
                    .is_none()
            );
        }
        assert!(
            svc.message_repo
                .get(&row_id(
                    &f.channel,
                    call.parse().unwrap(),
                    ChannelCallPhase::Started
                ))
                .unwrap()
                .is_none()
        );
        // Not a UUID: refused, not filed under some other id.
        assert!(
            svc.file_channel_call_event(
                &f.channel,
                &f.carol,
                "not-a-uuid",
                ChannelCallPhase::Started,
                0,
                10
            )
            .is_err()
        );
    }

    /// The wire form survives the control dispatch as the event the
    /// processor files.
    #[test]
    fn call_event_wire_form_reaches_the_inbox_as_a_channel_call_event() {
        let channel = ChannelId::new();
        let sender = UserId::new();
        let call = uuid::Uuid::new_v4().to_string();
        let content = call_event_content(&channel, &call, ChannelCallPhase::Ended, 42, true, 7);
        let (tx, mut rx) = tokio::sync::broadcast::channel(4);
        let stats = Arc::new(parking_lot::RwLock::new(Default::default()));
        crate::inbox::polling::device::dispatch_control_content(
            content.content.unwrap(),
            &sender,
            2,
            "guid",
            99,
            &tx,
            &stats,
        );
        match rx.try_recv().expect("an event") {
            InboxEvent::ChannelCallEvent {
                channel_id,
                sender_id,
                call_id,
                phase,
                duration_secs,
                timestamp,
            } => {
                assert_eq!((channel_id, sender_id), (channel, sender));
                assert_eq!(call_id, call);
                assert_eq!(phase, ChannelCallPhase::Ended);
                assert_eq!((duration_secs, timestamp), (42, 7));
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
}
