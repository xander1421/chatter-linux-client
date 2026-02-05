//! System tray.
//!
//! System tray integration using ksni (StatusNotifierItem protocol).

use crate::error::SystemError;
use ksni::{self, menu::StandardItem, Icon, MenuItem, Tray, TrayService};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use tokio::sync::mpsc;

/// Messages that can be sent to the system tray.
#[derive(Clone, Debug)]
pub enum TrayMessage {
    /// Update the unread message count badge.
    SetUnreadCount(u32),
    /// Update the tray status indicator.
    SetStatus(TrayStatus),
    /// Signal the application to quit.
    Quit,
    /// Signal to open the main window.
    OpenWindow,
}

/// Tray icon status indicator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrayStatus {
    /// User is online and available.
    #[default]
    Online,
    /// User is offline/disconnected.
    Offline,
    /// User is away.
    Away,
    /// User has enabled Do Not Disturb.
    DoNotDisturb,
}

/// System tray icon controller.
///
/// Provides methods to update the system tray state.
/// The actual tray service runs in a separate thread.
#[derive(Clone)]
pub struct SystemTray {
    sender: mpsc::Sender<TrayMessage>,
}

impl SystemTray {
    /// Spawn the system tray service and return a controller and handle.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::TrayInit` if the tray service fails to start.
    pub fn spawn() -> Result<(Self, TrayHandle), SystemError> {
        let (tx, rx) = mpsc::channel::<TrayMessage>(16);
        let (event_tx, event_rx) = mpsc::channel::<TrayMessage>(16);

        let tray_service = TrayServiceImpl::new(event_tx);

        // Spawn ksni service in blocking thread (it uses glib main loop)
        let handle = std::thread::spawn(move || {
            let service = TrayService::new(tray_service);
            let service_handle = service.handle();

            // Run the service (this blocks until shutdown)
            let _ = service.run();

            drop(service_handle);
        });

        // Spawn message handler task
        let controller = Self { sender: tx.clone() };
        let tray_handle = TrayHandle {
            thread_handle: Some(handle),
            sender: tx,
        };

        // Spawn a task to forward messages (if needed for future state updates)
        tokio::spawn(async move {
            let mut rx = rx;
            let mut event_rx = event_rx;

            loop {
                tokio::select! {
                    Some(msg) = rx.recv() => {
                        if let TrayMessage::Quit = msg { break }
                    }
                    Some(event) = event_rx.recv() => {
                        // Forward events from tray callbacks
                        // In a real app, this would notify the app of menu clicks
                        match event {
                            TrayMessage::Quit => break,
                            TrayMessage::OpenWindow => {
                                // Would trigger window show
                            }
                            _ => {}
                        }
                    }
                    else => break,
                }
            }
        });

        Ok((controller, tray_handle))
    }

    /// Update the unread message count badge.
    pub fn set_unread_count(&self, count: u32) {
        let _ = self.sender.try_send(TrayMessage::SetUnreadCount(count));
    }

    /// Update the tray status indicator.
    pub fn set_status(&self, status: TrayStatus) {
        let _ = self.sender.try_send(TrayMessage::SetStatus(status));
    }

    /// Signal the application to quit.
    pub fn quit(&self) {
        let _ = self.sender.try_send(TrayMessage::Quit);
    }
}

/// Handle to the system tray service for cleanup.
pub struct TrayHandle {
    thread_handle: Option<std::thread::JoinHandle<()>>,
    sender: mpsc::Sender<TrayMessage>,
}

impl TrayHandle {
    /// Shutdown the tray service and wait for cleanup.
    pub fn shutdown(mut self) {
        let _ = self.sender.try_send(TrayMessage::Quit);
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        let _ = self.sender.try_send(TrayMessage::Quit);
    }
}

/// Internal ksni tray implementation.
struct TrayServiceImpl {
    unread_count: Arc<AtomicU32>,
    event_sender: mpsc::Sender<TrayMessage>,
}

impl TrayServiceImpl {
    fn new(event_sender: mpsc::Sender<TrayMessage>) -> Self {
        Self {
            unread_count: Arc::new(AtomicU32::new(0)),
            event_sender,
        }
    }

    fn icon_name(&self) -> String {
        let count = self.unread_count.load(Ordering::Relaxed);
        if count > 0 {
            // Use generic icon with badge indicator
            // Custom icons would be: "chatter-linux-unread"
            "mail-unread".to_string()
        } else {
            // Default icon
            // Custom icon would be: "chatter-linux"
            "dialog-information".to_string()
        }
    }
}

impl Tray for TrayServiceImpl {
    fn id(&self) -> String {
        "chatter-linux".to_string()
    }

    fn title(&self) -> String {
        "Chatter".to_string()
    }

    fn icon_name(&self) -> String {
        self.icon_name()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![]
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let event_sender = self.event_sender.clone();
        let quit_sender = self.event_sender.clone();

        vec![
            StandardItem {
                label: "Open Chatter".to_string(),
                activate: Box::new(move |_| {
                    let _ = event_sender.try_send(TrayMessage::OpenWindow);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".to_string(),
                activate: Box::new(move |_| {
                    let _ = quit_sender.try_send(TrayMessage::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.event_sender.try_send(TrayMessage::OpenWindow);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tray_status_variants() {
        let statuses = [
            TrayStatus::Online,
            TrayStatus::Offline,
            TrayStatus::Away,
            TrayStatus::DoNotDisturb,
        ];

        assert_eq!(statuses.len(), 4);
        assert_eq!(TrayStatus::default(), TrayStatus::Online);
    }

    #[test]
    fn test_tray_message_variants() {
        let messages = [
            TrayMessage::SetUnreadCount(5),
            TrayMessage::SetStatus(TrayStatus::Online),
            TrayMessage::Quit,
            TrayMessage::OpenWindow,
        ];

        // Just verify they can be constructed
        assert_eq!(messages.len(), 4);
    }

    #[test]
    fn test_tray_service_icon_name() {
        let (tx, _rx) = mpsc::channel(1);
        let service = TrayServiceImpl::new(tx);

        // Default: no unread messages
        assert_eq!(service.icon_name(), "dialog-information");

        // Set unread count
        service.unread_count.store(5, Ordering::Relaxed);
        assert_eq!(service.icon_name(), "mail-unread");

        // Back to zero
        service.unread_count.store(0, Ordering::Relaxed);
        assert_eq!(service.icon_name(), "dialog-information");
    }
}
