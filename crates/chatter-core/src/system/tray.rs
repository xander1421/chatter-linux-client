//! System tray.
//!
//! System tray integration using ksni (StatusNotifierItem protocol).

use super::platform::{APP_DISPLAY_NAME, APP_ID};
use crate::error::SystemError;
use ksni::blocking::{Handle, TrayMethods};
use ksni::{self, Icon, MenuItem, Tray, menu::StandardItem};
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};
use tokio::sync::mpsc;

/// Messages emitted by the tray implementation (menu/activate events).
///
/// Carries user-driven tray events only. Controller-driven mutations
/// (the unread count) flow through `SystemTray`'s typed methods and
/// `Handle::update`, not through this channel.
#[derive(Clone, Debug)]
pub enum TrayMessage {
    /// Signal the application to quit.
    Quit,
    /// Signal to open the main window.
    OpenWindow,
}

/// Shared tray state. Mutated by `SystemTray` controller methods and read by
/// the ksni `Tray` impl when it renders.
#[derive(Default)]
struct TrayState {
    unread_count: AtomicU32,
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

        let handle = tray_service
            .spawn()
            .map_err(|e| SystemError::TrayInit(e.to_string()))?;

        let controller = Self {
            state,
            handle: handle.clone(),
        };
        let tray_handle = TrayHandle { handle };

        Ok((controller, tray_handle, event_rx))
    }

    /// Update the unread message count badge.
    pub fn set_unread_count(&self, count: u32) {
        self.state.unread_count.store(count, Ordering::Relaxed);
        // Force ksni to re-render so the icon updates.
        self.handle.update(|_| {});
    }

    /// Signal the application to quit by shutting down the ksni service.
    pub fn quit(&self) {
        self.handle.shutdown();
    }
}

/// Handle to the system tray service for cleanup.
pub struct TrayHandle {
    handle: Handle<TrayServiceImpl>,
}

impl TrayHandle {
    /// Shutdown the tray service and wait for cleanup.
    pub fn shutdown(self) {
        self.handle.shutdown().wait();
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

    /// Unread messages ask the host for attention; the icon stays Chatter's.
    fn compute_status(&self) -> ksni::Status {
        if self.state.unread_count.load(Ordering::Relaxed) > 0 {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }
}

/// The Chatter app icon as StatusNotifierItem pixmaps (ARGB32, network byte
/// order). Sent as pixmaps rather than an icon-theme name so the tray shows
/// the real icon whether or not the hicolor icons are installed.
fn app_icon_pixmaps() -> Vec<Icon> {
    static ICONS: std::sync::OnceLock<Vec<Icon>> = std::sync::OnceLock::new();
    ICONS
        .get_or_init(|| {
            [
                &include_bytes!("../../../../assets/icon-32.png")[..],
                &include_bytes!("../../../../assets/icon-64.png")[..],
            ]
            .iter()
            .filter_map(|png| image::load_from_memory(png).ok())
            .map(|img| {
                let rgba = img.to_rgba8();
                let (width, height) = rgba.dimensions();
                let data = rgba
                    .pixels()
                    .flat_map(|p| [p[3], p[0], p[1], p[2]])
                    .collect();
                Icon {
                    width: width as i32,
                    height: height as i32,
                    data,
                }
            })
            .collect()
        })
        .clone()
}

impl Tray for TrayServiceImpl {
    fn id(&self) -> String {
        APP_ID.to_string()
    }

    fn title(&self) -> String {
        APP_DISPLAY_NAME.to_string()
    }

    fn status(&self) -> ksni::Status {
        self.compute_status()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        app_icon_pixmaps()
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let event_sender = self.event_sender.clone();
        let quit_sender = self.event_sender.clone();

        vec![
            StandardItem {
                label: chatter_i18n::trf("Open {}", &[&APP_DISPLAY_NAME]),
                activate: Box::new(move |_| {
                    let _ = event_sender.try_send(TrayMessage::OpenWindow);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: chatter_i18n::tr("Quit"),
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
    fn test_tray_message_variants() {
        let messages = [TrayMessage::Quit, TrayMessage::OpenWindow];
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn test_tray_status_follows_unread() {
        let (tx, _rx) = mpsc::channel(1);
        let state = Arc::new(TrayState::default());
        let service = TrayServiceImpl::new(tx, state.clone());

        assert_eq!(service.compute_status(), ksni::Status::Active);
        state.unread_count.store(5, Ordering::Relaxed);
        assert_eq!(service.compute_status(), ksni::Status::NeedsAttention);
    }

    #[test]
    fn test_tray_icon_is_the_app_icon() {
        let icons = app_icon_pixmaps();
        assert_eq!(icons.len(), 2);
        for icon in icons {
            assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
        }
    }
}
