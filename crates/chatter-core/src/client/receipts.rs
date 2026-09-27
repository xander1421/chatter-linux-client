//! Receipt evidence for the message-info presentation.
use super::ChatterClient;
use crate::domain::ids::MessageId;
use crate::services::message::MessageError;
use crate::storage::repositories::message_receipt_repo::DeviceReceipt;

impl ChatterClient {
    pub fn message_receipt_info(&self, message_id: MessageId) -> Result<String, MessageError> {
        self.services
            .message_service
            .message_receipt_info(message_id)
    }

    /// Includes unacknowledged target devices, so message info can display the
    /// original recipients even after group membership changes.
    pub fn message_receipts(
        &self,
        message_id: MessageId,
    ) -> Result<Vec<DeviceReceipt>, MessageError> {
        self.services.message_service.message_receipts(message_id)
    }
}
