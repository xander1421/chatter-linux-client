//! Simple presence integration for UI.
//!
//! Provides simple hooks to initialize and track presence through the service layer.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use chatter_core::client::ChatterClient;
use chatter_core::domain::user::PresenceStatus;

/// Current presence state for this session
static CURRENT_PRESENCE: std::sync::LazyLock<RwLock<PresenceStatus>> =
    std::sync::LazyLock::new(|| RwLock::new(PresenceStatus::Offline));

/// Manual override chosen from the status picker. `None` = "auto" (start
/// online, idle detector may flip online↔away). `Some(s)` pins the wire
/// presence to `s` and disables both automatic transitions.
static MANUAL_OVERRIDE: std::sync::LazyLock<RwLock<Option<PresenceStatus>>> =
    std::sync::LazyLock::new(|| RwLock::new(None));

/// Last activity timestamp
static LAST_ACTIVITY: std::sync::LazyLock<RwLock<Instant>> =
    std::sync::LazyLock::new(|| RwLock::new(Instant::now()));

/// Initialize presence on login, honoring a persisted manual mode.
pub async fn initialize_presence(ui_ctx: &Arc<ChatterClient>) {
    let mode = ui_ctx
        .presence_mode()
        .ok()
        .flatten()
        .unwrap_or_else(|| "auto".to_string());

    if mode == "auto" {
        *MANUAL_OVERRIDE.write().await = None;
        set_presence(ui_ctx, PresenceStatus::Online).await;
    } else {
        let status = parse_status(&mode);
        *MANUAL_OVERRIDE.write().await = Some(status);
        set_presence(ui_ctx, status).await;
    }

    // Start idle detection task (no-ops while a manual override is set)
    start_idle_detection(ui_ctx.clone());

    tracing::info!(mode = %mode, "Presence initialized");
}

/// Shutdown presence on logout or app exit.
pub async fn shutdown_presence(ui_ctx: &ChatterClient) {
    // Send offline status
    set_presence(ui_ctx, PresenceStatus::Offline).await;

    tracing::info!("Presence shutdown complete");
}

/// Track user activity (call on any user interaction).
pub async fn track_activity(ui_ctx: &Arc<ChatterClient>) {
    // Update last activity
    {
        let mut last = LAST_ACTIVITY.write().await;
        *last = Instant::now();
    }

    // If we were auto-away, go back online. Never fights a manual choice.
    if MANUAL_OVERRIDE.read().await.is_some() {
        return;
    }
    let current = *CURRENT_PRESENCE.read().await;
    if current == PresenceStatus::Away {
        set_presence(ui_ctx, PresenceStatus::Online).await;
    }
}

/// Notify when opening an anon DM chat.
pub async fn notify_chat_opened(
    ui_ctx: &Arc<ChatterClient>,
    conversation_id: &chatter_core::domain::anon_dm::AnonDmId,
) {
    let current = *CURRENT_PRESENCE.read().await;
    if current == PresenceStatus::Offline {
        return;
    }

    // Send current presence to this specific conversation
    if let Err(e) = ui_ctx.send_anon_dm_presence_update(conversation_id, current).await {
        tracing::debug!("Failed to send presence on chat open: {}", e);
    }
}

/// Set presence and broadcast to all anon DM conversations.
/// Delegates to `ChatterClient::set_presence()` for the broadcast.
async fn set_presence(ui_ctx: &ChatterClient, presence: PresenceStatus) {
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
    let mode = status.to_lowercase();

    // Persist so the choice survives restart (best-effort; presence still applies).
    if let Err(e) = ui_ctx.set_presence_mode(&mode) {
        tracing::warn!("Failed to persist presence mode: {e}");
    }

    if mode == "auto" {
        *MANUAL_OVERRIDE.write().await = None;
        tracing::info!("Presence back to automatic");
        set_presence(ui_ctx, PresenceStatus::Online).await;
        return;
    }

    let presence = parse_status(&mode);
    *MANUAL_OVERRIDE.write().await = Some(presence);
    tracing::info!("User manually changed status to {:?}", presence);
    set_presence(ui_ctx, presence).await;
}

fn parse_status(status: &str) -> PresenceStatus {
    match status {
        "online" | "available" => PresenceStatus::Online,
        "away" | "idle" => PresenceStatus::Away,
        "dnd" | "busy" | "do-not-disturb" => PresenceStatus::DoNotDisturb,
        "offline" | "invisible" => PresenceStatus::Offline,
        _ => PresenceStatus::Online,
    }
}

/// Get current presence status as string for UI
pub async fn get_current_status() -> String {
    let current = *CURRENT_PRESENCE.read().await;
    current.as_str().to_string()
}

/// Start idle detection background task
fn start_idle_detection(ui_ctx: Arc<ChatterClient>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));

        loop {
            interval.tick().await;

            // Manual override pins the status — no auto-away.
            if MANUAL_OVERRIDE.read().await.is_some() {
                continue;
            }

            let last = *LAST_ACTIVITY.read().await;
            let current = *CURRENT_PRESENCE.read().await;

            // If online and idle for 5 minutes, set away
            if current == PresenceStatus::Online && last.elapsed() > Duration::from_secs(300) {
                tracing::debug!("User idle, setting away status");
                set_presence(&ui_ctx, PresenceStatus::Away).await;
            }
        }
    });
}
