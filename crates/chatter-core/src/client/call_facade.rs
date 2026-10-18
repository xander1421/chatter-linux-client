use super::*;

impl ChatterClient {
    /// Handle remote hangup: disconnect locally. Accepts the hangup's
    /// `call_id` so core can gate on active-call match without the UI having
    /// to inspect call state.
    #[cfg(feature = "media")]
    pub async fn handle_remote_hangup(&self, call_id: &str) -> Result<(), ClientError> {
        let call_worker = self
            .call_service()
            .get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;
        self.call_signaling()
            .handle_remote_hangup(call_worker, Some(call_id))
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }
    /// A channel header may offer to join the server's current group call.
    pub async fn active_channel_call(
        &self,
        channel_id: &str,
    ) -> Result<Option<(String, bool, i32)>, crate::error::NetworkError> {
        let call = self
            .services
            .grpc_pool
            .call_client()
            .get_active_call_in_channel(channel_id)
            .await?;
        Ok(call.map(|call| (
            call.id,
            call.r#type == crate::proto::chat_v1::CallType::Video as i32,
            call.current_participant_count,
        )))
    }
    /// Get the call signaling service (persistent instance for pending state).
    pub fn call_signaling(&self) -> &crate::services::call_signaling::CallSignalingService {
        self.call_signaling.get_or_init(|| {
            crate::services::call_signaling::CallSignalingService::new(
                self.messages().clone(),
                self.channels().clone(),
            )
        })
    }

}
