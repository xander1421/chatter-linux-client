use super::ChatterClient;
use crate::domain::ids::{ChannelId, MessageId};
use crate::state::ThreadPanel;

impl ChatterClient {
    pub fn active_thread(&self) -> Option<ThreadPanel> {
        self.services.chat_state.active_thread()
    }

    pub fn open_thread(&self, channel: ChannelId, root: MessageId) {
        self.services.chat_state.open_thread(channel, root);
    }

    pub fn close_thread(&self, root: MessageId) {
        self.services.chat_state.close_thread(root);
    }

    pub fn update_thread_reply_count(&self, root: MessageId, count: u32) {
        self.services.chat_state.update_thread_reply_count(root, count);
    }
}
