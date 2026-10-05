//! System tray for Windows.
//!
//! System tray integration using tray-icon and muda crates.

use super::platform::APP_DISPLAY_NAME;
use crate::error::SystemError;
use tokio::sync::mpsc;

/// Messages emitted by the tray implementation (menu/activate events).
///
/// Same shape as the Linux tray: user-driven events only. Controller
/// mutations go through `SystemTray`'s typed methods.
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

/// System tray icon controller.
#[derive(Clone)]
pub struct SystemTray {
    sender: mpsc::Sender<TrayMessage>,
}

impl SystemTray {
    /// Spawn the system tray service and return a controller and handle.
    pub fn spawn() -> Result<(Self, TrayHandle), SystemError> {
        let (tx, rx) = mpsc::channel::<TrayMessage>(16);
        let (event_tx, event_rx) = mpsc::channel::<TrayMessage>(16);

        // Create tray icon in a separate thread (needs its own message loop on Windows)
        let handle = std::thread::spawn(move || {
            Self::run_tray_loop(event_tx);
        });

        let controller = Self { sender: tx.clone() };
        let tray_handle = TrayHandle {
            thread_handle: Some(handle),
            sender: tx,
        };

        // Forward events
        tokio::spawn(async move {
            let mut rx = rx;
            let mut event_rx = event_rx;

            loop {
                tokio::select! {
                    Some(msg) = rx.recv() => {
                        if let TrayMessage::Quit = msg { break }
                    }
                    Some(event) = event_rx.recv() => {
                        match event {
                            TrayMessage::Quit => break,
                            TrayMessage::OpenWindow => {
                                // Would trigger window show
                            }
                        }
                    }
                    else => break,
                }
            }
        });

        Ok((controller, tray_handle))
    }

    /// Windows tray message loop.
    fn run_tray_loop(event_tx: mpsc::Sender<TrayMessage>) {
        use muda::{Menu, MenuItem, PredefinedMenuItem};
        use tray_icon::{Icon, TrayIconBuilder};

        // Create menu
        let menu = Menu::new();
        let open_item = MenuItem::new(&format!("Open {}", APP_DISPLAY_NAME), true, None);
        let quit_item = MenuItem::new("Quit", true, None);
        if let Err(e) = menu.append(&open_item) {
            tracing::warn!("Failed to add Open menu item: {}", e);
        }
        if let Err(e) = menu.append(&PredefinedMenuItem::separator()) {
            tracing::warn!("Failed to add menu separator: {}", e);
        }
        if let Err(e) = menu.append(&quit_item) {
            tracing::warn!("Failed to add Quit menu item: {}", e);
        }

        // Create a simple 16x16 RGBA icon (blue square as placeholder)
        let icon_data = create_default_icon();
        let icon = Icon::from_rgba(icon_data, 16, 16).unwrap_or_else(|_| {
            // Fallback to empty icon
            Icon::from_rgba(vec![0u8; 16 * 16 * 4], 16, 16).expect("fallback icon")
        });

        let _tray_icon = match TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(APP_DISPLAY_NAME)
            .with_icon(icon)
            .build()
        {
            Ok(tray) => tray,
            Err(e) => {
                tracing::error!("Failed to create tray icon: {}", e);
                return;
            }
        };

        // Handle menu events
        let menu_channel = muda::MenuEvent::receiver();
        let open_id = open_item.id().clone();
        let quit_id = quit_item.id().clone();

        loop {
            if let Ok(event) = menu_channel.recv() {
                if event.id == open_id {
                    if let Err(e) = event_tx.try_send(TrayMessage::OpenWindow) {
                        tracing::warn!("Failed to send OpenWindow tray message: {}", e);
                    }
                } else if event.id == quit_id {
                    if let Err(e) = event_tx.try_send(TrayMessage::Quit) {
                        tracing::warn!("Failed to send Quit tray message: {}", e);
                    }
                    break;
                }
            }
        }
    }

    /// Update the unread message count badge.
    ///
    /// No-op on Windows until the tray-icon/muda backend exposes a way to
    /// redraw the icon from outside the message loop. Kept so the
    /// cross-platform `SystemTray` API surface matches the Linux impl.
    pub fn set_unread_count(&self, _count: u32) {}

    /// Update the tray status indicator.
    ///
    /// No-op on Windows (see `set_unread_count`).
    pub fn set_status(&self, _status: TrayStatus) {}

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

/// Create a simple 16x16 blue icon as default tray icon.
fn create_default_icon() -> Vec<u8> {
    let mut data = Vec::with_capacity(16 * 16 * 4);
    for _ in 0..(16 * 16) {
        // Chatter blue: #4A90D9
        data.push(74); // R
        data.push(144); // G
        data.push(217); // B
        data.push(255); // A
    }
    data
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
    fn test_create_default_icon() {
        let icon = create_default_icon();
        assert_eq!(icon.len(), 16 * 16 * 4);
    }
}
