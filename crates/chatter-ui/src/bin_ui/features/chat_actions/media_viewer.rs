//! Gallery controls operate on attachments of one message, in their stored order.
#[cfg(target_os = "android")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
#[cfg(target_os = "android")]
static HAS_PREVIOUS: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "android")]
static HAS_NEXT: AtomicBool = AtomicBool::new(false);

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::AttachmentId;
use slint::{ComponentHandle, Model};

use crate::MainWindow;

static WINDOW: OnceLock<slint::Weak<MainWindow>> = OnceLock::new();
static REQUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) fn invalidate() {
    REQUEST.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}
pub(crate) fn is_current(request: u64) -> bool {
    REQUEST.load(std::sync::atomic::Ordering::SeqCst) == request
}
pub(crate) fn current_request() -> u64 {
    REQUEST.load(std::sync::atomic::Ordering::SeqCst)
}

fn gallery(w: &MainWindow, current: &str) -> Vec<String> {
    w.get_messages()
        .iter()
        .chain(w.get_thread_replies().iter())
        .find_map(|message| {
            let attachments: Vec<_> = message.attachments.iter().collect();
            attachments.iter().any(|a| a.id == current).then(|| {
                attachments
                    .iter()
                    .filter(|a| a.is_image || a.is_video)
                    .map(|a| a.id.to_string())
                    .collect()
            })
        })
        .unwrap_or_default()
}

pub(super) fn select(w: &MainWindow, id: &str) -> u64 {
    let request = REQUEST.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    #[cfg(target_os = "android")]
    crate::platform::android::media::invalidate_pending_video();
    let items = gallery(w, id);
    let index = items.iter().position(|item| item == id);
    #[cfg(target_os = "android")]
    {
        HAS_PREVIOUS.store(index.is_some_and(|i| i > 0), Ordering::Relaxed);
        HAS_NEXT.store(
            index.is_some_and(|i| i + 1 < items.len()),
            Ordering::Relaxed,
        );
    }
    w.set_media_viewer_attachment_id(id.into());
    w.set_media_viewer_has_previous(index.is_some_and(|i| i > 0));
    w.set_media_viewer_has_next(index.is_some_and(|i| i + 1 < items.len()));
    w.set_media_viewer_position(
        index
            .map(|i| format!("{} / {}", i + 1, items.len()))
            .unwrap_or_default()
            .into(),
    );
    request
}

fn navigate(w: &MainWindow, delta: i32) {
    let current = w.get_media_viewer_attachment_id();
    let items = gallery(w, current.as_str());
    let Some(index) = items.iter().position(|item| item == current.as_str()) else {
        return;
    };
    let next = index as i64 + i64::from(delta.signum());
    if next < 0 || next >= items.len() as i64 {
        return;
    }
    crate::bin_ui::gif_state::stop_gif_animation();
    crate::bin_ui::video_state::stop_video();
    w.set_image_viewer_visible(false);
    w.set_video_player_visible(false);
    w.invoke_view_attachment(items[next as usize].clone().into());
}

/// Native video controls use the same gallery selection as image controls.
#[cfg(target_os = "android")]
pub(crate) fn native_action(action: i32, request: u64) {
    if let Some(window) = WINDOW.get() {
        let _ = window.upgrade_in_event_loop(move |w| {
            if !is_current(request) {
                return;
            }
            match action {
                -1 | 1 => navigate(&w, action),
                2 => w.invoke_media_viewer_export(false),
                3 => w.invoke_media_viewer_open_downloads(),
                _ => (),
            }
        });
    }
}

#[cfg(target_os = "android")]
pub(crate) fn native_can_navigate(delta: i32) -> bool {
    match delta {
        -1 => HAS_PREVIOUS.load(Ordering::Relaxed),
        1 => HAS_NEXT.load(Ordering::Relaxed),
        _ => false,
    }
}

pub fn wire(window: &MainWindow, client: &Arc<ChatterClient>) {
    let _ = WINDOW.set(window.as_weak());
    let weak = window.as_weak();
    window.on_media_viewer_navigate(move |delta| {
        if let Some(w) = weak.upgrade() {
            navigate(&w, delta);
        }
    });
    let weak = window.as_weak();
    let client = client.clone();
    window.on_media_viewer_export(move |copy| {
        let Some(w) = weak.upgrade() else { return };
        let selected = w.get_media_viewer_attachment_id();
        if selected.is_empty() {
            return;
        }
        let id = selected
            .parse::<AttachmentId>()
            .unwrap_or_else(|_| AttachmentId::from_cdn_key(selected.as_str()));
        let client = client.clone();
        let weak = weak.clone();
        // Decryption, image decoding and file operations stay off the UI thread.
        std::thread::spawn(move || {
            let result = export(&client, &id, copy);
            let _ = weak.upgrade_in_event_loop(move |w| {
                match result {
                    Ok(message) => {
                        w.set_toast_message(message.into());
                        w.set_toast_variant("success".into());
                    }
                    Err(_) => {
                        w.set_toast_message("Unable to export this media".into());
                        w.set_toast_variant("error".into());
                    }
                }
                w.set_toast_visible(true);
            });
        });
    });
    window.on_media_viewer_open_downloads(|| {
        #[cfg(target_os = "android")]
        let _ = crate::platform::android::media_export::open_downloads();
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if let Some(path) = dirs::download_dir() {
            let folder = path.join("Chatter");
            let _ = std::fs::create_dir_all(&folder);
            let _ = chatter_core::system::ExternalOpener.open_folder(&folder);
        }
    });
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
static CLIPBOARD: std::sync::Mutex<Option<arboard::Clipboard>> = std::sync::Mutex::new(None);

fn stream_export(
    reader: &chatter_core::services::attachment::CbcRangeDecryptor,
    output: &mut std::fs::File,
) -> Result<(), String> {
    use std::io::Write;
    use zeroize::Zeroize;
    let mut offset = 0;
    while offset < reader.plaintext_size() {
        let expected = (reader.plaintext_size() - offset).min(64 * 1024);
        let mut chunk = reader.read_range(offset, expected);
        if chunk.len() != expected {
            chunk.zeroize();
            return Err("Incomplete authenticated read".into());
        }
        let result = output.write_all(&chunk);
        chunk.zeroize();
        result.map_err(|_| "Download write failed")?;
        offset += expected;
    }
    output.sync_all().map_err(|_| "Unable to finish download")
}

fn export(client: &ChatterClient, id: &AttachmentId, copy: bool) -> Result<String, String> {
    let attachment = client.get_attachment(id).ok_or("Attachment unavailable")?;
    // Whole-file authentication precedes creating any plaintext destination.
    let reader = client
        .open_attachment_range(id)
        .map_err(|_| "Authenticated source unavailable")?;
    #[cfg(target_os = "android")]
    {
        let mut session = crate::platform::android::media_export::ExportSession::begin(
            &attachment.metadata.name,
            &attachment.metadata.mime_type,
            copy,
        )?;
        stream_export(
            &reader,
            session
                .file
                .as_mut()
                .ok_or("Export descriptor unavailable")?,
        )?;
        session.finish()?;
        return Ok(if copy {
            "Image copied"
        } else {
            "Saved to Chatter downloads"
        }
        .into());
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        if copy {
            // Image decoding is bounded by ImageProcessor, and encoded input is bounded too.
            if reader.plaintext_size() > 32 * 1024 * 1024 {
                return Err("Image too large for clipboard".into());
            }
            let image = chatter_core::media::ImageProcessor::load_from_memory(
                &reader.read_range(0, reader.plaintext_size()),
            )
            .map_err(|_| "Image decoding failed")?
            .to_rgba8();
            let (width, height) = image.dimensions();
            // Keep ownership for the application lifetime on Wayland and X11.
            let mut clipboard = CLIPBOARD.lock().map_err(|_| "Clipboard unavailable")?;
            if clipboard.is_none() {
                *clipboard = Some(arboard::Clipboard::new().map_err(|_| "Clipboard unavailable")?);
            }
            clipboard
                .as_mut()
                .ok_or("Clipboard unavailable")?
                .set_image(arboard::ImageData {
                    width: width as usize,
                    height: height as usize,
                    bytes: std::borrow::Cow::Owned(image.into_raw()),
                })
                .map_err(|_| "Image copy failed")?;
            return Ok("Image copied".into());
        }
        let folder = dirs::download_dir()
            .ok_or("Downloads folder unavailable")?
            .join("Chatter");
        std::fs::create_dir_all(&folder).map_err(|_| "Unable to create download folder")?;
        let raw = attachment
            .metadata
            .name
            .rsplit(['/', '\\'])
            .next()
            .filter(|s| !s.is_empty() && *s != "." && *s != "..")
            .unwrap_or("media");
        let mut safe: String = raw
            .chars()
            .filter(|c| !c.is_control() && !"<>:\"|?*".contains(*c))
            .collect();
        while safe.len() > 180 {
            safe.pop();
        }
        if safe.is_empty() {
            safe = "media".into();
        }
        let mut created = None;
        for suffix in 0..1024 {
            let path = folder.join(if suffix == 0 {
                format!("{id}-{safe}")
            } else {
                format!("{id}-{suffix}-{safe}")
            });
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    created = Some((path, file));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err("Download cannot be created".into()),
            }
        }
        let (path, mut output) = created.ok_or("Download name unavailable")?;
        if let Err(error) = stream_export(&reader, &mut output) {
            drop(output);
            let _ = std::fs::remove_file(path);
            return Err(error);
        }
        Ok("Saved to Downloads/Chatter".into())
    }
    #[cfg(target_os = "ios")]
    {
        let _ = (attachment, reader, copy);
        Err("Media export unavailable".into())
    }
}
