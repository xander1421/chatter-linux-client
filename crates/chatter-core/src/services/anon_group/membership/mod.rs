//! Member management and roster operations.

use std::time::Duration;

use prost::Message as ProstMessage;
use rand::RngCore;
use tokio::time::sleep;

use crate::domain::UserId;
use crate::domain::anon_group::{
    AnonGroup, AnonGroupId, AnonGroupMember, AnonGroupRole, PendingActionType,
};
use crate::proto::content_v1::{
    self, AnonDmMessage as AnonDmMessageProto, AnonGroupInvite, AnonGroupJoinAck,
    AnonGroupMemberInfo, AnonGroupMemberList, AnonGroupProfileShare, Content,
};
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::current_user::CurrentUserProvider;
use crate::util::time::TimestampNanos;

use super::{AnonGroupError, AnonGroupService};

#[cfg(test)]
tokio::task_local! {
    static RECIPIENT_PREPARED: (std::sync::Arc<tokio::sync::Notify>, std::sync::Arc<tokio::sync::Notify>);
}

mod delivery;
mod invitation;
mod profile;
mod queries;
mod roster;

#[cfg(test)]
mod tests;
