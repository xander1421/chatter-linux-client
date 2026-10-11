//! Channel call events: the Call rows a group call leaves in its channel.
//!
//! A 1:1 call needs no message: each side files its own row from its own
//! orchestrator events (`call_history_recorder`). A channel call has members
//! who never join, so the caller posts STARTED and the last participant to
//! leave posts ENDED, end-to-end encrypted over the per-device sealed control
//! fan-out (`content_v1::CallEvent`). Every device files one row per phase:
//! its id is derived from (channel, call, phase), so a repeat, a resend or our
//! own local copy files nothing twice.

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
    /// File the Call row for `phase` of a channel call, at most once, and
    /// show it. `sender` is who reported it: us, or a member of `channel` —
    /// anyone else's event is dropped (fail closed: no roster row, no row).
    /// An ENDED row keeps the caller from the STARTED row when we have it, so
    /// both read the same direction.
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

        let initiator = match phase {
            ChannelCallPhase::Started => *sender,
            ChannelCallPhase::Ended => self
                .message_repo
                .get(&row_id(channel, call, ChannelCallPhase::Started))?
                .and_then(|m| match m.content {
                    MessageContent::Call(c) => Some(c.initiator_id),
                    _ => None,
                })
                .unwrap_or(*sender),
        };
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
            initiator_id: initiator,
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
    async fn a_members_call_files_one_started_and_one_ended_row() {
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
            .expect("ENDED files its own row");
        assert_eq!(call_of(&ended).call_type, CallMessageType::Ended);
        assert_eq!(call_of(&ended).initiator_id, f.carol);
        assert_eq!(
            call_of(&ended).duration,
            Some(chrono::Duration::seconds(95))
        );
        assert_ne!(ended.id, started.id);
        // Carol's ENDED for the same call, arriving later, is the same row.
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
