use super::{
    MAX_RESPONSES_PER_WINDOW, RESPONSE_RATE_LIMIT_WINDOW_SECS, RESPONSE_WINDOW_SECS,
    STALE_PEER_TIMEOUT_SECS,
};

#[derive(Clone, Debug, PartialEq)]
pub struct SyncConfig {
    /// Public messages held (and offered) at once.
    pub seen_capacity: usize,
    /// Byte budget for the GCS filter in a request.
    pub gcs_max_bytes: usize,
    /// Target false-positive rate for the filter.
    pub gcs_target_fpr: f64,
    /// Freshness window for fragments, file transfers and announces.
    pub max_message_age_secs: u64,
    /// Freshness window for whole public messages and group messages — the
    /// town-crier window. Must not exceed the receive side's acceptance window, or
    /// packets we serve would be dropped as stale by the peer that asked for them.
    pub public_message_max_age_secs: u64,
    /// Freshness window for prekey bundles.
    pub prekey_bundle_max_age_secs: u64,
    /// How often stale-announce cleanup runs, in seconds.
    pub stale_peer_cleanup_interval_secs: u64,
    /// An announce older than this makes its peer stale. See
    /// [`STALE_PEER_TIMEOUT_SECS`] for why the default is not Swift's.
    pub stale_peer_timeout_secs: u64,
    pub fragment_capacity: usize,
    pub file_transfer_capacity: usize,
    pub group_message_capacity: usize,
    pub prekey_bundle_capacity: usize,
    pub board_capacity: usize,
    /// Per-type round intervals. A zero interval disables that round entirely.
    pub message_sync_interval_secs: u64,
    pub fragment_sync_interval_secs: u64,
    pub file_transfer_sync_interval_secs: u64,
    pub prekey_bundle_sync_interval_secs: u64,
    pub board_sync_interval_secs: u64,
    pub response_rate_limit_max_responses: usize,
    pub response_rate_limit_window_secs: u64,
    /// How long an outstanding request keeps accepting RSR answers.
    pub request_response_window_secs: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            seen_capacity: 1000,
            gcs_max_bytes: 400,
            gcs_target_fpr: 0.01,
            max_message_age_secs: 15 * 60,
            public_message_max_age_secs: 6 * 60 * 60,
            prekey_bundle_max_age_secs: 24 * 60 * 60,
            stale_peer_cleanup_interval_secs: 60,
            stale_peer_timeout_secs: STALE_PEER_TIMEOUT_SECS,
            fragment_capacity: 600,
            file_transfer_capacity: 200,
            group_message_capacity: 200,
            prekey_bundle_capacity: 200,
            board_capacity: 200,
            message_sync_interval_secs: 15,
            fragment_sync_interval_secs: 30,
            file_transfer_sync_interval_secs: 60,
            prekey_bundle_sync_interval_secs: 60,
            board_sync_interval_secs: 60,
            response_rate_limit_max_responses: MAX_RESPONSES_PER_WINDOW,
            response_rate_limit_window_secs: RESPONSE_RATE_LIMIT_WINDOW_SECS,
            request_response_window_secs: RESPONSE_WINDOW_SECS,
        }
    }
}
