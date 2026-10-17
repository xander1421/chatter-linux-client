//! Awaited application and bounded replay for secret data posts.
use std::sync::Arc;

use crate::domain::{ChannelId, UserId};
use crate::events::EventBus;
use crate::inbox::handlers::DomainEvent;
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
use crate::storage::receive_artifact::{ReceiveArtifact, ReceiveKind};
use crate::storage::{Database, MessageRepository, SettingsRepository, UserRepository};

#[derive(Clone)]
pub struct ContentHandoff {
    pub dm: Arc<AnonDmService>,
    pub group: Arc<AnonGroupService>,
    pub events: EventBus,
    pub database: Arc<Database>,
}

impl ContentHandoff {
    fn retire_dropped(
        repo: &Arc<MessageRepository>,
        artifact: &ReceiveArtifact,
        reason: &'static str,
    ) -> Result<Option<(crate::domain::anon_dm::AnonDmId, u64)>, String> {
        tracing::warn!(envelope_id = %artifact.key.envelope_id, reason, "Secret data post rejected");
        repo.finish_content_artifact(artifact)
            .map_err(|error| error.to_string())?;
        tracing::info!(envelope_id = %artifact.key.envelope_id, "secret receive artifact applied");
        Ok(None)
    }

    pub(crate) async fn apply(
        &self,
        repo: &Arc<MessageRepository>,
        artifact: &ReceiveArtifact,
    ) -> Result<(), String> {
        let this = self.clone();
        let repo = repo.clone();
        let artifact = artifact.clone();
        let receipt = tokio::task::spawn_blocking(move || this.apply_blocking(&repo, &artifact))
            .await
            .map_err(|error| error.to_string())??;
        if let Some((id, timestamp)) = receipt {
            let dm = self.dm.clone();
            tokio::spawn(async move {
                if let Err(error) = dm.send_delivery_receipt(&id, vec![timestamp]).await {
                    tracing::warn!(%error, "Secret DM delivery receipt failed");
                }
            });
        }
        Ok(())
    }

    fn apply_blocking(
        &self,
        repo: &Arc<MessageRepository>,
        artifact: &ReceiveArtifact,
    ) -> Result<Option<(crate::domain::anon_dm::AnonDmId, u64)>, String> {
        use crate::proto::content_v1::content::Content as Body;
        if artifact.kind != ReceiveKind::Content || artifact.applied {
            return Err("expected pending secret data artifact".into());
        }
        let sender: UserId = artifact.sender.parse().map_err(|_| "invalid sender")?;
        let users = UserRepository::new(self.database.clone());
        let settings = SettingsRepository::new(self.database.clone());
        if crate::services::gateway::block_filter::sender_is_blocked(&users, &settings, &sender)
            .map_err(|error| error.to_string())?
        {
            return Self::retire_dropped(repo, artifact, "blocked sender");
        }
        let scope: ChannelId = artifact.key.scope.parse().map_err(|_| "invalid scope")?;
        let content = crate::network::validation::protovalidate::decode_inbound_content(
            &artifact.content,
            "secret_data_journal",
        )
        .map_err(|error| error.to_string())?;
        let mut receipt = None;
        let events = match (artifact.key.transport.as_str(), content.content) {
            ("anon_dm", Some(Body::AnonDmMessage(wrapper))) => {
                let id = crate::domain::anon_dm::AnonDmId::try_from_slice(&wrapper.conversation_id)
                    .ok_or("invalid DM destination")?;
                if ChannelId::from(id) != scope {
                    return Self::retire_dropped(
                        repo,
                        artifact,
                        "DM destination differs from mailbox",
                    );
                }
                let dm = self
                    .dm
                    .anon_dm_repo()
                    .get_conversation(&id)
                    .map_err(|error| error.to_string())?
                    .ok_or("DM conversation unavailable")?;
                if dm.peer_id != sender || dm.peer_device_id != Some(artifact.sender_device) {
                    return Self::retire_dropped(repo, artifact, "DM sender is not bound peer");
                }
                if !dm.is_ready() {
                    return Err("DM acceptance pending".into());
                }
                if self
                    .dm
                    .anon_dm_repo()
                    .is_user_blocked(&sender)
                    .map_err(|error| error.to_string())?
                {
                    Vec::new()
                } else {
                    let inner = wrapper.inner_content.ok_or("missing DM post")?;
                    if !matches!(inner.content, Some(Body::DataMessage(_))) {
                        return Err("non-post DM artifact".into());
                    }
                    let events = self
                        .dm
                        .process_incoming_content(&id, &sender, &inner)
                        .map_err(|error| error.to_string())?;
                    if events
                        .iter()
                        .any(|event| matches!(event, DomainEvent::MessageReceived { .. }))
                    {
                        receipt = Some((id, inner.timestamp));
                    }
                    events
                }
            }
            ("anon_group", Some(Body::AnonGroupMessage(wrapper))) => {
                let id = crate::domain::anon_group::AnonGroupId::try_from_slice(&wrapper.group_id)
                    .ok_or("invalid group destination")?;
                if ChannelId::from(id) != scope {
                    return Self::retire_dropped(
                        repo,
                        artifact,
                        "group destination differs from mailbox",
                    );
                }
                let inner = wrapper.inner_content.ok_or("missing group post")?;
                if !matches!(inner.content, Some(Body::DataMessage(_))) {
                    return Err("non-post group artifact".into());
                }
                if !self
                    .group
                    .anon_group_repo
                    .is_member(&id, &sender)
                    .map_err(|error| error.to_string())?
                {
                    return Err("group sender not yet in roster".into());
                }
                self.group
                    .process_incoming_content(&id, &sender, &inner)
                    .map_err(|error| error.to_string())?
            }
            _ => return Err("secret artifact transport/content mismatch".into()),
        };
        repo.finish_content_artifact(artifact)
            .map_err(|error| error.to_string())?;
        for event in events {
            self.events.emit_domain(event);
        }
        tracing::info!(envelope_id = %artifact.key.envelope_id, "secret receive artifact applied");
        Ok(receipt)
    }

    pub(crate) async fn replay(&self, repo: &Arc<MessageRepository>) {
        let mut cursor = None;
        for _ in 0..11 {
            let artifacts = match repo.pending_content_artifacts_after(cursor.as_ref()) {
                Ok(artifacts) => artifacts,
                Err(error) => {
                    tracing::warn!(%error, "Secret receive replay deferred");
                    break;
                }
            };
            if artifacts.is_empty() {
                break;
            }
            if let Some(last) = artifacts.last() {
                cursor = Some((last.key.server_timestamp_ns, last.id.clone()));
            }
            for artifact in artifacts {
                if let Err(error) = self.apply(repo, &artifact).await {
                    tracing::warn!(envelope_id = %artifact.key.envelope_id, %error, "Secret data post awaits replay");
                }
            }
        }
    }
}
