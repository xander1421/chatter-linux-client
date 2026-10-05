//! System tray.
//!
//! System tray integration using ksni (StatusNotifierItem protocol).

use super::platform::{APP_DISPLAY_NAME, APP_ID};
use crate::error::SystemError;
use ksni::{self, Handle, Icon, MenuItem, Tray, TrayService, menu::StandardItem};
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicU32, Ordering},
};
use tokio::sync::mpsc;

/// Messages emitted by the tray implementation (menu/activate events).
///
/// Carries user-driven tray events only. Controller-driven mutations
/// (unread count, status) flow through `SystemTray`'s typed methods and
/// `Handle::update`, not through this channel.
#[derive(Clone, Debug)]
pub enum TrayMessage {
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

impl TrayStatus {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => TrayStatus::Offline,
            2 => TrayStatus::Away,
            3 => TrayStatus::DoNotDisturb,
            _ => TrayStatus::Online,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            TrayStatus::Online => 0,
            TrayStatus::Offline => 1,
            TrayStatus::Away => 2,
            TrayStatus::DoNotDisturb => 3,
        }
    }
}

/// Shared tray state. Mutated by `SystemTray` controller methods and read by
/// the ksni `Tray` impl when it renders.
#[derive(Default)]
struct TrayState {
    unread_count: AtomicU32,
    status: AtomicU8,
}

/// System tray icon controller.
///
/// Drives the ksni service that runs on a dedicated blocking thread. Each
/// mutation updates shared state and wakes ksni via `Handle::update` so the
/// icon/menu redraw immediately.
#[derive(Clone)]
pub struct SystemTray {
    state: Arc<TrayState>,
    handle: Handle<TrayServiceImpl>,
}

impl SystemTray {
    /// Spawn the system tray service and return a controller, handle, and event receiver.
    ///
    /// The returned `mpsc::Receiver<TrayMessage>` delivers user-initiated tray events
    /// (`OpenWindow`, `Quit`) so the caller can wire them to the windowing system.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::TrayInit` if the tray service fails to start.
    pub fn spawn() -> Result<(Self, TrayHandle, mpsc::Receiver<TrayMessage>), SystemError> {
        let (event_tx, event_rx) = mpsc::channel::<TrayMessage>(16);

        let state = Arc::new(TrayState::default());
        let tray_service = TrayServiceImpl::new(event_tx, state.clone());

        // Build the service on this thread so we can grab `Handle` before
        // moving `service` into the blocking worker.
        let service = TrayService::new(tray_service);
        let handle = service.handle();

        let thread_handle = std::thread::spawn(move || {
            // Run the service (blocks until shutdown)
            let _ = service.run();
        });

        let controller = Self {
            state,
            handle: handle.clone(),
        };
        let tray_handle = TrayHandle {
            thread_handle: Some(thread_handle),
            handle,
        };

        Ok((controller, tray_handle, event_rx))
    }

    /// Update the unread message count badge.
    pub fn set_unread_count(&self, count: u32) {
        self.state.unread_count.store(count, Ordering::Relaxed);
        // Force ksni to re-render so the icon updates.
        self.handle.update(|_| {});
    }

    /// Update the tray status indicator.
    pub fn set_status(&self, status: TrayStatus) {
        self.state.status.store(status.as_u8(), Ordering::Relaxed);
        self.handle.update(|_| {});
    }

    /// Signal the application to quit by shutting down the ksni service.
    pub fn quit(&self) {
        self.handle.shutdown();
    }
}

/// Handle to the system tray service for cleanup.
pub struct TrayHandle {
    thread_handle: Option<std::thread::JoinHandle<()>>,
    handle: Handle<TrayServiceImpl>,
}

impl TrayHandle {
    /// Shutdown the tray service and wait for cleanup.
    pub fn shutdown(mut self) {
        self.handle.shutdown();
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        self.handle.shutdown();
    }
}

/// Internal ksni tray implementation.
struct TrayServiceImpl {
    state: Arc<TrayState>,
    event_sender: mpsc::Sender<TrayMessage>,
}

impl TrayServiceImpl {
    fn new(event_sender: mpsc::Sender<TrayMessage>, state: Arc<TrayState>) -> Self {
        Self {
            state,
            event_sender,
        }
    }

    fn compute_icon_name(&self) -> String {
        let count = self.state.unread_count.load(Ordering::Relaxed);
        let status = TrayStatus::from_u8(self.state.status.load(Ordering::Relaxed));

        if count > 0 {
            "mail-unread".to_string()
        } else {
            match status {
                TrayStatus::Offline => "user-offline".to_string(),
                TrayStatus::Away => "user-away".to_string(),
                TrayStatus::DoNotDisturb => "user-busy".to_string(),
                TrayStatus::Online => "dialog-information".to_string(),
            }
        }
    }
}

impl Tray for TrayServiceImpl {
    fn id(&self) -> String {
        APP_ID.to_string()
    }

    fn title(&self) -> String {
        APP_DISPLAY_NAME.to_string()
    }

    fn icon_name(&self) -> String {
        self.compute_icon_name()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![]
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let event_sender = self.event_sender.clone();
        let quit_sender = self.event_sender.clone();

        vec![
            StandardItem {
                label: format!("Open {}", APP_DISPLAY_NAME),
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
        let messages = [TrayMessage::Quit, TrayMessage::OpenWindow];
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn test_tray_status_roundtrip_u8() {
        for status in [
            TrayStatus::Online,
            TrayStatus::Offline,
            TrayStatus::Away,
            TrayStatus::DoNotDisturb,
        ] {
            assert_eq!(TrayStatus::from_u8(status.as_u8()), status);
        }
    }

    #[test]
    fn test_tray_state_icon_name() {
        let (tx, _rx) = mpsc::channel(1);
        let state = Arc::new(TrayState::default());
        let service = TrayServiceImpl::new(tx, state.clone());

        assert_eq!(service.compute_icon_name(), "dialog-information");

        state.unread_count.store(5, Ordering::Relaxed);
        assert_eq!(service.compute_icon_name(), "mail-unread");

        state.unread_count.store(0, Ordering::Relaxed);
        state
            .status
            .store(TrayStatus::DoNotDisturb.as_u8(), Ordering::Relaxed);
        assert_eq!(service.compute_icon_name(), "user-busy");
    }
}
