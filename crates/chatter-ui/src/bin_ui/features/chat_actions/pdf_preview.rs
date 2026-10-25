//! PDF preview callbacks.
//!
//! Handles PDF file viewing with inline desktop and Android renderers.

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use chatter_core::media::{FilePicker, FilePickerConfig};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use chatter_core::system::ExternalOpener;

use slint::ComponentHandle;

use crate::MainWindow;
use crate::bin_ui::preview_state;

/// Wire PDF preview callbacks.
pub fn wire_pdf_preview_callbacks(window: &MainWindow) {
    // Close PDF preview
    window.global::<crate::ChatPane>().on_close_pdf_preview(|| {
        super::media_viewer::invalidate();
        preview_state::clear_pdf_preview();
        #[cfg(feature = "desktop")]
        pdf_renderer::clear();
        tracing::debug!("PDF preview closed");
    });

    // Open PDF in external viewer - uses Save As for secure export
    let weak = window.as_weak();
    window.global::<crate::ChatPane>().on_open_pdf_external(move || {
        #[cfg(target_os = "android")]
        if let Some(mut content) = preview_state::get_pdf_preview_content() {
            let weak = weak.clone();
            let request = super::media_viewer::current_request();
            std::thread::spawn(move || {
                let result = crate::platform::android::media_export::prepare_pdf(&content.filename, &content.data);
                zeroize::Zeroize::zeroize(&mut content.data);
                let _ = weak.upgrade_in_event_loop(move |w| {
                    // Dropping a stale session removes its unpublished cache file.
                    if !super::media_viewer::is_current(request) || !w.global::<crate::ChatPane>().get_pdf_preview_visible() {
                        return;
                    }
                    let result = result.and_then(|session| session.finish());
                    if let Err(error) = result {
                        tracing::warn!(%error, "PDF external open failed");
                        w.global::<crate::ChatPane>().set_pdf_preview_error("Unable to open PDF. Install a PDF viewer and try Open externally again.".into());
                    }
                });
            });
        } else if let Some(w) = weak.upgrade() {
            w.global::<crate::ChatPane>().set_pdf_preview_error(
                "PDF content is no longer available. Reopen the attachment.".into(),
            );
        }
        #[cfg(not(target_os = "android"))]
        let _ = &weak;
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if let Some(content) = preview_state::get_pdf_preview_content() {
            // Show Save As dialog then open with system handler
            let filename = super::media_viewer::export_basename(&content.filename);
            let data = content.data.clone();
            tokio::spawn(async move {
                let config = FilePickerConfig::new()
                    .with_title("Save PDF file")
                    .with_default_path(
                        dirs::download_dir()
                            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                            .join(&filename),
                    );

                if let Some(destination) = FilePicker::save_file(config).await {
                    match super::media_viewer::save_export(&destination, &data) {
                        Ok(destination) => {
                            tracing::info!("Exported PDF to {}", destination.display());
                            let opener = ExternalOpener;
                            if let Err(e) = opener.open_file(&destination) {
                                tracing::warn!("Could not open exported file: {}", e);
                            }
                        }
                        Err(e) => tracing::error!("Failed to save PDF file: {}", e),
                    }
                }
            });
        } else {
            tracing::warn!("No PDF content available for external open");
        }
        #[cfg(target_os = "ios")]
        tracing::warn!("Open external not supported on mobile");
    });

    // Page navigation
    #[cfg(any(feature = "desktop", target_os = "android"))]
    {
        let weak = window.as_weak();
        window
            .global::<crate::ChatPane>()
            .on_pdf_page_prev(move || {
                if let Some(w) = weak.upgrade() {
                    #[cfg(feature = "desktop")]
                    pdf_renderer::navigate(&w, -1);
                    #[cfg(target_os = "android")]
                    android_renderer::navigate(&w, -1);
                }
            });

        let weak = window.as_weak();
        window
            .global::<crate::ChatPane>()
            .on_pdf_page_next(move || {
                if let Some(w) = weak.upgrade() {
                    #[cfg(feature = "desktop")]
                    pdf_renderer::navigate(&w, 1);
                    #[cfg(target_os = "android")]
                    android_renderer::navigate(&w, 1);
                }
            });
    }
}

/// Render first page of a PDF and display in the preview panel.
///
/// Called from `attachment_view::display_pdf_from_buffer` after the PDF
/// data is decrypted into memory. Desktop uses pdfium-render; Android uses
/// the platform PdfRenderer on a worker thread.
#[cfg(feature = "desktop")]
pub fn render_pdf_preview(window: &MainWindow, pdf_data: &[u8]) {
    window
        .global::<crate::ChatPane>()
        .set_pdf_preview_external_only(false);
    pdf_renderer::load_and_render(window, pdf_data);
}

#[cfg(target_os = "android")]
pub fn render_pdf_preview(window: &MainWindow, pdf_data: &[u8]) {
    let pane = window.global::<crate::ChatPane>();
    pane.set_pdf_preview_external_only(false);
    pane.set_pdf_preview_image(slint::Image::default());
    android_renderer::load_and_render(window, pdf_data);
}

#[cfg(target_os = "android")]
pub(super) fn clear_android_renderer() {
    android_renderer::clear();
}

#[cfg(all(not(feature = "desktop"), not(target_os = "android")))]
pub fn render_pdf_preview(window: &MainWindow, _pdf_data: &[u8]) {
    window
        .global::<crate::ChatPane>()
        .set_pdf_preview_external_only(true);
    window
        .global::<crate::ChatPane>()
        .set_pdf_preview_image(slint::Image::default());
    window
        .global::<crate::ChatPane>()
        .set_pdf_preview_error("".into());
}

#[cfg(target_os = "android")]
mod android_renderer {
    use super::super::media_viewer;
    use crate::MainWindow;
    use crate::bin_ui::slint_conversions::rgba_to_slint_image;
    use slint::ComponentHandle;
    use std::sync::Mutex;
    use zeroize::Zeroize;

    struct State {
        generation: u64,
        document: Option<Document>,
    }
    struct Document {
        token: i64,
        pages: i32,
        current: i32,
    }
    static STATE: Mutex<State> = Mutex::new(State {
        generation: 0,
        document: None,
    });

    pub fn clear() {
        let old = {
            let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
            state.generation = state.generation.wrapping_add(1);
            state.document.take().map(|doc| doc.token)
        };
        if let Some(token) = old {
            std::thread::spawn(move || crate::platform::android::pdf_renderer::close(token));
        }
    }

    pub fn load_and_render(window: &MainWindow, pdf_data: &[u8]) {
        clear();
        let generation = STATE.lock().unwrap_or_else(|e| e.into_inner()).generation;
        let request = media_viewer::current_request();
        let weak = window.as_weak();
        let mut data = pdf_data.to_vec();
        std::thread::spawn(move || {
            let loaded = crate::platform::android::pdf_renderer::load(&data);
            data.zeroize();
            let mut result = loaded.and_then(|(token, pages)| {
                let current =
                    STATE.lock().unwrap_or_else(|e| e.into_inner()).generation == generation;
                if !current || !media_viewer::is_current(request) {
                    crate::platform::android::pdf_renderer::close(token);
                    return Err("PDF preview closed".into());
                }
                match crate::platform::android::pdf_renderer::render(token, 0) {
                    Ok(image) => Ok((token, pages, image)),
                    Err(error) => {
                        crate::platform::android::pdf_renderer::close(token);
                        Err(error)
                    }
                }
            });
            let _ = weak.upgrade_in_event_loop(move |window| {
                let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
                if state.generation != generation
                    || !media_viewer::is_current(request)
                    || !window.global::<crate::ChatPane>().get_pdf_preview_visible()
                {
                    if let Ok((token, _, (_, _, rgba))) = &mut result {
                        rgba.zeroize();
                        let token = *token;
                        std::thread::spawn(move || {
                            crate::platform::android::pdf_renderer::close(token)
                        });
                    }
                    return;
                }
                match result {
                    Ok((token, pages, (width, height, mut rgba))) => {
                        state.document = Some(Document {
                            token,
                            pages,
                            current: 0,
                        });
                        let pane = window.global::<crate::ChatPane>();
                        pane.set_pdf_preview_page_count(pages);
                        pane.set_pdf_preview_current_page(1);
                        pane.set_pdf_preview_image(rgba_to_slint_image(&rgba, width, height));
                        rgba.zeroize();
                        pane.set_pdf_preview_error("".into());
                    }
                    Err(error) if error != "PDF preview closed" => {
                        tracing::warn!(%error, "PDF preview failed");
                        window
                            .global::<crate::ChatPane>()
                            .set_pdf_preview_error("Unable to preview PDF. Try Open externally.".into());
                    }
                    Err(_) => {}
                }
            });
        });
    }

    pub fn navigate(window: &MainWindow, delta: i32) {
        let (token, page, generation) = {
            let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
            let generation = state.generation;
            let Some(document) = state.document.as_mut() else {
                return;
            };
            let page = document.current + delta;
            if page < 0 || page >= document.pages {
                return;
            }
            document.current = page;
            (document.token, page, generation)
        };
        let request = media_viewer::current_request();
        let weak = window.as_weak();
        std::thread::spawn(move || {
            let mut result = crate::platform::android::pdf_renderer::render(token, page);
            let _ = weak.upgrade_in_event_loop(move |window| {
                let state = STATE.lock().unwrap_or_else(|e| e.into_inner());
                if state.generation != generation
                    || !media_viewer::is_current(request)
                    || state
                        .document
                        .as_ref()
                        .is_none_or(|doc| doc.current != page || doc.token != token)
                {
                    if let Ok((_, _, rgba)) = &mut result {
                        rgba.zeroize();
                    }
                    return;
                }
                let pane = window.global::<crate::ChatPane>();
                match result {
                    Ok((width, height, mut rgba)) => {
                        pane.set_pdf_preview_image(rgba_to_slint_image(&rgba, width, height));
                        rgba.zeroize();
                        pane.set_pdf_preview_current_page(page + 1);
                        pane.set_pdf_preview_error("".into());
                    }
                    Err(error) => {
                        tracing::warn!(%error, "PDF page render failed");
                        pane.set_pdf_preview_error("Unable to show this PDF page. Try Open externally.".into());
                    }
                }
            });
        });
    }
}

// ==========================================================================
// Desktop PDF renderer using pdfium-render
// ==========================================================================

#[cfg(feature = "desktop")]
mod pdf_renderer {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock, mpsc};

    use pdfium_render::prelude::*;
    use slint::ComponentHandle;

    use crate::MainWindow;
    use crate::bin_ui::slint_conversions::rgba_to_slint_image;

    const RENDER_WIDTH: u32 = 800;
    const CACHE_BYTES: usize = 16 * 1024 * 1024;
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    static WORKER: OnceLock<mpsc::Sender<Command>> = OnceLock::new();
    static PAGE: Mutex<(i32, i32)> = Mutex::new((0, 0));

    enum Command {
        Load(u64, Vec<u8>, slint::Weak<MainWindow>),
        Render(u64, u16, slint::Weak<MainWindow>),
        Clear,
    }

    struct CachedPage {
        index: u16,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    }

    fn worker() -> &'static mpsc::Sender<Command> {
        WORKER.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || run(rx));
            tx
        })
    }

    fn run(rx: mpsc::Receiver<Command>) {
        let pdfium = Pdfium::bind_to_system_library()
            .or_else(|_| Pdfium::bind_to_library("libpdfium.so"))
            .or_else(|_| Pdfium::bind_to_library("/usr/lib/libpdfium.so"))
            .or_else(|_| Pdfium::bind_to_library("/usr/local/lib/libpdfium.so"))
            .ok()
            .map(Pdfium::new);
        // Pdfium owns the loaded bytes, and the document never leaves this worker.
        let mut document = None;
        let mut cache: VecDeque<CachedPage> = VecDeque::new();
        let mut cache_bytes = 0usize;
        while let Ok(command) = rx.recv() {
            let (generation, page, weak, load_data) = match command {
                Command::Clear => {
                    document = None;
                    cache.clear();
                    cache_bytes = 0;
                    continue;
                }
                Command::Load(generation, bytes, weak) => {
                    document = None;
                    cache.clear();
                    cache_bytes = 0;
                    (generation, 0, weak, Some(bytes))
                }
                Command::Render(generation, page, weak) => (generation, page, weak, None),
            };
            if GENERATION.load(Ordering::Acquire) != generation {
                continue;
            }
            let result: Result<(u16, CachedPage), String> = (|| {
                let pdfium = pdfium.as_ref().ok_or("PDF rendering unavailable. Install libpdfium for in-app viewing.")?;
                if let Some(bytes) = load_data {
                    document = Some(pdfium.load_pdf_from_byte_vec(bytes, None)
                        .map_err(|e| format!("Failed to load PDF: {e}"))?);
                }
                let document = document.as_ref().ok_or("PDF is no longer loaded")?;
                let count = u16::try_from(document.pages().len()).map_err(|_| "PDF page count is unavailable")?;
                if count == 0 { return Err("PDF has no pages".into()); }
                if page >= count { return Err("PDF page is unavailable".into()); }
                if let Some(position) = cache.iter().position(|entry| entry.index == page) {
                    let cached = cache.remove(position).unwrap();
                    let response = CachedPage { index: page, width: cached.width, height: cached.height, rgba: cached.rgba.clone() };
                    cache.push_back(cached);
                    return Ok((count, response));
                }
                let rendered = render_page(&document, page)?;
                let size = rendered.rgba.len();
                if size <= CACHE_BYTES {
                    while cache_bytes + size > CACHE_BYTES {
                        if let Some(old) = cache.pop_front() { cache_bytes -= old.rgba.len(); }
                        else { break; }
                    }
                    cache_bytes += size;
                    cache.push_back(CachedPage { index: page, width: rendered.width, height: rendered.height, rgba: rendered.rgba.clone() });
                }
                Ok((count, rendered))
            })();
            if GENERATION.load(Ordering::Acquire) != generation { continue; }
            let _ = weak.upgrade_in_event_loop(move |window| {
                if GENERATION.load(Ordering::Acquire) != generation
                    || !window.global::<crate::ChatPane>().get_pdf_preview_visible() { return; }
                let pane = window.global::<crate::ChatPane>();
                match result {
                    Ok((count, image)) => {
                        *PAGE.lock().unwrap_or_else(|e| e.into_inner()) = (page as i32, count as i32);
                        pane.set_pdf_preview_page_count(count as i32);
                        pane.set_pdf_preview_current_page(page as i32 + 1);
                        pane.set_pdf_preview_image(rgba_to_slint_image(&image.rgba, image.width, image.height));
                        pane.set_pdf_preview_error("".into());
                    }
                    Err(error) => {
                        tracing::warn!(%error, "PDF page render failed");
                        pane.set_pdf_preview_error("Unable to show this PDF page. Try Open externally.".into());
                    }
                }
            });
        }
    }

    pub fn load_and_render(window: &MainWindow, pdf_data: &[u8]) {
        let generation = GENERATION.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        *PAGE.lock().unwrap_or_else(|e| e.into_inner()) = (0, 0);
        window.global::<crate::ChatPane>().set_pdf_preview_image(slint::Image::default());
        let _ = worker().send(Command::Load(generation, pdf_data.to_vec(), window.as_weak()));
    }

    pub fn navigate(window: &MainWindow, delta: i32) {
        let mut page = PAGE.lock().unwrap_or_else(|e| e.into_inner());
        let next = page.0 + delta;
        if page.1 == 0 || next < 0 || next >= page.1 { return; }
        page.0 = next;
        let generation = GENERATION.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        let _ = worker().send(Command::Render(generation, next as u16, window.as_weak()));
    }

    pub fn clear() {
        GENERATION.fetch_add(1, Ordering::AcqRel);
        *PAGE.lock().unwrap_or_else(|e| e.into_inner()) = (0, 0);
        let _ = worker().send(Command::Clear);
    }

    fn render_page(document: &PdfDocument, page_index: u16) -> Result<CachedPage, String> {
        let page = document.pages().get(page_index as i32)
            .map_err(|e| format!("Failed to get PDF page: {e}"))?;
        let config = PdfRenderConfig::new()
            .set_target_width(RENDER_WIDTH as Pixels)
            .set_maximum_height((RENDER_WIDTH * 2) as i32);
        let bitmap = page.render_with_config(&config)
            .map_err(|e| format!("Failed to render PDF page: {e}"))?;
        let image = bitmap.as_image().map_err(|e| format!("Failed to convert PDF page: {e}"))?.to_rgba8();
        Ok(CachedPage { index: page_index, width: image.width(), height: image.height(), rgba: image.into_raw() })
    }
}
