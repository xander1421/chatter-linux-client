//! Session presence policy, including the five-minute automatic away state.
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use super::ChatterClient;
use crate::domain::user::PresenceStatus;

#[cfg(test)]
pub(super) type PresenceTestSend =
    Arc<dyn Fn(PresenceStatus, bool) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

pub(super) struct PresenceController {
    #[cfg(test)]
    pub(super) test_send: parking_lot::Mutex<Option<PresenceTestSend>>,
    state: Mutex<PresenceState>,
    pub(super) publication_lock: Mutex<()>,
    pub(super) publications: Arc<super::presence_owner::Publications>,
    idle_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    peer_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct PresenceState {
    current: PresenceStatus,
    manual: Option<PresenceStatus>,
    last_activity: Instant,
}

impl PresenceController {
    pub fn new() -> Self {
        Self {
            #[cfg(test)]
            test_send: parking_lot::Mutex::new(None),
            state: Mutex::new(PresenceState {
                current: PresenceStatus::Offline,
                manual: None,
                last_activity: Instant::now(),
            }),
            publication_lock: Mutex::new(()),
            publications: Arc::new(super::presence_owner::Publications::default()),
            idle_task: Mutex::new(None),
            peer_task: Mutex::new(None),
        }
    }
}

impl ChatterClient {
    async fn publish_presence(&self, state: &mut PresenceState, status: PresenceStatus) {
        if state.current != status {
            state.current = status;
            tracing::info!("Setting presence to {:?}", status);
            self.send_presence_updates(status).await;
        }
    }

    pub async fn initialize_presence(self: &Arc<Self>) {
        let mode = self
            .presence_mode()
            .ok()
            .flatten()
            .unwrap_or_else(|| "auto".into());
        let revision = self.auth_state().get().session_revision;
        let mut retry = false;
        loop {
            if self
                .with_presence_publication(revision, || async {
                    // A queued initialization may resume after a manual choice.
                    let mode = self
                        .presence_mode()
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| "auto".into());
                    let manual = if mode == "auto" {
                        None
                    } else {
                        Some(parse_status(&mode))
                    };
                    let mut state = self.presence_controller.state.lock().await;
                    state.manual = manual;
                    state.last_activity = Instant::now();
                    let status = manual.unwrap_or(PresenceStatus::Online);
                    if retry {
                        // A cancelled send may already have updated local policy.
                        // Retry publication even when that status compares equal.
                        state.current = status;
                        self.send_presence_updates(status).await;
                    } else {
                        self.publish_presence(&mut state, status).await;
                    }
                })
                .await
            {
                break;
            }
            // A failed/cancelled deletion temporarily closes this owner. Its
            // lifecycle guard is released only after the suspension has chosen
            // resume or permanent retirement. Never wait here with policy held.
            let lifecycle = self.auth_lifecycle_lock.lock().await;
            if !self.auth_state().session_is_current(revision) {
                return;
            }
            let recovery = self.presence_controller.publications.recovery(revision);
            drop(lifecycle);
            match recovery {
                super::presence_owner::Recovery::Open => {}
                super::presence_owner::Recovery::Resuming => {
                    self.presence_controller.publications.drain().await
                }
                super::presence_owner::Recovery::Retired => return,
            }
            retry = true;
        }
        // Producer registration is short and fenced against logout. Never hold
        // policy state while waiting for lifecycle admission.
        let _lifecycle = self.auth_lifecycle_lock.lock().await;
        if !self.auth_state().session_is_current(revision)
            || !self.presence_controller.publications.accepts(revision)
        {
            return;
        }
        let mut task = self.presence_controller.idle_task.lock().await;
        if let Some(old) = task.take() {
            old.abort();
            let _ = old.await;
        }
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
                        client.refresh_identified_presence(revision).await;
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
                client
                    .with_presence_publication(revision, || async {
                        let mut state = client.presence_controller.state.lock().await;
                        if state.manual.is_none()
                            && state.current == PresenceStatus::Online
                            && state.last_activity.elapsed() > Duration::from_secs(300)
                        {
                            tracing::debug!("User idle, setting away status");
                            client
                                .publish_presence(&mut state, PresenceStatus::Away)
                                .await;
                        }
                    })
                    .await;
            }
        }));
        drop(task);
        self.spawn_presence_answer(revision).await;
        tracing::info!(mode = %mode, "Presence initialized");
    }

    /// The server fans presence out only when someone publishes, so a device
    /// that just came online saw every friend as "last seen …" until each
    /// friend's next two-minute refresh (tablet, 2026-10-29: Foxtrot and Alpha
    /// arrived 60 and 85 s after start). When a friend appears — first seen
    /// this session, back from Offline, or silent past the refresh window —
    /// publish our own status again so they see us at once. At most one
    /// answer per 15 s; a friend we already hold as present gets none, so two
    /// clients answer each other once and stop.
    async fn spawn_presence_answer(self: &Arc<Self>, revision: u64) {
        let mut events = self.services.event_bus.subscribe_domain();
        let client = self.clone();
        let handle = tokio::spawn(async move {
            let mut seen: std::collections::HashMap<
                crate::domain::ids::UserId,
                (PresenceStatus, Instant),
            > = std::collections::HashMap::new();
            let mut last_answer: Option<Instant> = None;
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let crate::events::DomainEvent::UserPresenceChanged {
                    user_id, presence, ..
                } = event
                else {
                    continue;
                };
                if client.session_current_user_id() == Some(user_id) || !client.is_friend(&user_id)
                {
                    continue;
                }
                let appeared = presence != PresenceStatus::Offline
                    && seen.get(&user_id).is_none_or(|(status, at)| {
                        *status == PresenceStatus::Offline
                            || at.elapsed() > Duration::from_secs(180)
                    });
                seen.insert(user_id, (presence, Instant::now()));
                if !appeared || last_answer.is_some_and(|at| at.elapsed() < Duration::from_secs(15))
                {
                    continue;
                }
                let current = client.presence_controller.state.lock().await.current;
                if current != PresenceStatus::Offline {
                    last_answer = Some(Instant::now());
                    client.refresh_identified_presence(revision).await;
                }
            }
        });
        if let Some(old) = self
            .presence_controller
            .peer_task
            .lock()
            .await
            .replace(handle)
        {
            old.abort();
            let _ = old.await;
        }
    }

    pub async fn record_presence_activity(&self) {
        let revision = self.auth_state().get().session_revision;
        self.with_presence_publication(revision, || async {
            let mut state = self.presence_controller.state.lock().await;
            state.last_activity = Instant::now();
            if state.manual.is_none() && state.current == PresenceStatus::Away {
                self.publish_presence(&mut state, PresenceStatus::Online)
                    .await;
            }
        })
        .await;
    }

    pub async fn set_presence_mode_choice(&self, status: &str) {
        let revision = self.auth_state().get().session_revision;
        let mode = status.to_lowercase();
        self.with_presence_publication(revision, || async {
            if let Err(e) = self.set_presence_mode(&mode) {
                tracing::warn!("Failed to persist presence mode: {e}");
            }
            let mut state = self.presence_controller.state.lock().await;
            state.manual = if mode == "auto" {
                None
            } else {
                Some(parse_status(&mode))
            };
            if state.manual.is_none() {
                state.last_activity = Instant::now();
            }
            let presence = state.manual.unwrap_or(PresenceStatus::Online);
            self.publish_presence(&mut state, presence).await;
        })
        .await;
    }

    pub async fn shutdown_presence(&self) {
        let revision = self.auth_state().get().session_revision;
        let _lifecycle = self.auth_lifecycle_lock.lock().await;
        if !self.auth_state().session_is_current(revision) {
            return;
        }
        self.set_presence_for_shutdown(PresenceStatus::Offline)
            .await;
        // All policy writers were cancelled and drained before this lock.
        self.presence_controller.state.lock().await.current = PresenceStatus::Offline;
        tracing::info!("Presence shutdown complete");
    }

    pub async fn notify_presence_chat_opened(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) {
        let revision = self.auth_state().get().session_revision;
        self.with_presence_publication(revision, || async {
            let current = self.presence_controller.state.lock().await.current;
            if current == PresenceStatus::Offline {
                return;
            }
            if let Err(e) = self
                .send_anon_dm_presence_update(conversation_id, current)
                .await
            {
                tracing::debug!("Failed to send presence on chat open: {}", e);
            }
        })
        .await;
    }

    /// Called only with lifecycle held. Stop all separately tracked producers
    /// before draining their publication registrations; no policy lock needed.
    pub(super) async fn retire_presence(&self) {
        self.presence_controller
            .publications
            .close(self.auth_state().get().session_revision);
        let idle = self.presence_controller.idle_task.lock().await.take();
        let peer = self.presence_controller.peer_task.lock().await.take();
        for task in [&idle, &peer].into_iter().flatten() {
            task.abort();
        }
        for task in [idle, peer].into_iter().flatten() {
            let _ = task.await;
        }
        self.presence_controller.publications.drain().await;
        // Cancellation can drop an actor response receiver after encryption was
        // enqueued. Fence those local writes before key/credential retirement.
        if self.signal().actor.drain_submitted().await.is_err() {
            tracing::warn!(target: "call_setup::presence_crypto_drain_failed", "Presence publication diagnostic");
        }
        tracing::info!(target: "call_setup::presence_retired", "Presence publication diagnostic");
    }

    pub(super) async fn current_presence_for_publication(&self) -> PresenceStatus {
        self.presence_controller.state.lock().await.current
    }

    pub async fn current_presence_status(&self) -> String {
        self.presence_controller
            .state
            .lock()
            .await
            .current
            .as_str()
            .into()
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

#[cfg(test)]
#[path = "presence_tests.rs"]
mod tests;
