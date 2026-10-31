use super::*;

impl ChatterClient {
    // ── Lifecycle ───────────────────────────────────────────────────

    /// Graceful shutdown. Stops inbox, aborts background tasks, disconnects.
    pub async fn shutdown(&self) {
        let revision = self.auth_state().get().session_revision;
        let _lifecycle = self.auth_lifecycle_lock.lock().await;
        if self.auth_state().get().session_revision != revision {
            return;
        }
        self.auth_state().invalidate_session();
        self.shutdown_session(self.auth_state().get().session_revision, true)
            .await;
    }

    pub(super) async fn shutdown_session(&self, revision: u64, publish_offline: bool) {
        if self.auth_state().get().session_revision != revision {
            return;
        }
        self.retire_presence().await;
        crate::system::messaging_background::set_signed_in(false);
        if let Some(task) = self.mailbox_wake_task.lock().take() {
            task.abort();
        }

        self.channel_delivery_shutdown.store(true, Ordering::SeqCst);
        let handles = std::mem::take(&mut *self.tasks.lock());
        for handle in &handles {
            handle.abort();
        }
        for handle in handles {
            let _ = handle.await;
        }
        let retired = self.authed_notif_session.lock().unwrap_or(revision);
        self.stop_authed_notification_streams(retired).await;
        #[cfg(feature = "media")]
        if let Some(worker) = self.call_service().get() {
            let actor_revision = worker
                .incoming_presentation_state()
                .session_revision()
                .unwrap_or(retired);
            match worker.reset_presentation_session(actor_revision).await {
                Ok(_) => {
                    if self.auth_state().get().session_revision == revision {
                        if let Some(signaling) = self.call_signaling.get() {
                            signaling.clear_call_session();
                        }
                    }
                }
                Err(error) => tracing::error!(%error, "call session reset failed"),
            }
        }
        // Leave any active call first. The call worker holds capture
        // threads (audio, video, screen) and a LiveKit Room whose
        // RoomManager pumps frames into `SharedVideoFrames`, which in
        // turn pushes UI updates via `slint::invoke_from_event_loop`.
        // If we don't end the call before signalling shutdown, those
        // pushes keep saturating the Slint event queue and a posted
        // `quit_event_loop` request can't drain — the window won't
        // close. Ending the call here stops the frame pump at the
        // source so the loop falls idle and quits.
        #[cfg(feature = "media")]
        {
            if let Err(e) = self.call_leave_room().await {
                tracing::warn!("Error leaving call during shutdown: {}", e);
            }
        }

        // Signal the ntfy push listener (if any) to exit cleanly so its SSE
        // connection closes instead of being torn down via `abort()`.
        if let Some(tx) = self.push_shutdown.lock().take() {
            let _ = tx.send(());
        }

        // Stop inbox provider
        if let Some(provider) = self.ctx.get_device_inbox_provider() {
            if let Err(e) = provider.stop().await {
                tracing::warn!("Error stopping inbox provider: {}", e);
            }
        }

        crate::network::streaming::mailbox_wakes::clear();

        // Ask the sync engine to exit at its next select point; the abort
        // below is the backstop if it's mid-cycle.
        self.services.sync_service.stop();

        // Abort background tasks
        let handles: Vec<_> = {
            let mut tasks = self.tasks.lock();
            std::mem::take(&mut *tasks)
        };
        for handle in &handles {
            handle.abort();
        }
        for handle in handles {
            let _ = handle.await;
        }

        // Abort the separately-tracked gRPC notification stream.
        if let Some(handle) = self.notif_stream_handle.lock().take() {
            handle.abort();
        }

        // Publish OFFLINE presence so our friends see us go offline. Best-effort
        // and done BEFORE the gRPC disconnect below so the RPC can still reach the
        // server (set_presence awaits the identified UpdatePresence internally).
        // Deleted accounts have already lost their auth token and cannot
        // publish presence; avoid delaying the local session transition.
        if publish_offline {
            self.set_presence_for_shutdown(crate::domain::user::PresenceStatus::Offline)
                .await;
        }

        // Disconnect gRPC
        self.services.auth_service.disconnect();

        self.started.store(false, Ordering::SeqCst);
    }

}
