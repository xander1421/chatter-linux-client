//! Call gRPC client.
//!
//! Client for call service operations including creating, joining, ending,
//! and leaving calls, as well as streaming call events.

use tokio_stream::{Stream, StreamExt};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    call_service_client::CallServiceClient, CallEvent, CallInfo, CallSettings, CallStatus,
    CallType, CreateCallRequest, CreateCallResponse, EndCallRequest, GetActiveCallInChannelRequest,
    GetActiveCallInChannelResponse, GetCallInfoRequest, JoinCallRequest, JoinCallResponse,
    LeaveCallRequest, ListChannelCallsRequest, ListChannelCallsResponse, StreamCallEventsRequest,
};

/// Call service client wrapper.
///
/// Wraps the generated CallServiceClient with ergonomic APIs and error mapping.
///
/// # Example
///
/// ```ignore
/// let pool = GrpcClientPool::new("http://localhost:50051")?;
/// pool.connect().await?;
///
/// let mut client = CallClient::new(pool.channel(), pool.auth_clone());
/// let response = client.create_call("channel-123", "Team Meeting", CallType::Video, None).await?;
/// ```
pub struct CallClient {
    inner: CallServiceClient<InterceptedService<Channel, AuthInterceptor>>,
}

impl CallClient {
    /// Create a new CallClient from pool components.
    pub fn new(channel: Channel, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: CallServiceClient::with_interceptor(channel, interceptor),
        }
    }

    // ==================== Core Call RPCs ====================

    /// Create a new call in a channel.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to create call in
    /// * `title` - Call title (1-100 chars)
    /// * `call_type` - Type of call (voice, video, screen share)
    /// * `settings` - Optional call settings
    ///
    /// # Returns
    ///
    /// CreateCallResponse containing call_id, room_name, livekit_token, livekit_url, and call_info.
    pub async fn create_call(
        &mut self,
        channel_id: &str,
        title: &str,
        call_type: CallType,
        settings: Option<CallSettings>,
    ) -> Result<CreateCallResponse, NetworkError> {
        let request = CreateCallRequest {
            channel_id: channel_id.to_string(),
            title: title.to_string(),
            r#type: call_type.into(),
            settings,
            client_request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .create_call(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Join an existing call.
    ///
    /// # Arguments
    ///
    /// * `call_id` - ID of the call to join
    ///
    /// # Returns
    ///
    /// JoinCallResponse containing livekit_token, livekit_url, expires_at, and call_info.
    pub async fn join_call(&mut self, call_id: &str) -> Result<JoinCallResponse, NetworkError> {
        let request = JoinCallRequest {
            call_id: call_id.to_string(),
            client_request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .join_call(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// End a call (creator/admin only).
    ///
    /// # Arguments
    ///
    /// * `call_id` - ID of the call to end
    /// * `reason` - Optional reason for ending the call (max 200 chars)
    pub async fn end_call(&mut self, call_id: &str, reason: &str) -> Result<(), NetworkError> {
        let request = EndCallRequest {
            call_id: call_id.to_string(),
            reason: reason.to_string(),
        };

        self.inner
            .end_call(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Leave a call (participant leaves without ending it).
    ///
    /// # Arguments
    ///
    /// * `call_id` - ID of the call to leave
    pub async fn leave_call(&mut self, call_id: &str) -> Result<(), NetworkError> {
        let request = LeaveCallRequest {
            call_id: call_id.to_string(),
        };

        self.inner
            .leave_call(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get call information.
    ///
    /// # Arguments
    ///
    /// * `call_id` - ID of the call to get info for
    ///
    /// # Returns
    ///
    /// CallInfo with call details, participants, and settings.
    pub async fn get_call_info(&mut self, call_id: &str) -> Result<CallInfo, NetworkError> {
        let request = GetCallInfoRequest {
            call_id: call_id.to_string(),
        };

        self.inner
            .get_call_info(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get active call in a channel (for joining existing calls).
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to check for active call
    ///
    /// # Returns
    ///
    /// Option<CallInfo> - Some if there's an active call, None otherwise.
    pub async fn get_active_call_in_channel(
        &mut self,
        channel_id: &str,
    ) -> Result<Option<CallInfo>, NetworkError> {
        let request = GetActiveCallInChannelRequest {
            channel_id: channel_id.to_string(),
        };

        let response: GetActiveCallInChannelResponse = self
            .inner
            .get_active_call_in_channel(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))?;

        if response.has_active_call {
            Ok(response.active_call)
        } else {
            Ok(None)
        }
    }

    /// List calls in a channel with pagination.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to list calls for
    /// * `status_filter` - Optional filter by call status
    /// * `limit` - Max calls to return (1-100)
    /// * `offset` - Pagination offset
    ///
    /// # Returns
    ///
    /// Tuple of (Vec<CallInfo>, total_count, has_more).
    pub async fn list_channel_calls(
        &mut self,
        channel_id: &str,
        status_filter: Option<CallStatus>,
        limit: i32,
        offset: i32,
    ) -> Result<(Vec<CallInfo>, i32, bool), NetworkError> {
        let request = ListChannelCallsRequest {
            channel_id: channel_id.to_string(),
            status_filter: status_filter.map(|s| s.into()).unwrap_or(0),
            limit,
            offset,
        };

        let response: ListChannelCallsResponse = self
            .inner
            .list_channel_calls(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok((response.calls, response.total_count, response.has_more))
    }

    /// Stream call events for real-time updates.
    ///
    /// Returns a long-lived stream of call events (created, ended, participant joined/left/updated).
    ///
    /// # Arguments
    ///
    /// * `call_ids` - Specific call IDs to stream events for
    /// * `channel_ids` - Channel IDs to stream events for (all calls in channels)
    ///
    /// # Returns
    ///
    /// Stream of CallEvent items.
    pub async fn stream_call_events(
        &mut self,
        call_ids: Vec<String>,
        channel_ids: Vec<String>,
    ) -> Result<impl Stream<Item = Result<CallEvent, NetworkError>>, NetworkError> {
        let request = StreamCallEventsRequest {
            call_ids,
            channel_ids,
        };

        let response = self
            .inner
            .stream_call_events(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_call_client_construction_types() {
        // Verify type signatures compile correctly.
        fn _verify_types(channel: Channel, interceptor: AuthInterceptor) {
            let _client = CallClient::new(channel, interceptor);
        }
    }

    #[test]
    fn test_call_type_values() {
        assert_eq!(CallType::Unspecified as i32, 0);
        assert_eq!(CallType::Voice as i32, 1);
        assert_eq!(CallType::Video as i32, 2);
        assert_eq!(CallType::ScreenShare as i32, 3);
    }

    #[test]
    fn test_call_status_values() {
        assert_eq!(CallStatus::Unspecified as i32, 0);
        assert_eq!(CallStatus::Active as i32, 1);
        assert_eq!(CallStatus::Ended as i32, 2);
    }

    #[test]
    fn test_call_settings_construction() {
        let settings = CallSettings {
            start_with_audio_muted: true,
            start_with_video_muted: false,
            enable_recording: false,
            enable_chat: true,
            max_participants: 10,
            require_password: false,
            password: String::new(),
        };

        assert!(settings.start_with_audio_muted);
        assert!(!settings.start_with_video_muted);
        assert_eq!(settings.max_participants, 10);
    }

    #[test]
    fn test_call_info_construction() {
        let call_info = CallInfo {
            id: "call-123".to_string(),
            channel_id: "channel-456".to_string(),
            room_name: "room-abc".to_string(),
            title: "Team Meeting".to_string(),
            r#type: CallType::Video.into(),
            status: CallStatus::Active.into(),
            created_by: "user-789".to_string(),
            created_at: 1234567890000,
            ended_at: 0,
            participants: vec![],
            settings: None,
            duration: None,
            current_participant_count: 1,
        };

        assert_eq!(call_info.id, "call-123");
        assert_eq!(call_info.title, "Team Meeting");
        assert_eq!(call_info.r#type, CallType::Video as i32);
        assert_eq!(call_info.status, CallStatus::Active as i32);
    }

    #[test]
    fn test_call_participant_construction() {
        use crate::proto::chat_v1::CallParticipant;

        let participant = CallParticipant {
            user_id: "user-123".to_string(),
            display_name: "Alice".to_string(),
            joined_at: 1234567890000,
            left_at: 0,
            is_creator: true,
            has_audio: true,
            has_video: true,
            duration_in_call: None,
        };

        assert_eq!(participant.user_id, "user-123");
        assert_eq!(participant.display_name, "Alice");
        assert!(participant.is_creator);
        assert!(participant.has_audio);
        assert!(participant.has_video);
    }

    #[test]
    fn test_call_event_construction() {
        use crate::proto::chat_v1::call_event::Data;
        use crate::proto::chat_v1::CallCreatedEvent;

        let call_info = CallInfo {
            id: "call-123".to_string(),
            channel_id: "channel-456".to_string(),
            room_name: "room-abc".to_string(),
            title: "Team Meeting".to_string(),
            r#type: CallType::Video.into(),
            status: CallStatus::Active.into(),
            created_by: "user-789".to_string(),
            created_at: 1234567890000,
            ended_at: 0,
            participants: vec![],
            settings: None,
            duration: None,
            current_participant_count: 1,
        };

        let event = CallEvent {
            event_id: "event-001".to_string(),
            timestamp: 1234567890000,
            call_id: "call-123".to_string(),
            channel_id: "channel-456".to_string(),
            r#type: crate::proto::chat_v1::CallEventType::Created.into(),
            data: Some(Data::CallCreated(CallCreatedEvent {
                call_info: Some(call_info),
                created_by_display_name: "Alice".to_string(),
            })),
        };

        assert_eq!(event.event_id, "event-001");
        assert_eq!(event.call_id, "call-123");
        assert!(matches!(event.data, Some(Data::CallCreated(_))));
    }
}
