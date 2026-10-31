use super::*;
use crate::client::ChatterClient;
use crate::domain::message::{ConversationType, MessageContent};

mod edit;
mod gate;
mod persistence;
mod transactional;
mod resync;


/// Inbound streams run only after authentication; fixtures must bind their
/// local owner before exercising fail-closed block checks and receipt intent.
pub(super) fn bind_test_owner(client: &ChatterClient) {
    if client.messages().settings_repo.current_user_id().is_err() {
        client.messages().settings_repo.set_string(
            crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
            &UserId::new().to_string(),
        ).unwrap();
    }
    let owner = client.messages().settings_repo.current_user_id().unwrap();
    client.auth_state().set_logged_in(owner, 1, "fixture".into());

}
