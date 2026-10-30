//! Gallery controls for the currently displayed conversation.
#[cfg(target_os = "android")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
#[cfg(target_os = "android")]
static HAS_PREVIOUS: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "android")]
static HAS_NEXT: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "android")]
static POSITION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::{AttachmentId, MessageId};
use chatter_core::services::media_gallery::{GalleryAttachment, GalleryMessage, ordered_media};
use chatter_core::state::ActiveConversation;
use slint::{ComponentHandle, Model};

use crate::MainWindow;

static WINDOW: OnceLock<slint::Weak<MainWindow>> = OnceLock::new();
static CLIENT: OnceLock<Arc<ChatterClient>> = OnceLock::new();
static GALLERY: std::sync::Mutex<(ActiveConversation, Vec<String>, bool, u64)> =
    std::sync::Mutex::new((ActiveConversation::None, Vec::new(), false, 0));
static REQUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn invalidate() {
    #[cfg(target_os = "android")]
    super::pdf_preview::clear_android_renderer();
    REQUEST.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    GALLERY
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .2 = false;
}
pub(crate) fn is_current(request: u64) -> bool {
    REQUEST.load(std::sync::atomic::Ordering::SeqCst) == request
}
pub(crate) fn current_request() -> u64 {
    REQUEST.load(std::sync::atomic::Ordering::SeqCst)
}

fn gallery(w: &MainWindow) -> Vec<String> {
    ordered_media(
        w.global::<crate::ChatPane>()
            .get_messages()
            .iter()
            .chain(w.global::<crate::ChatPane>().get_thread_replies().iter())
            .map(|message| GalleryMessage {
                sent_at_ns: message.timestamp_ns.parse().unwrap_or(0),
                attachments: message
                    .attachments
                    .iter()
                    .map(|attachment| GalleryAttachment {
                        id: attachment.id.to_string(),
                        is_image: attachment.is_image,
                        is_video: attachment.is_video,
                    })
                    .collect(),
            }),
    )
}

fn thumbnail(w: &MainWindow, id: &str) -> slint::Image {
    w.global::<crate::ChatPane>()
        .get_messages()
        .iter()
        .chain(w.global::<crate::ChatPane>().get_thread_replies().iter())
        .find_map(|message| {
            message
                .attachments
                .iter()
                .find(|attachment| attachment.id == id && attachment.has_thumbnail)
                .map(|attachment| attachment.thumbnail.clone())
        })
        .unwrap_or_default()
}

pub(super) fn select(w: &MainWindow, id: &str) -> u64 {
    let request = REQUEST.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    #[cfg(target_os = "android")]
    super::pdf_preview::clear_android_renderer();
    #[cfg(target_os = "android")]
    crate::platform::android::media::invalidate_pending_video();
    let conversation = CLIENT
        .get()
        .map(|client| client.active_conversation())
        .unwrap_or(ActiveConversation::None);
    let session_revision = CLIENT
        .get()
        .map(|client| client.auth_snapshot().session_revision)
        .unwrap_or(0);
    let mut stored = GALLERY.lock().unwrap_or_else(|poison| poison.into_inner());
    if stored.0 != conversation
        || stored.3 != session_revision
        || !stored.1.iter().any(|item| item == id)
    {
        *stored = (conversation, gallery(w), false, session_revision);
    }
    let items = stored.1.clone();
    let needs_refresh = !stored.2;
    drop(stored);
    let index = items.iter().position(|item| item == id);
    let previous = index
        .and_then(|i| i.checked_sub(1))
        .and_then(|i| items.get(i));
    let next = index.and_then(|i| items.get(i + 1));
    w.global::<crate::ChatPane>()
        .set_media_viewer_previous_thumbnail(
            previous.map(|item| thumbnail(w, item)).unwrap_or_default(),
        );
    w.global::<crate::ChatPane>()
        .set_media_viewer_next_thumbnail(next.map(|item| thumbnail(w, item)).unwrap_or_default());
    #[cfg(target_os = "android")]
    {
        HAS_PREVIOUS.store(index.is_some_and(|i| i > 0), Ordering::Relaxed);
        HAS_NEXT.store(
            index.is_some_and(|i| i + 1 < items.len()),
            Ordering::Relaxed,
        );
        POSITION.store(
            index.map(|i| ((i as u64 + 1) << 32) | items.len() as u64).unwrap_or(0),
            Ordering::Relaxed,
        );
    }
    w.global::<crate::ChatPane>()
        .set_media_viewer_attachment_id(id.into());
    w.global::<crate::ChatPane>()
        .set_image_viewer_source(slint::Image::default());
    w.global::<crate::ChatPane>()
        .set_image_viewer_sender("".into());
    w.global::<crate::ChatPane>()
        .set_image_viewer_datetime("".into());
    w.global::<crate::ChatPane>()
        .set_image_viewer_caption("".into());
    if let Some(client) = CLIENT.get() {
        let client = client.clone();
        let weak = w.as_weak();
        let selected = id.to_string();
        let message_id = w.global::<crate::ChatPane>()
            .get_messages()
            .iter()
            .chain(w.global::<crate::ChatPane>().get_thread_replies().iter())
            .find(|message| message.attachments.iter().any(|attachment| attachment.id == id))
            .and_then(|message| message.message_id.as_str().parse::<MessageId>().ok());
        let header_conversation = conversation.clone();
        std::thread::spawn(move || {
            let attachment_id = selected.parse::<AttachmentId>()
                .unwrap_or_else(|_| AttachmentId::from_cdn_key(&selected));
            let header = client.media_viewer_header(&header_conversation, message_id.as_ref(), &attachment_id);
            let _ = weak.upgrade_in_event_loop(move |w| {
                if !is_current(request)
                    || w.global::<crate::ChatPane>().get_media_viewer_attachment_id().as_str() != selected
                    || client.active_conversation() != header_conversation
                    || client.auth_snapshot().session_revision != session_revision
                {
                    return;
                }
                if let Some((sender, datetime, caption)) = header {
                    w.global::<crate::ChatPane>().set_image_viewer_sender(sender.into());
                    w.global::<crate::ChatPane>().set_image_viewer_datetime(datetime.into());
                    w.global::<crate::ChatPane>().set_image_viewer_caption(
                        crate::bin_ui::converters::markdown::strip_markdown(&caption).into(),
                    );
                }
            });
        });
    }
    w.global::<crate::ChatPane>()
        .set_media_viewer_has_previous(index.is_some_and(|i| i > 0));
    w.global::<crate::ChatPane>()
        .set_media_viewer_has_next(index.is_some_and(|i| i + 1 < items.len()));
    w.global::<crate::ChatPane>().set_media_viewer_position(
        index
            .map(|i| format!("{} / {}", i + 1, items.len()))
            .unwrap_or_default()
            .into(),
    );
    if let Some(client) = CLIENT.get() {
        let client = client.clone();
        let weak = w.as_weak();
        let selected = id.to_string();
        std::thread::spawn(move || {
            let all = if needs_refresh {
                let Ok(all) = chatter_core::services::media_gallery::conversation_media(
                    &client,
                    &conversation,
                ) else {
                    return;
                };
                all
            } else {
                items
            };
            if !all.iter().any(|item| item == &selected) {
                return;
            }
            let index = all.iter().position(|item| item == &selected).unwrap_or(0);
            let preview = |neighbor: Option<&String>| {
                neighbor
                    .and_then(|id| {
                        id.parse::<AttachmentId>()
                            .ok()
                            .or_else(|| Some(AttachmentId::from_cdn_key(id)))
                    })
                    .and_then(|id| client.get_attachment_preview_image(&id))
            };
            let previous = preview(index.checked_sub(1).and_then(|i| all.get(i)));
            let next = preview(all.get(index + 1));
            let _ = weak.upgrade_in_event_loop(move |w| {
                if !is_current(request)
                    || w.global::<crate::ChatPane>()
                        .get_media_viewer_attachment_id()
                        .as_str()
                        != selected
                    || client.active_conversation() != conversation
                    || client.auth_snapshot().session_revision != session_revision
                {
                    return;
                }
                let mut stored = GALLERY.lock().unwrap_or_else(|poison| poison.into_inner());
                *stored = (conversation, all.clone(), true, session_revision);
                w.global::<crate::ChatPane>()
                    .set_media_viewer_has_previous(index > 0);
                w.global::<crate::ChatPane>()
                    .set_media_viewer_has_next(index + 1 < all.len());
                w.global::<crate::ChatPane>()
                    .set_media_viewer_position(format!("{} / {}", index + 1, all.len()).into());
                w.global::<crate::ChatPane>()
                    .set_media_viewer_previous_thumbnail(
                        previous
                            .map(|(rgba, width, height)| {
                                crate::bin_ui::slint_conversions::rgba_to_slint_image(
                                    &rgba, width, height,
                                )
                            })
                            .unwrap_or_default(),
                    );
                w.global::<crate::ChatPane>()
                    .set_media_viewer_next_thumbnail(
                        next.map(|(rgba, width, height)| {
                            crate::bin_ui::slint_conversions::rgba_to_slint_image(
                                &rgba, width, height,
                            )
                        })
                        .unwrap_or_default(),
                    );
                #[cfg(target_os = "android")]
                {
                    HAS_PREVIOUS.store(index > 0, Ordering::Relaxed);
                    HAS_NEXT.store(index + 1 < all.len(), Ordering::Relaxed);
                    POSITION.store(((index as u64 + 1) << 32) | all.len() as u64, Ordering::Relaxed);
                }
            });
        });
    }
    request
}

#[cfg(target_os = "android")]
pub(crate) fn native_position() -> u64 {
    POSITION.load(Ordering::Relaxed)
}

fn navigate(w: &MainWindow, delta: i32) {
    let current = w
        .global::<crate::ChatPane>()
        .get_media_viewer_attachment_id();
    let items = GALLERY
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .1
        .clone();
    let Some(index) = items.iter().position(|item| item == current.as_str()) else {
        return;
    };
    let next = index as i64 + i64::from(delta.signum());
    if next < 0 || next >= items.len() as i64 {
        return;
    }
    crate::bin_ui::gif_state::stop_gif_animation();
    crate::bin_ui::video_state::stop_video();
    // Keep the lightbox alive across image pages so its image and chrome can
    // travel together. Video selections replace it in the attachment handler.
    w.global::<crate::ChatPane>()
        .set_video_player_visible(false);
    w.global::<crate::ChatPane>()
        .invoke_view_attachment(items[next as usize].clone().into());
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
                2 => w
                    .global::<crate::ChatPane>()
                    .invoke_media_viewer_export(false),
                3 => w
                    .global::<crate::ChatPane>()
                    .invoke_media_viewer_open_downloads(),
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
    #[cfg(target_os = "android")]
    crate::platform::android::media_export::wire_session(client);
    let _ = WINDOW.set(window.as_weak());
    let _ = CLIENT.set(client.clone());
    let weak = window.as_weak();
    window
        .global::<crate::ChatPane>()
        .on_media_viewer_navigate(move |delta| {
            if let Some(w) = weak.upgrade() {
                navigate(&w, delta);
            }
        });
    let weak = window.as_weak();
    let client = client.clone();
    window
        .global::<crate::ChatPane>()
        .on_media_viewer_export(move |copy| {
            let Some(w) = weak.upgrade() else { return };
            let selected = w
                .global::<crate::ChatPane>()
                .get_media_viewer_attachment_id();
            if selected.is_empty() {
                return;
            }
            let id = selected
                .parse::<AttachmentId>()
                .unwrap_or_else(|_| AttachmentId::from_cdn_key(selected.as_str()));
            let revision = client.auth_snapshot().session_revision;
            let request = current_request();
            let client = client.clone();
            let weak = weak.clone();
            // Decryption, image decoding and file operations stay off the UI thread.
            std::thread::spawn(move || {
                let result = export(&client, &id, copy);
                let _ = weak.upgrade_in_event_loop(move |w| {
                    if client.auth_snapshot().session_revision != revision || !is_current(request) {
                        return;
                    }
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
    window
        .global::<crate::ChatPane>()
        .on_media_viewer_open_downloads(|| {
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
    check: impl Fn() -> Result<(), String>,
) -> Result<(), String> {
    use std::io::Write;
    use zeroize::Zeroize;
    let mut offset = 0;
    while offset < reader.plaintext_size() {
        check()?;
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
    output
        .sync_all()
        .map_err(|_| "Unable to finish download".to_string())?;
    check()
}

pub(crate) fn export(
    client: &ChatterClient,
    id: &AttachmentId,
    copy: bool,
) -> Result<String, String> {
    let revision = client.auth_snapshot().session_revision;
    let check = || {
        if client.auth_snapshot().session_revision == revision {
            Ok(())
        } else {
            Err("Session changed".to_string())
        }
    };
    let attachment = client.get_attachment(id).ok_or("Attachment unavailable")?;
    // Whole-file authentication precedes creating any plaintext destination.
    let reader = client
        .open_attachment_range(id)
        .map_err(|_| "Authenticated source unavailable")?;
    check()?;
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
            check,
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
            check()?;
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
        let safe = export_basename(&attachment.metadata.name);
        let destination = folder.join(format!("{id}-{safe}"));
        let (path, mut output) =
            create_export_file(&destination).map_err(|_| "Download cannot be created")?;
        if let Err(error) = stream_export(&reader, &mut output, check) {
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

// Unicode General_Category=Cf, including bidi overrides/isolates and invisible
// direction marks. These must never disguise the type of an exported file.
fn is_format_char(c: char) -> bool {
    matches!(c as u32, 0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F |
        0x0890..=0x0891 | 0x08E2 | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E |
        0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB |
        0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3 |
        0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}

/// Basename only, portable filename characters and a bounded UTF-8 length.
pub(crate) fn export_basename(name: &str) -> String {
    let raw = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut safe: String = raw
        .chars()
        .filter(|c| !c.is_control() && !is_format_char(*c) && !"<>:\"|?*".contains(*c))
        .collect();
    while safe.len() > 180 {
        safe.pop();
    }
    let safe = safe.trim_end_matches(['.', ' ']);
    if safe.is_empty() {
        "attachment".into()
    } else {
        safe.into()
    }
}

/// Claim a fresh destination without following a colliding file or symlink.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn create_export_file(
    destination: &std::path::Path,
) -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    for suffix in 0..1024 {
        let path = if suffix == 0 {
            destination.to_path_buf()
        } else {
            let mut name = destination.file_stem().unwrap_or_default().to_os_string();
            name.push(format!(" ({suffix})"));
            if let Some(ext) = destination.extension() {
                name.push(".");
                name.push(ext);
            }
            destination.with_file_name(name)
        };
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "No unused export name",
    ))
}

/// Even a Save-As picker cannot authorize a race-time replacement by another
/// writer. A collision keeps the original and chooses a numbered sibling.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn save_export(
    destination: &std::path::Path,
    data: &[u8],
) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    let (path, mut file) = create_export_file(destination)?;
    if let Err(error) = file.write_all(data).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(path)
}

#[cfg(test)]
mod export_regressions {
    use super::*;

    #[test]
    fn peer_names_remain_basenames_without_bidi_or_invisible_formats() {
        for (raw, expected) in [
            ("/abs/x", "x"),
            ("../x", "x"),
            ("..\\x", "x"),
            ("C:\\x", "x"),
            ("a\u{202e}txt.exe", "atxt.exe"),
            ("a\u{2066}b\u{2069}.txt", "ab.txt"),
            (".", "attachment"),
            ("", "attachment"),
        ] {
            assert_eq!(export_basename(raw), expected);
        }
        let name = export_basename(&"é".repeat(200));
        assert!(name.len() <= 180);
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn export_collision_keeps_existing_file_and_suffix_preserves_extension() {
        let folder = tempfile::tempdir().unwrap();
        let destination = folder.path().join("report.pdf");
        std::fs::write(&destination, b"original").unwrap();
        let saved = save_export(&destination, b"new").unwrap();
        assert_ne!(saved, destination);
        assert_eq!(saved.extension().unwrap(), "pdf");
        assert_eq!(std::fs::read(&destination).unwrap(), b"original");
        assert_eq!(std::fs::read(saved).unwrap(), b"new");
    }
}
