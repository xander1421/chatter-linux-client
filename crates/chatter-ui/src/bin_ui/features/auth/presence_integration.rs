//! Simple presence integration for UI.
//!
//! Provides simple hooks to initialize and track presence through the service layer.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use chatter_core::client::ChatterClient;
use chatter_core::proto::UserPresence;

/// Current presence state for this session
static CURRENT_PRESENCE: std::sync::LazyLock<RwLock<UserPresence>> =
    std::sync::LazyLock::new(|| RwLock::new(UserPresence::Offline));

/// Last activity timestamp
static LAST_ACTIVITY: std::sync::LazyLock<RwLock<Instant>> =
    std::sync::LazyLock::new(|| RwLock::new(Instant::now()));

/// Initialize presence on login.
pub async fn initialize_presence(ui_ctx: &Arc<ChatterClient>) {
    // Set online and broadcast to all anon DMs
    set_presence(ui_ctx, UserPresence::Online).await;
    
    // Start idle detection task
    start_idle_detection(ui_ctx.clone());
    
    tracing::info!("Presence initialized");
}

/// Shutdown presence on logout or app exit.
pub async fn shutdown_presence(ui_ctx: &ChatterClient) {
    // Send offline status
    set_presence(ui_ctx, UserPresence::Offline).await;
    
    tracing::info!("Presence shutdown complete");
}

/// Track user activity (call on any user interaction).
pub async fn track_activity(ui_ctx: &Arc<ChatterClient>) {
    // Update last activity
    {
        let mut last = LAST_ACTIVITY.write().await;
        *last = Instant::now();
    }
    
    // If we were away, go back online
    let current = *CURRENT_PRESENCE.read().await;
    if current == UserPresence::Away {
        set_presence(ui_ctx, UserPresence::Online).await;
    }
}

/// Notify when opening an anon DM chat.
pub async fn notify_chat_opened(
    ui_ctx: &Arc<ChatterClient>,
    conversation_id: &chatter_core::domain::anon_dm::AnonDmId,
) {
    let current = *CURRENT_PRESENCE.read().await;
    if current == UserPresence::Offline {
        return;
    }
    
    // Send current presence to this specific conversation
    if let Err(e) = ui_ctx.send_anon_dm_presence_update(conversation_id, current).await {
        tracing::debug!("Failed to send presence on chat open: {}", e);
    }
}

/// Set presence and broadcast to all anon DM conversations.
/// Delegates to `ChatterClient::set_presence()` for the broadcast.
async fn set_presence(ui_ctx: &ChatterClient, presence: UserPresence) {
    {
        let mut current = CURRENT_PRESENCE.write().await;
        if *current == presence {
            return;
        }
        *current = presence;
    }
    tracing::info!("Setting presence to {:?}", presence);
    ui_ctx.set_presence(presence).await;
}

/// Manually set user status from UI
pub async fn set_user_status(ui_ctx: &Arc<ChatterClient>, status: &str) {
    let presence = match status.to_lowercase().as_str() {
        "online" | "available" => UserPresence::Online,
        "away" | "idle" => UserPresence::Away,
        "dnd" | "busy" | "do-not-disturb" => UserPresence::DoNotDisturb,
        "offline" | "invisible" => UserPresence::Offline,
        _ => UserPresence::Online,
    };
    
    tracing::info!("User manually changed status to {:?}", presence);
    set_presence(ui_ctx, presence).await;
}

/// Get current presence status as string for UI
pub async fn get_current_status() -> String {
    let current = *CURRENT_PRESENCE.read().await;
    match current {
        UserPresence::Online => "online".to_string(),
        UserPresence::Away => "away".to_string(),
        UserPresence::DoNotDisturb => "dnd".to_string(),
        UserPresence::Offline => "offline".to_string(),
        _ => "offline".to_string(),
    }
}

/// Start idle detection background task
fn start_idle_detection(ui_ctx: Arc<ChatterClient>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        
        loop {
            interval.tick().await;
            
            let last = *LAST_ACTIVITY.read().await;
            let current = *CURRENT_PRESENCE.read().await;
            
            // If online and idle for 5 minutes, set away
            if current == UserPresence::Online && last.elapsed() > Duration::from_secs(300) {
                tracing::debug!("User idle, setting away status");
                set_presence(&ui_ctx, UserPresence::Away).await;
            }
        }
    });
}