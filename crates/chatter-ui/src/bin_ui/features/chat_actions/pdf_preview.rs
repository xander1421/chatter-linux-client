//! PDF preview callbacks.
//!
//! Handles PDF file viewing with in-app rendering via pdfium-render (desktop)
//! or export-to-external-viewer fallback.

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use chatter_core::media::{FilePicker, FilePickerConfig};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use chatter_core::system::ExternalOpener;

#[cfg(feature = "desktop")]
use slint::ComponentHandle;

use crate::bin_ui::preview_state;
use crate::MainWindow;

/// Wire PDF preview callbacks.
pub fn wire_pdf_preview_callbacks(window: &MainWindow) {
    // Close PDF preview
    window.on_close_pdf_preview(|| {
        preview_state::clear_pdf_preview();
        #[cfg(feature = "desktop")]
        pdf_renderer::clear();
        tracing::debug!("PDF preview closed");
    });

    // Open PDF in external viewer - uses Save As for secure export
    window.on_open_pdf_external(|| {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if let Some(content) = preview_state::get_pdf_preview_content() {
            // Show Save As dialog then open with system handler
            let filename = content.filename.clone();
            let data = content.data.clone();
            tokio::spawn(async move {
                let config = FilePickerConfig::new()
                    .with_title("Save PDF File")
                    .with_default_path(
                        dirs::download_dir()
                            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                            .join(&filename),
                    );

                if let Some(destination) = FilePicker::save_file(config).await {
                    match std::fs::write(&destination, &data) {
                        Ok(()) => {
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
        #[cfg(any(target_os = "android", target_os = "ios"))]
        tracing::warn!("Open external not supported on mobile");
    });

    // Page navigation
    #[cfg(feature = "desktop")]
    {
        let weak = window.as_weak();
        window.on_pdf_page_prev(move || {
            if let Some(w) = weak.upgrade() {
                pdf_renderer::navigate(&w, -1);
            }
        });

        let weak = window.as_weak();
        window.on_pdf_page_next(move || {
            if let Some(w) = weak.upgrade() {
                pdf_renderer::navigate(&w, 1);
            }
        });
    }
}

/// Render first page of a PDF and display in the preview panel.
///
/// Called from `attachment_view::display_pdf_from_buffer` after the PDF
/// data is decrypted into memory. On desktop, uses pdfium-render for inline
/// rendering. Falls back to metadata-only display if PDFium is unavailable.
#[cfg(feature = "desktop")]
pub fn render_pdf_preview(window: &MainWindow, pdf_data: &[u8]) {
    pdf_renderer::load_and_render(window, pdf_data);
}

#[cfg(not(feature = "desktop"))]
pub fn render_pdf_preview(_window: &MainWindow, _pdf_data: &[u8]) {
    // No in-app rendering on mobile — external viewer only
}

// ==========================================================================
// Desktop PDF renderer using pdfium-render
// ==========================================================================

#[cfg(feature = "desktop")]
mod pdf_renderer {
    use std::cell::RefCell;
    use std::sync::OnceLock;

    use pdfium_render::prelude::*;

    use crate::bin_ui::slint_conversions::rgba_to_slint_image;
    use crate::MainWindow;

    /// Target render width in pixels. Height is calculated from page aspect ratio.
    const RENDER_WIDTH: u32 = 800;

    /// Lazily-initialized Pdfium binding.
    static PDFIUM: OnceLock<Option<Pdfium>> = OnceLock::new();

    /// Thread-local state for the currently-loaded PDF document.
    struct PdfState {
        /// Raw PDF bytes (kept alive for pdfium document lifetime).
        _data: Vec<u8>,
        /// Total page count.
        page_count: u16,
        /// Current page index (0-based).
        current_page: u16,
        /// Pre-rendered page images (lazily filled).
        page_cache: Vec<Option<slint::Image>>,
    }

    thread_local! {
        static PDF_STATE: RefCell<Option<PdfState>> = const { RefCell::new(None) };
    }

    /// Initialize PDFium library (once, lazily).
    ///
    /// Returns `None` if libpdfium is not installed on the system.
    fn get_pdfium() -> Option<&'static Pdfium> {
        PDFIUM
            .get_or_init(|| {
                // Try system library first, then common paths
                match Pdfium::bind_to_system_library()
                    .or_else(|_| Pdfium::bind_to_library("libpdfium.so"))
                    .or_else(|_| Pdfium::bind_to_library("/usr/lib/libpdfium.so"))
                    .or_else(|_| Pdfium::bind_to_library("/usr/local/lib/libpdfium.so"))
                {
                    Ok(bindings) => Some(Pdfium::new(bindings)),
                    Err(e) => {
                        tracing::warn!(
                            "PDFium library not found: {}. Install libpdfium for in-app PDF viewing.",
                            e
                        );
                        None
                    }
                }
            })
            .as_ref()
    }

    /// Load a PDF from bytes, render the first page, and update the UI.
    pub fn load_and_render(window: &MainWindow, pdf_data: &[u8]) {
        let Some(pdfium) = get_pdfium() else {
            window.set_pdf_preview_error("PDF rendering unavailable. Install libpdfium for in-app viewing.".into());
            return;
        };

        match pdfium.load_pdf_from_byte_slice(pdf_data, None) {
            Ok(document) => {
                let page_count = document.pages().len();
                if page_count == 0 {
                    window.set_pdf_preview_error("PDF has no pages".into());
                    return;
                }

                // Render first page
                let first_page_image = render_page(&document, 0);

                window.set_pdf_preview_page_count(page_count as i32);
                window.set_pdf_preview_current_page(1);

                if let Some(ref img) = first_page_image {
                    window.set_pdf_preview_image(img.clone());
                }

                // Store state for page navigation
                let mut cache = vec![None; page_count as usize];
                cache[0] = first_page_image;

                PDF_STATE.with(|state| {
                    *state.borrow_mut() = Some(PdfState {
                        _data: pdf_data.to_vec(),
                        page_count: page_count as u16,
                        current_page: 0,
                        page_cache: cache,
                    });
                });
            }
            Err(e) => {
                tracing::error!("Failed to load PDF: {:?}", e);
                window.set_pdf_preview_error(format!("Failed to load PDF: {}", e).into());
            }
        }
    }

    /// Navigate to a different page (delta: -1 for prev, +1 for next).
    pub fn navigate(window: &MainWindow, delta: i32) {
        PDF_STATE.with(|state_cell| {
            let mut state_opt = state_cell.borrow_mut();
            let Some(state) = state_opt.as_mut() else {
                return;
            };

            let new_page = state.current_page as i32 + delta;
            if new_page < 0 || new_page >= state.page_count as i32 {
                return;
            }

            let new_page = new_page as u16;
            state.current_page = new_page;

            // Check cache first
            if let Some(Some(cached)) = state.page_cache.get(new_page as usize) {
                window.set_pdf_preview_image(cached.clone());
                window.set_pdf_preview_current_page(new_page as i32 + 1);
                return;
            }

            // Need to render — reload document from stored data
            let pdfium = match get_pdfium() {
                Some(p) => p,
                None => return,
            };

            // We need the raw data to reload the document
            let data = &state._data;
            match pdfium.load_pdf_from_byte_slice(data, None) {
                Ok(document) => {
                    let image = render_page(&document, new_page);
                    if let Some(ref img) = image {
                        window.set_pdf_preview_image(img.clone());
                    }
                    state.page_cache[new_page as usize] = image;
                    window.set_pdf_preview_current_page(new_page as i32 + 1);
                }
                Err(e) => {
                    tracing::error!("Failed to reload PDF for page {}: {:?}", new_page, e);
                }
            }
        });
    }

    /// Clear the PDF state.
    pub fn clear() {
        PDF_STATE.with(|state| {
            *state.borrow_mut() = None;
        });
    }

    /// Render a single page to a Slint Image.
    fn render_page(document: &PdfDocument, page_index: u16) -> Option<slint::Image> {
        let page = match document.pages().get(page_index as i32) {
            Ok(page) => page,
            Err(e) => {
                tracing::error!("Failed to get page {}: {:?}", page_index, e);
                return None;
            }
        };

        let config = PdfRenderConfig::new()
            .set_target_width(RENDER_WIDTH as Pixels)
            .set_maximum_height((RENDER_WIDTH * 2) as i32); // Prevent absurdly tall pages

        match page.render_with_config(&config) {
            Ok(bitmap) => {
                let img = bitmap.as_image();
                let Ok(img) = img else {
                    tracing::error!("Failed to convert page {} to image", page_index);
                    return None;
                };
                let rgba = img.to_rgba8();
                let width = rgba.width();
                let height = rgba.height();
                let data = rgba.into_raw();
                Some(rgba_to_slint_image(&data, width, height))
            }
            Err(e) => {
                tracing::error!("Failed to render page {}: {:?}", page_index, e);
                None
            }
        }
    }
}
