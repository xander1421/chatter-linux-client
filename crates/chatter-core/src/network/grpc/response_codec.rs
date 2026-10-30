//! Shared decode boundary for every generated unary and streaming RPC client.
use crate::network::validation::protovalidate::{self, ContentValidationError};
use prost::Message;
use std::{marker::PhantomData, sync::Arc};
use tonic::{
    Status,
    codec::{Codec, DecodeBuf, Decoder},
};

/// A response refused before it can reach a service. Contains schema information only.
#[derive(Debug, thiserror::Error)]
#[error("RPC response validation failed: {0}")]
pub struct ResponseValidationError(pub ContentValidationError);

/// Prost encoding with declared validation on every decoded response message.
pub struct ResponseCodec<T, U> {
    path: &'static str,
    inner: tonic_prost::ProstCodec<T, U>,
}
impl<T, U> ResponseCodec<T, U> {
    /// Bind decoding to the declared output of this static `/service/method` path.
    pub fn new(path: &'static str) -> Self {
        Self {
            path,
            inner: tonic_prost::ProstCodec::default(),
        }
    }
}
impl<T: Message + Send + 'static, U: Message + Default + Send + 'static> Codec
    for ResponseCodec<T, U>
{
    type Encode = T;
    type Decode = U;
    type Encoder = <tonic_prost::ProstCodec<T, U> as Codec>::Encoder;
    type Decoder = ResponseDecoder<U>;
    fn encoder(&mut self) -> Self::Encoder {
        self.inner.encoder()
    }
    fn decoder(&mut self) -> Self::Decoder {
        ResponseDecoder {
            path: self.path,
            marker: PhantomData,
        }
    }
}

/// Decoder used by tonic for both unary replies and each streamed message.
pub struct ResponseDecoder<U> {
    path: &'static str,
    marker: PhantomData<U>,
}
impl<U: Message + Default> Decoder for ResponseDecoder<U> {
    type Item = U;
    type Error = Status;
    fn decode(&mut self, buf: &mut DecodeBuf<'_>) -> Result<Option<U>, Status> {
        use bytes::Buf;
        let bytes = buf.copy_to_bytes(buf.remaining());
        protovalidate::validate_rpc_response(self.path, &bytes)
            .map_err(|e| refusal(e, self.path))?;
        U::decode(bytes).map(Some).map_err(|_| {
            refusal(
                ContentValidationError::Decode("invalid protobuf".into()),
                self.path,
            )
        })
    }
}
fn refusal(error: ContentValidationError, path: &str) -> Status {
    // Never log a payload, field value, identity, or protobuf parser error.
    let (field, rule) = match &error {
        ContentValidationError::Violation { field, rule } => (field.as_str(), *rule),
        _ => ("", ""),
    };
    tracing::warn!(
        rpc = path,
        category = error.category(),
        field,
        rule,
        "refusing invalid RPC response"
    );
    let mut status = Status::data_loss("RPC response failed declared validation");
    status.set_source(Arc::new(ResponseValidationError(error)));
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::chat_v1::{self, user_service_client::UserServiceClient};
    use http_body::Frame;
    use std::{
        collections::VecDeque,
        pin::Pin,
        task::{Context, Poll},
    };

    struct Frames(VecDeque<Frame<bytes::Bytes>>);
    impl http_body::Body for Frames {
        type Data = bytes::Bytes;
        type Error = Status;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Status>>> {
            Poll::Ready(self.0.pop_front().map(Ok))
        }
    }
    #[derive(Clone)]
    struct Reply(Vec<Vec<u8>>);
    impl tower::Service<http::Request<tonic::body::Body>> for Reply {
        type Response = http::Response<tonic::body::Body>;
        type Error = std::io::Error;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, _: http::Request<tonic::body::Body>) -> Self::Future {
            let mut wire = Vec::new();
            for message in &self.0 {
                wire.push(0);
                wire.extend_from_slice(&(message.len() as u32).to_be_bytes());
                wire.extend_from_slice(message);
            }
            // Deliberately fragment headers and messages across HTTP data frames.
            let mut frames: VecDeque<_> = wire
                .chunks(3)
                .map(|chunk| Frame::data(bytes::Bytes::copy_from_slice(chunk)))
                .collect();
            let mut trailers = http::HeaderMap::new();
            trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
            frames.push_back(Frame::trailers(trailers));
            std::future::ready(Ok(http::Response::builder()
                .header("content-type", "application/grpc")
                .body(tonic::body::Body::new(Frames(frames)))
                .unwrap()))
        }
    }
    fn profile(id: &str) -> chat_v1::UserProfile {
        chat_v1::UserProfile {
            user_id: id.into(),
            ..Default::default()
        }
    }
    const UUID: &str = "12345678-1234-1234-1234-123456789abc";
    fn assert_typed(error: &Status) {
        use std::error::Error;
        assert_eq!(error.code(), tonic::Code::DataLoss);
        assert!(
            error
                .source()
                .unwrap()
                .downcast_ref::<ResponseValidationError>()
                .is_some()
        );
        assert!(!error.to_string().contains("invalid-private-value"));
        let network = crate::error::NetworkError::from_status(error);
        assert!(matches!(network, crate::error::NetworkError::DataLoss(_)));
        assert!(!network.is_retryable());
        assert!(!network.is_transport_error());
        // The server may already have accepted a mutating request.
        assert!(!network.is_definite_rejection());
    }
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[tokio::test]
    async fn unary_valid_passes_invalid_is_typed_error() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || Capture(writer.clone()))
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        for (id, valid) in [(UUID, true), ("invalid-private-value", false)] {
            let result = UserServiceClient::new(Reply(vec![profile(id).encode_to_vec()]))
                .get_user_profile(chat_v1::GetUserProfileRequest::default())
                .await;
            if valid {
                assert_eq!(result.unwrap().into_inner().user_id, UUID);
            } else {
                assert_typed(&result.unwrap_err());
            }
        }
        let logs = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("refusing invalid RPC response"));
        assert!(!logs.contains("invalid-private-value"));
    }
    #[tokio::test]
    async fn stream_checks_each_message_before_delivery_and_terminates_on_violation() {
        use chat_v1::user_event::Event;
        let heartbeat = chat_v1::UserEvent {
            event: Some(Event::Heartbeat(chat_v1::StreamHeartbeat {})),
            ..Default::default()
        };
        let invalid = chat_v1::UserEvent {
            event: Some(Event::ProfileUpdated(profile("invalid-private-value"))),
            ..Default::default()
        };
        let mut stream = UserServiceClient::new(Reply(vec![
            heartbeat.encode_to_vec(),
            invalid.encode_to_vec(),
            heartbeat.encode_to_vec(),
        ]))
        .stream_user_events(chat_v1::UserEventRequest::default())
        .await
        .unwrap()
        .into_inner();
        assert!(matches!(
            stream.message().await.unwrap().unwrap().event,
            Some(Event::Heartbeat(_))
        ));
        assert_typed(&stream.message().await.unwrap_err());
        assert!(stream.message().await.unwrap().is_none());
    }
    #[tokio::test]
    async fn malformed_wire_is_typed_error_without_parser_contents() {
        let error = UserServiceClient::new(Reply(vec![vec![0xff]]))
            .get_user_profile(chat_v1::GetUserProfileRequest::default())
            .await
            .unwrap_err();
        assert_typed(&error);
    }
}
