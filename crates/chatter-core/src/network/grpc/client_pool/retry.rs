use super::*;

impl GrpcClientPool {
    // ========================================================================
    // Retry middleware — transport-aware retry for all RPCs
    // ========================================================================

    /// Execute an RPC with automatic retry on transport errors.
    ///
    /// On transport failures the pool disconnects the stale channel so the
    /// next attempt creates a fresh HTTP/2 connection via the lazy-channel
    /// path. This fixes the "stale connection pool" problem where HTTP/2
    /// connections silently die and subsequent RPCs fail with
    /// `Code::Unknown / transport error`.
    ///
    /// The closure receives `&Self` so it can create a fresh client per
    /// attempt (important — a client holding a stale channel must not be
    /// reused after disconnect).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let events = pool.call_with_retry("CreateChannel", |p| {
    ///     let mut client = p.channel_client();
    ///     async move { client.create_channel("test", ChannelType::ChannelPublic, None, None, None, None).await }
    /// }).await?;
    /// ```
    pub async fn call_with_retry<F, Fut, T>(
        &self,
        operation: &str,
        mut f: F,
    ) -> Result<T, NetworkError>
    where
        F: FnMut(&Self) -> Fut,
        Fut: Future<Output = Result<T, NetworkError>>,
    {
        use crate::network::connection::{ServerStatus, set_server_status};

        let pool = self;
        let retry_config = RetryConfig::default();
        let attempts = retry_unary(
            &retry_config,
            operation,
            Some(|| {
                tracing::warn!(
                    "Transport error detected — disconnecting stale channel for reconnect"
                );
                // The transport boundary records the actual evidence. A
                // server's Unavailable reply also triggers retry here, but
                // still proves reachability.
                pool.disconnect();
                pool.note_transport_error();
            }),
            || f(pool),
        );

        // The pool is deliberately built without a request deadline, because a
        // deadline on the endpoint would also cut off server streams mid-stream.
        // That left unary calls with nothing but the connect timeout, which only
        // fires for a server that refuses TCP — not for one that accepts the
        // connection and then never answers, where the call hung forever. The
        // deadline goes here, around the whole retry sequence rather than each
        // attempt, so the worst case a caller has to survive is one number.
        let result = match tokio::time::timeout(UNARY_DEADLINE, attempts).await {
            Ok(inner) => inner,
            Err(_) => {
                tracing::warn!(
                    operation,
                    deadline_s = UNARY_DEADLINE.as_secs(),
                    "RPC exceeded its deadline — treating the server as unreachable"
                );
                set_server_status(ServerStatus::Unreachable);
                pool.disconnect();
                pool.note_transport_error();
                Err(NetworkError::Timeout)
            }
        };

        // Production GrpcTransport records replies before NetworkError mapping
        // loses their origin (server Unavailable versus local transport error).
        result
    }
}
