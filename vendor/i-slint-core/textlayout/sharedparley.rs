// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore unshareable
pub use parley;
pub use parley::fontique;

use crate::android_trace::Section as TraceSection;
use unicode_segmentation::UnicodeSegmentation;
use alloc::string::String;
use crate::{
    Color,
    graphics::FontRequest,
    item_rendering::PlainOrStyledText,
    items::TextStrokeStyle,
    lengths::{
        LogicalBorderRadius, LogicalLength, LogicalPoint, LogicalRect, LogicalSize, PhysicalPx,
        PointLengths, RectLengths, ScaleFactor, SizeLengths,
    },
    renderer::RendererSealed,
    textlayout::{TextHorizontalAlignment, TextOverflow, TextVerticalAlignment, TextWrap},
    window::WindowAdapter,
};
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::ops::Range;
use core::pin::Pin;
use euclid::num::Zero;
use i_slint_common::sharedfontique;
use skrifa::MetadataProvider as _;
use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(derive_more::Deref, derive_more::DerefMut)]
pub struct FontContext {
    #[deref]
    #[deref_mut]
    pub inner: parley::FontContext,
    /// `(ptr, len)` of each `&'static [u8]` already handed to fontique, so repeat
    /// `register_static_font` calls for the same embedded font are skipped.
    registered_static_fonts: HashSet<(usize, usize)>,
}

impl FontContext {
    pub fn new(inner: parley::FontContext) -> Self {
        Self { inner, registered_static_fonts: HashSet::default() }
    }

    pub fn register_static_font(&mut self, data: &'static [u8]) {
        let key = (data.as_ptr() as usize, data.len());
        if self.registered_static_fonts.insert(key) {
            self.inner.collection.register_fonts(fontique::Blob::new(Arc::new(data)), None);
        }
    }

    pub fn clear_registered_static_fonts(&mut self) {
        self.registered_static_fonts.clear();
    }
}

pub type PhysicalLength = euclid::Length<f32, PhysicalPx>;
pub type PhysicalRect = euclid::Rect<f32, PhysicalPx>;
type PhysicalSize = euclid::Size2D<f32, PhysicalPx>;
type PhysicalPoint = euclid::Point2D<f32, PhysicalPx>;

pub use super::DEFAULT_FONT_SIZE;

mod cache;
mod draw;
mod layout;
mod selection;
mod shaping;
#[cfg(test)]
mod tests;

pub use cache::TextLayoutCache;
pub use draw::{GlyphRenderer, RectangleBorder};

use cache::cached_paragraphs;
use layout::{Layout, LayoutOptions, layout};
use selection::{SelectionRendering, SelectionSpans};
use shaping::{
    Brush, LayoutWithoutLineBreaksBuilder, create_text_paragraphs, shape_paragraphs,
    shaping_builder,
};

/// Lays out the shaped text of an item and runs `f` over the result.
///
/// This is the one place that checks paragraphs out of a [`TextLayoutCache`] entry and hands them
/// back: `f` only borrows the [`Layout`], so no caller can lose the shaped paragraphs and cost the
/// next use of the entry a reshape.
///
/// The font context is borrowed from `window`'s Slint context for shaping and layout only, and
/// released before `f` runs -- glyph drawing inside `f` re-enters it, and property bindings
/// evaluated under `f` must not find it borrowed. The wrap mode and scale factor the cache entry
/// is keyed on come from `layout_builder`, so shaping and layout cannot disagree about either.
///
/// Returns `None` only when `window` has no Slint context yet.
fn with_text_layout<R>(
    cache: Option<&TextLayoutCache>,
    item_rc: Option<&crate::item_tree::ItemRc>,
    text: Pin<&dyn crate::item_rendering::RenderString>,
    layout_builder: &LayoutWithoutLineBreaksBuilder,
    options: LayoutOptions,
    window: &crate::api::Window,
    f: impl FnOnce(&Layout) -> R,
) -> Option<R> {
    let logical_source = (options.text_overflow == TextOverflow::Elide
        && layout_builder.text_wrap == TextWrap::NoWrap)
        .then(|| text.text())
        .and_then(|source| match source {
            PlainOrStyledText::Plain(value) if !value.contains(['\n', '\r']) => Some(value),
            _ => None,
        });
    let ctx = crate::window::WindowInner::from_pub(window).try_context()?;
    let mut font_ctx = ctx.font_context().borrow_mut();

    let text_wrap = layout_builder.text_wrap;
    let scale_factor = layout_builder.scale_factor;
    let mut guard = {
        let _trace = TraceSection::new(c"Slint text get_or_create_text_paragraphs");
        cached_paragraphs(cache, item_rc, text_wrap, window, &mut font_ctx, &|font_context| {
            shape_paragraphs(text, item_rc, text_wrap, scale_factor, font_context)
        })
    };

    let line_breaking = guard.take_line_breaking();
    let paragraphs = guard.take();
    // Logical elision is a display-only substitution. Never put its shortened
    // paragraphs into the shared shaping cache used by measurement and hit testing.
    let logical_elision = logical_source.as_ref().and_then(|source| {
        let width = (options.max_width? * scale_factor).get();
        let overflows = paragraphs.first()
            .is_some_and(|p| p.layout.calculate_content_widths().max > width);
        overflows.then(|| logically_elided_text(layout_builder, &mut font_ctx, source, width)).flatten()
    });
    let (layout, original) = if let Some(elided) = logical_elision {
        let display = create_text_paragraphs(
            layout_builder, &mut font_ctx, PlainOrStyledText::Plain(elided.as_str().into()),
            Color::default(),
        );
        let options = LayoutOptions { text_overflow: TextOverflow::Clip, ..options };
        (layout(layout_builder, &mut font_ctx, display, scale_factor, options, None),
            Some((paragraphs, line_breaking)))
    } else {
        (layout(layout_builder, &mut font_ctx, paragraphs, scale_factor, options, line_breaking), None)
    };
    drop(font_ctx);

    if layout.broke_lines {
        #[cfg(feature = "testing")]
        if let Some(cache) = cache {
            cache.count_layout_miss();
        }
    }

    let result = f(&layout);
    let (paragraphs, line_breaking) = original.unwrap_or_else(|| {
        let (paragraphs, line_breaking) = layout.dismantle();
        (paragraphs, Some(line_breaking))
    });
    guard.restore(paragraphs, line_breaking);
    Some(result)
}

/// Cut a single line in source order. Parley resolves the paragraph base from the first
/// strong character outside isolates (UAX #9 P2/P3); shaping the completed string also
/// puts the ellipsis on the logical end of an RTL isolate, rather than at a visual edge.
fn logically_elided_text(
    builder: &LayoutWithoutLineBreaksBuilder,
    font_context: &mut parley::FontContext,
    text: &str,
    max_width: f32,
) -> Option<String> {
    let mut full = builder.build(font_context, text, None, None);
    full.break_all_lines(None);
    if full.full_width() <= max_width {
        return None;
    }

    let boundaries: Vec<usize> =
        text.grapheme_indices(true).map(|(i, _)| i).chain([text.len()]).collect();
    let candidate = |cut: usize| {
        let prefix = &text[..cut];
        let mut depth = 0usize;
        for ch in prefix.chars() {
            match ch {
                '\u{2066}' | '\u{2067}' | '\u{2068}' => depth += 1,
                '\u{2069}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        let mut result = String::from(prefix);
        // A grapheme cut can land immediately before the body's FSI. Keep its
        // opener so the ellipsis still belongs to the body at that width.
        if text[cut..].starts_with('\u{2068}') {
            result.push('\u{2068}');
            depth += 1;
        }
        result.push('…');
        result.extend(core::iter::repeat_n('\u{2069}', depth));
        result
    };

    // Each probe is shaped, so ligatures and script fallback are measured in the
    // same font as the rendered result. The final candidate is laid out normally.
    let mut low = 0;
    let mut high = boundaries.len() - 1;
    while low < high {
        let mid = (low + high + 1) / 2;
        let probe = candidate(boundaries[mid]);
        let mut layout = builder.build(font_context, &probe, None, None);
        layout.break_all_lines(None);
        if layout.full_width() <= max_width {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    Some(candidate(boundaries[low]))
}

pub fn draw_text(
    item_renderer: &mut impl GlyphRenderer,
    text: Pin<&dyn crate::item_rendering::RenderText>,
    item_rc: Option<&crate::item_tree::ItemRc>,
    size: LogicalSize,
    cache: Option<&TextLayoutCache>,
) {
    let max_width = size.width_length();
    let max_height = size.height_length();

    if max_width.get() <= 0. || max_height.get() <= 0. {
        return;
    }

    let Some(platform_fill_brush) = item_renderer.platform_text_fill_brush(text.color(), size)
    else {
        // Nothing to draw
        return;
    };

    let scale_factor = item_renderer.scale_factor();

    let (stroke_brush, stroke_width, stroke_style) = text.stroke();
    let platform_stroke_brush = if !stroke_brush.is_transparent() {
        let stroke_width = if stroke_width.get() != 0.0 {
            (stroke_width * scale_factor).get()
        } else {
            // Hairline stroke
            1.0
        };
        let stroke_width = match stroke_style {
            TextStrokeStyle::Outside => stroke_width * 2.0,
            TextStrokeStyle::Center => stroke_width,
        };
        item_renderer.platform_text_stroke_brush(stroke_brush, stroke_width, size)
    } else {
        None
    };

    let layout_builder = shaping_builder(text, item_rc, text.wrap(), scale_factor);

    let window_adapter = item_renderer.window().window_adapter();

    let (horizontal_align, vertical_align) = text.alignment();
    let text_overflow = text.overflow();
    let text_color = text.color().color();

    let _ = with_text_layout(
        cache,
        item_rc,
        text,
        &layout_builder,
        LayoutOptions {
            horizontal_align,
            vertical_align,
            max_height: Some(max_height),
            max_width: Some(max_width),
            max_lines: text.line_limit(),
            text_overflow,
        },
        window_adapter.window(),
        |layout| {
            // When `overflow: elide` can't even fit the first line, the line is still drawn
            // (rather than dropped, which would render nothing) but its vertical overflow needs
            // to be clipped like `overflow: clip` would. Horizontal elision still applies, so a
            // line that is both too tall and too wide is clipped vertically and gets an ellipsis
            // horizontally.
            let clip_overflowing_first_line =
                text_overflow == TextOverflow::Elide && layout.first_line_exceeds_height();

            let render = if text_overflow == TextOverflow::Clip || clip_overflowing_first_line {
                item_renderer.save_state();

                item_renderer.combine_clip(
                    LogicalRect::new(LogicalPoint::default(), size),
                    LogicalBorderRadius::zero(),
                )
            } else {
                true
            };

            if render {
                layout.draw(
                    item_renderer,
                    platform_fill_brush,
                    platform_stroke_brush,
                    text_color,
                    // `Text` has no selection today; the machinery is shared so wiring one up
                    // later is a matter of passing spans here.
                    None,
                );
            }

            if text_overflow == TextOverflow::Clip || clip_overflowing_first_line {
                item_renderer.restore_state();
            }
        },
    );
}

#[cfg(feature = "std")]
pub fn link_under_cursor(
    scale_factor: ScaleFactor,
    text: Pin<&dyn crate::item_rendering::RenderText>,
    item_rc: &crate::item_tree::ItemRc,
    size: LogicalSize,
    cursor: PhysicalPoint,
    window: &crate::api::Window,
    cache: Option<&TextLayoutCache>,
) -> Option<(std::string::String, std::string::String)> {
    let layout_builder = shaping_builder(text, Some(item_rc), text.wrap(), scale_factor);

    let (horizontal_align, vertical_align) = text.alignment();

    let source = text.text();
    with_text_layout(
        cache,
        Some(item_rc),
        text,
        &layout_builder,
        LayoutOptions {
            horizontal_align,
            vertical_align,
            max_height: Some(size.height_length()),
            max_width: Some(size.width_length()),
            max_lines: text.line_limit(),
            text_overflow: text.overflow(),
        },
        window,
        |layout| link_in_layout(layout, cursor, &source),
    )
    .flatten()
}

fn link_in_layout(
    layout: &Layout, cursor: PhysicalPoint, source: &PlainOrStyledText,
) -> Option<(String, String)> {
    layout.paragraph_by_y(cursor.y_length()).and_then(|paragraph| {
        let paragraph_y: f64 = paragraph.y.cast::<f64>().get();

        paragraph
            .links
            .iter()
            .find(|(range, _)| {
                let start = parley::editing::Cursor::from_byte_index(
                    &paragraph.layout,
                    range.start,
                    Default::default(),
                );
                let end = parley::editing::Cursor::from_byte_index(
                    &paragraph.layout,
                    range.end,
                    Default::default(),
                );
                let mut clicked = false;
                let link_range = parley::Selection::new(start, end);
                link_range.geometry_with(&paragraph.layout, |mut bounding_box, _line| {
                    bounding_box.y0 += paragraph_y;
                    bounding_box.y1 += paragraph_y;
                    clicked = bounding_box.union(parley::BoundingBox::new(
                        cursor.x.into(),
                        cursor.y.into(),
                        cursor.x.into(),
                        cursor.y.into(),
                    )) == bounding_box;
                });
                clicked
            })
            .map(|(range, link)| {
                // Styled paragraph ranges are intentionally not global source offsets.
                // Identify their source paragraph without adding text copies to cache entries.
                let index = layout.paragraphs.iter()
                    .position(|p| core::ptr::eq(p, paragraph)).unwrap_or_default();
                let visible = match source {
                    PlainOrStyledText::Styled(styled) => styled.paragraphs.get(index)
                        .and_then(|p| p.text.get(range.clone())).unwrap_or_default(),
                    PlainOrStyledText::Plain(text) => text.get(paragraph.range.clone())
                        .and_then(|p| p.get(range.clone())).unwrap_or_default(),
                };
                (link.clone(), visible.into())
            })
    })
}

pub fn draw_text_input(
    item_renderer: &mut impl GlyphRenderer,
    text_input: Pin<&crate::items::TextInput>,
    item_rc: &crate::item_tree::ItemRc,
    size: LogicalSize,
    cache: &TextLayoutCache,
) {
    let width = size.width_length();
    let height = size.height_length();
    if width.get() <= 0. || height.get() <= 0. {
        return;
    }

    let visual_representation = text_input.visual_representation();

    let text_color = visual_representation.text_color.color();
    let Some(platform_fill_brush) =
        item_renderer.platform_text_fill_brush(visual_representation.text_color.clone(), size)
    else {
        return;
    };

    // Android composition is not selection: only paint the IME's actual subrange.
    let selection_range = visual_representation.selection_range.clone();

    let scale_factor = item_renderer.scale_factor();

    let layout_builder =
        shaping_builder(text_input, Some(item_rc), text_input.wrap(), scale_factor);

    let window_adapter = item_renderer.window().window_adapter();

    // The visual text shapes through the shared cache entry like any other text: a selection
    // doesn't make the entry unshareable, because it is applied when drawing, by clipping the
    // runs it cuts across, and never reaches shaping. Cluster ranges and advances are identical
    // with and without one, so a selected `TextInput` hits the same entry as an unselected one --
    // which is what keeps dragging a selection, or composing with an IME, from re-shaping the
    // document on every event. A password field shapes a substituted text, but the substitution
    // is the same everywhere, so it is cacheable too.
    let _ = with_text_layout(
        Some(cache),
        Some(item_rc),
        text_input,
        &layout_builder,
        LayoutOptions::new_from_textinput(text_input, Some(width), Some(height)),
        window_adapter.window(),
        |layout| {
            item_renderer.save_state();

            let render = item_renderer.combine_clip(
                LogicalRect::new(LogicalPoint::default(), size),
                LogicalBorderRadius::zero(),
            );

            if render {
                // When a piece of text is first selected, it gets an empty range like `1..1`. If
                // the text starts with a multi-byte character then this selection would be within
                // that character and parley would panic, so empty ranges are filtered out. The
                // spans only feed drawing (the highlight fill and the glyph clip), so only the
                // lines the clip lets through need any.
                let selection_spans = if selection_range.is_empty() {
                    SelectionSpans::default()
                } else {
                    layout.selection_geometry(selection_range, &draw::visible_band(item_renderer))
                };
                // Inside the clip, like the glyphs it sits under: a line box taller than the item
                // would otherwise paint the highlight over whatever follows the input.
                for background in selection_spans.backgrounds() {
                    item_renderer.fill_rectangle_with_color(
                        background,
                        text_input.selection_background_color(),
                    );
                }

                // Selected glyphs are recolored by clipping, not by restyling the layout, so that
                // a boundary landing inside a ligature cuts the glyph instead of recoloring all
                // of it.
                let selection = (!selection_spans.is_empty())
                    .then(|| {
                        item_renderer
                            .platform_brush_for_color(&text_input.selection_foreground_color())
                            .map(|foreground| SelectionRendering {
                                spans: &selection_spans,
                                foreground,
                            })
                    })
                    .flatten();

                layout.draw(
                    item_renderer,
                    platform_fill_brush,
                    None,
                    text_color,
                    selection.as_ref(),
                );

                if let Some(cursor_pos) = visual_representation.cursor_position {
                    let cursor_rect = layout.cursor_rect_for_byte_offset(
                        cursor_pos,
                        visual_representation.cursor_affinity,
                        text_input.text_cursor_width() * scale_factor,
                    );
                    item_renderer
                        .fill_rectangle_with_color(cursor_rect, visual_representation.cursor_color);
                }
            }

            item_renderer.restore_state();
        },
    );
}

// The public entry points taking a renderer are generic so that RendererSealed's default
// implementations can pass self. Each is a thin shim that extracts what it needs from the
// renderer and forwards to a monomorphic inner function, so that the layout code is not
// instantiated (and duplicated in the binary) once per renderer type.
pub fn text_size(
    renderer: &(impl RendererSealed + ?Sized),
    text_item: Pin<&dyn crate::item_rendering::RenderString>,
    item_rc: &crate::item_tree::ItemRc,
    max_width: Option<LogicalLength>,
    text_wrap: TextWrap,
    cache: Option<&TextLayoutCache>,
) -> Option<LogicalSize> {
    text_size_impl(
        renderer.scale_factor(),
        renderer.window_adapter(),
        text_item,
        item_rc,
        max_width,
        text_wrap,
        cache,
    )
}

fn text_size_impl(
    scale_factor: Option<ScaleFactor>,
    window_adapter: Option<Rc<dyn WindowAdapter>>,
    text_item: Pin<&dyn crate::item_rendering::RenderString>,
    item_rc: &crate::item_tree::ItemRc,
    max_width: Option<LogicalLength>,
    text_wrap: TextWrap,
    cache: Option<&TextLayoutCache>,
) -> Option<LogicalSize> {
    let scale_factor = scale_factor?;

    // Evaluate the properties that `shape_paragraphs` reads before borrowing font_context: they
    // can trigger property bindings that re-enter text_size for other elements, which would panic
    // on a second borrow_mut(). Afterwards they are clean, so shaping can read them again -- now
    // without re-entering -- inside the cache entry's dependency tracker.
    let _ = text_item.font_request(item_rc);
    let _ = text_item.stroke();
    let _ = text_item.link_color();
    let _ = text_item.text();

    let window_adapter = window_adapter?;

    // Only `layout()`'s elision glyph reads this, and `TextOverflow::Clip` never asks for one.
    let layout_builder = shaping_builder(text_item, Some(item_rc), text_wrap, scale_factor);

    with_text_layout(
        cache,
        Some(item_rc),
        text_item,
        &layout_builder,
        LayoutOptions {
            max_width,
            max_height: None,
            max_lines: text_item.line_limit(),
            horizontal_align: TextHorizontalAlignment::Left,
            vertical_align: TextVerticalAlignment::Top,
            text_overflow: TextOverflow::Clip,
        },
        window_adapter.window(),
        |layout| PhysicalSize::from_lengths(layout.max_width, layout.height) / scale_factor,
    )
}

/// The content widths of the text. See [`crate::renderer::ContentWidths`].
pub fn text_content_widths(
    renderer: &(impl RendererSealed + ?Sized),
    text_item: Pin<&dyn crate::item_rendering::RenderString>,
    item_rc: &crate::item_tree::ItemRc,
    cache: Option<&TextLayoutCache>,
) -> Option<crate::renderer::ContentWidths> {
    text_content_widths_impl(
        renderer.scale_factor(),
        renderer.window_adapter(),
        renderer.slint_context(),
        text_item,
        item_rc,
        cache,
    )
}

fn text_content_widths_impl(
    scale_factor: Option<ScaleFactor>,
    window_adapter: Option<Rc<dyn WindowAdapter>>,
    ctx: Option<crate::SlintContext>,
    text_item: Pin<&dyn crate::item_rendering::RenderString>,
    item_rc: &crate::item_tree::ItemRc,
    cache: Option<&TextLayoutCache>,
) -> Option<crate::renderer::ContentWidths> {
    let scale_factor = scale_factor?;

    // See text_size(): evaluate properties before borrowing font_context. Afterwards they are
    // clean, so `compute` can read them again -- now without re-entering -- inside the cache
    // entry's dependency tracker.
    let _ = text_item.font_request(item_rc);
    let _ = text_item.text();

    let ctx = ctx?;
    let mut font_ctx = ctx.font_context().borrow_mut();

    // Reads the text, the font request and the line limit, so the cache entry depends on all
    // three. Nothing else feeds the widths: the wrap mode, the width and the color don't.
    let compute = |font_ctx: &mut parley::FontContext| {
        let layout_builder =
            shaping::content_widths_builder(text_item.font_request(item_rc), scale_factor);

        let paragraphs_without_linebreaks =
            create_text_paragraphs(&layout_builder, font_ctx, text_item.text(), Color::default());

        // No line breaking needed: parley derives the content widths from the break
        // opportunities. Paragraphs stack vertically, so both widths are the widest.
        // Without wrapping each paragraph is one line, so a line limit drops the paragraphs
        // that are not drawn, from both widths.
        let (min, max) = paragraphs_without_linebreaks
            .iter()
            .take(text_item.line_limit().unwrap_or(usize::MAX))
            .fold((0., 0.), |(min, max), p| {
                let w = p.layout.calculate_content_widths();
                (f32::max(min, w.min), f32::max(max, w.max))
            });
        crate::renderer::ContentWidths {
            min: PhysicalLength::new(min) / scale_factor,
            max: PhysicalLength::new(max) / scale_factor,
        }
    };

    // Without a window there is no way to notice a scale factor change, and the shaped
    // advances are in physical pixels, so an entry kept across one would be wrong.
    let Some((cache, window_adapter)) = cache.zip(window_adapter) else {
        return Some(compute(&mut font_ctx));
    };
    cache.clear_if_scale_factor_changed(window_adapter.window());
    Some(cache.content_widths(item_rc, || compute(&mut font_ctx)))
}

pub fn char_size(
    font_ctx: &mut parley::FontContext,
    text_item: Pin<&dyn crate::item_rendering::HasFont>,
    item_rc: &crate::item_tree::ItemRc,
    ch: char,
) -> Option<LogicalSize> {
    let font_request = text_item.font_request(item_rc);
    let font = font_request.query_fontique(&mut font_ctx.collection, &mut font_ctx.source_cache)?;

    let char_map = font.charmap()?;

    let face = skrifa::FontRef::from_index(font.blob.data(), font.index).unwrap();

    let glyph_index = char_map.map(ch)?;

    let pixel_size = font_request.pixel_size.unwrap_or(DEFAULT_FONT_SIZE);

    let location = face.axes().location(font.synthesis.variation_settings());

    let glyph_metrics = skrifa::metrics::GlyphMetrics::new(
        &face,
        skrifa::instance::Size::new(pixel_size.get()),
        &location,
    );

    let advance_width = LogicalLength::new(glyph_metrics.advance_width(glyph_index.into())?);

    let font_metrics = skrifa::metrics::Metrics::new(
        &face,
        skrifa::instance::Size::new(pixel_size.get()),
        &location,
    );
    let natural_line_height = font_metrics.ascent - font_metrics.descent;
    let line_height = font_request
        .line_height_for_natural_height(natural_line_height)
        .unwrap_or(natural_line_height);

    Some(LogicalSize::from_lengths(advance_width, LogicalLength::new(line_height)))
}

/// The height of one line of text: what a shaped single-line layout reports.
pub fn text_line_height(
    font_ctx: &mut parley::FontContext,
    font_request: &FontRequest,
) -> Option<LogicalLength> {
    let pixel_size = font_request.pixel_size.unwrap_or(DEFAULT_FONT_SIZE);
    shaping::line_height_ratio(font_ctx, font_request).map(|ratio| pixel_size * ratio)
}

pub fn font_metrics(
    font_ctx: &mut parley::FontContext,
    font_request: FontRequest,
) -> crate::items::FontMetrics {
    let logical_pixel_size = font_request.pixel_size.unwrap_or(DEFAULT_FONT_SIZE).get();

    let Some(font) =
        font_request.query_fontique(&mut font_ctx.collection, &mut font_ctx.source_cache)
    else {
        return crate::items::FontMetrics::default();
    };

    let face = skrifa::FontRef::from_index(font.blob.data(), font.index).unwrap();
    let location = face.axes().location(font.synthesis.variation_settings());
    let metrics = face.metrics(skrifa::instance::Size::unscaled(), &location);

    let units_per_em = metrics.units_per_em as f32;

    crate::items::FontMetrics {
        ascent: metrics.ascent * logical_pixel_size / units_per_em,
        descent: metrics.descent * logical_pixel_size / units_per_em,
        x_height: metrics.x_height.unwrap_or_default() * logical_pixel_size / units_per_em,
        cap_height: metrics.cap_height.unwrap_or_default() * logical_pixel_size / units_per_em,
    }
}

pub fn text_input_byte_offset_for_position(
    renderer: &(impl RendererSealed + ?Sized),
    text_input: Pin<&crate::items::TextInput>,
    item_rc: &crate::item_tree::ItemRc,
    pos: LogicalPoint,
    cache: Option<&TextLayoutCache>,
) -> (usize, crate::items::TextCursorAffinity) {
    text_input_byte_offset_for_position_impl(
        renderer.scale_factor(),
        renderer.window_adapter(),
        text_input,
        item_rc,
        pos,
        cache,
    )
}

fn text_input_byte_offset_for_position_impl(
    scale_factor: Option<ScaleFactor>,
    window_adapter: Option<Rc<dyn WindowAdapter>>,
    text_input: Pin<&crate::items::TextInput>,
    item_rc: &crate::item_tree::ItemRc,
    pos: LogicalPoint,
    cache: Option<&TextLayoutCache>,
) -> (usize, crate::items::TextCursorAffinity) {
    let no_hit = (0, crate::items::TextCursorAffinity::NextCharacter);
    let Some(scale_factor) = scale_factor else {
        return no_hit;
    };
    let pos: PhysicalPoint = pos * scale_factor;

    let width = text_input.width();
    let height = text_input.height();
    if width.get() <= 0. || height.get() <= 0. || pos.y < 0. {
        return no_hit;
    }

    let layout_builder =
        shaping_builder(text_input, Some(item_rc), text_input.wrap(), scale_factor);
    let visual_representation = text_input.visual_representation();

    let Some(window_adapter) = window_adapter else {
        return no_hit;
    };

    let (byte_offset, affinity) = with_text_layout(
        cache,
        Some(item_rc),
        text_input,
        &layout_builder,
        LayoutOptions::new_from_textinput(text_input, Some(width), Some(height)),
        window_adapter.window(),
        |layout| layout.byte_offset_from_point(pos),
    )
    .unwrap_or(no_hit);
    (visual_representation.map_byte_offset_from_visual_text_to_actual_text(byte_offset), affinity)
}

pub fn text_input_cursor_rect_for_byte_offset(
    renderer: &(impl RendererSealed + ?Sized),
    text_input: Pin<&crate::items::TextInput>,
    item_rc: &crate::item_tree::ItemRc,
    byte_offset: usize,
    affinity: crate::items::TextCursorAffinity,
    cache: Option<&TextLayoutCache>,
) -> LogicalRect {
    text_input_cursor_rect_for_byte_offset_impl(
        renderer.scale_factor(),
        renderer.window_adapter(),
        text_input,
        item_rc,
        byte_offset,
        affinity,
        cache,
    )
}

fn text_input_cursor_rect_for_byte_offset_impl(
    scale_factor: Option<ScaleFactor>,
    window_adapter: Option<Rc<dyn WindowAdapter>>,
    text_input: Pin<&crate::items::TextInput>,
    item_rc: &crate::item_tree::ItemRc,
    byte_offset: usize,
    affinity: crate::items::TextCursorAffinity,
    cache: Option<&TextLayoutCache>,
) -> LogicalRect {
    let Some(scale_factor) = scale_factor else {
        return LogicalRect::default();
    };

    let layout_builder =
        shaping_builder(text_input, Some(item_rc), text_input.wrap(), scale_factor);

    let width = text_input.width();
    let height = text_input.height();
    if width.get() <= 0. || height.get() <= 0. {
        return LogicalRect::new(
            LogicalPoint::default(),
            LogicalSize::from_lengths(LogicalLength::new(1.0), layout_builder.pixel_size),
        );
    }

    let visual_representation = text_input.visual_representation();
    let cursor_width = text_input.text_cursor_width() * scale_factor;

    let Some(window_adapter) = window_adapter else {
        return LogicalRect::default();
    };

    let byte_offset = visual_representation.map_byte_offset_from_actual_to_visual_text(byte_offset);

    with_text_layout(
        cache,
        Some(item_rc),
        text_input,
        &layout_builder,
        LayoutOptions::new_from_textinput(text_input, Some(width), Some(height)),
        window_adapter.window(),
        |layout| {
            layout.cursor_rect_for_byte_offset(byte_offset, affinity, cursor_width) / scale_factor
        },
    )
    .unwrap_or_default()
}

/// A `TextInput`'s laid-out text, lent to [`with_text_input_layout`]'s callback for one call.
#[allow(dead_code)]
pub struct TextInputLayout<'a> {
    layout: &'a Layout,
    /// The string the paragraphs were shaped from, which [`TextInputParagraph::range`] indexes.
    text: &'a str,
}

#[allow(dead_code)]
impl<'a> TextInputLayout<'a> {
    /// The paragraphs, top to bottom. A hard line break separates two of them and belongs to
    /// neither, since Slint splits the text at `\n` before shaping.
    pub(crate) fn paragraphs(&self) -> impl Iterator<Item = TextInputParagraph<'a>> {
        let (text, y_offset) = (self.text, self.layout.y_offset);
        self.layout.paragraphs.iter().map(move |para| TextInputParagraph {
            range: para.range.clone(),
            text: &text[para.range.clone()],
            layout: &para.layout,
            y: y_offset + para.y,
        })
    }
}

/// One paragraph of a [`TextInputLayout`].
#[allow(dead_code)]
pub(crate) struct TextInputParagraph<'a> {
    /// Byte range within [`TextInputLayout::text`].
    range: Range<usize>,
    /// The slice of that text this paragraph covers.
    text: &'a str,
    /// Its shaped, line-broken and aligned glyphs.
    layout: &'a parley::Layout<Brush>,
    /// Physical y of its top edge, relative to the item's.
    y: PhysicalLength,
}

/// Lays `text_input` out the way `renderer` draws it and lends the result to `f`.
///
/// `f` must not lay text out itself: the cache entry stays checked out for the call, and
/// re-entering it panics.
///
/// Returns `None` if the renderer lays no text out through parley, so the caller can tell that
/// apart from an empty layout.
pub fn with_text_input_layout<R>(
    renderer: &(impl RendererSealed + ?Sized),
    text_input: Pin<&crate::items::TextInput>,
    item_rc: &crate::item_tree::ItemRc,
    size: LogicalSize,
    f: impl FnOnce(TextInputLayout<'_>) -> R,
) -> Option<R> {
    if !renderer.text_input_has_parley_layout(text_input, item_rc) {
        return None;
    }
    with_text_input_layout_impl(
        renderer.scale_factor(),
        renderer.window_adapter(),
        renderer.text_layout_cache(),
        text_input,
        item_rc,
        size,
        f,
    )
}

fn with_text_input_layout_impl<R>(
    scale_factor: Option<ScaleFactor>,
    window_adapter: Option<Rc<dyn WindowAdapter>>,
    cache: Option<&TextLayoutCache>,
    text_input: Pin<&crate::items::TextInput>,
    item_rc: &crate::item_tree::ItemRc,
    size: LogicalSize,
    f: impl FnOnce(TextInputLayout<'_>) -> R,
) -> Option<R> {
    let scale_factor = scale_factor?;
    let window_adapter = window_adapter?;

    let width = size.width_length();
    let height = size.height_length();
    if width.get() <= 0. || height.get() <= 0. {
        return None;
    }

    let layout_builder =
        shaping_builder(text_input, Some(item_rc), text_input.wrap(), scale_factor);

    // `RenderString for TextInput` yields plain text; a styled input doesn't exist.
    let PlainOrStyledText::Plain(text) = crate::item_rendering::RenderString::text(text_input)
    else {
        return None;
    };

    with_text_layout(
        cache,
        Some(item_rc),
        text_input,
        &layout_builder,
        LayoutOptions::new_from_textinput(text_input, Some(width), Some(height)),
        window_adapter.window(),
        |layout| f(TextInputLayout { layout, text: &text }),
    )
}

#[cfg(feature = "accessibility-text")]
mod accessibility;
#[cfg(feature = "accessibility-text")]
pub use accessibility::CachedTextInputAccessibilityState;

#[cfg(test)]
mod chatter_tests {
    use alloc::format;
    use super::*;
    #[test]
    fn elided_rtl_body_keeps_logical_start_after_ltr_label() {
        let mut font_ctx = parley::FontContext {
            collection: fontique::Collection::new(fontique::CollectionOptions {
                system_fonts: false,
                ..Default::default()
            }),
            source_cache: Default::default(),
        };
        let data = include_bytes!("../../../assets/fonts/Inter-Regular.ttf");
        let families = font_ctx.collection.register_fonts(fontique::Blob::new(Arc::new(data)), None);
        font_ctx.collection.set_generic_families(
            fontique::GenericFamily::SansSerif,
            families.iter().map(|(id, _)| *id),
        );
        let builder = LayoutWithoutLineBreaksBuilder::new(
            None,
            TextWrap::NoWrap,
            None,
            ScaleFactor::new(1.0),
        );
        let body = "שלום עולם מה שלומך היום חבר יקר שלי";
        let text = format!("You: \u{2068}{body}\u{2069}");
        let elided = logically_elided_text(&builder, &mut font_ctx, &text, 180.0)
            .expect("narrow preview must elide");
        assert!(elided.starts_with("You: \u{2068}שלום"));
        assert!(elided.ends_with("…\u{2069}"));
        assert!(!elided.contains("חבר"));

        let isolate_only = "You: \u{2068}…\u{2069}";
        let mut isolate_only_layout = builder.build(&mut font_ctx, isolate_only, None, None);
        isolate_only_layout.break_all_lines(None);
        let minimal_width = isolate_only_layout.full_width();
        assert_eq!(
            logically_elided_text(&builder, &mut font_ctx, &text, minimal_width).as_deref(),
            Some(isolate_only)
        );

        // Match draw_text's elided path: shape fresh paragraphs, lay them out at
        // the row width with no wrapping and left alignment, then inspect the
        // same visual runs that the renderer draws.
        let paragraphs = create_text_paragraphs(
            &builder,
            &mut font_ctx,
            PlainOrStyledText::Plain(elided.as_str().into()),
            Color::default(),
        );
        let laid_out = layout(
            &builder,
            &mut font_ctx,
            paragraphs,
            ScaleFactor::new(1.0),
            LayoutOptions {
                max_width: Some(LogicalLength::new(180.0)),
                horizontal_align: TextHorizontalAlignment::Left,
                text_overflow: TextOverflow::Clip,
                ..Default::default()
            },
            None,
        );
        let paragraph = &laid_out.paragraphs[0].layout;
        assert!(!paragraph.is_rtl());
        let first_line = paragraph.lines().next().unwrap();
        let mut x = first_line.metrics().offset + first_line.metrics().inline_min_coord;
        let mut visible_clusters = Vec::new();
        for run in first_line.runs() {
            for cluster in run.visual_clusters() {
                let value = &elided[cluster.text_range()];
                if value.chars().any(|ch| {
                    !ch.is_whitespace()
                        && !matches!(ch, '\u{2066}' | '\u{2067}' | '\u{2068}' | '\u{2069}')
                }) {
                    visible_clusters.push((x, value));
                }
                x += cluster.advance();
            }
        }
        let (leftmost_x, leftmost) = visible_clusters
            .iter()
            .min_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap())
            .unwrap();
        assert_eq!(*leftmost, "Y");
        assert!(*leftmost_x >= 0.0);
        let body_start = "You: \u{2068}".len();
        let ellipsis_at = elided.find('…').unwrap();
        let mut kept_body_clusters = Vec::new();
        for run in first_line.runs() {
            for cluster in run.clusters() {
                let range = cluster.text_range();
                if range.start >= body_start && range.end <= ellipsis_at && !range.is_empty() {
                    kept_body_clusters.push((range.start, &elided[range]));
                }
            }
        }
        kept_body_clusters.sort_by_key(|(start, _)| *start);
        let kept_body: String = kept_body_clusters.into_iter().map(|(_, cluster)| cluster).collect();
        assert!(body.starts_with(&kept_body));
        assert!(kept_body.starts_with("שלום"));
        assert!(paragraph.full_width() <= 180.0);
    }

}
