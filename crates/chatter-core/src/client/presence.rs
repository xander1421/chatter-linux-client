//! Session presence policy, including the five-minute automatic away state.
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use super::ChatterClient;
use crate::domain::user::PresenceStatus;

pub(super) struct PresenceController {
    state: Mutex<PresenceState>,
    pub(super) publication_lock: Mutex<()>,
    idle_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct PresenceState {
    current: PresenceStatus,
    manual: Option<PresenceStatus>,
    last_activity: Instant,
}

impl PresenceController {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(PresenceState {
                current: PresenceStatus::Offline, manual: None,
                last_activity: Instant::now(),
            }),
            publication_lock: Mutex::new(()),
            idle_task: Mutex::new(None),
        }
    }
}

impl ChatterClient {
    async fn publish_presence(&self, state: &mut PresenceState, status: PresenceStatus) {
        if state.current != status {
            state.current = status;
            tracing::info!("Setting presence to {:?}", status);
            self.set_presence(status).await;
        }
    }

    pub async fn initialize_presence(self: &Arc<Self>) {
        let mode = self.presence_mode().ok().flatten().unwrap_or_else(|| "auto".into());
        let manual = if mode == "auto" { None } else { Some(parse_status(&mode)) };
        {
            let mut state = self.presence_controller.state.lock().await;
            state.manual = manual;
            state.last_activity = Instant::now();
            self.publish_presence(&mut state, manual.unwrap_or(PresenceStatus::Online)).await;
        }
        let mut task = self.presence_controller.idle_task.lock().await;
        if let Some(old) = task.take() { old.abort(); }
        let client = self.clone();
        *task = Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            let mut ticks: u32 = 0;
            loop {
                interval.tick().await;
                ticks = ticks.wrapping_add(1);
                // Every two minutes, inside the server's five-minute presence
                // window: contacts expire a status that stops being refreshed
                // (`PRESENCE_STALE_SECS`), which is how a client killed without
                // sending Offline drops off. Offline (and Invisible) send nothing.
                if ticks % 4 == 0 {
                    let current = client.presence_controller.state.lock().await.current;
                    if current != PresenceStatus::Offline {
                        client.refresh_identified_presence(current).await;
                    }
                }
                // On a phone the app being on screen is the activity: taps and
                // scrolls do not reach this timer, so a user reading for five
                // minutes went Away to their contacts (S9). The screen's own
                // timeout backgrounds the app when it is really unattended.
                #[cfg(target_os = "android")]
                if client.is_app_visible() {
                    continue;
                }
                let mut state = client.presence_controller.state.lock().await;
                if state.manual.is_none() && state.current == PresenceStatus::Online
                    && state.last_activity.elapsed() > Duration::from_secs(300) {
                    tracing::debug!("User idle, setting away status");
                    client.publish_presence(&mut state, PresenceStatus::Away).await;
                }
            }
        }));
        tracing::info!(mode = %mode, "Presence initialized");
    }

    pub async fn record_presence_activity(&self) {
        let mut state = self.presence_controller.state.lock().await;
        state.last_activity = Instant::now();
        if state.manual.is_none() && state.current == PresenceStatus::Away {
            self.publish_presence(&mut state, PresenceStatus::Online).await;
        }
    }

    pub async fn set_presence_mode_choice(&self, status: &str) {
        let mode = status.to_lowercase();
        if let Err(e) = self.set_presence_mode(&mode) {
            tracing::warn!("Failed to persist presence mode: {e}");
        }
        let mut state = self.presence_controller.state.lock().await;
        state.manual = if mode == "auto" { None } else { Some(parse_status(&mode)) };
        if state.manual.is_none() { state.last_activity = Instant::now(); }
        let presence = state.manual.unwrap_or(PresenceStatus::Online);
        self.publish_presence(&mut state, presence).await;
    }

    pub async fn shutdown_presence(&self) {
        if let Some(task) = self.presence_controller.idle_task.lock().await.take() { task.abort(); }
        let mut state = self.presence_controller.state.lock().await;
        self.publish_presence(&mut state, PresenceStatus::Offline).await;
        tracing::info!("Presence shutdown complete");
    }

    pub async fn notify_presence_chat_opened(
        &self, conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) {
        let state = self.presence_controller.state.lock().await;
        let current = state.current;
        if current == PresenceStatus::Offline { return; }
        let revision = self.auth_state().get().session_revision;
        let _lifecycle = self.auth_lifecycle_lock.lock().await;
        if self.auth_state().get().session_revision != revision { return; }
        let _publication = self.presence_controller.publication_lock.lock().await;
        if let Err(e) = self.send_anon_dm_presence_update(conversation_id, current).await {
            tracing::debug!("Failed to send presence on chat open: {}", e);
        }
    }

    pub async fn current_presence_status(&self) -> String {
        self.presence_controller.state.lock().await.current.as_str().into()
    }
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
