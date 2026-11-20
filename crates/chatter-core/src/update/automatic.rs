//! Event-driven update discovery. No periodic manifest polling.
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

pub struct AutomaticChecks {
    hints: watch::Receiver<u64>,
    visible: watch::Receiver<bool>,
    pending: bool,
    due: Instant,
}

impl AutomaticChecks {
    pub fn for_client(hints: watch::Receiver<u64>) -> Self {
        Self::new(hints, crate::network::connection::subscribe_app_visible())
    }

    pub fn new(hints: watch::Receiver<u64>, visible: watch::Receiver<bool>) -> Self {
        Self {
            hints,
            visible,
            pending: true,
            due: Instant::now() + Duration::from_secs(5),
        }
    }

    /// Launch, publication/reconnect hint, or foreground return triggers one
    /// check. Bursts coalesce; hidden apps wait for foreground, retaining hints.
    pub async fn next(&mut self) -> bool {
        loop {
            if self.visible.has_changed().unwrap_or(false) && *self.visible.borrow() {
                self.pending = true;
            }
            let visible = *self.visible.borrow_and_update();
            tokio::select! {
                _ = tokio::time::sleep_until(self.due), if self.pending && visible => {
                    self.pending = false;
                    self.hints.borrow_and_update();
                    self.due = Instant::now() + Duration::from_secs(5);
                    return true;
                }
                result = self.hints.changed() => {
                    if result.is_err() { return false; }
                    self.hints.borrow_and_update();
                    self.pending = true;
                }
                result = self.visible.changed() => {
                    if result.is_err() { return false; }
                    if *self.visible.borrow_and_update() { self.pending = true; }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn launch_and_publication_trigger_checks_without_periodic_polling() {
        let (hints, rx) = watch::channel(0);
        let (_visibility, visible) = watch::channel(true);
        let mut checks = AutomaticChecks::new(rx, visible);
        assert!(checks.next().await);
        assert!(
            tokio::time::timeout(Duration::from_secs(24 * 3600), checks.next())
                .await
                .is_err()
        );
        hints.send_replace(1);
        assert!(checks.next().await);
        hints.send_replace(2);
        hints.send_replace(3);
        assert!(checks.next().await);
        assert!(
            tokio::time::timeout(Duration::from_secs(3600), checks.next())
                .await
                .is_err()
        );
    }
    #[tokio::test(start_paused = true)]
    async fn hidden_clients_retain_hints_and_check_on_resume() {
        let (hints, rx) = watch::channel(0);
        let (visibility, visible) = watch::channel(false);
        let mut checks = AutomaticChecks::new(rx, visible);
        hints.send_replace(1);
        assert!(
            tokio::time::timeout(Duration::from_secs(60), checks.next())
                .await
                .is_err()
        );
        visibility.send_replace(true);
        assert!(checks.next().await);
        visibility.send_replace(false);
        assert!(
            tokio::time::timeout(Duration::from_secs(60), checks.next())
                .await
                .is_err()
        );
        visibility.send_replace(true);
        assert!(checks.next().await);
    }
}
